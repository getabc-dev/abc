//! `cargo xtask check`: the format, lint and test gate that agents' stop hooks and CI share.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
use serde::Deserialize;

use crate::tools::Manifest;

/// Root files whose change can affect every crate.
const WORKSPACE_FILES: [&str; 5] = [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "clippy.toml",
    "rustfmt.toml",
];

/// Root directories whose change can affect every crate: cargo and nextest configuration.
const WORKSPACE_DIRS: [&str; 2] = [".cargo", ".config"];

/// The branch whose merge base `--changed` diffs against.
const BASE: &str = "origin/main";

/// Which crates clippy and the tests cover.
#[derive(Debug, PartialEq, Eq)]
enum Scope {
    Workspace,
    Packages(BTreeSet<String>),
}

impl Scope {
    fn cargo_args(&self) -> Vec<String> {
        match self {
            Self::Workspace => vec!["--workspace".to_owned()],
            Self::Packages(names) => names
                .iter()
                .flat_map(|name| ["-p".to_owned(), name.clone()])
                .collect(),
        }
    }
}

/// A workspace member, with paths relative to the workspace root.
#[derive(Debug)]
struct Package {
    name: String,
    dir: PathBuf,
    /// Directories of the packages it depends on by path, in any dependency section.
    path_dependencies: Vec<PathBuf>,
}

/// A step of the check that failed, with what to do about it.
struct Failure {
    step: &'static str,
    fix: &'static str,
}

/// Runs the check from `root` and reports whether it passed. With `changed`, clippy and the tests
/// cover only the crates the changes since the merge base with [`BASE`] can affect.
pub(crate) fn run(root: &Path, changed: bool, out: &mut impl Write) -> anyhow::Result<bool> {
    let nextest = Manifest::load(root)?
        .tool("cargo-nextest")?
        .installed(root)?;
    let scope = if changed {
        affected(&workspace_packages(root)?, &changed_files(root)?)
    } else {
        Scope::Workspace
    };

    let mut failures = Vec::new();
    if !step(
        root,
        out,
        Command::new(cargo()).args(["fmt", "--all", "--check"]),
    )? {
        failures.push(Failure {
            step: "format",
            fix: "run `cargo fmt --all`",
        });
    }
    if scope == Scope::Packages(BTreeSet::new()) {
        writeln!(
            out,
            "xtask: no crate is affected by the changes; skipping clippy and tests"
        )?;
    } else {
        let mut clippy = Command::new(cargo());
        clippy.arg("clippy").args(scope.cargo_args()).args([
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ]);
        if step(root, out, &mut clippy)? {
            let mut tests = Command::new(nextest);
            tests
                .args(["nextest", "run"])
                .args(scope.cargo_args())
                .arg("--locked");
            if !step(root, out, &mut tests)? {
                failures.push(Failure {
                    step: "tests",
                    fix: "fix the cause of each failing test above; never skip or weaken a test",
                });
            }
        } else {
            failures.push(Failure {
                step: "clippy",
                fix: "fix each finding above (tests run once clippy passes); silence a lint only \
                      with #[expect(lint, reason = \"...\")] when the code is right",
            });
        }
    }

    if failures.is_empty() {
        writeln!(out, "xtask check: passed")?;
        return Ok(true);
    }
    writeln!(out, "xtask check: failed")?;
    for failure in failures {
        writeln!(out, "  {}: {}", failure.step, failure.fix)?;
    }
    Ok(false)
}

/// Cargo as chosen by `cargo xtask`, so every step uses the pinned toolchain.
fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Runs one step from the repository root and reports whether it passed.
fn step(root: &Path, out: &mut impl Write, command: &mut Command) -> anyhow::Result<bool> {
    let program = command.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    writeln!(out, "xtask: {program} {}", args.join(" "))?;
    let status = command
        .current_dir(root)
        .status()
        .with_context(|| format!("could not run {program}"))?;
    Ok(status.success())
}

#[derive(Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    packages: Vec<MetadataPackage>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<MetadataDependency>,
}

#[derive(Deserialize)]
struct MetadataDependency {
    path: Option<PathBuf>,
}

fn workspace_packages(root: &Path) -> anyhow::Result<Vec<Package>> {
    let output = Command::new(cargo())
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(root)
        .output()
        .context("could not run cargo metadata")?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("could not parse cargo metadata")?;
    let relative = |path: &Path| {
        path.strip_prefix(&metadata.workspace_root)
            .ok()
            .map(Path::to_path_buf)
    };
    metadata
        .packages
        .iter()
        .map(|package| {
            let dir = package
                .manifest_path
                .parent()
                .and_then(relative)
                .with_context(|| format!("{} lies outside the workspace", package.name))?;
            let path_dependencies = package
                .dependencies
                .iter()
                .filter_map(|dependency| dependency.path.as_deref().and_then(relative))
                .collect();
            Ok(Package {
                name: package.name.clone(),
                dir,
                path_dependencies,
            })
        })
        .collect()
}

/// Files changed since the merge base with [`BASE`]: committed, staged, unstaged and untracked.
fn changed_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let merge_base = git(root, &["merge-base", "HEAD", BASE]).with_context(|| {
        format!("could not find the merge base with {BASE}; run `git fetch origin main`")
    })?;
    let merge_base = merge_base.trim();
    let tracked = git(
        root,
        &["diff", "--name-only", "--no-renames", "-z", merge_base],
    )?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(tracked
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("could not run git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git printed paths that are not UTF-8")
}

/// The crates whose lints or tests the changed files can affect: the crates containing them and
/// every crate that depends on those, directly or not.
fn affected(packages: &[Package], changed: &[PathBuf]) -> Scope {
    let mut names = BTreeSet::new();
    for path in changed {
        if WORKSPACE_FILES.iter().any(|file| path == Path::new(file))
            || WORKSPACE_DIRS.iter().any(|dir| path.starts_with(dir))
        {
            return Scope::Workspace;
        }
        let owner = packages
            .iter()
            .filter(|package| path.starts_with(&package.dir))
            .max_by_key(|package| package.dir.components().count());
        if let Some(package) = owner {
            names.insert(package.name.clone());
        }
    }
    loop {
        let dependents: Vec<&Package> = packages
            .iter()
            .filter(|package| !names.contains(&package.name))
            .filter(|package| {
                package.path_dependencies.iter().any(|dir| {
                    packages.iter().any(|dependency| {
                        &dependency.dir == dir && names.contains(&dependency.name)
                    })
                })
            })
            .collect();
        if dependents.is_empty() {
            return Scope::Packages(names);
        }
        names.extend(dependents.into_iter().map(|package| package.name.clone()));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use super::{Package, Scope, affected};

    fn package(name: &str, dir: &str, path_dependencies: &[&str]) -> Package {
        Package {
            name: name.to_owned(),
            dir: PathBuf::from(dir),
            path_dependencies: path_dependencies.iter().map(PathBuf::from).collect(),
        }
    }

    fn workspace() -> Vec<Package> {
        vec![
            package("abc-protocol", "crates/abc-protocol", &[]),
            package("abc-worker", "crates/abc-worker", &["crates/abc-protocol"]),
            package("abc", "crates/abc", &["crates/abc-worker"]),
            package(
                "abc-fuzz",
                "crates/abc-protocol/fuzz",
                &["crates/abc-protocol"],
            ),
            package("xtask", "xtask", &[]),
        ]
    }

    fn scope_for(changed: &[&str]) -> Scope {
        let changed: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
        affected(&workspace(), &changed)
    }

    fn packages(names: &[&str]) -> Scope {
        Scope::Packages(names.iter().map(|name| (*name).to_owned()).collect())
    }

    #[test]
    fn a_change_affects_its_crate_and_every_dependent() {
        assert_eq!(
            scope_for(&["crates/abc-protocol/src/lib.rs"]),
            packages(&["abc", "abc-fuzz", "abc-protocol", "abc-worker"]),
            "dependents are found transitively"
        );
    }

    #[test]
    fn a_change_to_a_leaf_affects_only_it() {
        assert_eq!(
            scope_for(&["crates/abc/src/main.rs"]),
            packages(&["abc"]),
            "nothing depends on abc"
        );
        assert_eq!(
            scope_for(&["xtask/tools.toml"]),
            packages(&["xtask"]),
            "any file in a crate counts"
        );
    }

    #[test]
    fn the_innermost_crate_owns_a_file() {
        assert_eq!(
            scope_for(&["crates/abc-protocol/fuzz/src/main.rs"]),
            packages(&["abc-fuzz"]),
            "a crate nested in another owns its files"
        );
    }

    #[test]
    fn crate_directories_match_whole_components() {
        assert_eq!(
            scope_for(&["crates/abc-protocol-extra/src/lib.rs"]),
            Scope::Packages(BTreeSet::new()),
            "crates/abc-protocol is not a prefix of crates/abc-protocol-extra"
        );
    }

    #[test]
    fn workspace_files_affect_every_crate() {
        for file in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "clippy.toml",
            "rustfmt.toml",
            ".cargo/config.toml",
            ".config/nextest.toml",
        ] {
            assert_eq!(
                scope_for(&[file]),
                Scope::Workspace,
                "{file} affects every crate"
            );
        }
    }

    #[test]
    fn files_outside_crates_affect_none() {
        assert_eq!(
            scope_for(&["AGENTS.md", ".github/workflows/ci.yml", "deny.toml"]),
            Scope::Packages(BTreeSet::new()),
            "documentation and CI files need no crate checks"
        );
    }

    #[test]
    fn scopes_become_cargo_arguments() {
        assert_eq!(
            Scope::Workspace.cargo_args(),
            ["--workspace"],
            "the whole workspace"
        );
        assert_eq!(
            packages(&["abc", "xtask"]).cargo_args(),
            ["-p", "abc", "-p", "xtask"],
            "one -p per crate"
        );
    }
}
