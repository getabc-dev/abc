//! The web app (`web/`): its pinned toolchain, its build and its gates.
//!
//! The pinned pnpm installs `web/node_modules` from `web/pnpm-lock.yaml`, with the pinned node
//! first on `PATH`, so every script it runs, and every tool in `web/node_modules/.bin`, runs on
//! that node. `web/package.json`'s scripts hold each gate's command; this module runs them and
//! says how to fix each one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;

use crate::tools;

/// The web app's directory, relative to the repository root.
pub(crate) const DIR: &str = "web";

/// The built app's page, relative to the repository root: the `abc` binary embeds `web/dist`.
pub(crate) const BUILT: &str = "web/dist/index.html";

/// Where Vitest writes the web app's lcov report, relative to `web/`.
pub(crate) const LCOV: &str = "coverage/lcov.info";

/// The pinned node and pnpm.
#[derive(Debug)]
pub(crate) struct Toolchain {
    node: PathBuf,
    pnpm: PathBuf,
}

impl Toolchain {
    /// The pinned tools, from `tool`.
    pub(crate) fn pinned(tool: &dyn Fn(&str) -> anyhow::Result<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            node: tool("node")?,
            pnpm: tool("pnpm")?,
        })
    }

    /// pnpm in `root`'s `web/`, with node first on `PATH`.
    pub(crate) fn pnpm(&self, root: &Path) -> anyhow::Result<Command> {
        let mut command = Command::new(&self.pnpm);
        command
            .current_dir(root.join(DIR))
            .env("PATH", tools::path_with(&self.node)?);
        Ok(command)
    }

    /// pnpm running `step`.
    pub(crate) fn step(&self, root: &Path, step: &Step) -> anyhow::Result<Command> {
        let mut command = self.pnpm(root)?;
        command.args(step.args);
        Ok(command)
    }
}

/// A step of the web app's build or gates: what it is called, its pnpm arguments and what to do
/// when it fails.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Step {
    pub(crate) name: &'static str,
    pub(crate) args: &'static [&'static str],
    pub(crate) fix: &'static str,
}

/// Installs `web/node_modules` exactly as `web/pnpm-lock.yaml` says.
pub(crate) const INSTALL: Step = Step {
    name: "web install",
    args: &["install", "--frozen-lockfile"],
    fix: "web/pnpm-lock.yaml must match web/package.json, with every package at least 3 days \
          old (pnpm-workspace.yaml's minimumReleaseAge): run `cargo xtask pnpm install` and \
          commit web/pnpm-lock.yaml",
};

/// Builds `web/dist`, and with it `web/src/routeTree.gen.ts`, which the type check reads.
pub(crate) const BUILD: Step = Step {
    name: "web build",
    args: &["run", "--silent", "build"],
    fix: "fix each build error above; `cargo xtask pnpm run build` repeats the build alone",
};

/// The gates, in the order the check runs them, after [`INSTALL`] and [`BUILD`].
pub(crate) const GATES: [Step; 5] = [
    Step {
        name: "web format and lint (Biome)",
        args: &["run", "--silent", "biome"],
        fix: "run `cargo xtask pnpm exec biome check --write` to format and apply safe fixes, \
              then fix what remains; silence a rule only with a `// biome-ignore <rule>: <why>` \
              comment when the code is right",
    },
    Step {
        name: "web lint (Oxlint, type-aware)",
        args: &["run", "--silent", "oxlint"],
        fix: "fix each finding above; silence a rule only with `// oxlint-disable-next-line \
              <rule> -- <why>` when the code is right",
    },
    Step {
        name: "web types (tsc)",
        args: &["run", "--silent", "typecheck"],
        fix: "fix each type error above; `cargo xtask pnpm run typecheck` repeats the check. API \
              types come from Rust (web/src/generated/): change the Rust type and regenerate \
              rather than editing them",
    },
    Step {
        name: "web unused code (knip)",
        args: &["run", "--silent", "knip"],
        fix: "delete each unused file, export or dependency above, or use it; `cargo xtask pnpm \
              run knip` repeats the check",
    },
    Step {
        name: "web tests (Vitest)",
        args: &["run", "--silent", "test"],
        fix: "fix the cause of each failing test above, never skipping or weakening one; \
              `cargo xtask pnpm run test` repeats them",
    },
];

/// Runs Vitest under coverage, writing [`LCOV`] with paths relative to `web/`.
pub(crate) const COVERAGE: Step = Step {
    name: "web tests under coverage (Vitest)",
    args: &["run", "--silent", "coverage"],
    fix: "fix the cause of each failing test above; `cargo xtask pnpm run test` runs them alone",
};

/// Whether a change to `path` can change what the web gates find: anything under `web/`, and the
/// pinned node and pnpm.
pub(crate) fn affected_by(path: &Path) -> bool {
    path.starts_with(DIR) || path == Path::new("xtask/tools.toml")
}

/// Whether the tree at `root` holds a built web app for the `abc` binary to embed.
pub(crate) fn built(root: &Path) -> bool {
    root.join(BUILT).is_file()
}

/// Whether the tree at `root` holds the web app at all.
pub(crate) fn present(root: &Path) -> bool {
    root.join(DIR).join("package.json").is_file()
}

/// What to do about a tree without the web app.
pub(crate) const MISSING: &str = "the tree has no web/package.json; check out the whole repository";

/// The `abc` binary that [`build_binary`] builds, under the target directory.
pub(crate) const BINARY: &str = "release/abc";

/// The target directory [`build_binary`] builds into: the tree's own `target/`, whatever
/// `CARGO_TARGET_DIR` says, so the binary is where `cargo xtask e2e` and the docs look for it.
pub(crate) fn target_dir(root: &Path) -> PathBuf {
    root.join("target")
}

/// Installs and builds the web app at `root`, each command's output going to this process's, and
/// reports whether both passed, saying in `out` what failed and how to fix it. A tree without the
/// web app, such as a test's fixture, needs no build.
pub(crate) fn build_here(
    root: &Path,
    toolchain: &Toolchain,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    if !present(root) {
        return Ok(true);
    }
    if let Some(step) = build(root, toolchain, &mut run_here)? {
        writeln!(out, "xtask: {} failed: {}", step.name, step.fix)?;
        return Ok(false);
    }
    Ok(true)
}

/// Runs `command` with this process's output, and reports whether it passed.
pub(crate) fn run_here(command: &mut Command) -> anyhow::Result<bool> {
    let program = command.get_program().to_string_lossy().into_owned();
    Ok(command
        .status()
        .with_context(|| format!("could not run {program}"))?
        .success())
}

/// Builds the web app, then the `abc` binary that embeds it, in release mode; reports whether
/// both passed, and says what failed and how to fix it in `out`. Each command's own output goes
/// to this process's.
pub(crate) fn build_binary(
    root: &Path,
    toolchain: &Toolchain,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    if !present(root) {
        writeln!(out, "xtask build: {MISSING}")?;
        return Ok(false);
    }
    if !build_here(root, toolchain, out)? {
        return Ok(false);
    }
    let mut cargo = Command::new(crate::check::cargo());
    cargo
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "abc",
            "--target-dir",
        ])
        .arg(target_dir(root))
        .env_remove("CARGO_TARGET_DIR")
        .current_dir(root);
    if !run_here(&mut cargo)? {
        writeln!(
            out,
            "xtask build: cargo build failed: fix each error above; `cargo xtask check` names the \
             fix for each gate"
        )?;
        return Ok(false);
    }
    writeln!(
        out,
        "xtask build: {} serves the web app built in web/dist",
        target_dir(root).join(BINARY).display()
    )?;
    Ok(true)
}

/// Installs the web app's dependencies and builds it, running each step with `step`, which
/// says whether it passed; the first step that fails is returned.
pub(crate) fn build(
    root: &Path,
    toolchain: &Toolchain,
    step: &mut dyn FnMut(&mut Command) -> anyhow::Result<bool>,
) -> anyhow::Result<Option<Step>> {
    for stage in [INSTALL, BUILD] {
        if !step(&mut toolchain.step(root, &stage)?)? {
            return Ok(Some(stage));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{BUILD, COVERAGE, GATES, INSTALL, affected_by, build_binary, build_here};
    use crate::testing::{abc_tree, fake_web, pnpm_calls};

    #[test]
    fn builds_the_web_app_with_node_first_on_the_path() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("tools");
        let toolchain = fake_web(&tools, "");
        let root = temp.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        let mut out = Vec::new();
        assert!(
            build_here(&root, &toolchain, &mut out).unwrap(),
            "a tree without the web app needs no build"
        );
        assert_eq!(pnpm_calls(&tools), "", "nothing ran");

        abc_tree(&root);
        assert!(build_here(&root, &toolchain, &mut out).unwrap());
        assert_eq!(
            pnpm_calls(&tools),
            "install --frozen-lockfile\nrun --silent build\n",
            "installed, then built, in web/"
        );
        assert!(super::built(&root), "web/dist/index.html is there");
    }

    #[test]
    fn a_failed_step_names_its_fix() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("tools");
        let root = temp.path().join("tree");
        abc_tree(&root);
        let mut out = Vec::new();
        assert!(!build_here(&root, &fake_web(&tools, "install"), &mut out).unwrap());
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("web install failed: web/pnpm-lock.yaml must match"),
            "{said}"
        );
        assert_eq!(
            pnpm_calls(&tools),
            "install --frozen-lockfile\n",
            "it stops there"
        );

        let mut built = Vec::new();
        assert!(!build_binary(&root, &fake_web(&tools, "build"), &mut built).unwrap());
        let reported = String::from_utf8(built).unwrap();
        assert!(
            reported.contains("web build failed: fix each build error"),
            "{reported}"
        );
    }

    #[test]
    fn the_binary_needs_the_web_app_and_a_crate_that_builds() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("tools");
        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let mut out = Vec::new();
        assert!(!build_binary(&empty, &fake_web(&tools, ""), &mut out).unwrap());
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("the tree has no web/package.json"), "{said}");

        let root = temp.path().join("tree");
        abc_tree(&root);
        std::fs::write(root.join("src/main.rs"), "fn main() { not rust }\n").unwrap();
        let mut built = Vec::new();
        assert!(!build_binary(&root, &fake_web(&tools, ""), &mut built).unwrap());
        let reported = String::from_utf8(built).unwrap();
        assert!(
            reported.contains("xtask build: cargo build failed"),
            "{reported}"
        );
    }

    #[test]
    fn web_changes_and_the_pinned_toolchain_affect_the_gates() {
        for path in [
            "web/src/app.tsx",
            "web/package.json",
            "web/pnpm-lock.yaml",
            "xtask/tools.toml",
        ] {
            assert!(affected_by(Path::new(path)), "{path}");
        }
        for path in ["crates/abc/src/main.rs", "webhook/a.rs", "xtask/src/web.rs"] {
            assert!(!affected_by(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn every_script_a_step_runs_is_in_package_json() {
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(crate::repo_root().unwrap().join("web/package.json")).unwrap(),
        )
        .unwrap();
        for step in GATES.iter().chain([&BUILD, &COVERAGE]) {
            let script = step.args.last().unwrap();
            assert!(
                manifest["scripts"][script].is_string(),
                "{}: web/package.json has a `{script}` script",
                step.name
            );
            assert_eq!(step.args[..2], ["run", "--silent"], "{}", step.name);
        }
        assert_eq!(INSTALL.args, ["install", "--frozen-lockfile"]);
        for step in GATES.iter().chain([&INSTALL, &BUILD, &COVERAGE]) {
            assert!(
                step.fix.contains('`'),
                "{} names a command or a form",
                step.name
            );
        }
    }
}
