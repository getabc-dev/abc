//! Failing tests as annotations, from nextest's `JUnit` report.
//!
//! On #33 a failing xtask test showed only "exit code 1" in the pull request's checks: the panic
//! was in the job log, which agent VMs cannot reach. nextest writes a `JUnit` report of every run
//! (`[profile.default.junit]` in `.config/nextest.toml`, at [`REPORT`] under the target
//! directory), so `check`, `coverage` and `mutants` read it after a failing run and, inside GitHub
//! Actions, annotate each failing test with its name, the file and line its panic names, and the
//! panic's message, which `cargo xtask pr checks` shows.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, ensure};

use crate::annotation::Annotation;

/// nextest's `JUnit` report of its last run, under the target directory.
pub(crate) const REPORT: &str = "nextest/default/junit.xml";

/// One failing test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failed {
    /// The test binary and the test, as nextest names them.
    pub(crate) test: String,
    /// Where it panicked, relative to the repository root, when that is a file of the repository.
    pub(crate) place: Option<(String, u64)>,
    /// What it said: the panic's message, or nextest's account of the failure (a timeout, a
    /// signal).
    pub(crate) message: String,
}

impl Failed {
    /// The test as an error annotation, on its panic's line when known.
    pub(crate) fn annotation(&self) -> Annotation {
        let title = format!("Test failed: {}", self.test);
        let message = format!("{} failed:\n{}", self.test, self.message);
        match &self.place {
            Some((file, line)) => Annotation::error(file, &title, message).at(*line, *line),
            None => Annotation {
                file: None,
                ..Annotation::error("", &title, message)
            },
        }
    }
}

/// The failing tests in the `JUnit` report `xml` of a run in the workspace at `dir` (relative to the
/// repository root; empty for the root's), whose panics name files relative to it.
pub(crate) fn failures(xml: &str, dir: &str) -> anyhow::Result<Vec<Failed>> {
    let document =
        roxmltree::Document::parse(xml).context("nextest's JUnit report is not valid XML")?;
    let mut failed = Vec::new();
    for case in document
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
    {
        let Some(failure) = case
            .children()
            .find(|node| node.has_tag_name("failure") || node.has_tag_name("error"))
        else {
            continue;
        };
        let test = match (case.attribute("classname"), case.attribute("name")) {
            (Some(binary), Some(name)) => format!("{binary} {name}"),
            (None, Some(name)) => name.to_owned(),
            _ => continue,
        };
        let account = failure.text().unwrap_or_default();
        let (place, message) = match panic(account) {
            Some((file, line, message)) => {
                let place = in_repository(file, dir).map(|file| (file, line));
                (place, message)
            }
            None => (
                None,
                failure
                    .attribute("message")
                    .or_else(|| failure.attribute("type"))
                    .map_or_else(|| account.trim().to_owned(), str::to_owned),
            ),
        };
        failed.push(Failed {
            test,
            place,
            message,
        });
    }
    Ok(failed)
}

/// The file, line and message of the first panic in `text`, as Rust prints it:
/// `thread '<name>' (<id>) panicked at <file>:<line>:<column>:` then the message, up to the note
/// on backtraces.
fn panic(text: &str) -> Option<(&str, u64, String)> {
    let (_, after) = text.split_once(" panicked at ")?;
    let (location, rest) = after.split_once(":\n").unwrap_or((after, ""));
    let mut parts = location.rsplitn(3, ':');
    let _column = parts.next()?;
    let line = parts.next()?.parse().ok()?;
    let file = parts.next()?;
    let message: Vec<&str> = rest
        .lines()
        .take_while(|said| !said.starts_with("note: run with `RUST_BACKTRACE"))
        .collect();
    Some((file, line, message.join("\n").trim().to_owned()))
}

/// `file`, as a panic in the workspace at `dir` names it, relative to the repository root; none
/// for a file outside it (the standard library's, a dependency's).
fn in_repository(file: &str, dir: &str) -> Option<String> {
    let path = Path::new(file);
    if path.is_absolute() || path.starts_with("..") {
        return None;
    }
    Some(if dir.is_empty() {
        file.to_owned()
    } else {
        format!("{dir}/{file}")
    })
}

/// The target directory cargo builds the workspace whose manifest `manifest_args` name into, from
/// `root`.
pub(crate) fn target_directory(root: &Path, manifest_args: &[String]) -> anyhow::Result<PathBuf> {
    let output = Command::new(crate::check::cargo())
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .args(manifest_args)
        .current_dir(root)
        .output()
        .context("could not run cargo metadata")?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("could not parse cargo metadata")?;
    metadata
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .context("cargo metadata named no target directory")
}

/// Where nextest will write its report for a run of the workspace whose manifest `manifest_args`
/// name, from `root`, building into `under` in the target directory (cargo-llvm-cov's
/// `llvm-cov-target`; empty for the target directory itself). An earlier report there is removed,
/// so a run that writes none (it did not build) is never read with an earlier run's.
pub(crate) fn prepare(
    root: &Path,
    manifest_args: &[String],
    under: &str,
) -> anyhow::Result<PathBuf> {
    let path = target_directory(root, manifest_args)?
        .join(under)
        .join(REPORT);
    clear(&path)?;
    Ok(path)
}

/// Removes the report at `path`, if there is one.
pub(crate) fn clear(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
    }
}

/// The annotations for the failing tests in the report `prepared` found the place of, of a run
/// in the workspace at `dir`; when that place is unknown, or there is no report, or none fails in
/// it, one annotation saying so and where the log is, so the failure is never only an exit code.
pub(crate) fn annotations_of(
    prepared: &anyhow::Result<PathBuf>,
    dir: &str,
    what: &str,
) -> Vec<Annotation> {
    match prepared {
        Ok(path) => annotations(path, dir, what),
        Err(error) => unexplained(what, &format!("could not find the test report: {error:#}")),
    }
}

/// One annotation for a failing `what` whose tests the report cannot name, saying `why`.
fn unexplained(what: &str, why: &str) -> Vec<Annotation> {
    vec![Annotation {
        file: None,
        ..Annotation::error(
            "",
            &format!("{what} failed"),
            format!("{why}; the job log has the output"),
        )
    }]
}

/// The annotations for the failing tests in the report at `path`, of a run in the workspace at
/// `dir`, as [`annotations_of`].
pub(crate) fn annotations(path: &Path, dir: &str, what: &str) -> Vec<Annotation> {
    let unexplained = |why: String| unexplained(what, &why);
    let xml = match fs::read_to_string(path) {
        Ok(xml) => xml,
        Err(error) => {
            return unexplained(format!(
                "no test report at {} ({error}): the tests did not run, as when they do not build",
                path.display()
            ));
        }
    };
    match failures(&xml, dir) {
        Ok(failed) if failed.is_empty() => {
            unexplained("the test report lists no failing test".to_owned())
        }
        Ok(failed) => failed.iter().map(Failed::annotation).collect(),
        Err(error) => unexplained(format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Failed, annotations, annotations_of, failures, panic};
    use crate::annotation::{self, Mode};

    /// A real report of cargo-nextest 0.9.151 (the pinned version) on a crate shaped like
    /// xtask, failing as #33's `broker::tests::a_client_has_its_patience_in_all_not_per_read`
    /// did, the one CI showed as "exit code 1".
    fn recorded() -> String {
        fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/nextest-junit-33.xml"),
        )
        .unwrap()
    }

    #[test]
    fn reads_33s_failing_test_its_place_and_its_panic() {
        let failed = failures(&recorded(), "xtask").unwrap();
        assert_eq!(
            failed,
            [Failed {
                test:
                    "xtask::bin/xtask broker::tests::a_client_has_its_patience_in_all_not_per_read"
                        .to_owned(),
                place: Some(("xtask/src/broker.rs".to_owned(), 6)),
                message: "a client that sends a byte at a time is cut off at 2 s in all, not per \
                          read: it was served for 3s"
                    .to_owned(),
            }],
            "the passing test is left out"
        );
        let commands = annotation::commands(
            Mode::Actions,
            &failed.iter().map(Failed::annotation).collect::<Vec<_>>(),
        );
        assert_eq!(
            commands,
            "::error file=xtask/src/broker.rs,line=6,title=Test failed%3A xtask%3A%3Abin/xtask \
             broker%3A%3Atests%3A%3Aa_client_has_its_patience_in_all_not_per_read::xtask::bin/\
             xtask broker::tests::a_client_has_its_patience_in_all_not_per_read failed:%0Aa \
             client that sends a byte at a time is cut off at 2 s in all, not per read: it was \
             served for 3s\n"
        );
        assert_eq!(
            failures(&recorded(), "").unwrap()[0].place,
            Some(("src/broker.rs".to_owned(), 6)),
            "the main workspace's panics name paths from the root already"
        );
    }

    #[test]
    fn reads_failures_without_a_panic() {
        let xml = r#"<testsuites><testsuite name="a"><testcase name="t::hangs" classname="abc-store"><failure message="test timed out after 120s" type="test timeout"/></testcase><testcase name="t::std" classname="abc"><failure type="test failure">thread 't::std' (1) panicked at /rustc/abc/library/core/src/option.rs:2:5:
called `Option::unwrap()` on a `None` value
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace</failure></testcase></testsuite></testsuites>"#;
        assert_eq!(
            failures(xml, "").unwrap(),
            [
                Failed {
                    test: "abc-store t::hangs".to_owned(),
                    place: None,
                    message: "test timed out after 120s".to_owned(),
                },
                Failed {
                    test: "abc t::std".to_owned(),
                    place: None,
                    message: "called `Option::unwrap()` on a `None` value".to_owned(),
                },
            ],
            "a timeout says so, and a panic outside the repository has no place"
        );
        assert!(failures("<testsuites>", "").is_err(), "not XML");
        assert_eq!(panic("no panic here"), None);
        assert_eq!(
            panic("thread 'a' panicked at src/a.rs:x:1:\nm"),
            None,
            "a line that is not a number"
        );
    }

    #[test]
    fn a_failure_without_a_report_still_says_where_to_look() {
        let temp = tempfile::tempdir().unwrap();
        let missing = annotations(&temp.path().join("junit.xml"), "", "tests");
        assert_eq!(missing.len(), 1);
        assert!(
            missing[0].message.contains("the tests did not run")
                && missing[0].message.contains("the job log"),
            "{missing:?}"
        );
        assert_eq!(missing[0].title, "tests failed");
        let empty = temp.path().join("empty.xml");
        fs::write(&empty, "<testsuites/>").unwrap();
        assert!(
            annotations(&empty, "", "tests")[0]
                .message
                .contains("lists no failing test")
        );
        let broken = temp.path().join("broken.xml");
        fs::write(&broken, "<testsuites>").unwrap();
        assert!(
            annotations(&broken, "", "tests")[0]
                .message
                .contains("not valid XML")
        );
        let report = temp.path().join("junit.xml");
        fs::write(&report, recorded()).unwrap();
        assert_eq!(
            annotations(&report, "xtask", "tests")[0].file.as_deref(),
            Some("xtask/src/broker.rs")
        );
        super::clear(&report).unwrap();
        assert!(!report.exists());
        super::clear(&report).unwrap();
        let unknown = annotations_of(&Err(anyhow::anyhow!("no manifest")), "", "tests");
        assert!(
            unknown[0]
                .message
                .contains("could not find the test report: no manifest"),
            "{unknown:?}"
        );
    }

    #[test]
    fn finds_the_target_directory() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "").unwrap();
        let locked = std::process::Command::new(crate::check::cargo())
            .arg("generate-lockfile")
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(locked.success());
        let target = super::target_directory(temp.path(), &[]).unwrap();
        assert_eq!(target, temp.path().canonicalize().unwrap().join("target"));
        let missing = super::target_directory(
            &temp.path().join("src"),
            &["--manifest-path".to_owned(), "none/Cargo.toml".to_owned()],
        )
        .unwrap_err();
        assert!(
            format!("{missing:#}").contains("cargo metadata failed"),
            "{missing:#}"
        );
    }
}
