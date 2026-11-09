//! Git plumbing shared by the gates: the default branch, merge bases, diffs, and files as they
//! were at a revision.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// How long a fetch or push xtask makes for itself may take: a minute, ample for one branch of a
/// repository this size, so a stalled network fails it rather than hanging the command.
pub(crate) const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// Fetches `branch` from `remote` in `root`, never waiting on a person (see [`remote`]), and stops
/// git after `timeout`.
pub(crate) fn fetch(
    root: &Path,
    remote: &str,
    branch: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    self::remote(root, &["fetch", "--quiet", remote, branch], timeout)
}

/// Pushes `commit` to `branch` on `remote` from `root`, never waiting on a person (see
/// [`remote`]), and stops git after `timeout`. A branch that exists and does not lead to `commit`
/// is refused, never forced.
pub(crate) fn push(
    root: &Path,
    remote: &str,
    commit: &str,
    branch: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    self::remote(
        root,
        &[
            "push",
            "--quiet",
            remote,
            &format!("{commit}:refs/heads/{branch}"),
        ],
        timeout,
    )
}

/// Runs git with `args`, a command that talks to a remote, in `root`, never waiting on a person:
/// git gets no stdin, no terminal prompt (`GIT_TERMINAL_PROMPT=0`) and no askpass program, which
/// git would still run with terminal prompts off, so a remote that wants credentials the
/// configured helpers do not have fails at once; and git is stopped after `timeout`.
fn remote(root: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<()> {
    let command = args
        .iter()
        .copied()
        .filter(|arg| *arg != "--quiet")
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = Command::new("git")
        .args(["-c", "core.askPass="])
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env_remove("SSH_ASKPASS")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run git")?;
    // Read what git says beside it, so a full pipe never stalls git; a remote helper git leaves
    // running past the timeout keeps only this thread waiting.
    let mut stderr = child.stderr.take().context("git has no stderr")?;
    let (said, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _unread = stderr.read_to_string(&mut text);
        let _unheard = said.send(text);
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("could not wait for git {command}"))?
        {
            break status;
        }
        if started.elapsed() >= timeout {
            child
                .kill()
                .with_context(|| format!("could not stop git {command}"))?;
            child
                .wait()
                .with_context(|| format!("could not reap git {command}"))?;
            bail!(
                "git {command} took longer than {} seconds and was stopped",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let message = heard
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or_default();
    ensure!(status.success(), "git {command} failed: {}", message.trim());
    Ok(())
}

/// The first twelve characters of a commit's name.
pub(crate) fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
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

/// The pathspec attribute that marks a generated file in `.gitattributes`.
pub(crate) const GENERATED: &str = "linguist-generated";

/// Where a diff ends: the working tree, as `check` and `pr` measure a change before it is pushed,
/// or a commit, as the review reads a pull request from git's objects alone, never through its
/// files on disk.
#[derive(Debug, Clone, Copy)]
pub(crate) enum To<'a> {
    WorkingTree,
    Commit(&'a str),
}

impl To<'_> {
    /// The arguments that end a `git diff` from `commit` here.
    fn range<'b>(self, commit: &'b str) -> Vec<&'b str>
    where
        Self: 'b,
    {
        match self {
            Self::WorkingTree => vec![commit],
            Self::Commit(head) => vec![commit, head],
        }
    }
}

/// `git diff` from `commit` to `to` with `options`, over `pathspecs`, reading `.gitattributes` as
/// it was at `commit`.
fn diff_at(
    root: &Path,
    commit: &str,
    to: To<'_>,
    options: &[&str],
    pathspecs: &[&str],
) -> anyhow::Result<String> {
    let attr_source = format!("--attr-source={commit}");
    let mut args = vec![
        attr_source.as_str(),
        "-c",
        "core.quotePath=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-M",
    ];
    args.extend_from_slice(options);
    args.extend(to.range(commit));
    args.push("--");
    args.extend_from_slice(pathspecs);
    git(root, &args)
}

/// The pathspec that leaves generated files out.
fn not_generated() -> String {
    format!(":(exclude,attr:{GENERATED})")
}

/// The diff drivers whose function boundaries the review's context follows: git's built-in
/// `rust` driver, set for `*.rs` in `.gitattributes`. git has no driver for TypeScript, YAML or
/// TOML, and without one any line that starts with a letter counts as a function's start, so
/// `--function-context` there sends whole blocks for a one-line change; git's `markdown` and
/// `bash` drivers make a whole section or everything after a function its context.
const FUNCTION_DRIVERS: [&str; 1] = ["rust"];

/// The change from `commit` to `to` as the review's size limit measures it: the plain diff with
/// three lines of context, without generated files, and the generated files it left out. Which
/// files are generated comes from `.gitattributes` as it was at `commit`, never from the change,
/// so a change cannot hide its own code by marking it generated.
pub(crate) fn reviewable_diff(
    root: &Path,
    commit: &str,
    to: To<'_>,
) -> anyhow::Result<(String, Vec<String>)> {
    let diff = diff_at(root, commit, to, &["-U3"], &[".", &not_generated()])?;
    let generated = diff_at(
        root,
        commit,
        to,
        &["--name-only"],
        &[&format!(":(attr:{GENERATED})")],
    )?;
    Ok((diff, generated.lines().map(str::to_owned).collect()))
}

/// The diff the review reads, from `commit` to `to`, without generated files: each change in a
/// file whose diff driver knows its functions ([`FUNCTION_DRIVERS`]) with its whole function as
/// context (`--function-context`), and every other change with three lines, as
/// [`reviewable_diff`] measures it. The drivers come from `.gitattributes` at `commit`.
pub(crate) fn context_diff(root: &Path, commit: &str, to: To<'_>) -> anyhow::Result<String> {
    let drivers: Vec<String> = FUNCTION_DRIVERS
        .iter()
        .map(|driver| format!(":(attr:diff={driver})"))
        .collect();
    let others: Vec<String> = FUNCTION_DRIVERS
        .iter()
        .map(|driver| format!(":(exclude,attr:diff={driver})"))
        .collect();
    let excluded = not_generated();
    let mut with_functions: Vec<&str> = drivers.iter().map(String::as_str).collect();
    with_functions.push(&excluded);
    let mut rest: Vec<&str> = vec!["."];
    rest.extend(others.iter().map(String::as_str));
    rest.push(&excluded);
    let mut diff = diff_at(root, commit, to, &["--function-context"], &with_functions)?;
    diff.push_str(&diff_at(root, commit, to, &["-U3"], &rest)?);
    Ok(diff)
}

/// The files the review may read in full beside the diff: those changed from `commit` to `to`
/// that existed at `commit` too (an added file is whole in the diff already), not generated (by
/// `.gitattributes` at `commit`), with their paths at `to`.
pub(crate) fn modified_files(root: &Path, commit: &str, to: To<'_>) -> anyhow::Result<Vec<String>> {
    let listing = diff_at(
        root,
        commit,
        to,
        &["--diff-filter=MR", "--name-only"],
        &[".", &not_generated()],
    )?;
    Ok(listing.lines().map(str::to_owned).collect())
}

/// A file at a commit, as [`blobs`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Blob {
    /// A regular file of at most the size asked for, in UTF-8.
    Text(String),
    /// A regular file larger than that, unread.
    TooLarge(u64),
    /// Not a regular file of text: a symlink, a submodule, or bytes that are not UTF-8.
    NotText,
}

/// The files `paths` at `revision`, read from git's objects alone, never the filesystem: only
/// regular files (modes 100644 and 100755) of at most `max_bytes` are read, so a symlink is never
/// followed and a large blob never loaded. A path missing at `revision` is left out.
pub(crate) fn blobs(
    root: &Path,
    revision: &str,
    paths: &[String],
    max_bytes: u64,
) -> anyhow::Result<Vec<(String, Blob)>> {
    let listing = git(
        root,
        &["ls-tree", "-r", "-l", "-z", "--full-tree", revision],
    )?;
    let mut entries = BTreeMap::new();
    for entry in listing.split('\0').filter(|entry| !entry.is_empty()) {
        let (fields, path) = entry
            .split_once('\t')
            .with_context(|| format!("git ls-tree listed {entry:?} without a path"))?;
        let fields: Vec<&str> = fields.split_whitespace().collect();
        let [mode, _, object, size] = fields[..] else {
            bail!("git ls-tree listed {entry:?}, not a mode, type, object and size");
        };
        entries.insert(path, (mode, object, size));
    }
    let mut read = Vec::new();
    for path in paths {
        let Some(&(mode, object, size)) = entries.get(path.as_str()) else {
            continue;
        };
        let blob = if mode == "100644" || mode == "100755" {
            let size: u64 = size
                .parse()
                .with_context(|| format!("git ls-tree gave {path} the size {size:?}"))?;
            if size > max_bytes {
                Blob::TooLarge(size)
            } else {
                let output = Command::new("git")
                    .args(["cat-file", "blob", object])
                    .current_dir(root)
                    .output()
                    .context("could not run git")?;
                ensure!(
                    output.status.success(),
                    "git cat-file blob {object} ({path}) failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                String::from_utf8(output.stdout).map_or(Blob::NotText, Blob::Text)
            }
        } else {
            Blob::NotText
        };
        read.push((path.clone(), blob));
    }
    Ok(read)
}

/// The content of `path` at `revision`, or `None` when the file did not exist there. A revision
/// git does not have is an error, never a missing file.
pub(crate) fn show(root: &Path, revision: &str, path: &str) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .args(["show", &format!("{revision}:{path}")])
        .current_dir(root)
        .output()
        .context("could not run git")?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8(output.stdout)
                .with_context(|| format!("{path} at {revision} is not UTF-8"))?,
        ));
    }
    // git show fails alike for a file the revision lacks and a revision git lacks.
    git(root, &["cat-file", "-e", &format!("{revision}^{{tree}}")])
        .with_context(|| format!("git has no {revision} to read {path} at"))?;
    Ok(None)
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
    use std::fmt::Write as _;
    use std::fs;

    use std::time::Duration;

    use super::{
        Blob, Change, To, added_lines, blobs, changes, context_diff, diff, merge_base,
        parse_name_status, reviewable_diff, show,
    };
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
    fn leaves_out_files_generated_at_the_merge_base_only() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        fs::write(repo.join(".gitattributes"), "gen/** linguist-generated\n").unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        git(&repo, &["branch", "base"]);
        fs::create_dir_all(repo.join("gen")).unwrap();
        fs::write(repo.join("gen/cache.json"), "{\"generated\": true}\n").unwrap();
        fs::write(repo.join("src.rs"), "fn hidden() {}\n").unwrap();
        // The change tries to hide its own code by marking it generated too.
        fs::write(
            repo.join(".gitattributes"),
            "gen/** linguist-generated\nsrc.rs linguist-generated\n",
        )
        .unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "change"]);

        let base = merge_base(&repo, "base").unwrap();
        let (text, generated) = reviewable_diff(&repo, &base, To::Commit("HEAD")).unwrap();
        assert_eq!(
            generated,
            ["gen/cache.json"],
            "the generated file is listed"
        );
        assert!(
            !text.contains("\"generated\": true"),
            "its content is left out"
        );
        assert!(
            text.contains("+fn hidden() {}"),
            "a file the change itself marks generated is still reviewed"
        );
        assert!(
            text.contains("+src.rs linguist-generated"),
            "and so is the change to .gitattributes"
        );
    }

    #[test]
    fn the_review_diff_shows_whole_functions_and_lists_modified_files() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        fs::write(repo.join(".gitattributes"), "*.rs diff=rust\n").unwrap();
        let body = |value: &str| {
            format!(
                "mod tests {{\n    fn first() {{\n        let a = 1;\n        let b = 2;\n        \
                 let c = 3;\n        let d = 4;\n        let value = {value};\n    }}\n\n    fn \
                 second() {{\n        let unrelated = 0;\n    }}\n}}\n"
            )
        };
        fs::write(repo.join("a.rs"), body("1")).unwrap();
        fs::write(repo.join("gone.rs"), "fn gone() {}\n").unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        git(&repo, &["branch", "base"]);
        fs::write(repo.join("a.rs"), body("2")).unwrap();
        fs::write(repo.join("new.rs"), "fn new() {}\n").unwrap();
        fs::remove_file(repo.join("gone.rs")).unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "change"]);
        let base = merge_base(&repo, "base").unwrap();
        let text = context_diff(&repo, &base, To::Commit("HEAD")).unwrap();
        assert!(
            text.contains(" fn first() {\n         let a = 1;"),
            "the changed function is whole, past three lines of context: {text}"
        );
        assert!(
            !text.contains("let unrelated"),
            "git's rust driver ends it at the next function, not at the end of the module: {text}"
        );
        assert_eq!(
            super::modified_files(&repo, &base, To::Commit("HEAD")).unwrap(),
            ["a.rs"],
            "an added file is whole in the diff and a deleted one is gone"
        );
    }

    /// git treats any line that starts with a letter as a function's start in a file with no
    /// diff driver, so with function context a one-line change in TSX or YAML counted the whole
    /// block around it against the review's limit.
    #[test]
    fn the_measured_size_is_the_change_itself_whatever_the_language() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        fs::write(repo.join(".gitattributes"), "*.rs diff=rust\n").unwrap();
        let tests = |changed: &str| {
            let mut cases = String::new();
            for case in 0..200 {
                let name = if case == 100 { changed } else { "unchanged" };
                writeln!(
                    cases,
                    "  it(\"case {case} is {name}\", () => expect({case}).toBe({case}));"
                )
                .unwrap();
            }
            format!("describe(\"menu\", () => {{\n{cases}}});\n")
        };
        let jobs = |runner: &str| {
            let mut steps = String::new();
            for step in 0..200 {
                let run = if step == 100 { runner } else { "true" };
                writeln!(steps, "      - run: {run} {step}").unwrap();
            }
            format!("jobs:\n  check:\n    steps:\n{steps}")
        };
        fs::write(repo.join("menu.test.tsx"), tests("old")).unwrap();
        fs::write(repo.join("ci.yml"), jobs("true")).unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "base"]);
        git(&repo, &["branch", "base"]);
        fs::write(repo.join("menu.test.tsx"), tests("new")).unwrap();
        fs::write(repo.join("ci.yml"), jobs("false")).unwrap();
        git(&repo, &["commit", "--quiet", "--all", "-m", "change"]);
        let base = merge_base(&repo, "base").unwrap();
        for to in [To::WorkingTree, To::Commit("HEAD")] {
            let (measured, _) = reviewable_diff(&repo, &base, to).unwrap();
            assert!(
                measured.len() < 1_500,
                "two changed lines with three lines of context, not their blocks ({} bytes): \
                 {measured}",
                measured.len()
            );
        }
        let context = context_diff(&repo, &base, To::Commit("HEAD")).unwrap();
        assert!(
            context.len() < 1_500 && !context.contains("case 0 is"),
            "the review's context follows functions only where a diff driver knows them: \
             {context}"
        );
    }

    #[test]
    fn reads_only_small_regular_files_from_objects() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        fs::write(repo.join("small.rs"), "fn a() {}\n").unwrap();
        fs::write(repo.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::write(repo.join("large.txt"), "x".repeat(65)).unwrap();
        fs::write(repo.join("limit.txt"), "x".repeat(64)).unwrap();
        fs::write(repo.join("image.png"), [0xff, 0xfe, 0x00]).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", repo.join("link")).unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["update-index", "--chmod=+x", "run.sh"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        git(
            &repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},sub", head.trim()),
            ],
        );
        git(&repo, &["commit", "--quiet", "-m", "files"]);
        // The working tree no longer matches: only the commit's objects are read.
        fs::write(repo.join("small.rs"), "on disk\n").unwrap();
        let paths: Vec<String> = [
            "small.rs",
            "run.sh",
            "limit.txt",
            "large.txt",
            "image.png",
            "link",
            "sub",
            "gone",
        ]
        .map(str::to_owned)
        .into();
        assert_eq!(
            blobs(&repo, "HEAD", &paths, 64).unwrap(),
            [
                ("small.rs".to_owned(), Blob::Text("fn a() {}\n".to_owned())),
                ("run.sh".to_owned(), Blob::Text("#!/bin/sh\n".to_owned())),
                ("limit.txt".to_owned(), Blob::Text("x".repeat(64))),
                ("large.txt".to_owned(), Blob::TooLarge(65)),
                ("image.png".to_owned(), Blob::NotText),
                ("link".to_owned(), Blob::NotText),
                ("sub".to_owned(), Blob::NotText),
            ],
            "a symlink is never followed, a submodule never entered, a missing path left out"
        );
    }

    /// `pr open` and `pr update` fetch the default branch. git asked for credentials on the
    /// terminal, or through an askpass program, and waited for an answer that never came.
    #[test]
    fn a_fetch_never_asks_for_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        let asked = temp.path().join("asked");
        crate::testing::script(
            &temp.path().join("askpass"),
            &format!("touch {}\necho secret\n", asked.display()),
        );
        git(
            &repo,
            &[
                "config",
                "core.askPass",
                temp.path().join("askpass").to_str().unwrap(),
            ],
        );
        let (url, server) = crate::testing::serve(vec![("401 Unauthorized", "")]);
        let error = super::fetch(
            &repo,
            &format!("{url}/abc.git"),
            "main",
            Duration::from_secs(30),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("terminal prompts disabled"),
            "it fails at once, saying why: {error}"
        );
        assert!(!asked.exists(), "no askpass program is run");
        server.join().unwrap();
    }

    #[test]
    fn a_stalled_fetch_is_stopped() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        // Accepts connections and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/abc.git", listener.local_addr().unwrap());
        let started = std::time::Instant::now();
        let error = super::fetch(&repo, &url, "main", Duration::from_secs(1)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "{error}");
        assert_eq!(
            error.to_string(),
            format!("git fetch {url} main took longer than 1 seconds and was stopped")
        );
        drop(listener);
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
        let unknown = "1111111111111111111111111111111111111111";
        assert!(
            format!("{:#}", show(&repo, unknown, "new.txt").unwrap_err())
                .starts_with(&format!("git has no {unknown} to read new.txt at")),
            "a commit git lacks is an error, never a missing file"
        );
        assert!(
            merge_base(&repo, "nowhere").is_err(),
            "an unknown base fails"
        );
    }
}
