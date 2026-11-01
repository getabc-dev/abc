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
mod rust;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Context;

pub(crate) use approval::PullRequest;

use crate::git;

/// Files that configure or run a gate.
const GATE_FILES: [&str; 18] = [
    "deny.toml",
    "clippy.toml",
    "rustfmt.toml",
    "rust-toolchain.toml",
    ".gitleaks.toml",
    ".claude/settings.json",
    ".config/nextest.toml",
    "CODEOWNERS",
    "xtask/tools.toml",
    "xtask/hooks/pre-tool-use.sh",
    "xtask/src/check.rs",
    "xtask/src/coverage.rs",
    "xtask/src/guard.rs",
    "xtask/src/shell.rs",
    "xtask/src/hook.rs",
    "xtask/src/mutants.rs",
    "xtask/src/review.rs",
    "xtask/src/watchdog.rs",
];

/// Directories whose every file configures or runs a gate.
const GATE_DIRS: [&str; 2] = [".github/", "xtask/src/integrity/"];

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
    WorkspaceLints,
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
            Self::WorkspaceLints => write!(f, "gate file changed  Cargo.toml [workspace.lints]"),
        }
    }
}

/// Runs the check against the merge base with `base`. With a pull request, findings pass when an
/// approver has approved them.
pub(crate) fn run(
    root: &Path,
    base: &str,
    pull_request: Option<&PullRequest>,
    out: &mut impl Write,
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
    let approvers = approval::approvers(root)?;
    let verdict = approval::approved(pull_request, &approvers, &mut |url| {
        approval::github_get(url, &pull_request.token)
    })?;
    writeln!(out, "integrity: {}", verdict.reason)?;
    Ok(verdict.approved)
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
        if change.new.as_deref() == Some("Cargo.toml") && lints_changed(root, commit)? {
            found.push(Finding::WorkspaceLints);
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
    GATE_FILES.contains(&path)
        || path.ends_with("/CODEOWNERS")
        || GATE_DIRS.iter().any(|dir| path.starts_with(dir))
}

/// Whether the `[workspace.lints]` table of the root `Cargo.toml` differs from `commit`'s.
fn lints_changed(root: &Path, commit: &str) -> anyhow::Result<bool> {
    // Compared as data, so reformatting the table is not a change.
    let lints = |text: &str| -> anyhow::Result<serde_json::Value> {
        let manifest: serde_json::Value =
            toml_edit::de::from_str(text).context("Cargo.toml is invalid")?;
        Ok(manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("lints"))
            .cloned()
            .unwrap_or_default())
    };
    let before = git::show(root, commit, "Cargo.toml")?.unwrap_or_default();
    let after = fs::read_to_string(root.join("Cargo.toml")).context("could not read Cargo.toml")?;
    Ok(lints(&before)? != lints(&after)?)
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
    use std::fs;

    use super::{Finding, Test, compare, findings, is_gate_file};
    use crate::testing::{git, repository};

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
            "docs/CODEOWNERS",
            "deny.toml",
            "xtask/src/integrity/rust.rs",
            "xtask/src/guard.rs",
            ".claude/settings.json",
        ] {
            assert!(is_gate_file(path), "{path} is a gate file");
        }
        for path in [
            "crates/abc-protocol/src/lib.rs",
            "AGENTS.md",
            "xtask/src/tools.rs",
        ] {
            assert!(!is_gate_file(path), "{path} is not a gate file");
        }
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
            Finding::WorkspaceLints,
        ] {
            assert!(
                found.contains(&expected),
                "{expected:?} missing from {found:?}"
            );
        }
        assert_eq!(found.len(), 6, "a renamed file keeps its tests: {found:?}");
        assert_eq!(
            Finding::WorkspaceLints.to_string(),
            "gate file changed  Cargo.toml [workspace.lints]",
            "the report names the table"
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
