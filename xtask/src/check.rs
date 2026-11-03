//! `cargo xtask check`: the format, lint and test gate that agents' stop hooks and CI share.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
use serde::Deserialize;

use crate::git::{default_branch, git};

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

/// The seed for property tests run by the check, so a run can be repeated exactly. Plain
/// `cargo test` keeps proptest's random seed and explores new cases.
pub(crate) const PROPTEST_RNG_SEED: &str = "20261031";

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
    /// Whether it depends on sqlx in any section, so its tests need Postgres.
    database: bool,
    /// Whether it commits a sqlx query cache (`.sqlx/`) that must match its queries.
    query_cache: bool,
}

/// A step of the check that failed, with what to do about it.
struct Failure {
    step: &'static str,
    fix: &'static str,
}

/// How the check reaches Postgres.
pub(crate) struct Postgres<'a> {
    /// Makes sure the database tests have a server (`crate::postgres::warm`).
    pub(crate) warm: &'a dyn Fn(&mut dyn Write) -> anyhow::Result<()>,
    /// The cargo-sqlx executable, resolved only when a query cache is checked.
    pub(crate) sqlx: &'a dyn Fn() -> anyhow::Result<PathBuf>,
}

/// What the crates in scope need from Postgres.
struct Database {
    /// Some crate's tests use the database.
    tests: bool,
    /// The directories of the crates whose query cache is checked.
    caches: Vec<PathBuf>,
}

impl Database {
    /// Whether anything in scope needs a running server.
    fn needs_server(&self) -> bool {
        self.tests || !self.caches.is_empty()
    }
}

/// How to bring a stale query cache up to date.
const QUERY_CACHE_FIX: &str = "regenerate the crate's .sqlx/ against a migrated database: \
    `cargo xtask tool cargo-sqlx sqlx database reset -y --source crates/<crate>/migrations \
    --database-url postgres://postgres@127.0.0.1:55432/abc_prepare`, then `cargo xtask --root \
    crates/<crate> tool cargo-sqlx sqlx prepare --database-url \
    postgres://postgres@127.0.0.1:55432/abc_prepare -- --all-targets --all-features --locked`";

/// Runs the check from `root` and reports whether it passed. With `changed`, clippy and the tests
/// cover only the crates that the changes since the merge base with the default branch can affect.
/// Builds go to `target_dir` when given, and wherever cargo is configured to otherwise.
pub(crate) fn run(
    root: &Path,
    changed: bool,
    nextest: &Path,
    postgres: &Postgres<'_>,
    target_dir: Option<&Path>,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    if !review_size(root, out)? {
        return Ok(false);
    }
    let (scope, packages) = if changed {
        let packages = workspace_packages(root)?;
        (affected(&packages, &changed_files(root)?), Some(packages))
    } else {
        (Scope::Workspace, None)
    };
    run_scope(root, &scope, nextest, packages, postgres, target_dir, out)
}

/// Whether the changes since the merge base with the default branch fit the Gemini review's
/// limit, measured on the same diff the review sends, so an oversized change is split before it
/// reaches CI rather than after. Where no default branch is known, as in a fresh clone or CI's
/// checkout, it says so and passes: the review job enforces the limit there.
fn review_size(root: &Path, out: &mut impl Write) -> anyhow::Result<bool> {
    let branch = match default_branch(root) {
        Ok(branch) => branch,
        Err(error) => {
            writeln!(out, "xtask: review size not measured: {error}")?;
            return Ok(true);
        }
    };
    let merge_base = crate::git::merge_base(root, &branch)?;
    let (diff, _) = crate::git::reviewable_diff(root, &merge_base, 3)?;
    if let Err(error) = crate::review::check_size(&diff) {
        writeln!(out, "xtask: since {branch}, {error}")?;
        return Ok(false);
    }
    writeln!(
        out,
        "xtask: review size {} of {} bytes",
        diff.len(),
        crate::review::MAX_DIFF_BYTES
    )?;
    Ok(true)
}

/// What the crates of `scope` among `packages` need from Postgres.
fn database(root: &Path, scope: &Scope, packages: &[Package]) -> Database {
    let in_scope: Vec<&Package> = packages
        .iter()
        .filter(|package| match scope {
            Scope::Workspace => true,
            Scope::Packages { names, .. } => names.contains(&package.name),
        })
        .collect();
    Database {
        tests: in_scope.iter().any(|package| package.database),
        caches: in_scope
            .iter()
            .filter(|package| package.query_cache)
            .map(|package| root.join(&package.dir))
            .collect(),
    }
}

/// Runs each step on `scope` from `root`, with the cargo-nextest executable at `nextest` and
/// Postgres reached through `postgres`, building into `target_dir` when given and wherever cargo is configured
/// to otherwise. `packages` are the workspace's, when already read.
fn run_scope(
    root: &Path,
    scope: &Scope,
    nextest: &Path,
    packages: Option<Vec<Package>>,
    postgres: &Postgres<'_>,
    target_dir: Option<&Path>,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    let mut failures = Vec::new();
    if !step(
        root,
        target_dir,
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
        if step(root, target_dir, out, &mut clippy)? {
            // Clippy passed, so the workspace's metadata reads.
            let packages = match packages {
                Some(packages) => packages,
                None => workspace_packages(root)?,
            };
            let database = database(root, scope, &packages);
            if database.needs_server() {
                (postgres.warm)(out)?;
            }
            if !step(root, target_dir, out, &mut test_command(nextest, scope))? {
                failures.push(Failure {
                    step: "tests",
                    fix: "fix the cause of each failing test above; never skip or weaken a test",
                });
            }
            if let Some(args) = scope.doc_test_args() {
                let mut doc_tests = Command::new(cargo());
                doc_tests.args(["test", "--doc"]).args(args).arg("--locked");
                if !step(root, target_dir, out, &mut doc_tests)? {
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
            for dir in &database.caches {
                if !query_cache_current(root, dir, postgres, target_dir, out)? {
                    failures.push(Failure {
                        step: "query cache",
                        fix: QUERY_CACHE_FIX,
                    });
                }
            }
            let mut docs = Command::new(cargo());
            docs.args(["doc", "--no-deps"])
                .args(scope.cargo_args())
                .arg("--locked")
                .env("RUSTDOCFLAGS", "-D warnings");
            if !step(root, target_dir, out, &mut docs)? {
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

/// Whether the query cache of the crate in `dir` matches its queries, checked against a fresh
/// database of its own that is dropped afterwards.
fn query_cache_current(
    root: &Path,
    dir: &Path,
    postgres: &Postgres<'_>,
    target_dir: Option<&Path>,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    let server = std::env::var(crate::postgres::TEST_DATABASE_URL)
        .unwrap_or_else(|_| crate::postgres::URL.to_owned());
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let url = crate::postgres::with_database(
        &server,
        // Named for its creation time, so a later test sweeps it if a killed check left it behind.
        &format!("abc_query_cache_{created}_{}", std::process::id()),
    )?;
    let [mut reset, mut prepare, mut drop] =
        crate::postgres::query_cache_check(&(postgres.sqlx)()?, dir, &url);
    let current =
        step(root, target_dir, out, &mut reset)? && step(root, target_dir, out, &mut prepare)?;
    step(root, target_dir, out, &mut drop)?;
    Ok(current)
}

/// The nextest run for `scope`, with property tests seeded by [`PROPTEST_RNG_SEED`].
fn test_command(nextest: &Path, scope: &Scope) -> Command {
    let mut command = Command::new(nextest);
    command
        .args(["nextest", "run"])
        .args(scope.cargo_args())
        .arg("--locked")
        .env("PROPTEST_RNG_SEED", PROPTEST_RNG_SEED);
    command
}

/// Cargo as chosen by `cargo xtask`, so every step uses the pinned toolchain.
pub(crate) fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Runs one step from the repository root, building into `target_dir` when given, and reports
/// whether it passed.
fn step(
    root: &Path,
    target_dir: Option<&Path>,
    out: &mut impl Write,
    command: &mut Command,
) -> anyhow::Result<bool> {
    if let Some(dir) = target_dir {
        command.env("CARGO_TARGET_DIR", dir);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    writeln!(out, "xtask: {program} {}", args.join(" "))?;
    if command.get_current_dir().is_none() {
        command.current_dir(root);
    }
    let status = command
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
    name: String,
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
            let database = package
                .dependencies
                .iter()
                .any(|dependency| dependency.name == "sqlx");
            let query_cache = root.join(&dir).join(".sqlx").is_dir();
            Ok(Package {
                name: package.name.clone(),
                dir,
                library,
                path_dependencies,
                database,
                query_cache,
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
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use std::collections::BTreeSet;

    use super::{
        PROPTEST_RNG_SEED, Package, Scope, affected, changed_since, review_size, run, test_command,
    };
    use crate::git::default_branch;
    use crate::testing::{git, repository};
    use crate::tools::Manifest;

    fn package(name: &str, dir: &str, library: bool, path_dependencies: &[&str]) -> Package {
        Package {
            name: name.to_owned(),
            dir: PathBuf::from(dir),
            library,
            path_dependencies: path_dependencies.iter().map(PathBuf::from).collect(),
            database: false,
            query_cache: false,
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
    fn crates_in_scope_decide_what_postgres_is_needed_for() {
        let mut store = package("store", "crates/store", true, &[]);
        store.database = true;
        store.query_cache = true;
        let mut server = package("server", "crates/server", true, &["crates/store"]);
        server.database = true;
        let plain = package("plain", "crates/plain", true, &[]);
        let packages = vec![store, server, plain];
        let root = Path::new("/repo");
        let all = super::database(root, &Scope::Workspace, &packages);
        assert!(all.tests, "the workspace has database tests");
        assert_eq!(
            all.caches,
            [PathBuf::from("/repo/crates/store")],
            "one query cache"
        );
        assert!(all.needs_server(), "so it needs a server");
        let only_plain = super::database(root, &packages_scope(&["plain"]), &packages);
        assert!(!only_plain.tests, "a crate without sqlx needs no database");
        assert!(only_plain.caches.is_empty(), "and has no cache");
        assert!(!only_plain.needs_server(), "nor a server");
        let server_only = super::database(root, &packages_scope(&["server"]), &packages);
        assert!(server_only.tests, "the server's tests use the database");
        assert!(server_only.caches.is_empty(), "but it keeps no cache");
        assert!(server_only.needs_server(), "and need a server");
        let cache_only = super::Database {
            tests: false,
            caches: vec![PathBuf::from("/repo/crates/store")],
        };
        assert!(
            cache_only.needs_server(),
            "checking a cache needs a server too"
        );
    }

    fn packages_scope(names: &[&str]) -> Scope {
        Scope::Packages {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            libraries: BTreeSet::new(),
        }
    }

    #[test]
    fn metadata_reads_which_crates_use_sqlx_and_keep_a_query_cache() {
        let root = crate::repo_root().unwrap();
        let packages = super::workspace_packages(&root).unwrap();
        let find = |name: &str| {
            packages
                .iter()
                .find(|package| package.name == name)
                .unwrap()
        };
        assert!(
            find("abc-store").database && find("abc-store").query_cache,
            "the store"
        );
        assert!(find("abc-server").database, "the server's tests");
        assert!(!find("abc-server").query_cache, "the server has no queries");
        assert!(
            !find("abc-core").database,
            "the core never touches a database"
        );
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

    /// A clone whose origin's default branch is `main`, at the clone's first commit.
    fn clone_with_origin(temp: &Path) -> PathBuf {
        let work = repository(&temp.join("work"));
        let bare = temp.join("origin.git");
        git(temp, &["init", "--quiet", "--bare", bare.to_str().unwrap()]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
        git(&work, &["fetch", "--quiet", "origin"]);
        git(&work, &["remote", "set-head", "origin", "main"]);
        work
    }

    #[test]
    fn a_change_within_the_review_limit_passes_and_reports_its_size() {
        let temp = tempfile::tempdir().unwrap();
        let work = clone_with_origin(temp.path());
        fs::write(work.join("small.txt"), "one line\n").unwrap();
        git(&work, &["add", "small.txt"]);
        git(&work, &["commit", "--quiet", "-m", "small"]);
        let mut out = Vec::new();
        assert!(review_size(&work, &mut out).unwrap());
        let said = String::from_utf8(out).unwrap();
        assert!(said.starts_with("xtask: review size "), "{said}");
        assert!(said.contains(" of 400000 bytes"), "{said}");
    }

    #[test]
    fn a_change_over_the_review_limit_fails_and_says_to_split_it() {
        let temp = tempfile::tempdir().unwrap();
        let work = clone_with_origin(temp.path());
        let line = "x".repeat(99);
        let big = format!("{line}\n").repeat(4_100);
        fs::write(work.join("big.txt"), big).unwrap();
        git(&work, &["add", "big.txt"]);
        git(&work, &["commit", "--quiet", "-m", "big"]);
        let mut out = Vec::new();
        assert!(!review_size(&work, &mut out).unwrap());
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("too large for automated review (the limit is 400000)"),
            "{said}"
        );
        assert!(said.trim_end().ends_with("split the PR"), "{said}");
        assert!(
            said.contains("refs/remotes/origin/main"),
            "names the base: {said}"
        );
    }

    #[test]
    fn without_a_default_branch_the_review_size_is_not_measured() {
        let temp = tempfile::tempdir().unwrap();
        let work = repository(&temp.path().join("work"));
        let mut out = Vec::new();
        assert!(review_size(&work, &mut out).unwrap());
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.starts_with("xtask: review size not measured: "),
            "{said}"
        );
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

    /// A one-crate workspace named `fixture-<name>` whose library is `lib`, pinned to this
    /// repository's toolchain.
    fn fixture(name: &str, lib: &str) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let package = format!("fixture-{name}");
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("Cargo.lock"),
            format!(
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual \
                 editing.\nversion = 4\n\n[[package]]\nname = \"{package}\"\nversion = \"0.1.0\"\n"
            ),
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

    /// Runs the whole check on a fixture named `name`, building inside it so neither
    /// `CARGO_TARGET_DIR` nor a configured target dir is shared, and returns whether it passed and
    /// what it printed.
    fn check_fixture(name: &str, lib: &str) -> (bool, String) {
        let fixture = fixture(name, lib);
        let no_postgres = |_: &mut dyn std::io::Write| anyhow::bail!("fixtures use no database");
        let no_sqlx = || anyhow::bail!("fixtures keep no query cache");
        let postgres = super::Postgres {
            warm: &no_postgres,
            sqlx: &no_sqlx,
        };
        check_in(&fixture, &postgres)
    }

    /// Runs the whole check on `fixture` with Postgres reached through `postgres`.
    fn check_in(fixture: &TempDir, postgres: &super::Postgres<'_>) -> (bool, String) {
        let repo = crate::repo_root().unwrap();
        let nextest = Manifest::load(&repo)
            .unwrap()
            .tool("cargo-nextest")
            .unwrap()
            .installed(&repo)
            .unwrap();
        let target_dir = fixture.path().join("target");
        let mut out = Vec::new();
        let passed = run(
            fixture.path(),
            false,
            &nextest,
            postgres,
            Some(&target_dir),
            &mut out,
        )
        .unwrap();
        (passed, String::from_utf8(out).unwrap())
    }

    /// Checks a fixture that keeps a query cache, with a fake cargo-sqlx whose `prepare` exits
    /// with `prepare_status`; returns whether the check passed, what it printed, whether it
    /// started the server, and the fake's calls.
    fn check_query_cache(name: &str, prepare_status: u8) -> (bool, String, bool, String) {
        let fixture = fixture(name, CLEAN);
        fs::create_dir(fixture.path().join(".sqlx")).unwrap();
        let tools = tempfile::tempdir().unwrap();
        let sqlx = tools.path().join("cargo-sqlx");
        let log = tools.path().join("calls");
        crate::testing::script(
            &sqlx,
            &format!(
                "echo \"$2 $3\" >> {}\ncase \"$2\" in\n  prepare) exit {prepare_status} ;;\n  *) exit 0 ;;\nesac\n",
                log.display()
            ),
        );
        let warmed = std::cell::Cell::new(false);
        let warm = |_: &mut dyn std::io::Write| {
            warmed.set(true);
            Ok(())
        };
        let resolve = || Ok(sqlx.clone());
        let postgres = super::Postgres {
            warm: &warm,
            sqlx: &resolve,
        };
        let (passed, out) = check_in(&fixture, &postgres);
        let calls = fs::read_to_string(&log).unwrap_or_default();
        (passed, out, warmed.get(), calls)
    }

    #[test]
    fn a_current_query_cache_passes_against_a_fresh_database() {
        let (passed, out, warmed, calls) = check_query_cache("cache-current", 0);
        assert!(passed, "{out}");
        assert!(warmed, "the server was started for the cache check");
        assert_eq!(
            calls, "database reset\nprepare --check\ndatabase drop\n",
            "a fresh database, the check, then cleanup"
        );
    }

    #[test]
    fn a_stale_query_cache_fails_and_says_how_to_regenerate_it() {
        let (passed, out, _, calls) = check_query_cache("cache-stale", 1);
        assert!(!passed, "{out}");
        assert!(out.contains("query cache: regenerate"), "{out}");
        assert!(
            calls.ends_with("database drop\n"),
            "the database is dropped anyway: {calls}"
        );
    }

    #[test]
    fn a_clean_workspace_passes() {
        let (passed, out) = check_fixture("clean", CLEAN);
        assert!(passed, "{out}");
        assert!(out.ends_with("xtask check: passed\n"), "{out}");
    }

    #[test]
    fn a_format_error_fails() {
        let (passed, out) = check_fixture(
            "format-error",
            &CLEAN.replace("add_one(value: u32)", "add_one( value: u32 )"),
        );
        assert!(!passed, "{out}");
        assert!(out.contains("  format: run `cargo fmt --all`"), "{out}");
    }

    #[test]
    fn a_clippy_warning_fails() {
        let lib = format!(
            "{CLEAN}\n/// Counts values.\n#[must_use]\npub fn count(values: &Vec<u32>) -> usize {{\n    values.len()\n}}\n"
        );
        let (passed, out) = check_fixture("clippy-warning", &lib);
        assert!(!passed, "a warning is an error: {out}");
        assert!(out.contains("  clippy: "), "{out}");
    }

    #[test]
    fn a_clippy_warning_in_test_code_fails() {
        let lib = CLEAN.replace(
            "    #[test]\n    fn adds_one() {",
            "    fn count(values: &Vec<u32>) -> usize {\n        values.len()\n    }\n\n    #[test]\n    fn counts() {\n        assert_eq!(count(&vec![1]), 1);\n    }\n\n    #[test]\n    fn adds_one() {",
        );
        let (passed, out) = check_fixture("clippy-test-warning", &lib);
        assert!(!passed, "clippy covers test targets too: {out}");
        assert!(out.contains("  clippy: "), "{out}");
    }

    #[test]
    fn a_failing_test_fails() {
        let (passed, out) = check_fixture(
            "failing-test",
            &CLEAN.replace("add_one(1), 2", "add_one(1), 3"),
        );
        assert!(!passed, "{out}");
        assert!(out.contains("  tests: "), "{out}");
    }

    #[test]
    fn a_failing_doctest_fails() {
        let lib = CLEAN.replace(
            "/// Adds one.\n",
            "/// Adds one.\n///\n/// ```\n/// assert_eq!(fixture_failing_doctest::add_one(1), 3);\n/// ```\n",
        );
        let (passed, out) = check_fixture("failing-doctest", &lib);
        assert!(!passed, "{out}");
        assert!(out.contains("  doctests: "), "{out}");
    }

    #[test]
    fn a_broken_doc_link_fails() {
        let lib = CLEAN.replace("/// Adds one.", "/// Adds one, unlike [`subtract_one`].");
        let (passed, out) = check_fixture("broken-doc-link", &lib);
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
    fn property_tests_run_with_a_fixed_seed() {
        let command = test_command(Path::new("cargo-nextest"), &Scope::Workspace);
        let seed = command
            .get_envs()
            .find(|(name, _)| *name == "PROPTEST_RNG_SEED")
            .and_then(|(_, value)| value);
        assert_eq!(
            seed,
            Some(OsStr::new(PROPTEST_RNG_SEED)),
            "the check repeats the same property test cases on every run"
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
