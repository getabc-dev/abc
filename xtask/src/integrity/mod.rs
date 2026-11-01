//! `cargo xtask integrity`: changes that weaken the gates, which a person must approve.
//!
//! Comparing each changed file with its version at the merge base, it reports deleted tests and
//! test files, newly added `#[ignore]` and `#[should_panic]`, tests that lost assertions, and
//! changes to gate files: CI, lint, dependency and toolchain settings, hook settings, and the
//! xtask code that runs the gates. In CI a finding fails the check unless a listed approver has
//! labelled the pull request `integrity-approved` after its latest push.
//!
//! Tests are read per language; Rust is the only one so far, and [`inventory`] is where another
//! joins.

mod approval;
mod reasons;
mod rust;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Context;

pub(crate) use approval::PullRequest;
pub(crate) use reasons::{Event, event};
pub(crate) use rust::TestCode;

use crate::git;

/// The gate files: whatever decides what passes. CODEOWNERS patterns, the single list both the
/// check and `.github/CODEOWNERS` follow: a trailing `/` covers a directory, anything else is one
/// file, both from the repository root.
pub(crate) const GATES: [&str; 14] = [
    "/.cargo/",
    "/.claude/settings.json",
    "/.config/",
    "/.github/",
    "/.gitleaks.toml",
    "/AGENTS.md",
    "/CLAUDE.md",
    "/Cargo.lock",
    "/Cargo.toml",
    "/clippy.toml",
    "/deny.toml",
    "/rust-toolchain.toml",
    "/rustfmt.toml",
    "/xtask/",
];

/// One test in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Test {
    /// The test's path within its file, such as `tests::parses`.
    pub(crate) name: String,
    pub(crate) ignored: bool,
    pub(crate) should_panic: bool,
    /// How many assertion macros it calls.
    pub(crate) assertions: usize,
}

/// The tests in a file, for the languages the check reads; `None` for any other file.
fn inventory(path: &str, source: &str) -> Option<anyhow::Result<Vec<Test>>> {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension == "rs")
        .then(|| rust::tests(source))
}

/// The test-only code in a file, for the languages the check reads; `None` for any other file.
pub(crate) fn test_code(path: &Path, source: &str) -> Option<anyhow::Result<TestCode>> {
    path.extension()
        .is_some_and(|extension| extension == "rs")
        .then(|| rust::test_code(source))
}

/// Something the check reports.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Finding {
    DeletedTest {
        file: String,
        test: String,
    },
    DeletedTestFile {
        file: String,
        tests: usize,
    },
    NewlyIgnored {
        file: String,
        test: String,
    },
    NewlyShouldPanic {
        file: String,
        test: String,
    },
    FewerAssertions {
        file: String,
        test: String,
        before: usize,
        after: usize,
    },
    GateFile {
        file: String,
    },
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeletedTest { file, test } => write!(f, "deleted test       {file}: {test}"),
            Self::DeletedTestFile { file, tests } => {
                write!(f, "deleted test file  {file} ({tests} tests)")
            }
            Self::NewlyIgnored { file, test } => write!(f, "newly #[ignore]    {file}: {test}"),
            Self::NewlyShouldPanic { file, test } => {
                write!(f, "newly #[should_panic] {file}: {test}")
            }
            Self::FewerAssertions {
                file,
                test,
                before,
                after,
            } => write!(
                f,
                "fewer assertions   {file}: {test} ({before} before, {after} now)"
            ),
            Self::GateFile { file } => write!(f, "gate file changed  {file}"),
        }
    }
}

impl Finding {
    /// The test this finding changes, as the description's "## Test changes" section names it:
    /// `<file>::<test>`, or the file for a deleted test file. `None` for gate changes.
    fn test(&self) -> Option<String> {
        match self {
            Self::DeletedTest { file, test }
            | Self::NewlyIgnored { file, test }
            | Self::NewlyShouldPanic { file, test }
            | Self::FewerAssertions { file, test, .. } => Some(format!("{file}::{test}")),
            Self::DeletedTestFile { file, .. } => Some(file.clone()),
            Self::GateFile { .. } => None,
        }
    }
}

/// Runs the check against the merge base with `base`. Each flagged test change needs a reason in
/// the pull request's description; with a pull request, findings then pass when an approver
/// listed at the merge base has approved them.
pub(crate) fn run(
    root: &Path,
    base: &str,
    pull_request: Option<&PullRequest>,
    event: &Event,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    run_with(root, base, pull_request, event, out, &mut |url, token| {
        approval::github_get(url, token)
    })
}

/// [`run`], reading GitHub's API through `fetch` (URL and token).
fn run_with(
    root: &Path,
    base: &str,
    pull_request: Option<&PullRequest>,
    event: &Event,
    out: &mut impl Write,
    fetch: &mut impl FnMut(&str, &str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<bool> {
    let commit = git::merge_base(root, base)?;
    let found = findings(root, &commit)?;
    if found.is_empty() {
        writeln!(
            out,
            "integrity: no deleted, skipped or weakened tests and no gate changes"
        )?;
        return Ok(true);
    }
    writeln!(out, "integrity: {} finding(s)", found.len())?;
    for finding in &found {
        writeln!(out, "  {finding}")?;
    }
    let flagged: Vec<String> = found
        .iter()
        .filter_map(Finding::test)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let explained = reasons::explained(&flagged, event.description.as_deref().unwrap_or_default());
    explained.report(out)?;
    let Some(pull_request) = pull_request else {
        writeln!(
            out,
            "integrity: failed; in a pull request, an approver listed in {} can accept these \
             by adding the label {} after the latest push",
            approval::APPROVERS,
            approval::LABEL
        )?;
        return Ok(false);
    };
    // The approvers come from the merge base, so a pull request cannot approve itself.
    let listed = git::show(root, &commit, approval::APPROVERS)?.with_context(|| {
        format!(
            "the merge base has no {}, so nobody can approve these findings",
            approval::APPROVERS
        )
    })?;
    let approvers = approval::approvers(&listed)?;
    let verdict = approval::approved(pull_request, &approvers, event.body_edited, &mut |url| {
        fetch(url, &pull_request.token)
    })?;
    writeln!(out, "integrity: {}", verdict.reason)?;
    Ok(verdict.approved && explained.complete())
}

/// Every finding in the changes from `commit` to the working tree.
fn findings(root: &Path, commit: &str) -> anyhow::Result<Vec<Finding>> {
    let mut found = Vec::new();
    for change in git::changes(root, commit)? {
        for path in change.old.iter().chain(change.new.iter()) {
            if is_gate_file(path) {
                found.push(Finding::GateFile { file: path.clone() });
                break;
            }
        }
        let before = match &change.old {
            Some(path) => match git::show(root, commit, path)? {
                Some(source) => tests_in(path, &source)?,
                None => None,
            },
            None => None,
        };
        let after = match &change.new {
            Some(path) => {
                let source = fs::read_to_string(root.join(path))
                    .with_context(|| format!("could not read {path}"))?;
                tests_in(path, &source)?
            }
            None => None,
        };
        let file = change
            .new
            .clone()
            .or_else(|| change.old.clone())
            .unwrap_or_default();
        found.extend(compare(&file, before.as_deref(), after.as_deref()));
    }
    Ok(found)
}

fn tests_in(path: &str, source: &str) -> anyhow::Result<Option<Vec<Test>>> {
    inventory(path, source)
        .transpose()
        .with_context(|| format!("could not read the tests in {path}"))
}

fn is_gate_file(path: &str) -> bool {
    GATES.iter().any(|pattern| {
        let pattern = pattern.trim_start_matches('/');
        if pattern.ends_with('/') {
            path.starts_with(pattern)
        } else {
            path == pattern
        }
    })
}

/// The findings between a file's tests before and after; `None` means the file did not exist or
/// is not in a language the check reads.
fn compare(file: &str, before: Option<&[Test]>, after: Option<&[Test]>) -> Vec<Finding> {
    let before = before.unwrap_or_default();
    let Some(after) = after else {
        return if before.is_empty() {
            Vec::new()
        } else {
            vec![Finding::DeletedTestFile {
                file: file.to_owned(),
                tests: before.len(),
            }]
        };
    };
    let old: BTreeMap<&str, &Test> = before
        .iter()
        .map(|test| (test.name.as_str(), test))
        .collect();
    let new: BTreeMap<&str, &Test> = after
        .iter()
        .map(|test| (test.name.as_str(), test))
        .collect();
    let mut found: Vec<Finding> = old
        .keys()
        .filter(|name| !new.contains_key(*name))
        .map(|name| Finding::DeletedTest {
            file: file.to_owned(),
            test: (*name).to_owned(),
        })
        .collect();
    for (name, test) in &new {
        let previous = old.get(name);
        if test.ignored && !previous.is_some_and(|earlier| earlier.ignored) {
            found.push(Finding::NewlyIgnored {
                file: file.to_owned(),
                test: (*name).to_owned(),
            });
        }
        if test.should_panic && !previous.is_some_and(|earlier| earlier.should_panic) {
            found.push(Finding::NewlyShouldPanic {
                file: file.to_owned(),
                test: (*name).to_owned(),
            });
        }
        if let Some(earlier) = previous
            && test.assertions < earlier.assertions
        {
            found.push(Finding::FewerAssertions {
                file: file.to_owned(),
                test: (*name).to_owned(),
                before: earlier.assertions,
                after: test.assertions,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{
        Event, Finding, GATES, PullRequest, Test, compare, findings, is_gate_file, run, run_with,
    };
    use crate::testing::{git, repository, repository_with_change};

    fn test(name: &str, ignored: bool, should_panic: bool, assertions: usize) -> Test {
        Test {
            name: name.to_owned(),
            ignored,
            should_panic,
            assertions,
        }
    }

    #[test]
    fn compares_tests_before_and_after() {
        let before = [
            test("a", false, false, 2),
            test("b", false, false, 1),
            test("c", false, false, 3),
            test("d", true, false, 1),
        ];
        let after = [
            test("a", true, false, 2),
            test("c", false, true, 1),
            test("d", true, false, 1),
            test("e", true, false, 1),
        ];
        assert_eq!(
            compare("x.rs", Some(&before), Some(&after)),
            [
                Finding::DeletedTest {
                    file: "x.rs".into(),
                    test: "b".into()
                },
                Finding::NewlyIgnored {
                    file: "x.rs".into(),
                    test: "a".into()
                },
                Finding::NewlyShouldPanic {
                    file: "x.rs".into(),
                    test: "c".into()
                },
                Finding::FewerAssertions {
                    file: "x.rs".into(),
                    test: "c".into(),
                    before: 3,
                    after: 1
                },
                Finding::NewlyIgnored {
                    file: "x.rs".into(),
                    test: "e".into()
                },
            ],
            "a test ignored all along is no finding; a new ignored test is"
        );
        assert_eq!(
            compare("x.rs", Some(&before), None),
            [Finding::DeletedTestFile {
                file: "x.rs".into(),
                tests: 4
            }],
            "a deleted file with tests"
        );
        assert!(
            compare("x.rs", None, Some(&after[..1])).len() == 1,
            "a new ignored test only"
        );
        assert!(
            compare("x.rs", Some(&[]), None).is_empty(),
            "a deleted file without tests"
        );
    }

    #[test]
    fn recognises_gate_files() {
        for path in [
            ".github/workflows/ci.yml",
            ".github/CODEOWNERS",
            ".cargo/config.toml",
            ".config/nextest.toml",
            "deny.toml",
            "Cargo.lock",
            "Cargo.toml",
            "AGENTS.md",
            "CLAUDE.md",
            "rust-toolchain.toml",
            "xtask/src/tools.rs",
            "xtask/Cargo.toml",
            ".claude/settings.json",
        ] {
            assert!(is_gate_file(path), "{path} is a gate file");
        }
        for path in [
            "crates/abc-protocol/src/lib.rs",
            "crates/abc-protocol/Cargo.toml",
            "README.md",
            ".claude/settings.local.json",
            "docs/AGENTS.md",
            "xtask",
        ] {
            assert!(!is_gate_file(path), "{path} is not a gate file");
        }
    }

    #[test]
    fn codeowners_follow_the_gate_list() {
        let owned: BTreeSet<&str> = include_str!("../../../.github/CODEOWNERS")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split_whitespace();
                let pattern = fields.next().unwrap();
                assert_eq!(
                    fields.collect::<Vec<_>>(),
                    ["@snehilvj"],
                    "{pattern}'s owner"
                );
                pattern
            })
            .collect();
        assert_eq!(
            owned,
            GATES.into_iter().collect::<BTreeSet<_>>(),
            "CODEOWNERS owns exactly the gate files"
        );
    }

    #[test]
    fn finds_weakened_tests_and_gate_changes_in_a_repository() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        let lib = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn kept() { assert!(true); assert!(true); }\n    #[test]\n    fn dropped() {}\n}\n";
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), lib).unwrap();
        fs::write(repo.join("src/gone.rs"), "#[test]\nfn only() {}\n").unwrap();
        fs::write(
            repo.join("src/moved.rs"),
            "#[test]\nfn stays() { assert!(true); }\n// padding\n",
        )
        .unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.lints.clippy]\nall = \"deny\"\n",
        )
        .unwrap();
        fs::write(repo.join("deny.toml"), "[bans]\n").unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        fs::write(
            repo.join("src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    #[ignore]\n    fn kept() { assert!(true); }\n}\n",
        )
        .unwrap();
        fs::remove_file(repo.join("src/gone.rs")).unwrap();
        git(&repo, &["mv", "src/moved.rs", "src/renamed.rs"]);
        fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.lints.clippy]\nall = \"warn\"\n",
        )
        .unwrap();
        fs::write(
            repo.join("deny.toml"),
            "[bans]\nmultiple-versions = \"allow\"\n",
        )
        .unwrap();

        let found = findings(&repo, &base).unwrap();
        for expected in [
            Finding::DeletedTest {
                file: "src/lib.rs".into(),
                test: "tests::dropped".into(),
            },
            Finding::NewlyIgnored {
                file: "src/lib.rs".into(),
                test: "tests::kept".into(),
            },
            Finding::FewerAssertions {
                file: "src/lib.rs".into(),
                test: "tests::kept".into(),
                before: 2,
                after: 1,
            },
            Finding::DeletedTestFile {
                file: "src/gone.rs".into(),
                tests: 1,
            },
            Finding::GateFile {
                file: "deny.toml".into(),
            },
            Finding::GateFile {
                file: "Cargo.toml".into(),
            },
        ] {
            assert!(
                found.contains(&expected),
                "{expected:?} missing from {found:?}"
            );
        }
        assert_eq!(found.len(), 6, "a renamed file keeps its tests: {found:?}");
    }

    /// A repository whose base branch lists `snehilvj` as the integrity approver, with `files`
    /// written and committed on top.
    fn approved_repository(path: &Path, files: &[(&str, &str)]) -> PathBuf {
        let repo = repository(path);
        fs::create_dir_all(repo.join(".github")).unwrap();
        fs::write(repo.join(".github/integrity-approvers"), "snehilvj\n").unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/t.rs"), "#[test]\nfn kept() {}\n").unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "approvers"]);
        git(&repo, &["branch", "base"]);
        for (file, content) in files {
            let target = repo.join(file);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, content).unwrap();
        }
        git(&repo, &["add", "--all"]);
        git(
            &repo,
            &["commit", "--quiet", "--allow-empty", "-m", "change"],
        );
        repo
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            api: "https://api.example".to_owned(),
            repository: "o/r".to_owned(),
            number: 7,
            token: "token".to_owned(),
        }
    }

    /// GitHub's API for pull request 7, whose `integrity-approved` label `login` added after
    /// the push.
    fn labelled_by(login: &'static str) -> impl FnMut(&str, &str) -> anyhow::Result<Vec<u8>> {
        move |url: &str, token: &str| {
            assert_eq!(token, "token", "the token CI gives");
            Ok(if url.ends_with("/pulls/7") {
                br#"{"head": {"sha": "abc"}, "labels": [{"name": "integrity-approved"}]}"#.to_vec()
            } else if url.contains("/actions/runs") {
                br#"{"workflow_runs": [{"created_at": "2026-11-01T09:00:00Z"}]}"#.to_vec()
            } else if url.ends_with("page=1") {
                format!(r#"[{{"event": "labeled", "created_at": "2026-11-01T10:00:00Z", "actor": {{"login": "{login}"}}, "label": {{"name": "integrity-approved"}}}}]"#).into_bytes()
            } else {
                b"[]".to_vec()
            })
        }
    }

    fn described(text: &str) -> Event {
        Event {
            description: Some(text.to_owned()),
            body_edited: false,
        }
    }

    #[test]
    fn runs_end_to_end_with_and_without_approval() {
        let temp = tempfile::tempdir().unwrap();
        let clean = approved_repository(&temp.path().join("clean"), &[("src/a.rs", "fn a() {}\n")]);
        let unused = &mut |_: &str, _: &str| -> anyhow::Result<Vec<u8>> {
            panic!("no findings, no API call")
        };
        let mut out = Vec::new();
        assert!(
            run_with(&clean, "base", None, &Event::default(), &mut out, unused).unwrap(),
            "no findings pass"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("no deleted, skipped"),
            "said so"
        );

        let gated = approved_repository(&temp.path().join("gated"), &[("deny.toml", "[bans]\n")]);
        let mut refused = Vec::new();
        assert!(
            !run_with(
                &gated,
                "base",
                None,
                &Event::default(),
                &mut refused,
                unused
            )
            .unwrap(),
            "findings fail outside a pull request"
        );
        assert!(
            String::from_utf8(refused)
                .unwrap()
                .contains("gate file changed  deny.toml"),
            "listed"
        );

        let mut accepted = Vec::new();
        assert!(
            run_with(
                &gated,
                "base",
                Some(&pull_request()),
                &Event::default(),
                &mut accepted,
                &mut labelled_by("snehilvj")
            )
            .unwrap(),
            "approved"
        );
        assert!(
            String::from_utf8(accepted)
                .unwrap()
                .contains("accepted by snehilvj"),
            "who"
        );

        let mut edited = Vec::new();
        assert!(
            !run_with(
                &gated,
                "base",
                Some(&pull_request()),
                &Event {
                    description: None,
                    body_edited: true
                },
                &mut edited,
                &mut labelled_by("snehilvj")
            )
            .unwrap(),
            "a description edit needs a new approval"
        );
        assert!(
            String::from_utf8(edited).unwrap().contains("stale"),
            "said so"
        );
    }

    #[test]
    fn a_pull_request_cannot_name_its_own_approver() {
        let temp = tempfile::tempdir().unwrap();
        let repo = approved_repository(
            &temp.path().join("repo"),
            &[(".github/integrity-approvers", "agent\n")],
        );
        let mut out = Vec::new();
        assert!(
            !run_with(
                &repo,
                "base",
                Some(&pull_request()),
                &Event::default(),
                &mut out,
                &mut labelled_by("agent")
            )
            .unwrap(),
            "the approvers come from the merge base"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("added by agent, who is not listed"),
            "said so"
        );

        let bare = repository_with_change(&temp.path().join("bare"), &[("deny.toml", "\n")]);
        let error = run_with(
            &bare,
            "base",
            Some(&pull_request()),
            &Event::default(),
            &mut Vec::new(),
            &mut labelled_by("snehilvj"),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("the merge base has no .github/integrity-approvers"),
            "{error:#}"
        );
    }

    #[test]
    fn outside_a_pull_request_it_reads_no_api() {
        let temp = tempfile::tempdir().unwrap();
        let clean =
            repository_with_change(&temp.path().join("clean"), &[("src/a.rs", "fn a() {}\n")]);
        assert!(
            run(&clean, "base", None, &Event::default(), &mut Vec::new()).unwrap(),
            "clean"
        );
        let gated = repository_with_change(&temp.path().join("gated"), &[("deny.toml", "\n")]);
        assert!(
            !run(&gated, "base", None, &Event::default(), &mut Vec::new()).unwrap(),
            "a gate change fails without asking GitHub"
        );
    }

    #[test]
    fn test_changes_need_a_reason_even_when_approved() {
        let temp = tempfile::tempdir().unwrap();
        let repo = approved_repository(&temp.path().join("repo"), &[]);
        fs::remove_file(repo.join("src/t.rs")).unwrap();
        git(&repo, &["commit", "--quiet", "--all", "-m", "drop it"]);

        let mut bare = Vec::new();
        assert!(
            !run_with(
                &repo,
                "base",
                Some(&pull_request()),
                &described("Drops t."),
                &mut bare,
                &mut labelled_by("snehilvj")
            )
            .unwrap(),
            "the label alone does not pass a test change"
        );
        let bare = String::from_utf8(bare).unwrap();
        assert!(
            bare.contains("deleted test file  src/t.rs (1 tests)"),
            "{bare}"
        );
        assert!(bare.contains("- `src/t.rs`: <which requirement"), "{bare}");
        assert!(
            bare.contains("accepted by snehilvj"),
            "approved all the same: {bare}"
        );

        let reasoned = described(
            "Drops t.\n\n## Test changes\n\n- `src/t.rs`: t covered the removed parser.\n",
        );
        assert!(
            run_with(
                &repo,
                "base",
                Some(&pull_request()),
                &reasoned,
                &mut Vec::new(),
                &mut labelled_by("snehilvj")
            )
            .unwrap(),
            "a stated reason, approved, passes"
        );
        assert!(
            !run_with(
                &repo,
                "base",
                None,
                &reasoned,
                &mut Vec::new(),
                &mut labelled_by("snehilvj")
            )
            .unwrap(),
            "a reason without approval still fails"
        );
    }

    #[test]
    fn names_the_test_each_finding_changes() {
        assert_eq!(
            Finding::FewerAssertions {
                file: "a.rs".into(),
                test: "tests::t".into(),
                before: 2,
                after: 1
            }
            .test()
            .as_deref(),
            Some("a.rs::tests::t"),
            "file and test"
        );
        assert_eq!(
            Finding::DeletedTestFile {
                file: "a.rs".into(),
                tests: 2
            }
            .test()
            .as_deref(),
            Some("a.rs"),
            "a deleted file by its path"
        );
        assert_eq!(
            Finding::GateFile {
                file: "deny.toml".into()
            }
            .test(),
            None,
            "gate changes need approval, not a test reason"
        );
    }

    #[test]
    fn describes_every_finding() {
        let lines: Vec<String> = [
            Finding::DeletedTest {
                file: "a.rs".into(),
                test: "t".into(),
            },
            Finding::DeletedTestFile {
                file: "a.rs".into(),
                tests: 2,
            },
            Finding::NewlyIgnored {
                file: "a.rs".into(),
                test: "t".into(),
            },
            Finding::NewlyShouldPanic {
                file: "a.rs".into(),
                test: "t".into(),
            },
            Finding::FewerAssertions {
                file: "a.rs".into(),
                test: "t".into(),
                before: 2,
                after: 1,
            },
            Finding::GateFile {
                file: "deny.toml".into(),
            },
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            lines,
            [
                "deleted test       a.rs: t",
                "deleted test file  a.rs (2 tests)",
                "newly #[ignore]    a.rs: t",
                "newly #[should_panic] a.rs: t",
                "fewer assertions   a.rs: t (2 before, 1 now)",
                "gate file changed  deny.toml",
            ],
            "each finding reads plainly"
        );
    }
}
