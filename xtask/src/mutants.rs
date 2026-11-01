//! `cargo xtask mutants`: mutation testing on the lines changed since the merge base.
//!
//! cargo-mutants changes each changed function in small ways (returns a default, flips an
//! operator) and runs the tests against every change. A mutant the tests do not catch means a
//! behaviour no test pins down; the gate fails on any and lists them.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail, ensure};

use crate::check::PROPTEST_RNG_SEED;
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
#[derive(Debug, PartialEq, Eq)]
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
}

/// Runs cargo-mutants, with the cargo-mutants and cargo-nextest executables given, on the Rust
/// lines changed since the merge base with `base`.
pub(crate) fn run(
    root: &Path,
    base: &str,
    options: &Options,
    mutants: &Path,
    nextest: &Path,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    let commit = git::merge_base(root, base)?;
    let diff = git::diff(root, &commit, 3)?;
    if !changes_rust(&diff) {
        writeln!(out, "mutants: no Rust lines changed; nothing to mutate")?;
        return Ok(true);
    }
    let output = root.join(OUTPUT);
    fs::create_dir_all(&output)
        .with_context(|| format!("could not create {}", output.display()))?;
    let results = output.join("mutants.out");
    // Results of an earlier run must not decide this one.
    if results.exists() {
        fs::remove_dir_all(&results)
            .with_context(|| format!("could not remove {}", results.display()))?;
    }
    let diff_file = output.join("changes.diff");
    fs::write(&diff_file, &diff).context("could not write the diff for cargo-mutants")?;
    writeln!(out, "xtask: mutating the changed Rust code")?;
    let status = Command::new(mutants)
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
        // The tests run the pinned tools in .tools/, which .gitignore lists; copy them too.
        .args(["--gitignore", "false"])
        .arg("--timeout-multiplier")
        .arg(timeout_multiplier(options.jobs).to_string())
        .args(["--minimum-test-timeout", MINIMUM_TEST_TIMEOUT])
        .arg("--jobs")
        .arg(options.jobs.to_string())
        .args(
            options
                .shard
                .map(|shard| vec!["--shard".to_owned(), format!("{}/{}", shard.index, shard.count)])
                .unwrap_or_default(),
        )
        .arg("--output")
        .arg(&output)
        .env("PATH", tools::path_with(nextest)?)
        .env("PROPTEST_RNG_SEED", PROPTEST_RNG_SEED)
        .current_dir(root)
        .status()
        .context("could not run cargo-mutants")?;
    let result = outcome(status.code())?;
    let missed = listed(&results.join("missed.txt"))?;
    let timed_out = listed(&results.join("timeout.txt"))?;
    let passed = missed.is_empty() && result != Outcome::Missed;
    write!(out, "{}", describe(passed, &missed, &timed_out))?;
    Ok(passed)
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

    use super::{
        Options, Outcome, Shard, changes_rust, describe, listed, outcome, run, timeout_multiplier,
    };
    use crate::testing::{repository_with_change, script};

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
    };

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
        let passed = run(&repo, "base", &TWO, &missing, &nextest, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!passed, "a missed mutant fails: {text}");
        assert!(
            text.contains("missed: src/a.rs:1:18: replace a -> bool with false"),
            "{text}"
        );

        let mixed = temp.path().join("mixed");
        script(
            &mixed,
            "while [ $# -gt 0 ]; do case \"$1\" in --output) out=$2 ;; esac; shift; done\nmkdir -p \"$out/mutants.out\"\necho 'src/a.rs:1:18: replace a -> bool with false' > \"$out/mutants.out/missed.txt\"\necho 'src/a.rs:1:1: replace a with ()' > \"$out/mutants.out/timeout.txt\"\nexit 3\n",
        );
        let mut both = Vec::new();
        assert!(
            !run(&repo, "base", &TWO, &mixed, &nextest, &mut both).unwrap(),
            "a missed mutant fails even when another timed out"
        );
        assert!(
            String::from_utf8(both).unwrap().contains("mutants: failed"),
            "said so"
        );

        let caught = temp.path().join("caught");
        script(
            &caught,
            "while [ $# -gt 0 ]; do case \"$1\" in --shard) shard=$2 ;; esac; shift; done\n[ \"$shard\" = 1/4 ] || exit 9\n",
        );
        let sharded = Options {
            jobs: 2,
            shard: Some(Shard::parse("1/4").unwrap()),
        };
        assert!(
            run(&repo, "base", &sharded, &caught, &nextest, &mut Vec::new()).unwrap(),
            "all caught, in the shard asked for"
        );

        let docs = repository_with_change(&temp.path().join("docs"), &[("README.md", "# x\n")]);
        let never = temp.path().join("never");
        script(&never, "exit 99\n");
        let mut skipped = Vec::new();
        assert!(
            run(&docs, "base", &TWO, &never, &nextest, &mut skipped).unwrap(),
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
