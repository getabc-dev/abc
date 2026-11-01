//! Git plumbing shared by the gates: the default branch, merge bases, diffs, and files as they
//! were at a revision.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail, ensure};

/// Remotes whose `HEAD` names the default branch, in order of preference: a fork's `upstream`,
/// then `origin`.
const DEFAULT_BRANCH_REMOTES: [&str; 2] = ["upstream", "origin"];

/// Runs git in `root` and returns what it prints, failing with git's message.
pub(crate) fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("could not run git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git printed text that is not UTF-8")
}

/// The remote-tracking ref of the repository's default branch, such as `refs/remotes/origin/main`.
pub(crate) fn default_branch(root: &Path) -> anyhow::Result<String> {
    for remote in DEFAULT_BRANCH_REMOTES {
        let output = Command::new("git")
            .args(["symbolic-ref", "--quiet"])
            .arg(format!("refs/remotes/{remote}/HEAD"))
            .current_dir(root)
            .output()
            .context("could not run git")?;
        if output.status.success() {
            let target = String::from_utf8(output.stdout).context("git printed a non-UTF-8 ref")?;
            return Ok(target.trim().to_owned());
        }
    }
    bail!(
        "cannot tell this repository's default branch: neither upstream/HEAD nor origin/HEAD is \
         set; run `git remote set-head origin --auto`"
    )
}

/// The commit where `HEAD` left `base`.
pub(crate) fn merge_base(root: &Path, base: &str) -> anyhow::Result<String> {
    Ok(git(root, &["merge-base", "HEAD", base])
        .with_context(|| format!("could not find the merge base with {base}; fetch it first"))?
        .trim()
        .to_owned())
}

/// The unified diff from `commit` to the working tree, with renames detected and `context`
/// lines around each change.
pub(crate) fn diff(root: &Path, commit: &str, context: u32) -> anyhow::Result<String> {
    git(
        root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            &format!("-U{context}"),
            commit,
        ],
    )
}

/// The content of `path` at `revision`, or `None` when the file did not exist there.
pub(crate) fn show(root: &Path, revision: &str, path: &str) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .args(["show", &format!("{revision}:{path}")])
        .current_dir(root)
        .output()
        .context("could not run git")?;
    if output.status.success() {
        Ok(Some(String::from_utf8(output.stdout).with_context(
            || format!("{path} at {revision} is not UTF-8"),
        )?))
    } else {
        Ok(None)
    }
}

/// One changed file between a commit and the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    /// The path before the change; `None` for an added file.
    pub(crate) old: Option<String>,
    /// The path after the change; `None` for a deleted file.
    pub(crate) new: Option<String>,
}

/// The files changed from `commit` to the working tree, renames paired up.
pub(crate) fn changes(root: &Path, commit: &str) -> anyhow::Result<Vec<Change>> {
    let listing = git(
        root,
        &["diff", "--name-status", "--no-color", "-M", "-z", commit],
    )?;
    parse_name_status(&listing)
}

/// Parses `git diff --name-status -z` output.
fn parse_name_status(listing: &str) -> anyhow::Result<Vec<Change>> {
    let mut fields = listing.split('\0').filter(|field| !field.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let mut path = || {
            fields
                .next()
                .map(str::to_owned)
                .with_context(|| format!("git listed status {status} without a path"))
        };
        let change = match status.chars().next() {
            Some('A') => Change {
                old: None,
                new: Some(path()?),
            },
            Some('D') => Change {
                old: Some(path()?),
                new: None,
            },
            Some('R' | 'C') => Change {
                old: Some(path()?),
                new: Some(path()?),
            },
            Some(_) => {
                let same = path()?;
                Change {
                    old: Some(same.clone()),
                    new: Some(same),
                }
            }
            None => bail!("git listed an empty status"),
        };
        changes.push(change);
    }
    Ok(changes)
}

/// The lines each file gained or changed, by path in its new version and line number, read from
/// a unified diff.
pub(crate) fn added_lines(diff: &str) -> BTreeMap<String, BTreeSet<u32>> {
    let mut added: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut file: Option<String> = None;
    let mut line: u32 = 0;
    // Each file starts with a `diff --git` header that runs to its first hunk; only there does a
    // `+++` line name the file, since inside a hunk it is an added line starting with `++`.
    let mut header = false;
    for text in diff.lines() {
        if text.starts_with("diff --git ") {
            header = true;
            file = None;
        } else if let Some(hunk) = text.strip_prefix("@@ ") {
            header = false;
            line = hunk_start(hunk).unwrap_or(0);
        } else if header {
            if let Some(path) = text.strip_prefix("+++ ") {
                file = path.strip_prefix("b/").map(str::to_owned);
            }
        } else if let Some(path) = &file {
            if text.starts_with('+') {
                added.entry(path.clone()).or_default().insert(line);
                line = line.saturating_add(1);
            } else if text.starts_with(' ') {
                line = line.saturating_add(1);
            }
        }
    }
    added
}

/// The first new-file line of a hunk header such as `-3,2 +5,4 @@`.
fn hunk_start(header: &str) -> Option<u32> {
    let new = header
        .split_whitespace()
        .find_map(|part| part.strip_prefix('+'))?;
    new.split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use super::{Change, added_lines, changes, diff, merge_base, parse_name_status, show};
    use crate::testing::{git, repository};

    #[test]
    fn reads_added_lines_from_a_diff() {
        let text = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,5 @@\n fn a() {}\n-fn b() {}\n+fn b() { 1 }\n+fn c() {}\n+++ looks like a header\n fn d() {}\n@@ -10 +11,2 @@\n+x\n+y\n\\ No newline at end of file\ndiff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n";
        let added = added_lines(text);
        assert_eq!(
            added.get("src/a.rs"),
            Some(&BTreeSet::from([2, 3, 4, 11, 12])),
            "context lines advance the count, removals do not, and `+++` in a hunk is an added line"
        );
        assert!(
            !added.contains_key("gone.rs"),
            "a deleted file gains nothing"
        );
    }

    #[test]
    fn reads_name_status_with_renames() {
        let changes = parse_name_status("M\0a.rs\0A\0b.rs\0D\0c.rs\0R087\0d.rs\0e.rs\0").unwrap();
        assert_eq!(
            changes,
            [
                Change {
                    old: Some("a.rs".into()),
                    new: Some("a.rs".into())
                },
                Change {
                    old: None,
                    new: Some("b.rs".into())
                },
                Change {
                    old: Some("c.rs".into()),
                    new: None
                },
                Change {
                    old: Some("d.rs".into()),
                    new: Some("e.rs".into())
                },
            ],
            "every status maps to its paths"
        );
        assert!(parse_name_status("M\0").is_err(), "a status needs a path");
    }

    #[test]
    fn diffs_against_the_merge_base() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        fs::write(repo.join("kept.txt"), "one\n").unwrap();
        fs::write(repo.join("old.txt"), "a\nb\nc\nd\n").unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        git(&repo, &["branch", "base"]);
        fs::write(repo.join("kept.txt"), "one\ntwo\n").unwrap();
        git(&repo, &["mv", "old.txt", "new.txt"]);
        git(&repo, &["commit", "--quiet", "--all", "-m", "change"]);

        let base = merge_base(&repo, "base").unwrap();
        assert_eq!(
            added_lines(&diff(&repo, &base, 0).unwrap()).get("kept.txt"),
            Some(&BTreeSet::from([2])),
            "the second line is new"
        );
        assert!(
            changes(&repo, &base).unwrap().contains(&Change {
                old: Some("old.txt".into()),
                new: Some("new.txt".into())
            }),
            "the rename is paired"
        );
        assert_eq!(
            show(&repo, &base, "old.txt").unwrap().as_deref(),
            Some("a\nb\nc\nd\n"),
            "the old content"
        );
        assert_eq!(show(&repo, &base, "new.txt").unwrap(), None, "absent then");
        assert!(
            merge_base(&repo, "nowhere").is_err(),
            "an unknown base fails"
        );
    }
}
