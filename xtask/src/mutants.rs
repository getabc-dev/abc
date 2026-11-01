//! `cargo xtask mutants`: mutation testing on the lines changed since the merge base.
//!
//! cargo-mutants changes each changed function in small ways (returns a default, flips an
//! operator) and runs the tests against every change. A mutant the tests do not catch means a
//! behaviour no test pins down; the gate fails on any and lists them.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};

use crate::check::PROPTEST_RNG_SEED;
use crate::git;
use crate::tools::{self, Manifest};

/// Where cargo-mutants writes its results, relative to the repository root.
const OUTPUT: &str = "target/mutants";

/// How much longer than the clean test run a mutant's tests may take before they count as hung.
const TIMEOUT_MULTIPLIER: &str = "3";

/// How cargo-mutants ended, from its exit code.
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

/// Runs cargo-mutants on the Rust lines changed since the merge base with `base`, with `jobs`
/// mutants built and tested in parallel.
pub(crate) fn run(
    root: &Path,
    base: &str,
    jobs: u32,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    let manifest = Manifest::load(root)?;
    let mutants = manifest.tool("cargo-mutants")?.installed(root)?;
    let nextest = manifest.tool("cargo-nextest")?.installed(root)?;
    let commit = git::merge_base(root, base)?;
    let diff = git::diff(root, &commit, 3)?;
    if !changes_rust(&diff) {
        writeln!(out, "mutants: no Rust lines changed; nothing to mutate")?;
        return Ok(true);
    }
    let output = root.join(OUTPUT);
    fs::create_dir_all(&output)
        .with_context(|| format!("could not create {}", output.display()))?;
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
        .args(["--timeout-multiplier", TIMEOUT_MULTIPLIER])
        .arg("--jobs")
        .arg(jobs.to_string())
        .arg("--output")
        .arg(&output)
        .env("PATH", tools::path_with(&nextest)?)
        .env("PROPTEST_RNG_SEED", PROPTEST_RNG_SEED)
        .current_dir(root)
        .status()
        .context("could not run cargo-mutants")?;
    let result = outcome(status.code())?;
    let results = output.join("mutants.out");
    let missed = listed(&results.join("missed.txt"))?;
    let timed_out = listed(&results.join("timeout.txt"))?;
    write!(out, "{}", describe(&result, &missed, &timed_out))?;
    Ok(result != Outcome::Missed)
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

fn describe(result: &Outcome, missed: &[String], timed_out: &[String]) -> String {
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
    text.push_str(match result {
        Outcome::AllCaught | Outcome::TimedOut => {
            "mutants: passed; the tests caught every mutant\n"
        }
        Outcome::Missed => {
            "mutants: failed; add or sharpen tests so each missed mutant above makes one fail\n"
        }
    });
    text
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Outcome, changes_rust, describe, listed, outcome};

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
    fn describes_the_result() {
        let missed = ["src/a.rs:3:5: replace f -> bool with true".to_owned()];
        let text = describe(&Outcome::Missed, &missed, &[]);
        assert!(text.starts_with("  missed: src/a.rs:3:5"), "{text}");
        assert!(
            text.ends_with("missed mutant above makes one fail\n"),
            "{text}"
        );
        let timeouts = ["src/b.rs:1:1: replace g with ()".to_owned()];
        let passing = describe(&Outcome::TimedOut, &[], &timeouts);
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
