//! `cargo xtask mutants`: mutation testing on the lines changed since the merge base.
//!
//! cargo-mutants changes each changed function in small ways (returns a default, flips an
//! operator) and runs the tests against every change. A mutant the tests do not catch means a
//! behaviour no test pins down; the gate fails on any and lists them.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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
    if crate::postgres::needed(root) {
        crate::postgres::warm(out)?;
    }
    let diff_file = output.join("changes.diff");
    fs::write(&diff_file, &diff).context("could not write the diff for cargo-mutants")?;
    writeln!(out, "xtask: mutating the changed Rust code")?;
    // cargo-mutants copies the tree, once per job, into the temporary directory and builds each
    // copy there. That directory is small in an agent VM, where the build output has the room.
    let scratch = scratch(root);
    fs::create_dir_all(&scratch)
        .with_context(|| format!("could not create {}", scratch.display()))?;
    // The tests must not inherit that directory: inside the checkout it is inside its git
    // repository and its Cargo workspace, and its paths can be too long for a Unix socket. Cargo's
    // `[env]`, forced, gives every build and test cargo-mutants runs this process's own one back.
    let test_env = output.join("test-env.toml");
    fs::write(&test_env, test_env_config(&std::env::temp_dir())?)
        .with_context(|| format!("could not write {}", test_env.display()))?;
    let mut cargo_config = OsString::from("--cargo-arg=--config=");
    cargo_config.push(&test_env);
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
        .arg(&cargo_config)
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
        .env("TMPDIR", &scratch)
        .current_dir(root)
        .status()
        .context("could not run cargo-mutants");
    // Whatever cargo-mutants left behind goes, so repeated runs never fill the disk.
    fs::remove_dir_all(&scratch)
        .with_context(|| format!("could not remove {}", scratch.display()))?;
    let result = outcome(status?.code())?;
    let missed = listed(&results.join("missed.txt"))?;
    let timed_out = listed(&results.join("timeout.txt"))?;
    let passed = missed.is_empty() && result != Outcome::Missed;
    write!(out, "{}", describe(passed, &missed, &timed_out))?;
    Ok(passed)
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
        Options, Outcome, Shard, changes_rust, describe, listed, outcome, run, test_env_config,
        timeout_multiplier,
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
            &format!(
                "while [ $# -gt 0 ]; do case \"$1\" in --shard) shard=$2 ;; --cargo-arg=--config=*) config=${{1#--cargo-arg=--config=}} ;; esac; shift; done\n[ \"$shard\" = 1/4 ] || exit 9\nmkdir \"$TMPDIR/cargo-mutants-copy\"\necho \"$TMPDIR\" > {}\ncp \"$config\" {}\n",
                temp.path().join("tmpdir").display(),
                temp.path().join("config.toml").display()
            ),
        );
        let sharded = Options {
            jobs: 2,
            shard: Some(Shard::parse("1/4").unwrap()),
        };
        assert!(
            run(&repo, "base", &sharded, &caught, &nextest, &mut Vec::new()).unwrap(),
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
