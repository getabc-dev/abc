//! `cargo xtask fuzz`: fuzzes protocol message decoding (`crates/abc-protocol/fuzz`, target
//! `decode`) for a set time with the pinned cargo-fuzz.
//!
//! libFuzzer starts from the committed seeds (one well-formed message of each kind) and keeps what
//! it learns in `target/fuzz/corpus/decode`, so later runs on the same machine start further on.
//! It runs on the pinned stable toolchain without a sanitizer: abc forbids unsafe code, so a
//! failure is a panic, which libFuzzer catches either way. A failure fails the run, and its input
//! is saved under `target/fuzz/artifacts/decode/`; once fixed, add it to the seeds so every run
//! replays it.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::Context;

/// The fuzz crate, relative to the repository root.
const FUZZ_DIR: &str = "crates/abc-protocol/fuzz";
/// The fuzz target.
const TARGET: &str = "decode";
/// The committed seeds.
const SEEDS: &str = "crates/abc-protocol/fuzz/seeds/decode";
/// What libFuzzer learns, kept between runs on one machine.
const CORPUS: &str = "target/fuzz/corpus/decode";
/// Where a failing input is saved.
const ARTIFACTS: &str = "target/fuzz/artifacts/decode/";

/// The cargo-fuzz command that fuzzes for `seconds` in `root`.
fn command(root: &Path, cargo_fuzz: &Path, seconds: u64) -> Command {
    let mut command = Command::new(cargo_fuzz);
    command
        .current_dir(root)
        .args([
            "fuzz",
            "run",
            "--sanitizer",
            "none",
            "--fuzz-dir",
            FUZZ_DIR,
            TARGET,
            CORPUS,
            SEEDS,
            "--",
        ])
        .arg(format!("-max_total_time={seconds}"))
        .arg(format!(
            "-artifact_prefix={}",
            root.join(ARTIFACTS).display()
        ));
    command
}

/// Fuzzes for `seconds`, and reports to `report`. `Ok(false)` when the fuzzer found a failure.
pub(crate) fn run(
    root: &Path,
    cargo_fuzz: &Path,
    seconds: u64,
    report: &mut impl Write,
) -> anyhow::Result<bool> {
    for dir in [CORPUS, ARTIFACTS] {
        std::fs::create_dir_all(root.join(dir))
            .with_context(|| format!("could not create {dir}"))?;
    }
    writeln!(report, "fuzz: {TARGET} for {seconds} s, from {SEEDS}")?;
    let status = command(root, cargo_fuzz, seconds)
        .status()
        .with_context(|| format!("could not run {}", cargo_fuzz.display()))?;
    if status.success() {
        writeln!(report, "fuzz: {TARGET} found no failure in {seconds} s")?;
        Ok(true)
    } else {
        writeln!(
            report,
            "fuzz: {TARGET} failed ({status}); the failing input is in {ARTIFACTS}. Fix the \
             cause, then add the input to {SEEDS} so every run replays it"
        )?;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::{command, run};

    #[test]
    fn runs_the_decode_target_from_the_seeds_for_the_time_given() {
        let root = std::path::Path::new("/repo");
        let built = command(root, std::path::Path::new("/tools/cargo-fuzz"), 90);
        let args: Vec<String> = built
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "fuzz",
                "run",
                "--sanitizer",
                "none",
                "--fuzz-dir",
                "crates/abc-protocol/fuzz",
                "decode",
                "target/fuzz/corpus/decode",
                "crates/abc-protocol/fuzz/seeds/decode",
                "--",
                "-max_total_time=90",
                "-artifact_prefix=/repo/target/fuzz/artifacts/decode/",
            ],
            "the pinned stable toolchain needs no sanitizer; the corpus comes first so libFuzzer \
             writes there, never to the seeds"
        );
        assert_eq!(built.get_current_dir(), Some(root), "from the repository");
    }

    #[test]
    fn the_checked_in_seeds_and_target_exist() {
        let root = crate::repo_root().unwrap();
        assert!(
            root.join(super::FUZZ_DIR)
                .join("fuzz_targets")
                .join(format!("{}.rs", super::TARGET))
                .is_file(),
            "the target"
        );
        let seeds = fs::read_dir(root.join(super::SEEDS)).unwrap().count();
        assert!(seeds >= 3, "a seed for each message: {seeds}");
    }

    fn fake(dir: &std::path::Path, exit: u8) -> std::path::PathBuf {
        let path = dir.join(format!("cargo-fuzz-{exit}"));
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"$*\" > \"$(dirname \"$0\")/args\"\nexit {exit}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn passes_or_fails_with_the_fuzzer_and_says_where_the_input_is() {
        let temp = tempfile::tempdir().unwrap();
        let mut report = Vec::new();
        assert!(
            run(temp.path(), &fake(temp.path(), 0), 5, &mut report).unwrap(),
            "no failure found"
        );
        assert!(
            temp.path().join("target/fuzz/corpus/decode").is_dir(),
            "the corpus is made"
        );
        assert!(
            fs::read_to_string(temp.path().join("args"))
                .unwrap()
                .contains("-max_total_time=5"),
            "for the time given"
        );
        assert_eq!(
            String::from_utf8(report).unwrap(),
            "fuzz: decode for 5 s, from crates/abc-protocol/fuzz/seeds/decode\nfuzz: decode found \
             no failure in 5 s\n",
            "reported"
        );
        let mut failed = Vec::new();
        assert!(
            !run(temp.path(), &fake(temp.path(), 1), 5, &mut failed).unwrap(),
            "a failure fails the run"
        );
        assert!(
            String::from_utf8(failed)
                .unwrap()
                .contains("the failing input is in target/fuzz/artifacts/decode/"),
            "and says where its input is"
        );
        let missing = run(
            temp.path(),
            &temp.path().join("no-such-tool"),
            5,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{missing:#}").starts_with("could not run "),
            "{missing:#}"
        );
    }
}
