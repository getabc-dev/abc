//! `cargo xtask check`: the format, lint and test gate that agents' stop hooks and CI share.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};
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

/// Remotes whose `HEAD` names the default branch `--changed` diffs against, in order of
/// preference: a fork's `upstream`, then `origin`.
const DEFAULT_BRANCH_REMOTES: [&str; 2] = ["upstream", "origin"];

/// Which crates the lint, test and documentation steps cover.
#[derive(Debug, PartialEq, Eq)]
enum Scope {
    Workspace,
    Packages {
        names: BTreeSet<String>,
        /// The named crates that have a library, the only target doctests run on.
        libraries: BTreeSet<String>,
    },
}

impl Scope {
    fn is_empty(&self) -> bool {
        match self {
            Self::Workspace => false,
            Self::Packages { names, .. } => names.is_empty(),
        }
    }

    fn cargo_args(&self) -> Vec<String> {
        match self {
            Self::Workspace => vec!["--workspace".to_owned()],
            Self::Packages { names, .. } => package_args(names),
        }
    }

    /// Arguments for `cargo test --doc`, or `None` when no crate in scope has a library.
    fn doc_test_args(&self) -> Option<Vec<String>> {
        match self {
            Self::Workspace => Some(vec!["--workspace".to_owned()]),
            Self::Packages { libraries, .. } => {
                (!libraries.is_empty()).then(|| package_args(libraries))
            }
        }
    }
}

fn package_args(names: &BTreeSet<String>) -> Vec<String> {
    names
        .iter()
        .flat_map(|name| ["-p".to_owned(), name.clone()])
        .collect()
}

/// A workspace member, with paths relative to the workspace root.
#[derive(Debug)]
struct Package {
    name: String,
    dir: PathBuf,
    /// Whether it has a library target.
    library: bool,
    /// Directories of the packages it depends on by path, in any dependency section.
    path_dependencies: Vec<PathBuf>,
}

/// A step of the check that failed, with what to do about it.
struct Failure {
    step: &'static str,
    fix: &'static str,
}

/// Runs the check from `root` and reports whether it passed. With `changed`, clippy and the tests
/// cover only the crates that the changes since the merge base with the default branch can affect.
pub(crate) fn run(root: &Path, changed: bool, out: &mut impl Write) -> anyhow::Result<bool> {
    let nextest = Manifest::load(root)?
        .tool("cargo-nextest")?
        .installed(root)?;
    let scope = if changed {
        affected(&workspace_packages(root)?, &changed_files(root)?)
    } else {
        Scope::Workspace
    };
    run_scope(root, &scope, &nextest, out)
}

/// Runs each step on `scope` from `root`, with the cargo-nextest executable at `nextest`.
fn run_scope(
    root: &Path,
    scope: &Scope,
    nextest: &Path,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
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
    if scope.is_empty() {
        writeln!(
            out,
            "xtask: no crate is affected by the changes; skipping clippy, tests and docs"
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
            if let Some(args) = scope.doc_test_args() {
                let mut doc_tests = Command::new(cargo());
                doc_tests.args(["test", "--doc"]).args(args).arg("--locked");
                if !step(root, out, &mut doc_tests)? {
                    failures.push(Failure {
                        step: "doctests",
                        fix: "fix each failing example above; examples are tests too",
                    });
                }
            } else {
                writeln!(
                    out,
                    "xtask: no affected crate has a library; skipping doctests"
                )?;
            }
            let mut docs = Command::new(cargo());
            docs.args(["doc", "--no-deps"])
                .args(scope.cargo_args())
                .arg("--locked")
                .env("RUSTDOCFLAGS", "-D warnings");
            if !step(root, out, &mut docs)? {
                failures.push(Failure {
                    step: "docs",
                    fix: "fix each rustdoc warning above, such as a broken intra-doc link",
                });
            }
        } else {
            failures.push(Failure {
                step: "clippy",
                fix: "fix each finding above (tests and docs run once clippy passes); silence a \
                      lint only with #[expect(lint, reason = \"...\")] when the code is right",
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
    targets: Vec<MetadataTarget>,
    dependencies: Vec<MetadataDependency>,
}

#[derive(Deserialize)]
struct MetadataTarget {
    kind: Vec<String>,
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
            let library = package.targets.iter().any(|target| {
                target
                    .kind
                    .iter()
                    .any(|kind| ["lib", "rlib", "proc-macro"].contains(&kind.as_str()))
            });
            Ok(Package {
                name: package.name.clone(),
                dir,
                library,
                path_dependencies,
            })
        })
        .collect()
}

/// Files changed since the merge base with the default branch.
fn changed_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let branch = default_branch(root)?;
    let merge_base = git(root, &["merge-base", "HEAD", &branch]).with_context(|| {
        format!("could not find the merge base with {branch}; fetch it and try again")
    })?;
    changed_since(root, merge_base.trim())
}

/// The remote-tracking ref of the repository's default branch, such as `refs/remotes/origin/main`.
fn default_branch(root: &Path) -> anyhow::Result<String> {
    for remote in DEFAULT_BRANCH_REMOTES {
        let output = Command::new("git")
            .args(["symbolic-ref", "--quiet"])
            .arg(format!("refs/remotes/{remote}/HEAD"))
            .current_dir(root)
            .output()
            .context("could not run git")?;
        if output.status.success() {
            let target = String::from_utf8(output.stdout).context("git printed a non-UTF-8 ref")?;
            return Ok(target.trim().to_owned());
        }
    }
    bail!(
        "cannot tell this repository's default branch: neither upstream/HEAD nor origin/HEAD is \
         set; run `git remote set-head origin --auto`"
    )
}

/// Files changed since commit `base`: committed, staged, unstaged and untracked, including deleted
/// ones but not ignored ones.
fn changed_since(root: &Path, base: &str) -> anyhow::Result<Vec<PathBuf>> {
    let tracked = git(root, &["diff", "--name-only", "--no-renames", "-z", base])?;
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
            let libraries = packages
                .iter()
                .filter(|package| package.library && names.contains(&package.name))
                .map(|package| package.name.clone())
                .collect();
            return Scope::Packages { names, libraries };
        }
        names.extend(dependents.into_iter().map(|package| package.name.clone()));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::{Package, Scope, affected, changed_since, default_branch, run_scope};
    use crate::testing::{git, repository};
    use crate::tools::Manifest;

    fn package(name: &str, dir: &str, library: bool, path_dependencies: &[&str]) -> Package {
        Package {
            name: name.to_owned(),
            dir: PathBuf::from(dir),
            library,
            path_dependencies: path_dependencies.iter().map(PathBuf::from).collect(),
        }
    }

    fn workspace() -> Vec<Package> {
        vec![
            package("abc-protocol", "crates/abc-protocol", true, &[]),
            package(
                "abc-worker",
                "crates/abc-worker",
                true,
                &["crates/abc-protocol"],
            ),
            package("abc", "crates/abc", false, &["crates/abc-worker"]),
            package(
                "abc-fuzz",
                "crates/abc-protocol/fuzz",
                false,
                &["crates/abc-protocol"],
            ),
            package("xtask", "xtask", false, &[]),
        ]
    }

    fn scope_for(changed: &[&str]) -> Scope {
        let changed: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
        affected(&workspace(), &changed)
    }

    /// The scope naming `names`, with their libraries taken from [`workspace`].
    fn packages(names: &[&str]) -> Scope {
        let libraries = workspace()
            .into_iter()
            .filter(|package| package.library && names.contains(&package.name.as_str()))
            .map(|package| package.name)
            .collect();
        Scope::Packages {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            libraries,
        }
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
            packages(&[]),
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
            packages(&[]),
            "documentation and CI files need no crate checks"
        );
    }

    #[test]
    fn the_default_branch_comes_from_the_remote_head() {
        let temp = tempfile::tempdir().unwrap();
        let work = repository(&temp.path().join("work"));
        for (remote, branch) in [("origin", "main"), ("upstream", "trunk")] {
            let bare = temp.path().join(format!("{remote}.git"));
            git(
                temp.path(),
                &["init", "--quiet", "--bare", bare.to_str().unwrap()],
            );
            git(&work, &["remote", "add", remote, bare.to_str().unwrap()]);
            git(
                &work,
                &["push", "--quiet", remote, &format!("HEAD:{branch}")],
            );
            git(&work, &["fetch", "--quiet", remote]);
            git(&work, &["remote", "set-head", remote, branch]);
            let expected = format!("refs/remotes/{remote}/{branch}");
            assert_eq!(
                default_branch(&work).unwrap(),
                expected,
                "a fork's upstream wins over origin"
            );
        }
    }

    #[test]
    fn an_unknown_default_branch_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let work = repository(&temp.path().join("work"));
        let error = default_branch(&work).unwrap_err().to_string();
        assert!(
            error.contains("git remote set-head origin --auto"),
            "{error}"
        );
    }

    #[test]
    fn changed_files_cover_commits_staged_unstaged_untracked_and_deleted_files() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        for file in ["committed", "unstaged", "deleted", "untouched"] {
            fs::write(repo.join(file), "before\n").unwrap();
        }
        fs::write(repo.join(".gitignore"), "ignored\n").unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);

        fs::write(repo.join("committed"), "after\n").unwrap();
        git(&repo, &["commit", "--quiet", "--all", "-m", "change"]);
        fs::write(repo.join("staged"), "new\n").unwrap();
        git(&repo, &["add", "staged"]);
        fs::write(repo.join("unstaged"), "after\n").unwrap();
        fs::remove_file(repo.join("deleted")).unwrap();
        fs::write(repo.join("untracked"), "new\n").unwrap();
        fs::write(repo.join("ignored"), "new\n").unwrap();

        let mut changed = changed_since(&repo, base.trim()).unwrap();
        changed.sort();
        assert_eq!(
            changed,
            ["committed", "deleted", "staged", "unstaged", "untracked"].map(PathBuf::from),
            "every change counts, ignored files and untouched ones do not"
        );
    }

    /// A one-crate workspace whose library is `lib`, pinned to this repository's toolchain.
    fn fixture(lib: &str) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual \
             editing.\nversion = 4\n\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::copy(
            crate::repo_root().unwrap().join("rust-toolchain.toml"),
            root.join("rust-toolchain.toml"),
        )
        .unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), lib).unwrap();
        dir
    }

    const CLEAN: &str = "//! A fixture crate.

/// Adds one.
#[must_use]
pub fn add_one(value: u32) -> u32 {
    value.saturating_add(1)
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds_one() {
        assert_eq!(super::add_one(1), 2);
    }
}
";

    /// Runs the whole check on a fixture and returns whether it passed and what it printed.
    fn check_fixture(lib: &str) -> (bool, String) {
        let repo = crate::repo_root().unwrap();
        let nextest = Manifest::load(&repo)
            .unwrap()
            .tool("cargo-nextest")
            .unwrap()
            .installed(&repo)
            .unwrap();
        let fixture = fixture(lib);
        let mut out = Vec::new();
        let passed = run_scope(fixture.path(), &Scope::Workspace, &nextest, &mut out).unwrap();
        (passed, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_clean_workspace_passes() {
        let (passed, out) = check_fixture(CLEAN);
        assert!(passed, "{out}");
        assert!(out.ends_with("xtask check: passed\n"), "{out}");
    }

    #[test]
    fn a_format_error_fails() {
        let (passed, out) =
            check_fixture(&CLEAN.replace("add_one(value: u32)", "add_one( value: u32 )"));
        assert!(!passed, "{out}");
        assert!(out.contains("  format: run `cargo fmt --all`"), "{out}");
    }

    #[test]
    fn a_clippy_warning_fails() {
        let lib = format!(
            "{CLEAN}\n/// Counts values.\n#[must_use]\npub fn count(values: &Vec<u32>) -> usize {{\n    values.len()\n}}\n"
        );
        let (passed, out) = check_fixture(&lib);
        assert!(!passed, "a warning is an error: {out}");
        assert!(out.contains("  clippy: "), "{out}");
    }

    #[test]
    fn a_failing_test_fails() {
        let (passed, out) = check_fixture(&CLEAN.replace("add_one(1), 2", "add_one(1), 3"));
        assert!(!passed, "{out}");
        assert!(out.contains("  tests: "), "{out}");
    }

    #[test]
    fn a_failing_doctest_fails() {
        let lib = CLEAN.replace(
            "/// Adds one.\n",
            "/// Adds one.\n///\n/// ```\n/// assert_eq!(fixture::add_one(1), 3);\n/// ```\n",
        );
        let (passed, out) = check_fixture(&lib);
        assert!(!passed, "{out}");
        assert!(out.contains("  doctests: "), "{out}");
    }

    #[test]
    fn a_broken_doc_link_fails() {
        let lib = CLEAN.replace("/// Adds one.", "/// Adds one, unlike [`subtract_one`].");
        let (passed, out) = check_fixture(&lib);
        assert!(!passed, "{out}");
        assert!(out.contains("  docs: "), "{out}");
    }

    #[test]
    fn doctests_cover_only_crates_with_a_library() {
        assert_eq!(
            packages(&["abc", "abc-worker"]).doc_test_args(),
            Some(vec!["-p".to_owned(), "abc-worker".to_owned()]),
            "a binary-only crate has no doctests"
        );
        assert_eq!(
            packages(&["xtask"]).doc_test_args(),
            None,
            "nothing to doctest"
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
