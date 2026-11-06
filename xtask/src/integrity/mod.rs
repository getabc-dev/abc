//! `cargo xtask integrity`: changes that weaken the gates, which a person must approve.
//!
//! Comparing each changed file with its version at the merge base, it reports deleted tests and
//! test files, newly added `#[ignore]` and `#[should_panic]`, tests that lost assertions, tests
//! that a commit of the pull request added and a later one deleted, and
//! changes to gate files: CI, lint, dependency and toolchain settings, hook settings, and the
//! xtask code that runs the gates. In CI a finding fails the check unless a listed approver has
//! labelled the pull request `integrity-approved` after its latest push.
//!
//! Tests are read per language, in [`inventory`]: Rust's, and the web app's TypeScript test files
//! (`*.test.ts`, `*.test.tsx`), where `.skip` and its kin count as `#[ignore]` and `.fails` as
//! `#[should_panic]`.

mod approval;
mod reasons;
mod rust;
mod typescript;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Context;

pub(crate) use approval::PullRequest;
use reasons::Explained;
pub(crate) use reasons::{Event, event};
pub(crate) use rust::TestCode;

use crate::annotation::{self, Annotation, Mode};
use crate::git;

/// The gate files: whatever decides what passes. CODEOWNERS patterns, the single list both the
/// check and `.github/CODEOWNERS` follow: a trailing `/` covers a directory, anything else is one
/// file, both from the repository root.
pub(crate) const GATES: [&str; 30] = [
    "/.cargo/",
    "/.claude/settings.json",
    "/.config/",
    "/.gitattributes",
    "/.github/",
    "/.gitleaks.toml",
    "/AGENTS.md",
    "/CLAUDE.md",
    "/Cargo.lock",
    "/Cargo.toml",
    "/clippy.toml",
    "/deny.toml",
    "/fuzz/Cargo.lock",
    "/fuzz/Cargo.toml",
    "/fuzz/deny.toml",
    "/rust-toolchain.toml",
    "/rustfmt.toml",
    "/web/.oxlintrc.json",
    "/web/biome.json",
    "/web/knip.jsonc",
    "/web/osv-scanner.toml",
    "/web/package.json",
    "/web/playwright.config.ts",
    "/web/pnpm-lock.yaml",
    "/web/pnpm-workspace.yaml",
    "/web/tsconfig.base.json",
    "/web/tsconfig.json",
    "/web/tsconfig.node.json",
    "/web/vite.config.ts",
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

/// Whether the file at `path` is in a language the check reads tests in.
fn has_tests(path: &str) -> bool {
    is_rust(path) || typescript::is_test_file(path)
}

fn is_rust(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension == "rs")
}

/// The tests in a file, for the languages the check reads; `None` for any other file.
fn inventory(path: &str, source: &str) -> Option<anyhow::Result<Vec<Test>>> {
    if is_rust(path) {
        Some(rust::tests(source))
    } else if typescript::is_test_file(path) {
        Some(typescript::tests(path, source))
    } else {
        None
    }
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
    /// A test that a commit since the merge base added and a later one deleted, which comparing
    /// the merge base with the tree cannot see.
    AddedThenDeleted {
        file: String,
        test: String,
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
            Self::AddedThenDeleted { file, test } => {
                write!(f, "added then deleted {file}: {test}")
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
    /// The file the finding is about, and what kind of finding it is, as a short title.
    fn place(&self) -> (&str, &'static str) {
        match self {
            Self::DeletedTest { file, .. } => (file, "Deleted test"),
            Self::DeletedTestFile { file, .. } => (file, "Deleted test file"),
            Self::AddedThenDeleted { file, .. } => (file, "Test added then deleted"),
            Self::NewlyIgnored { file, .. } => (file, "Newly ignored test"),
            Self::NewlyShouldPanic { file, .. } => (file, "Newly should_panic test"),
            Self::FewerAssertions { file, .. } => (file, "Fewer assertions"),
            Self::GateFile { file } => (file, "Gate file changed"),
        }
    }

    /// The test this finding changes, as the description's "## Test changes" section names it:
    /// `<file>::<test>`, or the file for a deleted test file. `None` for gate changes.
    fn test(&self) -> Option<String> {
        match self {
            Self::DeletedTest { file, test }
            | Self::AddedThenDeleted { file, test }
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
/// listed at the merge base has approved them. In `mode` it annotates each finding for GitHub.
pub(crate) fn run(
    root: &Path,
    base: &str,
    pull_request: Option<&PullRequest>,
    event: &Event,
    mode: Mode,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    run_with(
        root,
        base,
        pull_request,
        event,
        mode,
        out,
        &mut |url, token| approval::github_get(url, token),
    )
}

/// [`run`], reading GitHub's API through `fetch` (URL and token).
fn run_with(
    root: &Path,
    base: &str,
    pull_request: Option<&PullRequest>,
    event: &Event,
    mode: Mode,
    out: &mut impl Write,
    fetch: &mut impl FnMut(&str, &str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<bool> {
    let commit = git::merge_base(root, base)?;
    let review = Review::of(root, &commit, event.description.as_deref())?;
    let Review { found, explained } = &review;
    if found.is_empty() && explained.complete() {
        writeln!(
            out,
            "integrity: no deleted, skipped or weakened tests and no gate changes"
        )?;
        return Ok(true);
    }
    if !found.is_empty() {
        writeln!(out, "integrity: {} finding(s)", found.len())?;
    }
    for finding in found {
        writeln!(out, "  {finding}")?;
    }
    explained.report(out)?;
    write!(out, "{}", annotation::commands(mode, &review.annotations()))?;
    if found.is_empty() {
        return Ok(false);
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

/// The findings since a merge base, and how a description explains them.
#[derive(Debug)]
pub(crate) struct Review {
    found: Vec<Finding>,
    explained: Explained,
}

impl Review {
    /// The findings from `commit` to the working tree of `root`, against `description`.
    pub(crate) fn of(root: &Path, commit: &str, description: Option<&str>) -> anyhow::Result<Self> {
        let found = findings(root, commit)?;
        let explained = reasons::explained(&flagged(&found), description.unwrap_or_default());
        Ok(Self { found, explained })
    }

    /// Whether the description gives a reason for every flagged test and names no other.
    pub(crate) const fn explained(&self) -> bool {
        self.explained.complete()
    }

    /// What the description lacks or names in excess, as `integrity` reports it.
    pub(crate) fn report(&self, out: &mut impl Write) -> std::io::Result<()> {
        self.explained.report(out)
    }

    /// A "## Test changes" section naming every flagged test, its reason left to write.
    pub(crate) fn skeleton(&self) -> String {
        reasons::skeleton(&flagged(&self.found))
    }

    /// An annotation for each finding, saying whether the description gives its reason, and
    /// one for each entry that names a test no finding changes.
    fn annotations(&self) -> Vec<Annotation> {
        let mut annotations: Vec<Annotation> = self
            .found
            .iter()
            .map(|finding| {
                let (file, title) = finding.place();
                let needs = match finding.test() {
                    Some(test) if self.explained.unexplained.contains(&test) => format!(
                        "No reason for `{test}` in the description's \"## {}\" section.",
                        reasons::HEADING
                    ),
                    Some(test) => format!(
                        "The description's \"## {}\" section gives a reason for `{test}`.",
                        reasons::HEADING
                    ),
                    None => format!(
                        "A gate file: it needs the {} label from an approver in {} and a code \
                         owner's review.",
                        approval::LABEL,
                        approval::APPROVERS
                    ),
                };
                Annotation::error(file, title, format!("{finding}. {needs}"))
            })
            .collect();
        annotations.extend(self.explained.extra.iter().map(|test| Annotation {
            file: None,
            ..Annotation::error(
                "",
                "Stale test change",
                format!(
                    "The description's \"## {}\" section names `{test}`, which has no flagged \
                     change.",
                    reasons::HEADING
                ),
            )
        }));
        annotations
    }
}

/// The tests `found` changes, each once, in order.
fn flagged(found: &[Finding]) -> Vec<String> {
    found
        .iter()
        .filter_map(Finding::test)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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
        // Only files with tests are read: any other may be binary, or a link to a directory.
        let before = match &change.old {
            Some(path) if has_tests(path) => match git::show(root, commit, path)? {
                Some(source) => tests_in(path, &source)?,
                None => None,
            },
            Some(_) | None => None,
        };
        let after = match &change.new {
            Some(path) if has_tests(path) => {
                let source = fs::read_to_string(root.join(path))
                    .with_context(|| format!("could not read {path}"))?;
                tests_in(path, &source)?
            }
            Some(_) | None => None,
        };
        let file = change
            .new
            .clone()
            .or_else(|| change.old.clone())
            .unwrap_or_default();
        found.extend(compare(&file, before.as_deref(), after.as_deref()));
    }
    found.extend(added_then_deleted(root, commit)?);
    Ok(found)
}

/// Tests that exist in a file at some commit between `commit` and HEAD but neither at `commit`
/// nor in the working tree: added, then deleted, within the pull request. Only the files with
/// tests that those commits touched are read, once per version.
///
/// Each commit of the range lists the files it changed itself, so the walk covers every commit
/// of the pull request whether HEAD is its head or GitHub's merge commit of it into the base.
/// A path-limited `rev-list` would not: at a merge whose result equals its first parent for that
/// path (the base, where a file the pull request created and deleted never existed), git's
/// history simplification follows that parent alone and drops the pull request's side (#38).
fn added_then_deleted(root: &Path, commit: &str) -> anyhow::Result<Vec<Finding>> {
    let log = git::git(
        root,
        &[
            "-c",
            "core.quotePath=false",
            "log",
            "--format=%x00%H",
            "--name-only",
            "--no-renames",
            &format!("{commit}..HEAD"),
        ],
    )?;
    let mut versions: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in log.split('\0').skip(1) {
        let mut lines = entry.lines();
        let Some(revision) = lines.next() else {
            continue;
        };
        for path in lines.filter(|path| has_tests(path)) {
            versions.entry(path).or_default().push(revision);
        }
    }
    let names = |path: &str, source: Option<String>| -> anyhow::Result<BTreeSet<String>> {
        Ok(match source {
            Some(source) => tests_in(path, &source)?
                .unwrap_or_default()
                .into_iter()
                .map(|test| test.name)
                .collect(),
            None => BTreeSet::new(),
        })
    };
    let mut found = Vec::new();
    for (path, revisions) in versions {
        let now = fs::read_to_string(root.join(path)).ok();
        let mut kept = names(path, git::show(root, commit, path)?)?;
        kept.extend(names(path, now)?);
        let mut gone = BTreeSet::new();
        for revision in revisions {
            gone.extend(
                names(path, git::show(root, revision, path)?)?
                    .into_iter()
                    .filter(|test| !kept.contains(test)),
            );
        }
        found.extend(gone.into_iter().map(|test| Finding::AddedThenDeleted {
            file: path.to_owned(),
            test,
        }));
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

    use crate::annotation::Mode;

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
    fn reads_tests_in_rust_and_in_the_web_apps_test_files() {
        let rust = super::inventory("src/a.rs", "#[test]\nfn a() { assert!(true); }\n")
            .unwrap()
            .unwrap();
        assert_eq!(rust.len(), 1, "a Rust test");
        let web = super::inventory("web/src/a.test.ts", "test(\"a\", () => { expect(1); });\n")
            .unwrap()
            .unwrap();
        assert_eq!(web.len(), 1, "a Vitest test");
        assert!(
            super::inventory("web/src/a.ts", "test(\"a\", () => {});\n").is_none(),
            "not a test file"
        );
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
            "web/osv-scanner.toml",
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

    /// osv-scanner's ignores are scoped and temporary: each names one advisory, says why it
    /// cannot reach what abc ships, and expires, so the scan fails again unless someone renews
    /// it on purpose.
    #[test]
    fn every_osv_ignore_names_its_reason_and_expires() {
        let config: toml_edit::DocumentMut =
            crate::testing::read_in_repository("web/osv-scanner.toml")
                .parse()
                .unwrap();
        let ignores = config["IgnoredVulns"].as_array_of_tables().unwrap();
        assert!(!ignores.is_empty(), "the config ignores something");
        for ignore in ignores {
            let id = ignore["id"].as_str().unwrap();
            assert!(id.starts_with("GHSA-"), "{id} is one advisory");
            assert!(
                ignore["ignoreUntil"].as_datetime().is_some(),
                "{id} has an ignoreUntil date"
            );
            assert!(
                ignore["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.len() > 40),
                "{id} says why"
            );
            let keys: Vec<&str> = ignore.iter().map(|(key, _)| key).collect();
            assert_eq!(keys, ["id", "ignoreUntil", "reason"], "{id}'s fields");
        }
    }

    #[test]
    fn codeowners_follow_the_gate_list() {
        let codeowners = crate::testing::read_in_repository(".github/CODEOWNERS");
        let owned: BTreeSet<&str> = codeowners
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
        // A binary file at the base is not read for tests when it goes away.
        fs::write(repo.join("old.bin"), [0xff_u8, 0xfe, 0x00]).unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

        fs::remove_file(repo.join("old.bin")).unwrap();
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
        // Files with no tests to read: a link to a directory and a binary file.
        std::os::unix::fs::symlink("src", repo.join("linked")).unwrap();
        fs::write(repo.join("logo.bin"), [0xff_u8, 0xfe, 0x00]).unwrap();
        git(&repo, &["add", "linked", "logo.bin"]);

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
                br#"{"workflow_runs": [{"created_at": "2026-11-01T09:00:00Z", "actor": {"login": "getabc-dev-agent[bot]"}}]}"#.to_vec()
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
            run_with(
                &clean,
                "base",
                None,
                &Event::default(),
                Mode::Plain,
                &mut out,
                unused
            )
            .unwrap(),
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
                Mode::Plain,
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
                Mode::Plain,
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
                Mode::Plain,
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
                Mode::Plain,
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
            Mode::Plain,
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
            run(
                &clean,
                "base",
                None,
                &Event::default(),
                Mode::Plain,
                &mut Vec::new()
            )
            .unwrap(),
            "clean"
        );
        let gated = repository_with_change(&temp.path().join("gated"), &[("deny.toml", "\n")]);
        assert!(
            !run(
                &gated,
                "base",
                None,
                &Event::default(),
                Mode::Plain,
                &mut Vec::new()
            )
            .unwrap(),
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
                Mode::Plain,
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
                Mode::Plain,
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
                Mode::Plain,
                &mut Vec::new(),
                &mut labelled_by("snehilvj")
            )
            .unwrap(),
            "a reason without approval still fails"
        );
    }

    #[test]
    fn annotates_each_finding_and_fails_an_entry_for_a_test_not_flagged() {
        let temp = tempfile::tempdir().unwrap();
        let repo = approved_repository(&temp.path().join("repo"), &[("deny.toml", "\n")]);
        fs::remove_file(repo.join("src/t.rs")).unwrap();
        git(&repo, &["commit", "--quiet", "--all", "-m", "drop it"]);
        let stale = described(
            "## Test changes\n\n- `src/t.rs`: t covered the removed parser.\n- \
             `src/u.rs::tests::gone`: an entry left from an earlier push.\n",
        );
        let mut out = Vec::new();
        assert!(
            !run_with(
                &repo,
                "base",
                Some(&pull_request()),
                &stale,
                Mode::Actions,
                &mut out,
                &mut labelled_by("snehilvj")
            )
            .unwrap(),
            "an entry for a test integrity does not flag fails, approved or not; #25 had three"
        );
        let text = String::from_utf8(out).unwrap();
        for line in [
            "::error file=deny.toml,title=Gate file changed::gate file changed  deny.toml. A gate \
             file: it needs the integrity-approved label from an approver in \
             .github/integrity-approvers and a code owner's review.",
            "::error file=src/t.rs,title=Deleted test file::deleted test file  src/t.rs (1 tests). \
             The description's \"## Test changes\" section gives a reason for `src/t.rs`.",
            "::error title=Stale test change::The description's \"## Test changes\" section names \
             `src/u.rs::tests::gone`, which has no flagged change.",
        ] {
            assert!(
                text.lines().any(|printed| printed == line),
                "{line}\n{text}"
            );
        }

        let clean = approved_repository(&temp.path().join("clean"), &[("src/a.rs", "fn a() {}\n")]);
        let mut only_stale = Vec::new();
        assert!(
            !run_with(
                &clean,
                "base",
                None,
                &stale,
                Mode::Plain,
                &mut only_stale,
                &mut |_: &str, _: &str| -> anyhow::Result<Vec<u8>> {
                    panic!("no findings, no API call")
                }
            )
            .unwrap(),
            "stale entries fail without any finding"
        );
        let only_stale = String::from_utf8(only_stale).unwrap();
        assert!(
            only_stale.contains("names `src/t.rs`, which has no flagged change"),
            "{only_stale}"
        );
        assert!(!only_stale.contains("::error"), "{only_stale}");
        assert!(
            !only_stale.contains("finding(s)"),
            "no count of findings when there are none: {only_stale}"
        );
        let mut unexplained = Vec::new();
        assert!(
            !run_with(
                &repo,
                "base",
                Some(&pull_request()),
                &described("None"),
                Mode::Actions,
                &mut unexplained,
                &mut labelled_by("snehilvj")
            )
            .unwrap()
        );
        let unexplained = String::from_utf8(unexplained).unwrap();
        assert!(
            unexplained.contains(
                "::error file=src/t.rs,title=Deleted test file::deleted test file  src/t.rs (1 \
                 tests). No reason for `src/t.rs` in the description's \"## Test changes\" \
                 section."
            ),
            "{unexplained}"
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
    fn finds_a_test_added_then_deleted_within_the_pull_request() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        fs::create_dir_all(repo.join("src")).unwrap();
        let tests = |names: &[&str]| -> String {
            names
                .iter()
                .map(|name| format!("#[test]\nfn {name}() {{ assert!(true); }}\n"))
                .collect::<Vec<_>>()
                .concat()
        };
        let commit = |files: &[(&str, &str)]| {
            for (path, text) in files {
                fs::write(repo.join(path), text).unwrap();
            }
            git(&repo, &["add", "--all"]);
            git(&repo, &["commit", "--quiet", "-m", "change"]);
        };
        commit(&[("src/a.rs", &tests(&["old", "gone"])), ("README.md", "x\n")]);
        let base = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        // A build agent once added a test, then deleted it in a later commit of the same pull
        // request to fit the review's size limit; base and head alone never showed it.
        commit(&[
            ("src/a.rs", &tests(&["old", "gone", "added", "kept"])),
            ("src/b.rs", &tests(&["brief"])),
        ]);
        commit(&[("src/a.rs", &tests(&["old", "kept"])), ("README.md", "y\n")]);
        fs::remove_file(repo.join("src/b.rs")).unwrap();
        let found = findings(&repo, &base).unwrap();
        assert_eq!(
            found,
            [
                Finding::DeletedTest {
                    file: "src/a.rs".into(),
                    test: "gone".into(),
                },
                Finding::AddedThenDeleted {
                    file: "src/a.rs".into(),
                    test: "added".into(),
                },
                Finding::AddedThenDeleted {
                    file: "src/b.rs".into(),
                    test: "brief".into(),
                },
            ],
            "a test at the base is a deleted test, once; one kept to the end is no finding"
        );
    }

    /// #38: `web/src/ui/ui.test.tsx` was created and deleted within the pull request. On the
    /// head, integrity flagged its tests as added then deleted; on GitHub's merge commit, the
    /// path-limited walk followed only the merge's first parent (main, where the file never
    /// existed) and missed them, so the description's reasons became "stale" there. Both runs
    /// must walk the pull request's own commits.
    #[test]
    fn the_head_and_the_merge_commit_find_the_same_added_then_deleted_tests() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        let commit = |files: &[(&str, Option<&str>)]| {
            for (path, text) in files {
                let target = repo.join(path);
                match text {
                    Some(text) => {
                        fs::create_dir_all(target.parent().unwrap()).unwrap();
                        fs::write(target, text).unwrap();
                    }
                    None => fs::remove_file(target).unwrap(),
                }
            }
            git(&repo, &["add", "--all"]);
            git(&repo, &["commit", "--quiet", "-m", "change"]);
        };
        let ui = "web/src/ui/ui.test.tsx";
        commit(&[("README.md", Some("x\n"))]);
        git(&repo, &["checkout", "--quiet", "-b", "pr"]);
        commit(&[(
            ui,
            Some(
                "test(\"copies\", () => { expect(1).toBe(1); });\ntest(\"wraps\", () => { expect(1).toBe(1); });\n",
            ),
        )]);
        commit(&[(ui, None), ("web/src/app.ts", Some("export {};\n"))]);
        git(&repo, &["checkout", "--quiet", "main"]);
        commit(&[("CONTRIBUTING.md", Some("main moved on\n"))]);
        let expected = [
            Finding::AddedThenDeleted {
                file: ui.into(),
                test: "copies".into(),
            },
            Finding::AddedThenDeleted {
                file: ui.into(),
                test: "wraps".into(),
            },
        ];
        git(&repo, &["checkout", "--quiet", "pr"]);
        let head = findings(&repo, &crate::git::merge_base(&repo, "main").unwrap()).unwrap();
        assert_eq!(head, expected, "on the head");
        // GitHub's refs/pull/N/merge: main first, the pull request's head second.
        git(&repo, &["checkout", "--quiet", "--detach", "main"]);
        git(&repo, &["merge", "--quiet", "--no-ff", "--no-edit", "pr"]);
        let merge = findings(&repo, &crate::git::merge_base(&repo, "main").unwrap()).unwrap();
        assert_eq!(merge, expected, "on the merge commit, as on the head");
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
            Finding::AddedThenDeleted {
                file: "a.rs".into(),
                test: "t".into(),
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
                "added then deleted a.rs: t",
                "newly #[ignore]    a.rs: t",
                "newly #[should_panic] a.rs: t",
                "fewer assertions   a.rs: t (2 before, 1 now)",
                "gate file changed  deny.toml",
            ],
            "each finding reads plainly"
        );
    }
}
