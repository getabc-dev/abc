//! `cargo xtask mutants`: mutation testing on the lines changed since the merge base.
//!
//! cargo-mutants changes each changed function in small ways (returns a default, flips an
//! operator) and runs the tests against every change. A mutant the tests do not catch means a
//! behaviour no test pins down; the gate fails on any and lists them, and in GitHub Actions
//! annotates each on its line.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};

use crate::annotation::{self, Annotation, Mode};
use crate::check::{self, PROPTEST_RNG_SEED, Workspace};
use crate::git;
use crate::tools;

/// Where cargo-mutants writes its results, relative to the repository root.
const OUTPUT: &str = "target/mutants";

/// How much longer than the clean test run a mutant's tests may take before they count as hung,
/// for each job: parallel jobs share the CPUs, so each test run slows by up to the number of jobs.
const TIMEOUT_MULTIPLIER_PER_JOB: u32 = 3;

/// The least time, in seconds, a mutant's tests get before they count as hung.
const MINIMUM_TEST_TIMEOUT: &str = "60";

/// The test timeout multiplier for `jobs` mutants tested at once.
const fn timeout_multiplier(jobs: u32) -> u32 {
    TIMEOUT_MULTIPLIER_PER_JOB.saturating_mul(jobs)
}

/// How cargo-mutants ended, from its exit code. It exits with [`Outcome::TimedOut`] when any
/// mutant timed out, even if others were missed, so the result files decide the verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    AllCaught,
    Missed,
    TimedOut,
}

fn outcome(code: Option<i32>) -> anyhow::Result<Outcome> {
    match code {
        Some(0) => Ok(Outcome::AllCaught),
        Some(2) => Ok(Outcome::Missed),
        Some(3) => Ok(Outcome::TimedOut),
        Some(4) => bail!("the tests fail without any mutation; run `cargo xtask check` first"),
        Some(other) => bail!("cargo-mutants failed with exit code {other}"),
        None => bail!("cargo-mutants was stopped by a signal"),
    }
}

/// One of `count` equal shares of the mutants, numbered from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shard {
    index: u32,
    count: u32,
}

impl Shard {
    /// A shard written `k/n`, as cargo-mutants takes it.
    pub(crate) fn parse(text: &str) -> anyhow::Result<Self> {
        let (index, count) = text
            .split_once('/')
            .with_context(|| format!("{text} is not a shard; write k/n, such as 0/4"))?;
        let index: u32 = index
            .parse()
            .with_context(|| format!("{text}: {index} is not a number"))?;
        let count: u32 = count
            .parse()
            .with_context(|| format!("{text}: {count} is not a number"))?;
        ensure!(
            index < count,
            "{text}: the shard number must be below the count"
        );
        Ok(Self { index, count })
    }
}

/// How to run cargo-mutants.
#[derive(Debug)]
pub(crate) struct Options {
    /// How many mutants to build and test at once.
    pub(crate) jobs: u32,
    /// The share of the mutants to test; all of them when `None`.
    pub(crate) shard: Option<Shard>,
    /// Whether to annotate each missed mutant for GitHub too.
    pub(crate) mode: Mode,
}

/// Runs cargo-mutants, with the cargo-mutants and cargo-nextest executables given, on the Rust
/// lines changed since the merge base with `base`, in each workspace of the repository.
///
/// The main workspace's mutants are built in copies of the tree, as many at once as
/// xtask's are tested in place, one at a time: its tests read the repository
/// around the workspace (the main manifest, the hook launcher, the CI workflows), which a copy of
/// the workspace alone would not have. cargo-mutants puts each source file back after its mutants,
/// but a mutant's tests can write into the tree, as a property test that fails does
/// (`proptest-regressions/`), so the run puts xtask's tree back as it found it ([`Tree`]).
pub(crate) fn run(
    root: &Path,
    base: &str,
    options: &Options,
    mutants: &Path,
    nextest: &Path,
    web: &crate::web::Toolchain,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    let commit = git::merge_base(root, base)?;
    let mut passed = true;
    let mut mutated = false;
    for &workspace in check::REPOSITORY.workspaces {
        let diff = workspace_diff(root, &commit, workspace)?;
        if !changes_rust(&diff) {
            continue;
        }
        mutated = true;
        // The main workspace's abc crate embeds the built web app, which the copies of the tree
        // take along.
        if workspace == check::MAIN && !crate::web::build_here(root, web, out)? {
            passed = false;
            continue;
        }
        let run = Run {
            root,
            workspace,
            options,
            mutants,
            nextest,
        };
        passed &= run.mutate(&diff, out)?;
    }
    if !mutated {
        writeln!(out, "mutants: no Rust lines changed; nothing to mutate")?;
    }
    Ok(passed)
}

/// The diff from `commit` of the files of `workspace`, with paths relative to its directory, as
/// cargo-mutants reads them. The main workspace's leaves out the other workspaces.
fn workspace_diff(root: &Path, commit: &str, workspace: Workspace) -> anyhow::Result<String> {
    let mut args = vec![
        "-c".to_owned(),
        "core.quotePath=false".to_owned(),
        "diff".to_owned(),
        "--no-color".to_owned(),
        "--no-ext-diff".to_owned(),
        "-M".to_owned(),
        "-U3".to_owned(),
    ];
    if workspace.dir().is_empty() {
        args.extend([commit.to_owned(), "--".to_owned(), ".".to_owned()]);
        args.extend(
            check::REPOSITORY
                .workspaces
                .iter()
                .filter(|other| !other.dir().is_empty())
                .map(|other| format!(":(exclude){}", other.dir())),
        );
    } else {
        args.extend([
            format!("--relative={}", workspace.dir()),
            commit.to_owned(),
            "--".to_owned(),
            workspace.dir().to_owned(),
        ]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git::git(root, &args)
}

/// One workspace's cargo-mutants run.
struct Run<'a> {
    root: &'a Path,
    workspace: Workspace,
    options: &'a Options,
    mutants: &'a Path,
    nextest: &'a Path,
}

impl Run<'_> {
    /// Where the run's results go: `target/mutants`, and a directory in it for each workspace
    /// but the main one.
    fn output(&self) -> PathBuf {
        self.root.join(OUTPUT).join(self.workspace.dir())
    }

    /// Mutates the lines `diff` changes and reports whether the tests caught every mutant.
    fn mutate(&self, diff: &str, out: &mut impl Write) -> anyhow::Result<bool> {
        let root = self.root;
        let output = self.output();
        fs::create_dir_all(&output)
            .with_context(|| format!("could not create {}", output.display()))?;
        let results = output.join("mutants.out");
        // Results of an earlier run must not decide this one.
        if results.exists() {
            fs::remove_dir_all(&results)
                .with_context(|| format!("could not remove {}", results.display()))?;
        }
        if crate::postgres::needed(root) {
            crate::postgres::warm(out)?;
        }
        let diff_file = output.join("changes.diff");
        fs::write(&diff_file, diff).context("could not write the diff for cargo-mutants")?;
        let in_place = !self.workspace.dir().is_empty();
        writeln!(
            out,
            "xtask: mutating the changed Rust code{}",
            if in_place {
                format!(" of {}/, in place", self.workspace.dir())
            } else {
                String::new()
            }
        )?;
        // cargo-mutants copies the tree, once per job, into the temporary directory and builds
        // each copy there. That directory is small in an agent VM, where the build output has the
        // room.
        let scratch = scratch(root);
        fs::create_dir_all(&scratch)
            .with_context(|| format!("could not create {}", scratch.display()))?;
        // The tests must not inherit that directory: inside the checkout it is inside its git
        // repository and its Cargo workspace, and its paths can be too long for a Unix socket.
        // Cargo's `[env]`, forced, gives every build and test cargo-mutants runs this process's
        // own one back.
        let test_env = output.join("test-env.toml");
        fs::write(&test_env, test_env_config(&std::env::temp_dir())?)
            .with_context(|| format!("could not write {}", test_env.display()))?;
        let mut cargo_config = OsString::from("--cargo-arg=--config=");
        cargo_config.push(&test_env);
        let mut command = Command::new(self.mutants);
        command
            .arg("mutants")
            .arg("--in-diff")
            .arg(&diff_file)
            .args([
                "--test-tool",
                "nextest",
                "--no-shuffle",
                "--colors",
                "never",
            ])
            .arg(&cargo_config)
            .arg("--timeout-multiplier")
            .arg(timeout_multiplier(self.options.jobs).to_string())
            .args(["--minimum-test-timeout", MINIMUM_TEST_TIMEOUT]);
        if in_place {
            command.args(["--dir", self.workspace.dir(), "--in-place"]);
        } else {
            command
                .arg("--jobs")
                .arg(self.options.jobs.to_string())
                // The tests run the pinned tools in .tools/, which .gitignore lists; copy them too.
                .args(["--gitignore", "false"]);
        }
        let before = if in_place {
            Some(Tree::read(&root.join(self.workspace.dir()))?)
        } else {
            None
        };
        let status = command
            .args(
                self.options
                    .shard
                    .map(|shard| {
                        vec![
                            "--shard".to_owned(),
                            format!("{}/{}", shard.index, shard.count),
                        ]
                    })
                    .unwrap_or_default(),
            )
            .arg("--output")
            .arg(&output)
            .env("PATH", tools::path_with(self.nextest)?)
            .env("PROPTEST_RNG_SEED", PROPTEST_RNG_SEED)
            .env("TMPDIR", &scratch)
            .current_dir(root)
            .status()
            .context("could not run cargo-mutants");
        // Whatever cargo-mutants left behind goes, so repeated runs never fill the disk.
        fs::remove_dir_all(&scratch)
            .with_context(|| format!("could not remove {}", scratch.display()))?;
        if let Some(before) = before {
            self.put_back(&before, diff, out)?;
        }
        self.report(&results, outcome(status?.code())?, out)
    }

    /// After an in-place run, puts back what it changed of the workspace as `before` found it,
    /// the files it mutated being those `diff` changes, and reports that and what else changed.
    fn put_back(&self, before: &Tree, diff: &str, out: &mut impl Write) -> anyhow::Result<()> {
        let root = self.root;
        let workspace = self.workspace.dir();
        // The diff of an in-place workspace names its files relative to it.
        let mutated: BTreeSet<PathBuf> = git::added_lines(diff).keys().map(PathBuf::from).collect();
        let done = before.put_back(&root.join(workspace), &mutated)?;
        for change in done.restored {
            writeln!(
                out,
                "mutants: put back {workspace}/{change}, which the run in place left behind"
            )?;
        }
        for (path, what) in done.left {
            writeln!(
                out,
                "mutants: {workspace}/{path} {what} during the run, not by it; left as it is"
            )?;
        }
        Ok(())
    }

    /// Reads the results cargo-mutants wrote in `results` and reports them, with its `result`;
    /// whether the tests caught every mutant.
    fn report(
        &self,
        results: &Path,
        result: Outcome,
        out: &mut impl Write,
    ) -> anyhow::Result<bool> {
        let named = |line: String| {
            if self.workspace.dir().is_empty() {
                line
            } else {
                format!("{}/{line}", self.workspace.dir())
            }
        };
        let missed: Vec<String> = listed(&results.join("missed.txt"))?
            .into_iter()
            .map(named)
            .collect();
        let timed_out: Vec<String> = listed(&results.join("timeout.txt"))?
            .into_iter()
            .map(named)
            .collect();
        let passed = missed.is_empty() && result != Outcome::Missed;
        write!(out, "{}", describe(passed, &missed, &timed_out))?;
        let annotations: Vec<Annotation> = missed.iter().map(|line| annotate(line)).collect();
        write!(
            out,
            "{}",
            annotation::commands(self.options.mode, &annotations)
        )?;
        Ok(passed)
    }
}

/// One entry of a [`Tree`] that is not a directory.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    /// A file, with its contents.
    File(Vec<u8>),
    /// A symbolic link, with its target, never followed.
    Link(PathBuf),
}

/// What [`Tree::put_back`] did and found.
#[derive(Debug, Default, PartialEq, Eq)]
struct PutBack {
    /// What it put back or removed, by path relative to the workspace.
    restored: Vec<String>,
    /// What changed during the run but not by it, left as it is: each path and what happened.
    left: Vec<(String, &'static str)>,
}

/// A workspace's files as they were before an in-place run, to put back afterwards. The workspace
/// holds sources only (its build output is under the root's `target/`), so it is read whole.
#[derive(Debug, PartialEq, Eq)]
struct Tree {
    /// Each file and symbolic link, by its path relative to the workspace.
    entries: BTreeMap<PathBuf, Entry>,
    /// Each directory, by its path relative to the workspace.
    dirs: BTreeSet<PathBuf>,
}

/// Whether the in-place run may change `path`, relative to the workspace: one of the files it
/// mutates, `mutated`, or what proptest records in `proptest-regressions/` when a mutant fails a
/// property test. Anyone else's change is left alone.
fn owned(path: &Path, mutated: &BTreeSet<PathBuf>) -> bool {
    mutated.contains(path)
        || path
            .components()
            .any(|component| component.as_os_str() == "proptest-regressions")
}

impl Tree {
    /// The tree under `dir`. A symbolic link is read as the link, never followed.
    fn read(dir: &Path) -> anyhow::Result<Self> {
        let mut tree = Self {
            entries: BTreeMap::new(),
            dirs: BTreeSet::new(),
        };
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            let here = dir.join(&relative);
            let entries = fs::read_dir(&here)
                .with_context(|| format!("could not read {}", here.display()))?;
            for entry in entries {
                let entry = entry.with_context(|| format!("could not read {}", here.display()))?;
                let path = relative.join(entry.file_name());
                let kind = entry
                    .file_type()
                    .with_context(|| format!("could not read {}", entry.path().display()))?;
                if kind.is_dir() {
                    tree.dirs.insert(path.clone());
                    pending.push(path);
                } else if kind.is_symlink() {
                    let target = fs::read_link(entry.path())
                        .with_context(|| format!("could not read {}", entry.path().display()))?;
                    tree.entries.insert(path, Entry::Link(target));
                } else {
                    let contents = fs::read(entry.path())
                        .with_context(|| format!("could not read {}", entry.path().display()))?;
                    tree.entries.insert(path, Entry::File(contents));
                }
            }
        }
        Ok(tree)
    }

    /// Puts back under `dir` what the run changed, as `self` found it: of the paths it may
    /// change ([`owned`], with the files it mutates, `mutated`), files and directories that
    /// appeared go, and files and links that changed or went come back as they were, links as
    /// links. Anything else that changed is left as it is and reported: someone else changed it.
    fn put_back(&self, dir: &Path, mutated: &BTreeSet<PathBuf>) -> anyhow::Result<PutBack> {
        let now = Self::read(dir)?;
        let mut done = PutBack::default();
        let shown = |path: &Path| path.display().to_string();
        for path in now
            .entries
            .keys()
            .filter(|path| !self.entries.contains_key(*path))
        {
            if owned(path, mutated) {
                let full = dir.join(path);
                // Removes a link itself, never what it points to.
                fs::remove_file(&full)
                    .with_context(|| format!("could not remove {}", full.display()))?;
                done.restored.push(shown(path));
            } else {
                done.left.push((shown(path), "appeared"));
            }
        }
        // Deepest first, so a new directory is empty by the time it goes.
        for path in now
            .dirs
            .iter()
            .rev()
            .filter(|path| !self.dirs.contains(*path))
        {
            let full = dir.join(path);
            let empty = fs::read_dir(&full)
                .with_context(|| format!("could not read {}", full.display()))?
                .next()
                .is_none();
            if owned(path, mutated) && empty {
                fs::remove_dir(&full)
                    .with_context(|| format!("could not remove {}", full.display()))?;
                done.restored.push(format!("{}/", path.display()));
            } else if !done
                .left
                .iter()
                .any(|(left, _)| Path::new(left).starts_with(path))
            {
                done.left.push((format!("{}/", path.display()), "appeared"));
            }
        }
        for (path, entry) in &self.entries {
            let found = now.entries.get(path);
            if found == Some(entry) {
                continue;
            }
            let what = if found.is_some() { "changed" } else { "went" };
            // Only into a directory that is still one, so nothing is written through a link that
            // took a directory's place.
            let parent_is_dir = path
                .parent()
                .is_none_or(|parent| parent.as_os_str().is_empty() || now.dirs.contains(parent));
            if owned(path, mutated) && parent_is_dir {
                restore(&dir.join(path), entry)?;
                done.restored.push(shown(path));
            } else {
                done.left.push((shown(path), what));
            }
        }
        Ok(done)
    }
}

/// Puts `entry` at `path`, replacing whatever is there without following it: the entry is made
/// beside it under a fresh name, then renamed over it, which replaces a link itself.
fn restore(path: &Path, entry: &Entry) -> anyhow::Result<()> {
    let name = path
        .file_name()
        .with_context(|| format!("{} has no name", path.display()))?;
    let mut fresh = OsString::from(".");
    fresh.push(name);
    fresh.push(format!(".xtask-restore-{}", std::process::id()));
    let temporary = path.with_file_name(fresh);
    match entry {
        Entry::File(contents) => fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .and_then(|mut file| file.write_all(contents)),
        Entry::Link(target) => std::os::unix::fs::symlink(target, &temporary),
    }
    .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("could not restore {}", path.display()))
}

/// Cargo configuration that sets `TMPDIR` to `temp` for everything cargo runs, overriding the
/// environment.
fn test_env_config(temp: &Path) -> anyhow::Result<String> {
    let temp = temp.to_str().with_context(|| {
        format!(
            "the temporary directory {} is not UTF-8, so Cargo's configuration cannot name it",
            temp.display()
        )
    })?;
    let mut tmpdir = toml_edit::InlineTable::new();
    tmpdir.insert("value", temp.into());
    tmpdir.insert("force", true.into());
    let mut config = toml_edit::DocumentMut::new();
    let mut env = toml_edit::Table::new();
    env.insert("TMPDIR", toml_edit::value(tmpdir));
    config.insert("env", toml_edit::Item::Table(env));
    Ok(config.to_string())
}

/// Where cargo-mutants copies and builds the tree: under the checkout's build output.
fn scratch(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), |target| root.join(target))
        .join("mutants-tmp")
}

/// Whether the diff adds or changes lines in Rust files.
fn changes_rust(diff: &str) -> bool {
    git::added_lines(diff).keys().any(|path| {
        Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "rs")
    })
}

/// The lines of a cargo-mutants result file; none when it was not written.
fn listed(path: &Path) -> anyhow::Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

/// A missed mutant as cargo-mutants lists it, `<file>:<line>:<column>: <mutation>`, as an
/// annotation on its line; on no line when the list says none.
fn annotate(missed: &str) -> Annotation {
    let mut parts = missed.splitn(4, ':');
    let (file, line, mutation) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(file), Some(line), Some(_column), Some(mutation)) => {
            (file, line.parse::<u64>().ok(), mutation.trim())
        }
        _ => ("", None, missed),
    };
    let annotation = Annotation::error(
        file,
        "Missed mutant",
        format!("No test fails when the code is mutated: {mutation}."),
    );
    match line {
        Some(line) => annotation.at(line, line),
        None => Annotation {
            file: None,
            message: format!("No test fails when the code is mutated: {missed}."),
            ..annotation
        },
    }
}

fn describe(passed: bool, missed: &[String], timed_out: &[String]) -> String {
    let mut text = String::new();
    for (label, mutants) in [
        ("missed", missed),
        ("timed out (counted as caught)", timed_out),
    ] {
        for mutant in mutants {
            text.push_str("  ");
            text.push_str(label);
            text.push_str(": ");
            text.push_str(mutant);
            text.push('\n');
        }
    }
    text.push_str(if passed {
        "mutants: passed; the tests caught every mutant\n"
    } else {
        "mutants: failed; add or sharpen tests so each missed mutant above makes one fail\n"
    });
    text
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{
        Options, Outcome, Shard, annotate, changes_rust, describe, listed, outcome, run,
        test_env_config, timeout_multiplier,
    };
    use crate::annotation::Mode;
    use crate::testing::{repository_with_change, script};

    /// node and pnpm for a fixture, which holds no web app, so they are never run.
    fn no_web() -> crate::web::Toolchain {
        crate::web::Toolchain::pinned(&|name| Ok(Path::new("unused").join(name))).unwrap()
    }

    #[test]
    fn reads_cargo_mutants_exit_codes() {
        assert_eq!(outcome(Some(0)).unwrap(), Outcome::AllCaught, "success");
        assert_eq!(outcome(Some(2)).unwrap(), Outcome::Missed, "missed mutants");
        assert_eq!(outcome(Some(3)).unwrap(), Outcome::TimedOut, "timeouts");
        let baseline = outcome(Some(4)).unwrap_err().to_string();
        assert!(baseline.contains("fail without any mutation"), "{baseline}");
        assert!(outcome(Some(70)).is_err(), "an internal error");
        assert!(outcome(None).is_err(), "a signal");
    }

    #[test]
    fn mutates_only_when_rust_changed() {
        let rust = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(changes_rust(rust), "a Rust change");
        assert!(
            !changes_rust(&rust.replace("a.rs", "a.md")),
            "documentation only"
        );
        assert!(!changes_rust(""), "no change");
    }

    const TWO: Options = Options {
        jobs: 2,
        shard: None,
        mode: Mode::Plain,
    };

    #[test]
    fn the_tests_get_the_callers_temporary_directory_back() {
        let config: toml_edit::DocumentMut = test_env_config(Path::new("/tmp/a \"b\""))
            .unwrap()
            .parse()
            .unwrap();
        let tmpdir = &config["env"]["TMPDIR"];
        assert_eq!(tmpdir["value"].as_str(), Some("/tmp/a \"b\""), "{config}");
        assert_eq!(
            tmpdir["force"].as_bool(),
            Some(true),
            "it overrides cargo-mutants' TMPDIR: {config}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let invalid = std::ffi::OsStr::from_bytes(b"/tmp/\xff");
            let error = test_env_config(Path::new(invalid)).unwrap_err();
            assert!(error.to_string().contains("not UTF-8"), "{error}");
        }
    }

    #[test]
    fn reads_shards() {
        assert_eq!(
            Shard::parse("1/4").unwrap(),
            Shard { index: 1, count: 4 },
            "k/n"
        );
        for bad in ["4/4", "1", "a/4", "1/b"] {
            assert!(Shard::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_timeout_grows_with_the_jobs() {
        assert_eq!(timeout_multiplier(1), 3, "one job");
        assert_eq!(timeout_multiplier(4), 12, "four jobs share the CPUs");
        assert_eq!(timeout_multiplier(u32::MAX), u32::MAX, "never overflows");
    }

    #[test]
    fn reads_result_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("missed.txt");
        fs::write(&file, "src/a.rs:3:5: replace f -> bool with true\n\n").unwrap();
        assert_eq!(
            listed(&file).unwrap(),
            ["src/a.rs:3:5: replace f -> bool with true"],
            "blank lines dropped"
        );
        assert!(
            listed(&dir.path().join("absent.txt")).unwrap().is_empty(),
            "no file"
        );
        assert!(
            listed(dir.path()).is_err(),
            "a directory is not a result file"
        );
    }

    #[test]
    fn gates_a_repository_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let web = no_web();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("src/a.rs", "fn a() -> bool { true }\n")],
        );
        let nextest = temp.path().join("cargo-nextest");
        let missing = temp.path().join("cargo-mutants");
        script(
            &missing,
            "[ \"$1\" = mutants ] || exit 7\nwhile [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; --in-diff) diff=$2 ;; --timeout-multiplier) multiplier=$2 ;; esac; shift; done\ngrep -q 'src/a.rs' \"$diff\" || exit 8\n[ \"$multiplier\" = 6 ] || exit 9\nmkdir -p \"$out/mutants.out\"\necho 'src/a.rs:1:18: replace a -> bool with false' > \"$out/mutants.out/missed.txt\"\nexit 2\n",
        );
        let mut out = Vec::new();
        let passed = run(&repo, "base", &TWO, &missing, &nextest, &web, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!passed, "a missed mutant fails: {text}");
        assert!(
            text.contains("missed: src/a.rs:1:18: replace a -> bool with false"),
            "{text}"
        );
        assert!(
            !text.contains("::error"),
            "no annotations outside GitHub Actions"
        );

        let mixed = temp.path().join("mixed");
        script(
            &mixed,
            "while [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; esac; shift; done\nmkdir -p \"$out/mutants.out\"\necho 'src/a.rs:1:18: replace a -> bool with false' > \"$out/mutants.out/missed.txt\"\necho 'src/a.rs:1:1: replace a with ()' > \"$out/mutants.out/timeout.txt\"\nexit 3\n",
        );
        let mut both = Vec::new();
        assert!(
            !run(&repo, "base", &TWO, &mixed, &nextest, &web, &mut both).unwrap(),
            "a missed mutant fails even when another timed out"
        );
        assert!(
            String::from_utf8(both).unwrap().contains("mutants: failed"),
            "said so"
        );

        let caught = temp.path().join("caught");
        script(
            &caught,
            &format!(
                "while [ $# -gt 0 ]; do case \"$1\" in --shard) shard=$2 ;; --cargo-arg=--config=*) config=${{1#--cargo-arg=--config=}} ;; esac; shift; done\n[ \"$shard\" = 1/4 ] || exit 9\nmkdir \"$TMPDIR/cargo-mutants-copy\"\necho \"$TMPDIR\" > {}\ncp \"$config\" {}\n",
                temp.path().join("tmpdir").display(),
                temp.path().join("config.toml").display()
            ),
        );
        let sharded = Options {
            jobs: 2,
            shard: Some(Shard::parse("1/4").unwrap()),
            mode: Mode::Plain,
        };
        assert!(
            run(
                &repo,
                "base",
                &sharded,
                &caught,
                &nextest,
                &web,
                &mut Vec::new()
            )
            .unwrap(),
            "all caught, in the shard asked for"
        );
        let tmpdir = fs::read_to_string(temp.path().join("tmpdir")).unwrap();
        assert_eq!(
            Path::new(tmpdir.trim()),
            repo.join("target/mutants-tmp"),
            "cargo-mutants copies the tree under the build output"
        );
        assert!(
            !repo.join("target/mutants-tmp").exists(),
            "and the copy is removed afterwards, with what it left in it"
        );
        let config: toml_edit::DocumentMut = fs::read_to_string(temp.path().join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            config["env"]["TMPDIR"]["value"].as_str(),
            std::env::temp_dir().to_str(),
            "while the tests get this process's temporary directory: {config}"
        );

        let docs = repository_with_change(&temp.path().join("docs"), &[("README.md", "# x\n")]);
        let never = temp.path().join("never");
        script(&never, "exit 99\n");
        let mut skipped = Vec::new();
        assert!(
            run(&docs, "base", &TWO, &never, &nextest, &web, &mut skipped).unwrap(),
            "nothing to mutate"
        );
        assert!(
            String::from_utf8(skipped)
                .unwrap()
                .contains("no Rust lines changed"),
            "said so"
        );
    }

    #[test]
    fn the_main_workspace_needs_the_web_app_built_first() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[
                ("web/package.json", "{}\n"),
                ("src/a.rs", "fn a() -> bool { true }\n"),
            ],
        );
        let never = temp.path().join("cargo-mutants");
        script(&never, "exit 7\n");
        let web = temp.path().join("web-tools");
        let mut out = Vec::new();
        let passed = run(
            &repo,
            "base",
            &TWO,
            &never,
            &temp.path().join("cargo-nextest"),
            &crate::testing::fake_web(&web, "build"),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!passed, "{text}");
        assert!(text.contains("web build failed"), "{text}");
        assert_eq!(
            crate::testing::pnpm_calls(&web),
            "install --frozen-lockfile\nrun --silent build\n",
            "cargo-mutants never ran"
        );
    }

    #[test]
    fn mutates_xtask_in_place_with_paths_relative_to_it() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[
                ("xtask/src/a.rs", "fn a() -> bool { true }\n"),
                ("README.md", "# x\n"),
            ],
        );
        let nextest = temp.path().join("cargo-nextest");
        let mutants = temp.path().join("cargo-mutants");
        let args = temp.path().join("args");
        script(
            &mutants,
            &format!(
                "echo \"$*\" > {}\nwhile [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; --in-diff) diff=$2 ;; esac; shift; done\ngrep -q '^+++ b/src/a.rs' \"$diff\" || exit 8\n! grep -q README \"$diff\" || exit 9\nmkdir -p \"$out/mutants.out\"\necho 'src/a.rs:1:18: replace a -> bool with false' > \"$out/mutants.out/missed.txt\"\nexit 2\n",
                args.display()
            ),
        );
        let mut out = Vec::new();
        let actions = Options {
            mode: Mode::Actions,
            ..TWO
        };
        let passed = run(
            &repo,
            "base",
            &actions,
            &mutants,
            &nextest,
            &no_web(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!passed, "{text}");
        assert!(
            text.ends_with(
                "::error file=xtask/src/a.rs,line=1,title=Missed mutant::No test fails when the \
                 code is mutated: replace a -> bool with false.\n"
            ),
            "annotated in GitHub Actions, named from the repository root: {text}"
        );
        assert!(
            text.contains("missed: xtask/src/a.rs:1:18: replace a -> bool with false"),
            "named from the repository root: {text}"
        );
        assert!(text.contains("of xtask/, in place"), "{text}");
        let called = fs::read_to_string(&args).unwrap();
        assert!(called.contains("--dir xtask --in-place"), "{called}");
        assert!(
            !called.contains("--jobs"),
            "one at a time in place: {called}"
        );
        assert!(
            !called.contains("--gitignore"),
            "nothing is copied in place, and cargo-mutants refuses the option there: {called}"
        );
        assert!(
            called.contains(&format!(
                "--output {}",
                repo.join("target/mutants/xtask").display()
            )),
            "{called}"
        );
    }

    /// In-place xtask mutants left stray files behind: a mutant that fails a property test makes
    /// proptest record the case in `proptest-regressions/` beside the source. The run now puts
    /// xtask's tree back as it found it, and leaves nothing in `target/` but its results.
    #[test]
    fn an_in_place_run_leaves_nothing_behind() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[
                ("xtask/src/a.rs", "fn a() -> bool { true }\n"),
                ("xtask/src/b.rs", "fn b() {}\n"),
            ],
        );
        fs::create_dir_all(repo.join("xtask/src/proptest-regressions")).unwrap();
        fs::write(
            repo.join("xtask/src/proptest-regressions/kept.txt"),
            "cc 1\n",
        )
        .unwrap();
        let mutants = temp.path().join("cargo-mutants");
        script(
            &mutants,
            "while [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; esac; shift; done\n\
             mkdir -p xtask/src/proptest-regressions/new/deeper \"$TMPDIR/copy\"\n\
             echo 'cc 2' > xtask/src/proptest-regressions/stop.txt\n\
             echo 'cc 3' >> xtask/src/proptest-regressions/kept.txt\n\
             echo 'x' > xtask/src/proptest-regressions/new/deeper/stray.txt\n\
             echo 'fn a() -> bool { false }' > xtask/src/a.rs\n\
             rm xtask/src/b.rs\n\
             mkdir -p \"$out/mutants.out\"\n",
        );
        let before = super::Tree::read(&repo.join("xtask")).unwrap();
        let mut out = Vec::new();
        assert!(
            run(
                &repo,
                "base",
                &TWO,
                &mutants,
                &temp.path().join("cargo-nextest"),
                &no_web(),
                &mut out
            )
            .unwrap()
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            super::Tree::read(&repo.join("xtask")).unwrap(),
            before,
            "xtask's tree is as the run found it: {text}"
        );
        for put_back in [
            "xtask/src/proptest-regressions/stop.txt",
            "xtask/src/proptest-regressions/kept.txt",
            "xtask/src/proptest-regressions/new/",
            "xtask/src/proptest-regressions/new/deeper/",
            "xtask/src/proptest-regressions/new/deeper/stray.txt",
            "xtask/src/a.rs",
            "xtask/src/b.rs",
        ] {
            assert!(
                text.contains(&format!("mutants: put back {put_back}, ")),
                "{put_back}: {text}"
            );
        }
        let left: Vec<String> = fs::read_dir(repo.join("target"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["mutants"], "only the results stay in target/");
    }

    /// The in-place run put xtask's whole tree back as it found it: an edit made meanwhile by
    /// someone else was silently reverted, a symlink came back as a file holding its target's name,
    /// and a file the run left as a symlink was written through. It now puts back only what the
    /// run may change (the files it mutates and proptest's regressions), as they were, and
    /// reports the rest.
    #[test]
    fn an_in_place_run_puts_back_only_what_it_changed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("xtask/src/a.rs", "fn a() -> bool { true }\n")],
        );
        fs::write(repo.join("xtask/notes.md"), "mine\n").unwrap();
        let regressions = repo.join("xtask/src/proptest-regressions");
        fs::create_dir_all(&regressions).unwrap();
        fs::write(repo.join("xtask/shared.txt"), "cc 0\n").unwrap();
        std::os::unix::fs::symlink("../../shared.txt", regressions.join("linked.txt")).unwrap();
        let outside = temp.path().join("outside.rs");
        fs::write(&outside, "outside\n").unwrap();
        let mutants = temp.path().join("cargo-mutants");
        script(
            &mutants,
            &format!(
                "while [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; esac; shift; done\n\
                 echo 'edited meanwhile' >> xtask/notes.md\n\
                 echo 'new meanwhile' > xtask/new.md\n\
                 rm xtask/src/proptest-regressions/linked.txt\n\
                 echo 'cc 1' > xtask/src/proptest-regressions/linked.txt\n\
                 rm xtask/src/a.rs\n\
                 ln -s {} xtask/src/a.rs\n\
                 mkdir -p \"$out/mutants.out\"\n",
                outside.display()
            ),
        );
        let mut out = Vec::new();
        run(
            &repo,
            "base",
            &TWO,
            &mutants,
            &temp.path().join("cargo-nextest"),
            &no_web(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "outside\n",
            "nothing is written through a symlink: {text}"
        );
        let a = repo.join("xtask/src/a.rs");
        assert!(fs::symlink_metadata(&a).unwrap().is_file(), "{text}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "fn a() -> bool { true }\n");
        assert_eq!(
            fs::read_link(regressions.join("linked.txt")).unwrap(),
            Path::new("../../shared.txt"),
            "a symlink comes back as a symlink: {text}"
        );
        assert_eq!(
            fs::read_to_string(repo.join("xtask/notes.md")).unwrap(),
            "mine\nedited meanwhile\n",
            "someone else's edit is kept: {text}"
        );
        assert!(repo.join("xtask/new.md").is_file(), "{text}");
        for put_back in [
            "xtask/src/a.rs",
            "xtask/src/proptest-regressions/linked.txt",
        ] {
            assert!(
                text.contains(&format!("mutants: put back {put_back}, ")),
                "{put_back}: {text}"
            );
        }
        for left in [
            "mutants: xtask/notes.md changed during the run, not by it; left as it is",
            "mutants: xtask/new.md appeared during the run, not by it; left as it is",
        ] {
            assert!(text.contains(left), "{left}: {text}");
        }
    }

    #[test]
    fn annotates_a_missed_mutant_on_its_line() {
        assert_eq!(
            annotate("src/a.rs:3:5: replace f -> bool with true").to_string(),
            "::error file=src/a.rs,line=3,title=Missed mutant::No test fails when the code is \
             mutated: replace f -> bool with true."
        );
        assert_eq!(
            annotate("something else").to_string(),
            "::error title=Missed mutant::No test fails when the code is mutated: something else.",
            "a line it cannot read is annotated whole"
        );
        assert_eq!(
            annotate("src/a.rs:x:5: replace f with ()").to_string(),
            "::error title=Missed mutant::No test fails when the code is mutated: src/a.rs:x:5: \
             replace f with ().",
        );
    }

    #[test]
    fn describes_the_result() {
        let missed = ["src/a.rs:3:5: replace f -> bool with true".to_owned()];
        let text = describe(false, &missed, &[]);
        assert!(text.starts_with("  missed: src/a.rs:3:5"), "{text}");
        assert!(
            text.ends_with("missed mutant above makes one fail\n"),
            "{text}"
        );
        let timeouts = ["src/b.rs:1:1: replace g with ()".to_owned()];
        let passing = describe(true, &[], &timeouts);
        assert!(
            passing.contains("timed out (counted as caught)"),
            "{passing}"
        );
        assert!(
            passing.ends_with("mutants: passed; the tests caught every mutant\n"),
            "{passing}"
        );
    }
}
