//! Failing tests as annotations, from nextest's `JUnit` report.
//!
//! On #33 a failing xtask test showed only "exit code 1" in the pull request's checks: the panic
//! was in the job log, which agent VMs cannot reach. nextest writes a `JUnit` report of every run
//! (`[profile.default.junit]` in `.config/nextest.toml`, at [`REPORT`] in the workspace), so `check`, `coverage` and `mutants` read it after a failing run and, inside GitHub
//! Actions, annotate each failing test with its name, the file and line its panic names, and the
//! panic's message, which `cargo xtask pr checks` shows.

use std::fs;
use std::path::Path;

use anyhow::Context as _;

use crate::annotation::Annotation;

/// nextest's `JUnit` report of its last run, from the workspace's root. nextest keeps its store
/// at `target/nextest` there (its `store.dir`), whatever directory cargo builds into: for
/// xtask's workspace that is `xtask/target/`, though its builds go to the root's `target/`.
pub(crate) const REPORT: &str = "target/nextest/default/junit.xml";

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

/// Removes the report at `path`, if there is one, before a run, so a run that writes none (it
/// did not build) is never read with an earlier run's.
pub(crate) fn clear(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
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
/// `dir`; when there is no report, or none fails in it, one annotation saying so and where the
/// log is, so the failure is never only an exit code.
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

    use super::{Failed, annotations, failures, panic};
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
        assert_eq!(
            failures(xml, "").unwrap()[0].annotation().file,
            None,
            "an annotation without a place names no file"
        );
        let unnamed = r#"<testsuites><testcase name="t::lone"><failure message="m"/></testcase></testsuites>"#;
        assert_eq!(
            failures(unnamed, "").unwrap()[0].test,
            "t::lone",
            "a test case without its binary keeps its name"
        );
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
        let directory = temp.path().join("a directory");
        fs::create_dir(&directory).unwrap();
        assert!(
            format!("{:#}", super::clear(&directory).unwrap_err()).contains("could not remove"),
            "only a missing report is no error"
        );
    }
}
