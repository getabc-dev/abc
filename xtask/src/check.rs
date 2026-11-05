//! `cargo xtask check`: the format, lint and test gate that agents' stop hooks and CI share.
//!
//! It covers the repository's two Cargo workspaces, the main one and xtask's own (`xtask/`), and
//! lints its shell scripts with the pinned shellcheck. The fuzz workspace is `cargo xtask fuzz`'s.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, ensure};
use serde::Deserialize;

use crate::git::{default_branch, git};

/// Root files whose change can affect every crate of every workspace.
const SHARED_FILES: [&str; 3] = ["rust-toolchain.toml", "clippy.toml", "rustfmt.toml"];

/// Root directories whose change can affect every crate: cargo and nextest configuration.
const SHARED_DIRS: [&str; 2] = [".cargo", ".config"];

/// A Cargo workspace of the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Workspace {
    /// The directory of its root manifest, relative to the repository root; empty for the main
    /// workspace.
    dir: &'static str,
    /// Files outside its crates, beside [`SHARED_FILES`], whose change can affect all of them.
    files: &'static [&'static str],
    /// Directories outside its crates, beside [`SHARED_DIRS`], whose change can affect all of them.
    dirs: &'static [&'static str],
}

/// The main workspace: the crates abc ships.
pub(crate) const MAIN: Workspace = Workspace {
    dir: "",
    files: &["Cargo.toml", "Cargo.lock"],
    dirs: &[],
};

/// xtask's own workspace. Its tests compare its lints with the main workspace's manifest, read the
/// agent settings and the fuzz workspace's manifest, and check the CI workflows and rulesets.
pub(crate) const XTASK: Workspace = Workspace {
    dir: "xtask",
    files: &["Cargo.toml", ".claude/settings.json", "fuzz/Cargo.toml"],
    dirs: &[".github"],
};

/// The shell scripts of the repository, which shellcheck lints.
pub(crate) const SCRIPTS: [&str; 3] = [
    "xtask/hooks/hook.sh",
    "xtask/vm/entrypoint.sh",
    "xtask/vm/agent-exec",
];

/// What the check covers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tree<'a> {
    pub(crate) workspaces: &'a [Workspace],
    /// Shell scripts, relative to the root.
    pub(crate) scripts: &'a [&'a str],
}

/// This repository: both workspaces and every script.
pub(crate) const REPOSITORY: Tree<'static> = Tree {
    workspaces: &[MAIN, XTASK],
    scripts: &SCRIPTS,
};

impl Workspace {
    /// The directory of its root manifest, relative to the repository root; empty for the main
    /// workspace.
    pub(crate) const fn dir(self) -> &'static str {
        self.dir
    }

    /// The arguments that point cargo at this workspace from the repository root.
    pub(crate) fn manifest_args(self) -> Vec<String> {
        if self.dir.is_empty() {
            Vec::new()
        } else {
            vec![
                "--manifest-path".to_owned(),
                format!("{}/Cargo.toml", self.dir),
            ]
        }
    }

    /// How the workspace is named in the check's findings.
    fn label(self) -> String {
        if self.dir.is_empty() {
            String::new()
        } else {
            format!(" ({}/)", self.dir)
        }
    }

    /// Whether a change to `path` can affect every crate of the workspace.
    fn affected_by(self, path: &Path) -> bool {
        SHARED_FILES
            .iter()
            .chain(self.files)
            .any(|file| path == Path::new(file))
            || SHARED_DIRS
                .iter()
                .chain(self.dirs)
                .any(|dir| path.starts_with(dir))
    }
}

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

/// A workspace member, with paths relative to the repository root.
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
    step: String,
    fix: String,
}

impl Failure {
    fn new(step: &str, workspace: Workspace, fix: &str) -> Self {
        Self {
            step: format!("{step}{}", workspace.label()),
            fix: fix.to_owned(),
        }
    }
}

/// The pinned tools the check runs.
#[derive(Debug)]
pub(crate) struct Tools {
    pub(crate) nextest: PathBuf,
    pub(crate) shellcheck: PathBuf,
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

/// Where a check's steps run and build.
struct Run<'a, W> {
    root: &'a Path,
    tools: &'a Tools,
    postgres: &'a Postgres<'a>,
    /// Builds go here when given, and wherever cargo is configured to otherwise.
    target_dir: Option<&'a Path>,
    out: &'a mut W,
    failures: Vec<Failure>,
}

/// Runs the check of `tree` from `root` and reports whether it passed. With `changed`, clippy and
/// the tests cover only the crates that the changes since the merge base with the default branch
/// can affect. Builds go to `target_dir` when given, and wherever cargo is configured to
/// otherwise. Every step's output goes to `out`.
pub(crate) fn run(
    root: &Path,
    tree: Tree<'_>,
    changed: bool,
    tools: &Tools,
    postgres: &Postgres<'_>,
    target_dir: Option<&Path>,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    if !review_size(root, out)? {
        return Ok(false);
    }
    let changed_files = if changed {
        Some(changed_files(root)?)
    } else {
        None
    };
    let mut run = Run {
        root,
        tools,
        postgres,
        target_dir,
        out,
        failures: Vec::new(),
    };
    run.shellcheck(tree.scripts)?;
    for &workspace in tree.workspaces {
        let (scope, packages) = match &changed_files {
            Some(files) => {
                let packages = workspace_packages(root, workspace)?;
                (affected(workspace, &packages, files), Some(packages))
            }
            None => (Scope::Workspace, None),
        };
        run.workspace(workspace, &scope, packages)?;
    }
    let Run { failures, .. } = run;
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
    if let Err(error) = crate::review::check_size("the diff", diff.len()) {
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

impl<W: Write> Run<'_, W> {
    /// Lints `scripts` with shellcheck, which follows what they source.
    fn shellcheck(&mut self, scripts: &[&str]) -> anyhow::Result<()> {
        if scripts.is_empty() {
            return Ok(());
        }
        let mut shellcheck = Command::new(&self.tools.shellcheck);
        shellcheck.args(["--external-sources", "--"]).args(scripts);
        if !self.step(&mut shellcheck)? {
            self.failures.push(Failure {
                step: "shellcheck".to_owned(),
                fix: "fix each finding above; disable a check only with a `# shellcheck \
                      disable=SC<code>` comment that says why the script is right"
                    .to_owned(),
            });
        }
        Ok(())
    }

    /// Runs each step on `scope` of `workspace`. `packages` are the workspace's, when already
    /// read.
    fn workspace(
        &mut self,
        workspace: Workspace,
        scope: &Scope,
        packages: Option<Vec<Package>>,
    ) -> anyhow::Result<()> {
        let manifest = workspace.manifest_args();
        let mut format = Command::new(cargo());
        format.args(["fmt", "--all", "--check"]).args(&manifest);
        if !self.step(&mut format)? {
            let command: Vec<&str> = ["cargo", "fmt", "--all"]
                .into_iter()
                .chain(manifest.iter().map(String::as_str))
                .collect();
            let fix = format!("run `{}`", command.join(" "));
            self.failures.push(Failure::new("format", workspace, &fix));
        }
        if scope.is_empty() {
            writeln!(
                self.out,
                "xtask: no crate of the workspace{} is affected by the changes; skipping clippy, \
                 tests and docs",
                workspace.label()
            )?;
            return Ok(());
        }
        let mut clippy = Command::new(cargo());
        clippy
            .arg("clippy")
            .args(&manifest)
            .args(scope.cargo_args())
            .args(["--all-targets", "--locked", "--", "-D", "warnings"]);
        if !self.step(&mut clippy)? {
            self.failures.push(Failure::new(
                "clippy",
                workspace,
                "fix each finding above (tests and docs run once clippy passes); silence a lint \
                 only with #[expect(lint, reason = \"...\")] when the code is right",
            ));
            return Ok(());
        }
        // Clippy passed, so the workspace's metadata reads.
        let packages = match packages {
            Some(packages) => packages,
            None => workspace_packages(self.root, workspace)?,
        };
        let database = database(self.root, scope, &packages);
        if database.needs_server() {
            (self.postgres.warm)(self.out)?;
        }
        if !self.step(&mut test_command(&self.tools.nextest, workspace, scope))? {
            self.failures.push(Failure::new(
                "tests",
                workspace,
                "fix the cause of each failing test above; never skip or weaken a test",
            ));
        }
        // A workspace of binaries alone, such as xtask's, has no doctests to run.
        let doc_test_args = if packages.iter().any(|package| package.library) {
            scope.doc_test_args()
        } else {
            None
        };
        if let Some(args) = doc_test_args {
            let mut doc_tests = Command::new(cargo());
            doc_tests
                .args(["test", "--doc"])
                .args(&manifest)
                .args(args)
                .arg("--locked");
            if !self.step(&mut doc_tests)? {
                self.failures.push(Failure::new(
                    "doctests",
                    workspace,
                    "fix each failing example above; examples are tests too",
                ));
            }
        } else {
            writeln!(
                self.out,
                "xtask: no affected crate of the workspace{} has a library; skipping doctests",
                workspace.label()
            )?;
        }
        for dir in &database.caches {
            if !self.query_cache_current(dir)? {
                self.failures
                    .push(Failure::new("query cache", workspace, QUERY_CACHE_FIX));
            }
        }
        let mut docs = Command::new(cargo());
        docs.args(["doc", "--no-deps"])
            .args(&manifest)
            .args(scope.cargo_args())
            .arg("--locked")
            .env("RUSTDOCFLAGS", "-D warnings");
        if !self.step(&mut docs)? {
            self.failures.push(Failure::new(
                "docs",
                workspace,
                "fix each rustdoc warning above, such as a broken intra-doc link",
            ));
        }
        Ok(())
    }

    /// Whether the query cache of the crate in `dir` matches its queries, checked against a fresh
    /// database of its own that is dropped afterwards.
    fn query_cache_current(&mut self, dir: &Path) -> anyhow::Result<bool> {
        let server = std::env::var(crate::postgres::TEST_DATABASE_URL)
            .unwrap_or_else(|_| crate::postgres::URL.to_owned());
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let url = crate::postgres::with_database(
            &server,
            // Named for its creation time, so a later test sweeps it if a killed check left it
            // behind.
            &format!("abc_query_cache_{created}_{}", std::process::id()),
        )?;
        let [mut reset, mut prepare, mut drop] =
            crate::postgres::query_cache_check(&(self.postgres.sqlx)()?, dir, &url);
        let current = self.step(&mut reset)? && self.step(&mut prepare)?;
        self.step(&mut drop)?;
        Ok(current)
    }

    /// Runs one step from the repository root, building into the run's target directory when it
    /// has one, and reports whether it passed. The step's output, stdout and stderr together, goes
    /// to the run's `out`: a hook's stdout carries only its answer, and the agent reads why a
    /// step failed from what the hook says.
    fn step(&mut self, command: &mut Command) -> anyhow::Result<bool> {
        if let Some(dir) = self.target_dir {
            command.env("CARGO_TARGET_DIR", dir);
        }
        let program = command.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        writeln!(self.out, "xtask: {program} {}", args.join(" "))?;
        if command.get_current_dir().is_none() {
            command.current_dir(self.root);
        }
        let (mut output, writer) = std::io::pipe().context("could not open a pipe")?;
        command
            .stdin(Stdio::null())
            .stdout(writer.try_clone().context("could not share a pipe")?)
            .stderr(writer);
        let spawned = command.spawn();
        // The command holds the pipe's write ends until they are replaced; let them go, so the
        // copy below ends when the step and whatever it started are done.
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawned.with_context(|| format!("could not run {program}"))?;
        std::io::copy(&mut output, self.out)
            .with_context(|| format!("could not pass on what {program} printed"))?;
        let status = child
            .wait()
            .with_context(|| format!("could not wait for {program}"))?;
        Ok(status.success())
    }
}

/// The nextest run for `scope` of `workspace`, with property tests seeded by
/// [`PROPTEST_RNG_SEED`].
fn test_command(nextest: &Path, workspace: Workspace, scope: &Scope) -> Command {
    let mut command = Command::new(nextest);
    command
        .args(["nextest", "run"])
        .args(workspace.manifest_args())
        .args(scope.cargo_args())
        .arg("--locked")
        .env("PROPTEST_RNG_SEED", PROPTEST_RNG_SEED);
    command
}

/// Cargo as chosen by `cargo xtask`, so every step uses the pinned toolchain.
pub(crate) fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
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

/// The members of `workspace` in the repository at `root`, with paths relative to `root`.
fn workspace_packages(root: &Path, workspace: Workspace) -> anyhow::Result<Vec<Package>> {
    let output = Command::new(cargo())
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .args(workspace.manifest_args())
        .current_dir(root)
        .output()
        .context("could not run cargo metadata")?;
    ensure!(
        output.status.success(),
        "cargo metadata{} failed: {}",
        workspace.label(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("could not parse cargo metadata")?;
    let relative = |path: &Path| {
        path.strip_prefix(&metadata.workspace_root)
            .ok()
            .map(|inside| Path::new(workspace.dir).join(inside))
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

/// The crates of `workspace`, its members being `packages`, whose lints or tests the changed files
/// can affect: the crates containing them and every crate that depends on those, directly or not.
fn affected(workspace: Workspace, packages: &[Package], changed: &[PathBuf]) -> Scope {
    let mut names = BTreeSet::new();
    for path in changed {
        if workspace.affected_by(path) {
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
        MAIN, PROPTEST_RNG_SEED, Package, Scope, Tools, Tree, XTASK, affected, changed_since,
        review_size, run, test_command,
    };
    use crate::git::default_branch;
    use crate::testing::{clone_with_origin, git, repository};

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
                "abc-protocol-bench",
                "crates/abc-protocol/bench",
                false,
                &["crates/abc-protocol"],
            ),
        ]
    }

    fn scope_for(changed: &[&str]) -> Scope {
        let changed: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
        affected(MAIN, &workspace(), &changed)
    }

    /// The scope of xtask's own workspace for `changed`.
    fn xtask_scope_for(changed: &[&str]) -> Scope {
        let changed: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
        affected(XTASK, &[package("xtask", "xtask", false, &[])], &changed)
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
    fn a_workspace_is_labelled_by_its_directory_except_the_main_one() {
        assert_eq!(MAIN.label(), "");
        assert_eq!(XTASK.label(), " (xtask/)");
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
        let packages = super::workspace_packages(&root, MAIN).unwrap();
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
            packages(&["abc", "abc-protocol", "abc-protocol-bench", "abc-worker"]),
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
            scope_for(&["xtask/src/main.rs"]),
            packages(&[]),
            "xtask is a workspace of its own"
        );
        assert_eq!(
            scope_for(&["fuzz/fuzz_targets/decode.rs"]),
            packages(&[]),
            "the fuzz workspace is not a member; `cargo xtask fuzz` lints it"
        );
    }

    #[test]
    fn xtask_is_checked_for_its_own_files_and_the_main_manifest() {
        let xtask = Scope::Packages {
            names: BTreeSet::from(["xtask".to_owned()]),
            libraries: BTreeSet::new(),
        };
        assert_eq!(
            xtask_scope_for(&["xtask/tools.toml"]),
            xtask,
            "any file in the crate counts"
        );
        assert_eq!(
            xtask_scope_for(&["xtask/Cargo.lock"]),
            xtask,
            "its own lockfile is in it"
        );
        for file in [
            ".claude/settings.json",
            "fuzz/Cargo.toml",
            ".github/workflows/ci.yml",
            ".github/rulesets/main.json",
        ] {
            assert_eq!(
                xtask_scope_for(&[file]),
                Scope::Workspace,
                "{file}: a test of xtask reads it"
            );
        }
        assert!(
            xtask_scope_for(&["fuzz/seeds/decode/a", ".claude/other.json"]).is_empty(),
            "only the files its tests read"
        );
        for file in ["Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml"] {
            assert_eq!(
                xtask_scope_for(&[file]),
                Scope::Workspace,
                "{file}: its tests compare its lints with the main manifest's, and the toolchain \
                 and cargo configuration are shared"
            );
        }
        assert!(
            xtask_scope_for(&["Cargo.lock", "crates/abc-server/Cargo.toml"]).is_empty(),
            "the main workspace's crates and lockfile are not xtask's"
        );
    }

    #[test]
    fn xtask_metadata_reads_from_its_own_workspace() {
        let root = crate::repo_root().unwrap();
        let packages = super::workspace_packages(&root, XTASK).unwrap();
        let names: Vec<(&str, &Path)> = packages
            .iter()
            .map(|package| (package.name.as_str(), package.dir.as_path()))
            .collect();
        assert_eq!(
            names,
            [("xtask", Path::new("xtask"))],
            "one crate, its directory relative to the repository root"
        );
        assert!(
            super::workspace_packages(&root, MAIN)
                .unwrap()
                .iter()
                .all(|package| package.name != "xtask"),
            "and not a member of the main workspace"
        );
    }

    #[test]
    fn the_innermost_crate_owns_a_file() {
        assert_eq!(
            scope_for(&["crates/abc-protocol/bench/src/main.rs"]),
            packages(&["abc-protocol-bench"]),
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
    fn changed_files_are_those_since_the_merge_base_with_the_default_branch() {
        let temp = tempfile::tempdir().unwrap();
        let work = repository(&temp.path().join("work"));
        let bare = temp.path().join("origin.git");
        git(
            temp.path(),
            &["init", "--quiet", "--bare", bare.to_str().unwrap()],
        );
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
        git(&work, &["fetch", "--quiet", "origin"]);
        git(&work, &["remote", "set-head", "origin", "main"]);
        assert_eq!(
            super::changed_files(&work).unwrap(),
            Vec::<PathBuf>::new(),
            "nothing yet"
        );
        fs::write(work.join("new"), "x\n").unwrap();
        git(&work, &["add", "new"]);
        git(&work, &["commit", "--quiet", "-m", "change"]);
        assert_eq!(
            super::changed_files(&work).unwrap(),
            [PathBuf::from("new")],
            "the branch's own commit"
        );
    }

    #[test]
    fn a_scope_is_empty_only_when_it_names_no_crate() {
        assert!(packages(&[]).is_empty());
        assert!(!packages(&["abc"]).is_empty());
        assert!(!Scope::Workspace.is_empty());
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
        let nextest = crate::testing::pinned_for_tests("cargo-nextest");
        let target_dir = fixture.path().join("target");
        let mut out = Vec::new();
        let tools = Tools {
            nextest,
            shellcheck: crate::testing::pinned_for_tests("shellcheck"),
        };
        let tree = Tree {
            workspaces: &[MAIN],
            scripts: &[],
        };
        let passed = run(
            fixture.path(),
            tree,
            false,
            &tools,
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

    /// Runs only the shellcheck step on `scripts` of `dir`.
    fn shellcheck_in(dir: &Path, scripts: &[&str]) -> (bool, String) {
        let tools = Tools {
            nextest: PathBuf::from("unused"),
            shellcheck: crate::testing::pinned_for_tests("shellcheck"),
        };
        let unused = |_: &mut dyn std::io::Write| anyhow::bail!("no database");
        let no_sqlx = || anyhow::bail!("no query cache");
        let postgres = super::Postgres {
            warm: &unused,
            sqlx: &no_sqlx,
        };
        let tree = Tree {
            workspaces: &[],
            scripts,
        };
        let mut out = Vec::new();
        let passed = run(dir, tree, false, &tools, &postgres, None, &mut out).unwrap();
        (passed, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_shellcheck_finding_fails_and_a_clean_script_passes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("run.sh"), "#!/bin/sh\necho $1\n").unwrap();
        let (passed, out) = shellcheck_in(dir.path(), &["run.sh"]);
        assert!(!passed, "{out}");
        assert!(out.contains("SC2086"), "the finding is shown: {out}");
        assert!(out.contains("  shellcheck: fix each finding"), "{out}");
        fs::write(dir.path().join("run.sh"), "#!/bin/sh\necho \"$1\"\n").unwrap();
        let (clean, said) = shellcheck_in(dir.path(), &["run.sh"]);
        assert!(clean, "{said}");
    }

    #[test]
    fn the_repositorys_scripts_are_all_listed() {
        let root = crate::repo_root().unwrap();
        let listed = git(
            &root,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "xtask/hooks",
                "xtask/vm",
            ],
        );
        let scripts: BTreeSet<&str> = listed
            .lines()
            .filter(|file| {
                fs::read_to_string(root.join(file)).is_ok_and(|text| text.starts_with("#!/bin/sh"))
            })
            .collect();
        assert_eq!(
            scripts,
            super::SCRIPTS.into_iter().collect::<BTreeSet<_>>(),
            "shellcheck lints every shell script"
        );
    }

    #[test]
    fn the_xtask_workspace_is_reached_by_its_manifest() {
        assert_eq!(
            XTASK.manifest_args(),
            ["--manifest-path", "xtask/Cargo.toml"],
            "from the repository root"
        );
        assert!(
            MAIN.manifest_args().is_empty(),
            "the main workspace is the root's"
        );
        let command = test_command(Path::new("cargo-nextest"), XTASK, &Scope::Workspace);
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "nextest",
                "run",
                "--manifest-path",
                "xtask/Cargo.toml",
                "--workspace",
                "--locked"
            ],
            "nextest runs xtask's own tests"
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
            packages(&["abc"]).doc_test_args(),
            None,
            "nothing to doctest"
        );
    }

    #[test]
    fn property_tests_run_with_a_fixed_seed() {
        let command = test_command(Path::new("cargo-nextest"), MAIN, &Scope::Workspace);
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
            packages(&["abc", "abc-worker"]).cargo_args(),
            ["-p", "abc", "-p", "abc-worker"],
            "one -p per crate"
        );
    }
}
