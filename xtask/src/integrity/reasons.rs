//! The reasons a pull request gives for its test changes.
//!
//! When the check flags a test change, the pull request description must say why in a
//! `## Test changes` section, one list item per flagged test: the test in backticks, then the
//! requirement or behaviour that changed.
//!
//! ```markdown
//! ## Test changes
//!
//! - `xtask/src/a.rs::tests::parses_tabs`: tabs are now rejected, so the test that accepted them
//!   is gone.
//! - `xtask/src/old.rs`: the old parser was removed with its tests.
//! ```
//!
//! A test is named `<file>::<test>`, or by its file alone when the whole file was deleted. The
//! `integrity-approved` label approves a stated reason, so the reasons are required with or
//! without it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// The heading of the section, without its `## `.
pub(super) const HEADING: &str = "Test changes";

/// The reason given for each test the section names, keyed by the test; a reason may be empty.
fn entries(description: &str) -> Option<BTreeMap<String, String>> {
    let mut lines = description.lines();
    lines.find(|line| {
        line.strip_prefix("## ")
            .is_some_and(|title| title.trim().eq_ignore_ascii_case(HEADING))
    })?;
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in lines {
        if line.starts_with("# ") || line.starts_with("## ") {
            break;
        }
        let trimmed = line.trim_start();
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            current = None;
            let Some((test, reason)) = item
                .trim_start()
                .strip_prefix('`')
                .and_then(|rest| rest.split_once('`'))
            else {
                continue;
            };
            let reason = reason
                .trim_start_matches(|c: char| {
                    matches!(c, ':' | '-' | '—' | '–') || c.is_whitespace()
                })
                .trim();
            let entry = entries.entry(test.trim().to_owned()).or_default();
            append(entry, reason);
            current = Some(test.trim().to_owned());
        } else if let Some(test) = &current
            && line.starts_with(char::is_whitespace)
            && let Some(entry) = entries.get_mut(test)
        {
            // An indented line continues the item above.
            append(entry, line.trim());
        }
    }
    Some(entries)
}

fn append(reason: &mut String, more: &str) {
    if more.is_empty() {
        return;
    }
    if !reason.is_empty() {
        reason.push(' ');
    }
    reason.push_str(more);
}

/// How well a description explains the flagged test changes.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Explained {
    /// Whether the description has the section at all.
    pub(super) section: bool,
    /// Flagged tests without an entry, or whose entry gives no reason.
    pub(super) unexplained: Vec<String>,
    /// Entries for tests the check did not flag.
    pub(super) extra: Vec<String>,
}

impl Explained {
    pub(super) const fn complete(&self) -> bool {
        self.unexplained.is_empty()
    }

    /// Tells the author what is missing, and notes entries for tests that were not flagged.
    pub(super) fn report(&self, out: &mut impl Write) -> io::Result<()> {
        if !self.complete() {
            let problem = if self.section {
                format!("the \"## {HEADING}\" section gives no reason for")
            } else {
                format!("the pull request description has no \"## {HEADING}\" section for")
            };
            writeln!(
                out,
                "integrity: failed; {problem} these test changes. Add one entry per test, saying \
                 which requirement or behaviour changed:\n\n## {HEADING}\n"
            )?;
            for test in &self.unexplained {
                writeln!(out, "- `{test}`: <which requirement or behaviour changed>")?;
            }
        }
        for test in &self.extra {
            writeln!(
                out,
                "integrity: note: \"## {HEADING}\" names `{test}`, which has no flagged change"
            )?;
        }
        Ok(())
    }
}

/// Checks that `description` gives a reason for each of the `flagged` tests.
pub(super) fn explained(flagged: &[String], description: &str) -> Explained {
    let Some(entries) = entries(description) else {
        return Explained {
            section: false,
            unexplained: flagged.to_vec(),
            extra: Vec::new(),
        };
    };
    Explained {
        section: true,
        unexplained: flagged
            .iter()
            .filter(|test| entries.get(*test).is_none_or(String::is_empty))
            .cloned()
            .collect(),
        extra: entries
            .keys()
            .filter(|test| !flagged.contains(test))
            .cloned()
            .collect(),
    }
}

#[derive(Deserialize)]
struct Event {
    pull_request: Option<EventPullRequest>,
}

#[derive(Deserialize)]
struct EventPullRequest {
    body: Option<String>,
}

/// The pull request description: from `file` when given, else from the GitHub Actions event
/// payload at `event` when this is a pull request; `None` when there is neither.
pub(crate) fn description(
    file: Option<&Path>,
    event: Option<PathBuf>,
) -> anyhow::Result<Option<String>> {
    if let Some(file) = file {
        return fs::read_to_string(file)
            .map(Some)
            .with_context(|| format!("could not read the description in {}", file.display()));
    }
    let Some(event) = event else {
        return Ok(None);
    };
    let text = fs::read_to_string(&event)
        .with_context(|| format!("could not read the event payload {}", event.display()))?;
    let payload: Event = serde_json::from_str(&text).context("the event payload is invalid")?;
    let pull_request = payload
        .pull_request
        .context("the event is not about a pull request")?;
    Ok(Some(pull_request.body.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Explained, description, entries, explained};

    fn report(result: &Explained) -> String {
        let mut out = Vec::new();
        result.report(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn flagged() -> Vec<String> {
        vec![
            "src/a.rs::tests::one".to_owned(),
            "src/a.rs::tests::two".to_owned(),
            "src/gone.rs".to_owned(),
        ]
    }

    #[test]
    fn every_flagged_test_explained_passes() {
        let description = "Adds x.\n\n## Test changes\n\n- `src/a.rs::tests::one`: tabs are now rejected.\n* `src/a.rs::tests::two` — the limit moved\n  from 10 to 20.\n- `src/gone.rs` - the old parser is gone.\n\n## Plan\n\n- `src/other.rs`: not in the section\n";
        let result = explained(&flagged(), description);
        assert_eq!(
            result,
            Explained {
                section: true,
                unexplained: Vec::new(),
                extra: Vec::new()
            },
            "each test has a reason; the next section is not read"
        );
        assert!(result.complete(), "complete");
        assert!(report(&result).is_empty(), "nothing to say");
        assert_eq!(
            entries(description).unwrap()["src/a.rs::tests::two"],
            "the limit moved from 10 to 20.",
            "an indented line continues its item"
        );
    }

    #[test]
    fn a_missing_section_lists_every_test() {
        let result = explained(
            &flagged(),
            "Adds x.\n\n### Test changes\n- `src/gone.rs`: x\n",
        );
        assert!(!result.section, "only a level-2 heading counts");
        assert_eq!(result.unexplained, flagged(), "all of them");
        let report = report(&result);
        assert!(
            report.contains("has no \"## Test changes\" section"),
            "{report}"
        );
        assert!(
            report.contains("- `src/a.rs::tests::one`: <which requirement or behaviour changed>"),
            "a template for each: {report}"
        );
    }

    #[test]
    fn a_partial_section_lists_the_tests_left_out() {
        let description = "## test changes\n\n- `src/a.rs::tests::one`:\n- `src/a.rs::tests::two`: renamed.\n- src/gone.rs: no backticks\n";
        let result = explained(&flagged(), description);
        assert!(result.section, "the heading is matched without case");
        assert_eq!(
            result.unexplained,
            ["src/a.rs::tests::one", "src/gone.rs"],
            "an entry without a reason or without the test in backticks explains nothing"
        );
        assert!(!result.complete(), "incomplete");
        let report = report(&result);
        assert!(report.contains("gives no reason for"), "{report}");
        assert!(!report.contains("tests::two`"), "{report}");
    }

    #[test]
    fn extra_entries_are_noted_but_pass() {
        let description = "## Test changes\n- `src/a.rs::tests::one`: a\n- `src/a.rs::tests::two`: b\n- `src/gone.rs`: c\n- `src/b.rs::tests::three`: d\n";
        let result = explained(&flagged(), description);
        assert!(result.complete(), "every flagged test is explained");
        assert_eq!(result.extra, ["src/b.rs::tests::three"], "the extra entry");
        assert!(
            report(&result).contains("names `src/b.rs::tests::three`, which has no flagged change"),
            "noted"
        );
        assert!(
            explained(&[], "no section").complete(),
            "nothing flagged needs no section"
        );
    }

    #[test]
    fn reads_the_description_from_a_file_or_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("description.md");
        fs::write(&file, "## Test changes\n").unwrap();
        let event = dir.path().join("event.json");
        fs::write(&event, r#"{"pull_request": {"body": "From the event."}}"#).unwrap();
        assert_eq!(
            description(Some(&file), Some(event.clone()))
                .unwrap()
                .as_deref(),
            Some("## Test changes\n"),
            "a file wins"
        );
        assert_eq!(
            description(None, Some(event.clone())).unwrap().as_deref(),
            Some("From the event."),
            "the pull request's body"
        );
        fs::write(&event, r#"{"pull_request": {"body": null}}"#).unwrap();
        assert_eq!(
            description(None, Some(event.clone())).unwrap().as_deref(),
            Some(""),
            "an empty description"
        );
        fs::write(&event, r#"{"merge_group": {}}"#).unwrap();
        assert!(
            description(None, Some(event)).is_err(),
            "not a pull request"
        );
        assert_eq!(description(None, None).unwrap(), None, "neither");
        assert!(
            description(Some(&dir.path().join("absent")), None).is_err(),
            "an unreadable file"
        );
    }
}
