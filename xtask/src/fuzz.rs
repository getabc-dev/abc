//! `cargo xtask fuzz`: lints the fuzz workspace (`fuzz/`), then fuzzes protocol message decoding
//! (target `decode`) for a set time with the pinned cargo-fuzz.
//!
//! The fuzz targets are a Cargo workspace of their own, laid out as `cargo fuzz init
//! --fuzzing-workspace true` makes it, so the main workspace never builds libFuzzer. `check` does
//! not cover it, so this command first runs rustfmt and clippy on it with the lints the main
//! workspace sets, and `--locked`, so a stale `fuzz/Cargo.lock` fails rather than being rewritten.
//!
//! libFuzzer starts from the committed seeds (one well-formed message of each kind) and keeps what
//! it learns in `fuzz/corpus/decode`, so later runs on the same machine start further on. It runs
//! on the pinned stable toolchain without a sanitizer: abc forbids unsafe code, so a failure is a
//! panic, which libFuzzer catches either way. A failure fails the run, and cargo-fuzz saves its
//! input under `fuzz/artifacts/decode/`; once fixed, add it to the seeds so every run replays it.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::Context;

/// The fuzz workspace, relative to the repository root.
const FUZZ_DIR: &str = "fuzz";
/// The fuzz workspace's manifest.
const MANIFEST: &str = "fuzz/Cargo.toml";
/// The fuzz target.
const TARGET: &str = "decode";
/// The committed seeds.
const SEEDS: &str = "fuzz/seeds/decode";
/// What libFuzzer learns, kept between runs on one machine: cargo-fuzz's corpus directory.
const CORPUS: &str = "fuzz/corpus/decode";
/// Where cargo-fuzz saves a failing input.
const ARTIFACTS: &str = "fuzz/artifacts/decode/";

/// The steps that lint the fuzz workspace before fuzzing, each with what to do when it fails.
fn lint_commands(root: &Path, cargo: &OsStr) -> [(Command, &'static str); 2] {
    let mut format = Command::new(cargo);
    format
        .current_dir(root)
        .args(["fmt", "--check", "--manifest-path", MANIFEST]);
    let mut clippy = Command::new(cargo);
    clippy.current_dir(root).args([
        "clippy",
        "--manifest-path",
        MANIFEST,
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ]);
    [
        (
            format,
            "run `cargo fmt --manifest-path fuzz/Cargo.toml` and fuzz again",
        ),
        (
            clippy,
            "fix each finding above and fuzz again; silence a lint only with #[expect(lint, \
             reason = \"...\")] when the code is right. If cargo says fuzz/Cargo.lock needs \
             updating, as after a main-workspace bump of a dependency abc-protocol uses, run \
             `cargo update --manifest-path fuzz/Cargo.toml --workspace` and commit the lockfile",
        ),
    ]
}

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
        .arg(format!("-max_total_time={seconds}"));
    command
}

/// Lints the fuzz workspace with `cargo`, then fuzzes for `seconds`, and reports to `report`.
/// `Ok(false)` when a lint failed or the fuzzer found a failure.
pub(crate) fn run(
    root: &Path,
    cargo: &OsStr,
    cargo_fuzz: &Path,
    seconds: u64,
    report: &mut impl Write,
) -> anyhow::Result<bool> {
    for (mut lint, fix) in lint_commands(root, cargo) {
        let args: Vec<String> = lint
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        writeln!(report, "fuzz: cargo {}", args.join(" "))?;
        let status = lint
            .status()
            .with_context(|| format!("could not run {}", cargo.display()))?;
        if !status.success() {
            writeln!(report, "fuzz: the fuzz workspace failed a lint: {fix}")?;
            return Ok(false);
        }
    }
    std::fs::create_dir_all(root.join(CORPUS))
        .with_context(|| format!("could not create {CORPUS}"))?;
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
    use std::path::{Path, PathBuf};

    use super::{command, lint_commands, run};

    fn args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn runs_the_decode_target_from_the_seeds_for_the_time_given() {
        let root = Path::new("/repo");
        let built = command(root, Path::new("/tools/cargo-fuzz"), 90);
        assert_eq!(
            args(&built),
            [
                "fuzz",
                "run",
                "--sanitizer",
                "none",
                "--fuzz-dir",
                "fuzz",
                "decode",
                "fuzz/corpus/decode",
                "fuzz/seeds/decode",
                "--",
                "-max_total_time=90",
            ],
            "the pinned stable toolchain needs no sanitizer; the corpus comes first so libFuzzer \
             writes there, never to the seeds"
        );
        assert_eq!(built.get_current_dir(), Some(root), "from the repository");
    }

    #[test]
    fn lints_the_fuzz_workspace_with_rustfmt_and_clippy_locked() {
        let root = Path::new("/repo");
        let [(format, _), (clippy, _)] = lint_commands(root, "cargo".as_ref());
        assert_eq!(
            args(&format),
            ["fmt", "--check", "--manifest-path", "fuzz/Cargo.toml"],
            "rustfmt checks the fuzz workspace"
        );
        assert_eq!(
            args(&clippy),
            [
                "clippy",
                "--manifest-path",
                "fuzz/Cargo.toml",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            "clippy denies warnings and never rewrites fuzz/Cargo.lock"
        );
        for lint in [&format, &clippy] {
            assert_eq!(lint.get_current_dir(), Some(root), "from the repository");
        }
    }

    #[test]
    fn the_checked_in_seeds_and_target_exist() {
        let root = crate::testing::checkout();
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

    /// The `[workspace.lints]` table of the main workspace's manifest and the `[lints]` table of
    /// the fuzz workspace's, and the two `rust-version`s.
    fn lints_and_rust_versions(root: &Path) -> [(serde_json::Value, serde_json::Value); 2] {
        let read = |path: PathBuf| -> serde_json::Value {
            toml_edit::de::from_str(&fs::read_to_string(path).unwrap()).unwrap()
        };
        let main = read(root.join("Cargo.toml"));
        let fuzz = read(root.join(super::MANIFEST));
        [
            (main["workspace"]["lints"].clone(), fuzz["lints"].clone()),
            (
                main["workspace"]["package"]["rust-version"].clone(),
                fuzz["package"]["rust-version"].clone(),
            ),
        ]
    }

    #[test]
    fn the_fuzz_workspace_keeps_the_main_workspace_lints_and_rust_version() {
        let root = crate::testing::checkout();
        let [(main_lints, fuzz_lints), (main_rust, fuzz_rust)] = lints_and_rust_versions(&root);
        assert!(main_lints.is_object(), "the main workspace sets lints");
        assert_eq!(
            fuzz_lints, main_lints,
            "fuzz/Cargo.toml's [lints] must equal Cargo.toml's [workspace.lints]"
        );
        assert!(
            main_rust.is_string(),
            "the main workspace sets rust-version"
        );
        assert_eq!(
            fuzz_rust, main_rust,
            "fuzz/Cargo.toml's rust-version must equal the main workspace's"
        );
    }

    /// A fake tool in `dir` that appends its arguments to `dir/calls` and exits `exit`.
    fn fake(dir: &Path, name: &str, exit: u8) -> PathBuf {
        let path = dir.join(format!("{name}-{exit}"));
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"{name} $*\" >> \"$(dirname \"$0\")/calls\"\nexit {exit}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn calls(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn passes_or_fails_with_the_fuzzer_and_says_where_the_input_is() {
        let temp = tempfile::tempdir().unwrap();
        let cargo = fake(temp.path(), "cargo", 0);
        let mut report = Vec::new();
        assert!(
            run(
                temp.path(),
                cargo.as_os_str(),
                &fake(temp.path(), "cargo-fuzz", 0),
                5,
                &mut report
            )
            .unwrap(),
            "no failure found"
        );
        assert!(
            temp.path().join("fuzz/corpus/decode").is_dir(),
            "the corpus is made"
        );
        let made = calls(temp.path());
        assert_eq!(made.len(), 3, "rustfmt, clippy, then the fuzzer: {made:?}");
        assert!(made[0].starts_with("cargo fmt "), "rustfmt first: {made:?}");
        assert!(
            made[1].starts_with("cargo clippy "),
            "then clippy: {made:?}"
        );
        assert!(
            made[2].ends_with("-max_total_time=5"),
            "then the fuzzer, for the time given: {made:?}"
        );
        assert_eq!(
            String::from_utf8(report).unwrap(),
            "fuzz: cargo fmt --check --manifest-path fuzz/Cargo.toml\nfuzz: cargo clippy \
             --manifest-path fuzz/Cargo.toml --all-targets --locked -- -D warnings\nfuzz: decode \
             for 5 s, from fuzz/seeds/decode\nfuzz: decode found no failure in 5 s\n",
            "reported"
        );
        let mut failed = Vec::new();
        assert!(
            !run(
                temp.path(),
                cargo.as_os_str(),
                &fake(temp.path(), "cargo-fuzz", 1),
                5,
                &mut failed
            )
            .unwrap(),
            "a failure fails the run"
        );
        assert!(
            String::from_utf8(failed)
                .unwrap()
                .contains("the failing input is in fuzz/artifacts/decode/"),
            "and says where its input is"
        );
        let missing = run(
            temp.path(),
            cargo.as_os_str(),
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

    #[test]
    fn a_failed_lint_stops_before_fuzzing() {
        let temp = tempfile::tempdir().unwrap();
        let mut report = Vec::new();
        assert!(
            !run(
                temp.path(),
                fake(temp.path(), "cargo", 1).as_os_str(),
                &fake(temp.path(), "cargo-fuzz", 0),
                5,
                &mut report
            )
            .unwrap(),
            "a lint failure fails the run"
        );
        assert_eq!(
            calls(temp.path()),
            ["cargo fmt --check --manifest-path fuzz/Cargo.toml"],
            "nothing runs after the first failed lint"
        );
        assert!(
            String::from_utf8(report)
                .unwrap()
                .ends_with("fuzz: the fuzz workspace failed a lint: run `cargo fmt --manifest-path fuzz/Cargo.toml` and fuzz again\n"),
            "and says how to fix it"
        );
        let missing = run(
            temp.path(),
            temp.path().join("no-such-cargo").as_os_str(),
            &fake(temp.path(), "cargo-fuzz", 0),
            5,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{missing:#}").starts_with("could not run "),
            "{missing:#}"
        );
    }

    #[test]
    fn a_clippy_finding_stops_before_fuzzing_and_says_how_to_refresh_the_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        let cargo = temp.path().join("cargo");
        fs::write(
            &cargo,
            "#!/bin/sh\necho \"cargo $*\" >> \"$(dirname \"$0\")/calls\"\n[ \"$1\" != clippy ]\n",
        )
        .unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let mut report = Vec::new();
        assert!(
            !run(
                temp.path(),
                cargo.as_os_str(),
                &fake(temp.path(), "cargo-fuzz", 0),
                5,
                &mut report
            )
            .unwrap(),
            "a clippy finding fails the run"
        );
        let made = calls(temp.path());
        assert_eq!(
            made.len(),
            2,
            "rustfmt and clippy, never the fuzzer: {made:?}"
        );
        let report = String::from_utf8(report).unwrap();
        assert!(
            report.contains("`cargo update --manifest-path fuzz/Cargo.toml --workspace`"),
            "says how to refresh a stale fuzz/Cargo.lock: {report}"
        );
    }
}
