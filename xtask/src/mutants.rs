//! `cargo xtask mutants`: mutation testing on the lines changed since the merge base.
//!
//! cargo-mutants changes each changed function in small ways (returns a default, flips an
//! operator) and runs the tests against every change. A mutant the tests do not catch means a
//! behaviour no test pins down; the gate fails on any and lists them, and in GitHub Actions
//! annotates each on its line.

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

/// The variable that names the checkout to xtask's tests when they run in a copy of `xtask/`
/// alone (`testing::checkout`, in test builds). Only test code reads it, at run time; the
/// shipped binary never does.
pub(crate) const TEST_REPOSITORY: &str = "XTASK_TEST_REPOSITORY";

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

/// cargo-mutants' exit code when the tests fail in the unmutated tree.
const BASELINE_FAILED: i32 = 4;

fn outcome(code: Option<i32>) -> anyhow::Result<Outcome> {
    match code {
        Some(0) => Ok(Outcome::AllCaught),
        Some(2) => Ok(Outcome::Missed),
        Some(3) => Ok(Outcome::TimedOut),
        Some(BASELINE_FAILED) => {
            bail!("the tests fail without any mutation; run `cargo xtask check` first")
        }
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
/// Every workspace's mutants are built and tested in copies of it, as many at once as
/// `options.jobs`, never in the checkout: there, any other `cargo xtask` command would compile the
/// mutated code (on 6 Nov, the pull request watch's `cargo xtask gh` built a mutant). xtask's
/// tests read the repository around their workspace (the main manifest, the hook launcher, the
/// CI workflows), which a copy of `xtask/` alone does not have, so their runs name the checkout
/// in [`TEST_REPOSITORY`].
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
    // The step's annotations, across every workspace's missed mutants.
    let mut step = annotation::Step::new(options.mode);
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
        let result = run.mutate(&diff, &mut step, out);
        if result.is_err() {
            write!(out, "{}", step.finish())?;
        }
        passed &= result?;
    }
    if !mutated {
        writeln!(out, "mutants: no Rust lines changed; nothing to mutate")?;
    }
    write!(out, "{}", step.finish())?;
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
    fn mutate(
        &self,
        diff: &str,
        step: &mut annotation::Step,
        out: &mut impl Write,
    ) -> anyhow::Result<bool> {
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
        writeln!(
            out,
            "xtask: mutating the changed Rust code{}, in copies of it",
            if self.workspace.dir().is_empty() {
                String::new()
            } else {
                format!(" of {}/", self.workspace.dir())
            }
        )?;
        // cargo-mutants copies the tree, once per job, into the temporary directory and builds
        // each copy there. That directory is small in an agent VM, where the build output has the
        // room.
        let scratch = Scratch::create(scratch(root))?;
        let test_env = output.join("test-env.toml");
        fs::write(&test_env, test_env_config(&std::env::temp_dir())?)
            .with_context(|| format!("could not write {}", test_env.display()))?;
        let mut cargo_config = OsString::from("--cargo-arg=--config=");
        cargo_config.push(&test_env);
        // A copy of xtask/ alone has no .config/: nextest gets the checkout's configuration by
        // name, or it would run without its timeouts and write no report.
        let mut nextest_config = OsString::from("--cargo-test-arg=--config-file=");
        nextest_config.push(root.join(check::NEXTEST_CONFIG));
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
            .arg(&nextest_config)
            .arg("--timeout-multiplier")
            .arg(timeout_multiplier(self.options.jobs).to_string())
            .args(["--minimum-test-timeout", MINIMUM_TEST_TIMEOUT]);
        if !self.workspace.dir().is_empty() {
            command.args(["--dir", self.workspace.dir()]);
        }
        command
            .arg("--jobs")
            .arg(self.options.jobs.to_string())
            // The tests run the pinned tools in .tools/, which .gitignore lists; copy them too.
            .args(["--gitignore", "false"])
            // The copies stay until this run removes the scratch directory below, so a failing
            // baseline's test report can be read first.
            .arg("--leak-dirs");
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
            .env(TEST_REPOSITORY, root)
            .env("TMPDIR", scratch.path())
            // Each copy builds into its own target/: a copy of xtask/ alone has no .cargo/, and
            // cargo, looking up from it, would read the checkout's and build every mutant into the
            // checkout's target/, the jobs overwriting each other's binaries. cargo-mutants runs
            // cargo in the copy, so a relative directory is the copy's.
            .env("CARGO_TARGET_DIR", "target")
            .current_dir(root)
            .status()
            .context("could not run cargo-mutants")?;
        if self.options.mode == Mode::Actions && status.code() == Some(BASELINE_FAILED) {
            let failed = baseline_failures(scratch.path(), self.workspace.dir());
            write!(out, "{}", step.emit(&failed))?;
        }
        // The outcome's error, such as a failing baseline, comes first; the scratch directory
        // goes on every path, so repeated runs never fill the disk.
        let result = outcome(status.code())?;
        scratch.remove()?;
        self.report(&results, result, step, out)
    }

    /// Reads the results cargo-mutants wrote in `results` and reports them, with its `result`;
    /// whether the tests caught every mutant.
    fn report(
        &self,
        results: &Path,
        result: Outcome,
        step: &mut annotation::Step,
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
        write!(out, "{}", step.emit(&annotations))?;
        Ok(passed)
    }
}

/// Cargo configuration, forced over the environment, for everything cargo-mutants has cargo
/// build and run: `TMPDIR` is `temp`, this process's own, not cargo-mutants' scratch directory,
/// which is inside the checkout's git repository and Cargo workspace and can make paths too long
/// for a Unix socket.
fn test_env_config(temp: &Path) -> anyhow::Result<String> {
    let text = |path: &Path, what: &str| {
        path.to_str().map(str::to_owned).with_context(|| {
            format!(
                "{what} {} is not UTF-8, so Cargo's configuration cannot name it",
                path.display()
            )
        })
    };
    let forced = |value: String| {
        let mut table = toml_edit::InlineTable::new();
        table.insert("value", value.into());
        table.insert("force", true.into());
        toml_edit::value(table)
    };
    let mut env = toml_edit::Table::new();
    env.insert("TMPDIR", forced(text(temp, "the temporary directory")?));
    let mut config = toml_edit::DocumentMut::new();
    config.insert("env", toml_edit::Item::Table(env));
    Ok(config.to_string())
}

/// The annotations for the tests that failed in cargo-mutants' unmutated copy, from the `JUnit`
/// report nextest wrote in it, among this run's copies in `scratch`, of the workspace at `dir`.
/// They only explain the failure cargo-mutants reports, so a directory that cannot be read
/// becomes an annotation saying so, never an error in its place.
fn baseline_failures(scratch: &Path, dir: &str) -> Vec<Annotation> {
    let what = "mutants' unmutated tests";
    let copies: std::io::Result<Vec<PathBuf>> = fs::read_dir(scratch).and_then(|entries| {
        entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    });
    let mut copies = match copies {
        Ok(copies) => copies,
        Err(error) => {
            return vec![Annotation {
                file: None,
                ..Annotation::error(
                    "",
                    &format!("Test failed: {what}"),
                    format!(
                        "the tests failed without any mutation, and the copies in {} could not \
                         be read: {error}",
                        scratch.display()
                    ),
                )
            }];
        }
    };
    copies.sort();
    let reports: Vec<PathBuf> = copies
        .into_iter()
        .map(|copy| copy.join(crate::junit::REPORT))
        .filter(|report| report.is_file())
        .collect();
    if reports.is_empty() {
        return crate::junit::annotations(
            &scratch.join("cargo-mutants left no test report here"),
            dir,
            what,
        );
    }
    reports
        .iter()
        .flat_map(|report| crate::junit::annotations(report, dir, what))
        .collect()
}

/// cargo-mutants' scratch directory for one run: emptied when the run starts, so copies a killed
/// run leaked (`--leak-dirs`) are never read as this run's, and removed when it ends, on every
/// path, so they never fill the disk.
struct Scratch {
    path: PathBuf,
    removed: bool,
}

impl Scratch {
    /// Empties, or creates, the directory at `path`.
    fn create(path: PathBuf) -> anyhow::Result<Self> {
        remove(&path).with_context(|| {
            format!("could not empty {}, left by an earlier run", path.display())
        })?;
        fs::create_dir_all(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        Ok(Self {
            path,
            removed: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the directory, failing when it cannot.
    fn remove(mut self) -> anyhow::Result<()> {
        self.removed = true;
        remove(&self.path).with_context(|| format!("could not remove {}", self.path.display()))
    }
}

/// Removes whatever is at `path`, a directory with its content or anything else; nothing there is
/// fine.
fn remove(path: &Path) -> std::io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

impl Drop for Scratch {
    /// Removes the directory when the run ends early with an error, which stays the one reported.
    /// A directory that cannot be removed here is emptied by the next run ([`Scratch::create`]).
    fn drop(&mut self) {
        if !self.removed {
            let _left_for_the_next_run = remove(&self.path);
        }
    }
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
        assert_eq!(
            config["env"].as_table().map(toml_edit::Table::len),
            Some(1),
            "nothing else, and nothing xtask reads when it is built: {config}"
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
    fn mutates_xtask_in_copies_with_paths_relative_to_it() {
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
                "echo \"$XTASK_TEST_REPOSITORY\" > {}\necho \"$*\" > {}\nwhile [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; --in-diff) diff=$2 ;; esac; shift; done\ngrep -q '^+++ b/src/a.rs' \"$diff\" || exit 8\n! grep -q README \"$diff\" || exit 9\nmkdir -p \"$out/mutants.out\"\necho 'src/a.rs:1:18: replace a -> bool with false' > \"$out/mutants.out/missed.txt\"\nexit 2\n",
                temp.path().join("checkout").display(),
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
            !text.contains("unmutated tests failed"),
            "a baseline that passed is not annotated: {text}"
        );
        assert!(
            text.contains("missed: xtask/src/a.rs:1:18: replace a -> bool with false"),
            "named from the repository root: {text}"
        );
        assert!(text.contains("of xtask/, in copies of it"), "{text}");
        let called = fs::read_to_string(&args).unwrap();
        assert!(
            called.contains("--dir xtask --jobs 2 --gitignore false"),
            "in copies, as many at once as the jobs: {called}"
        );
        assert!(!called.contains("--in-place"), "{called}");
        assert!(
            called.contains(&format!(
                "--output {}",
                repo.join("target/mutants/xtask").display()
            )),
            "{called}"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("checkout"))
                .unwrap()
                .trim(),
            repo.to_str().unwrap(),
            "the tests in the copies are told the checkout at run time"
        );
    }

    /// #33: a test that failed in mutants' unmutated copy showed only an exit code in the
    /// checks. Inside GitHub Actions, each one is an annotation, read from the copy's report.
    #[test]
    fn annotates_the_tests_that_fail_without_a_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("xtask/src/a.rs", "fn a() -> bool { true }\n")],
        );
        let recorded = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/nextest-junit-33.xml");
        let mutants = temp.path().join("cargo-mutants");
        script(
            &mutants,
            &format!(
                "case \"$*\" in *--leak-dirs*) ;; *) exit 9 ;; esac\n\
                 report=\"$TMPDIR/cargo-mutants-xtask-a.tmp/target/nextest/default\"\n\
                 mkdir -p \"$report\" && cp {} \"$report/junit.xml\"\nexit 4\n",
                recorded.display()
            ),
        );
        let actions = Options {
            mode: Mode::Actions,
            ..TWO
        };
        let mut out = Vec::new();
        let error = run(
            &repo,
            "base",
            &actions,
            &mutants,
            &temp.path().join("cargo-nextest"),
            &no_web(),
            &mut out,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("fail without any mutation"),
            "{error}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "::error file=xtask/src/broker.rs,line=6,title=Test failed%3A xtask%3A%3Abin/xtask \
                 broker%3A%3Atests%3A%3Aa_client_has_its_patience_in_all_not_per_read::"
            ),
            "{text}"
        );
        assert!(
            !repo.join("target/mutants-tmp").exists(),
            "the copies go once read"
        );
        let empty = temp.path().join("empty");
        fs::create_dir_all(empty.join("cargo-mutants-a.tmp")).unwrap();
        let none = super::baseline_failures(&empty, "xtask");
        assert!(
            none[0]
                .message
                .contains("cargo-mutants left no test report"),
            "{none:?}"
        );
    }

    /// #33's annotations never fired for xtask's mutants: a copy of xtask/ alone took its
    /// `.config` symlink to `../.config`, which dangles there, so nextest ran with its defaults,
    /// with no timeouts for hung tests and no `JUnit` report. This runs the real pinned nextest in
    /// such a copy, as cargo-mutants runs it, with the arguments the run gives cargo-mutants.
    #[test]
    fn a_copy_of_xtask_runs_nextest_with_the_repository_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let config = crate::testing::read_in_repository(crate::check::NEXTEST_CONFIG);
        let toolchain = crate::testing::read_in_repository("rust-toolchain.toml");
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[
                (".config/nextest.toml", &config),
                (
                    "xtask/Cargo.toml",
                    "[package]\nname = \"copied\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
                ),
                ("xtask/rust-toolchain.toml", &toolchain),
                (
                    "xtask/src/lib.rs",
                    "#[test]\nfn fails_without_a_mutation() {\n    assert_eq!(1, 2, \"the baseline fails\");\n}\n",
                ),
            ],
        );
        let mutants = temp.path().join("cargo-mutants");
        script(
            &mutants,
            "dir=.\ntest_args=\nwhile [ $# -gt 0 ]; do case \"$1\" in --dir) dir=$2 ;; --cargo-arg=--config=*) config=${1#--cargo-arg=--config=} ;; --cargo-test-arg=*) test_args=\"$test_args ${1#--cargo-test-arg=}\" ;; esac; shift; done\n\
             copy=\"$TMPDIR/cargo-mutants-copied-baseline.tmp\"\nmkdir -p \"$copy\" && cp -R \"$dir\"/. \"$copy\" && cd \"$copy\" || exit 9\n\
             cargo --config \"$config\" nextest run $test_args >/dev/null 2>&1 && exit 0\nexit 4\n",
        );
        let actions = Options {
            jobs: 1,
            shard: None,
            mode: Mode::Actions,
        };
        let mut out = Vec::new();
        let error = run(
            &repo,
            "base",
            &actions,
            &mutants,
            &crate::testing::pinned_for_tests("cargo-nextest"),
            &no_web(),
            &mut out,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("fail without any mutation"),
            "{error}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "::error file=xtask/src/lib.rs,line=3,title=Test failed%3A copied \
                 fails_without_a_mutation::"
            ),
            "the copy's nextest wrote its report, so the failing test is annotated: {text}"
        );
    }

    /// With `--leak-dirs`, the copies of a killed earlier run stayed in the scratch directory,
    /// and a failing baseline read their reports as this run's.
    #[test]
    fn a_killed_runs_copies_are_never_read_as_this_runs() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("xtask/src/a.rs", "fn a() -> bool { true }\n")],
        );
        let stale = repo.join("target/mutants-tmp/cargo-mutants-xtask-killed.tmp");
        let report = stale.join(crate::junit::REPORT);
        fs::create_dir_all(report.parent().unwrap()).unwrap();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/nextest-junit-33.xml"),
            &report,
        )
        .unwrap();
        let mutants = temp.path().join("cargo-mutants");
        script(
            &mutants,
            "[ -e \"$TMPDIR/cargo-mutants-xtask-killed.tmp\" ] && exit 9\n\
             mkdir -p \"$TMPDIR/cargo-mutants-xtask-now.tmp\"\nexit 4\n",
        );
        let actions = Options {
            mode: Mode::Actions,
            ..TWO
        };
        let mut out = Vec::new();
        let error = run(
            &repo,
            "base",
            &actions,
            &mutants,
            &temp.path().join("cargo-nextest"),
            &no_web(),
            &mut out,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("fail without any mutation"),
            "{error}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("a_client_has_its_patience_in_all_not_per_read"),
            "the killed run's failure is not this run's: {text}"
        );
        assert!(text.contains("cargo-mutants left no test report"), "{text}");
        assert!(
            !repo.join("target/mutants-tmp").exists(),
            "and nothing is left"
        );
    }

    /// An error between starting cargo-mutants and removing its scratch directory returned
    /// before the removal, leaving gigabytes of copies behind, and replaced cargo-mutants' own
    /// error with its own.
    #[test]
    fn an_error_leaves_no_copies_and_stays_the_one_reported() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("xtask/src/a.rs", "fn a() -> bool { true }\n")],
        );
        let unreadable = temp.path().join("unreadable");
        script(
            &unreadable,
            "rm -rf \"$TMPDIR\" && echo 'not a directory' > \"$TMPDIR\"\nexit 4\n",
        );
        let actions = Options {
            mode: Mode::Actions,
            ..TWO
        };
        let mut out = Vec::new();
        let error = run(
            &repo,
            "base",
            &actions,
            &unreadable,
            &temp.path().join("cargo-nextest"),
            &no_web(),
            &mut out,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("fail without any mutation"),
            "cargo-mutants' error, not the scratch directory's: {error}"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("could not be read"),
            "the annotation says why it names no test"
        );

        let crashing = temp.path().join("crashing");
        script(
            &crashing,
            "mkdir -p \"$TMPDIR/cargo-mutants-xtask-a.tmp/target\"\nexit 70\n",
        );
        let crashed = run(
            &repo,
            "base",
            &TWO,
            &crashing,
            &temp.path().join("cargo-nextest"),
            &no_web(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(
            crashed.to_string(),
            "cargo-mutants failed with exit code 70"
        );
        assert!(
            !repo.join("target/mutants-tmp").exists(),
            "the copies go on the error's path too"
        );

        let missing = run(
            &repo,
            "base",
            &TWO,
            &temp.path().join("absent"),
            &temp.path().join("cargo-nextest"),
            &no_web(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            missing.to_string().contains("could not run cargo-mutants"),
            "{missing}"
        );
        assert!(!repo.join("target/mutants-tmp").exists(), "{missing}");
    }

    /// Each workspace's missed mutants were annotated on their own, so one step could print more
    /// than the ten errors GitHub keeps, and it dropped the rest without a note.
    #[test]
    fn one_step_annotates_ten_across_the_workspaces() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[
                ("src/a.rs", "fn a() -> bool { true }\n"),
                ("xtask/src/a.rs", "fn a() -> bool { true }\n"),
            ],
        );
        let mutants = temp.path().join("cargo-mutants");
        script(
            &mutants,
            "count=10\nwhile [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; --dir) count=3 ;; esac; shift; done\n\
             mkdir -p \"$out/mutants.out\"\n\
             for n in $(seq 1 $count); do echo \"src/a.rs:$n:1: replace a -> bool with false\"; done > \"$out/mutants.out/missed.txt\"\n\
             exit 2\n",
        );
        let actions = Options {
            mode: Mode::Actions,
            ..TWO
        };
        let mut out = Vec::new();
        assert!(
            !run(
                &repo,
                "base",
                &actions,
                &mutants,
                &temp.path().join("cargo-nextest"),
                &no_web(),
                &mut out,
            )
            .unwrap()
        );
        let text = String::from_utf8(out).unwrap();
        let errors: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("::error"))
            .collect();
        assert_eq!(errors.len(), 10, "what GitHub keeps for the step: {text}");
        assert_eq!(
            errors.last().copied(),
            Some(
                "::error title=4 more::4 more findings of this level; the job's log lists every one"
            ),
            "{text}"
        );
        assert!(
            text.contains("missed: xtask/src/a.rs:3:1"),
            "the log lists every one: {text}"
        );
    }

    /// Every regular file under `dir` with its bytes, `target/` and `.git/` left out.
    fn snapshot(dir: &Path) -> Vec<(std::path::PathBuf, String)> {
        let mut files = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in fs::read_dir(&next).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path != dir.join("target") && path != dir.join(".git") {
                        pending.push(path);
                    }
                } else {
                    files.push((
                        path.strip_prefix(dir).unwrap().to_owned(),
                        String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned(),
                    ));
                }
            }
        }
        files.sort();
        files
    }

    /// #33's mutants ran xtask's in place: while a mutant was in xtask/src, any other `cargo
    /// xtask` in the checkout compiled it (on 6 Nov, the pull request watch's `cargo xtask gh`
    /// built `vm::Pull: Default`), and a copy of xtask/ alone would still build into the
    /// checkout's target/ through its .cargo/config.toml. The run now leaves the checkout
    /// byte-identical while cargo-mutants works and after, each copy building into its own
    /// target/: this fake mutates and writes as cargo-mutants and its tests
    /// do, in the copy it is given, and records the checkout as it finds it meanwhile.
    #[test]
    fn the_checkout_is_untouched_during_and_after_a_run() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[
                ("xtask/src/a.rs", "fn a() -> bool { true }\n"),
                ("src/b.rs", "fn b() -> bool { true }\n"),
            ],
        );
        let during = temp.path().join("during");
        let mutants = temp.path().join("cargo-mutants");
        script(
            &mutants,
            &format!(
                "[ \"$CARGO_TARGET_DIR\" = target ] || exit 10\n\
                 dir=.\nwhile [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; --dir) dir=$2 ;; --in-place) exit 9 ;; esac; shift; done\n\
                 copy=\"$TMPDIR/copy-$$\"\nmkdir -p \"$copy\"\ncp -R \"$dir\"/. \"$copy\"\n\
                 echo 'fn a() -> bool {{ false }}' > \"$copy/src/a.rs\"\n\
                 mkdir -p \"$copy/src/proptest-regressions\" && echo 'cc 1' > \"$copy/src/proptest-regressions/a.txt\"\n\
                 rm -rf {during} && cp -R {repo} {during}\n\
                 mkdir -p \"$out/mutants.out\"\n",
                repo = repo.display(),
                during = during.display(),
            ),
        );
        let before = snapshot(&repo);
        let mut out = Vec::new();
        assert!(
            run(
                &repo,
                "base",
                &TWO,
                &mutants,
                &temp.path().join("cargo-nextest"),
                &no_web(),
                &mut out,
            )
            .unwrap(),
            "{}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(
            snapshot(&during),
            before,
            "the checkout is as it was while cargo-mutants ran"
        );
        assert_eq!(
            snapshot(&repo),
            before,
            "the checkout is as it was after the run"
        );
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
