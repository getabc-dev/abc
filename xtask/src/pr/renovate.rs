//! `cargo xtask pr renovate <number>`: folds Renovate's weekly pull request into one of ours.
//!
//! Renovate opens one pull request a week carrying every update (`renovate.json`: `group:all`,
//! `schedule:weekly`). It cannot run `cargo xtask pin` for the tools it bumps in
//! `xtask/tools.toml`, and `cargo xtask setup` refuses archives whose sha256 is not pinned, so
//! such a pull request never passed (#30, #31); the agent App cannot push to Renovate's
//! `renovate/*` branches either. So this command fetches the pull request's head, re-applies the
//! whole group onto the freshly fetched default branch with a three-way merge, puts the default
//! branch's tool list with the group's version bumps in place of Renovate's (a bump the default
//! branch already has is left out), pins the bumped tools alone, commits that on a branch of its
//! own (`agent/renovate-<number>`) without touching the checkout, opens the replacement as a
//! draft, since the B1 reviews come before it is ready, and closes Renovate's with a link to it.
//!
//! A group that conflicts with the default branch outside the tool list, or changes workflows,
//! which the App cannot push, is refused, naming the files. Renovate recreates a closed group on
//! its next weekly run while its updates are not on the default branch, so a group is refused
//! while another replacement is open. Run again after a failure part way, it takes up the branch,
//! the pull request and the linking comment it made when they hold the same tree, and refuses a
//! branch that holds another.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail, ensure};
use serde_json::{Value, json};

use super::{say, text};
use crate::git;
use crate::github::{self, Repo, Send};
use crate::tools::{self, MANIFEST};

/// Renovate's login, as GitHub names the pull requests it opens.
const RENOVATE_LOGIN: &str = "renovate[bot]";

/// The prefix of every replacement's branch.
const BRANCH_PREFIX: &str = "agent/renovate-";

/// The branch the replacement for Renovate's pull request `number` comes from.
fn branch(number: u64) -> String {
    format!("{BRANCH_PREFIX}{number}")
}

/// Where workflows live: the agent App cannot push a change to them.
const WORKFLOWS: &str = ".github/workflows/";

/// Replaces Renovate's pull request `number` in `repo` with one built on the default branch and
/// pinned: reads it through `send`, pins with downloads from `fetch`, commits and pushes from the
/// checkout at `root`, then opens the replacement and closes Renovate's. Says what it did in
/// `out`.
pub(super) fn run(
    number: u64,
    root: &Path,
    repo: &Repo,
    send: &mut Send<'_>,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>,
    out: &mut String,
) -> anyhow::Result<bool> {
    let pull = github::get(send, repo.path(&format!("pulls/{number}")))?;
    ensure!(
        text(&pull, "/user/login") == RENOVATE_LOGIN,
        "#{number} is {}'s, not Renovate's",
        text(&pull, "/user/login")
    );
    ensure!(
        text(&pull, "/state") == "open",
        "#{number} is {}",
        text(&pull, "/state")
    );
    let open = open_pulls(send, repo)?;
    let ours = branch(number);
    if let Some(other) = open.iter().find(|listed| {
        let head = text(listed, "/head/ref");
        head.starts_with(BRANCH_PREFIX) && head != ours
    }) {
        bail!(
            "#{} ({}) still folds an earlier Renovate group, whose updates Renovate repeats in \
             #{number}: merge or close it first",
            other
                .get("number")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            text(other, "/head/ref")
        );
    }
    let head = text(&pull, "/head/sha").to_owned();
    let folded = fold(number, &head, root, fetch, out)?;
    let title = format!("Fold Renovate's #{number}: {}", text(&pull, "/title"));
    let pushed = push_once(&folded, number, root, &title, out)?;
    let body = description(number, &folded, &ours);
    let (opened, verb) = match open.iter().find(|listed| text(listed, "/head/ref") == ours) {
        Some(found) => (found.clone(), "found"),
        None => (
            send(&github::Request {
                method: "POST",
                path: repo.path("pulls"),
                body: Some(json!({
                    "title": title,
                    "head": ours,
                    "base": folded.base_branch,
                    "body": body,
                    "draft": true
                })),
            })?,
            "opened",
        ),
    };
    let replacement = opened
        .get("number")
        .and_then(Value::as_u64)
        .context("GitHub gave the replacement pull request no number")?;
    say(
        out,
        &format!(
            "pr: {verb} #{replacement} {}; it stays a draft until the B1 reviews are in its \
             description",
            text(&opened, "/html_url")
        ),
    );
    link_and_close(send, repo, number, replacement, &folded, &pushed, out)?;
    Ok(true)
}

/// The replacement's description: what it folds, and that the B1 reviews come before it is ready.
fn description(number: u64, folded: &Folded, ours: &str) -> String {
    let updates = if folded.bumps.is_empty() {
        String::new()
    } else {
        let moved: Vec<String> = folded.bumps.iter().map(ToString::to_string).collect();
        format!(
            " Renovate cannot run `cargo xtask pin`, and `cargo xtask setup` refuses an archive \
             whose sha256 is not pinned, so the tool bumps ({}) are pinned here, each checksum \
             checked against what its publisher vouches for.",
            moved.join(", ")
        )
    };
    format!(
        "## What and why\n\nFolds Renovate's #{number} into a branch the agent App can push: \
         the same updates, re-applied on `{}` by a three-way merge.{updates}\n\nChanged: {}.\n\n\
         ## Plan\n\n`cargo xtask pr renovate {number}`; read each update's release notes, and fix \
         what an update breaks in this pull request.\n\n## Test changes\n\nNone\n\n\
         ## Review findings\n\nNot run yet, so this pull request is a draft. Run `/code-review \
         high` and `/security-review` on `{ours}`, fix what they find and what an update breaks, \
         put what they found here with `cargo xtask pr update`, then mark it ready.\n",
        folded.base_branch,
        folded.files.join(", ")
    )
}

/// Links Renovate's pull request `number` to its replacement, unless a comment already does, and
/// closes it.
fn link_and_close(
    send: &mut Send<'_>,
    repo: &Repo,
    number: u64,
    replacement: u64,
    folded: &Folded,
    pushed: &str,
    out: &mut String,
) -> anyhow::Result<()> {
    let link = format!("Folded into #{replacement}");
    let comments = github::get(
        send,
        repo.path(&format!("issues/{number}/comments?per_page=100")),
    )?;
    let linked = comments.as_array().is_some_and(|comments| {
        comments
            .iter()
            .any(|comment| text(comment, "/body").starts_with(&link))
    });
    if !linked {
        send(&github::Request {
            method: "POST",
            path: repo.path(&format!("issues/{number}/comments")),
            body: Some(json!({
                "body": format!(
                    "{link}, which carries these updates on `{}` with any tool bump pinned \
                     (`cargo xtask pr renovate {number}`, at {}).",
                    folded.base_branch,
                    git::short(pushed)
                )
            })),
        })?;
    }
    send(&github::Request {
        method: "PATCH",
        path: repo.path(&format!("pulls/{number}")),
        body: Some(json!({"state": "closed"})),
    })?;
    say(
        out,
        &format!("pr: closed #{number} with a link to #{replacement}"),
    );
    Ok(())
}

/// Every open pull request in `repo`, a page of a hundred at a time.
fn open_pulls(send: &mut Send<'_>, repo: &Repo) -> anyhow::Result<Vec<Value>> {
    let mut pulls = Vec::new();
    for page in 1.. {
        let listed = github::get(
            send,
            repo.path(&format!("pulls?state=open&per_page=100&page={page}")),
        )?;
        let listed = listed
            .as_array()
            .context("GitHub listed open pull requests as something other than a list")?;
        pulls.extend(listed.iter().cloned());
        if listed.len() < 100 {
            break;
        }
    }
    Ok(pulls)
}

/// What [`fold`] made.
struct Folded {
    /// The default branch's name, and its commit the fold is built on.
    base_branch: String,
    parent: String,
    /// The tree with every update.
    tree: String,
    /// The files it changes on the default branch.
    files: Vec<String>,
    /// The tool versions it moves.
    bumps: Vec<tools::Bump>,
}

/// Fetches origin's default branch and Renovate's pull request `number`, at `head`, into the
/// checkout at `root` and merges the pull request's updates onto the default branch, in git's
/// object store alone. When the pull request changes the tool list, the tool list is the default
/// branch's with the versions it moves that the default branch does not have yet, those tools
/// pinned with downloads from `fetch`. Nothing is written unless every pin was computed and the
/// merge has no conflict outside the tool list.
fn fold(
    number: u64,
    head: &str,
    root: &Path,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>,
    out: &mut String,
) -> anyhow::Result<Folded> {
    // origin, where the pull request opens (`Repo::of`), never another remote.
    let base = git::git(
        root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .context("origin/HEAD is not set; run `git remote set-head origin --auto`")?
    .trim()
    .to_owned();
    let base_branch = base
        .strip_prefix("refs/remotes/origin/")
        .with_context(|| format!("origin/HEAD names {base}, not a branch of origin"))?
        .to_owned();
    git::fetch(root, "origin", &base_branch, git::REMOTE_TIMEOUT)?;
    git::fetch(
        root,
        "origin",
        &format!("pull/{number}/head"),
        git::REMOTE_TIMEOUT,
    )?;
    let fetched = git::git(root, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"])?;
    ensure!(
        fetched.trim() == head,
        "#{number}'s head moved to {} since GitHub named {}; run this again",
        git::short(fetched.trim()),
        git::short(head)
    );
    let parent = git::git(
        root,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )?
    .trim()
    .to_owned();
    // Where Renovate's group left the default branch: its changes are the ones since, as the merge
    // takes them. GitHub's base sha is the default branch as GitHub last saw it, so the default
    // branch's own changes since the group left it would read as the group's, reversed.
    let fork = git::git(root, &["merge-base", "--end-of-options", &parent, head])?
        .trim()
        .to_owned();
    let mut tree = merge(root, number, &base_branch, &parent, head)?;
    let bumps = retool(
        root,
        number,
        &base_branch,
        [&fork, &parent, head],
        &mut tree,
        fetch,
        out,
    )?;
    let files: Vec<String> = git::git(root, &["diff", "--name-only", &parent, &tree, "--"])?
        .lines()
        .map(str::to_owned)
        .collect();
    ensure!(
        !files.is_empty(),
        "#{number} changes nothing origin/{base_branch} does not already have"
    );
    let workflows: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|file| file.starts_with(WORKFLOWS))
        .collect();
    ensure!(
        workflows.is_empty(),
        "#{number} changes workflows ({}), which the agent App cannot push; GitHub Actions \
         updates come in a group of their own (renovate.json), for the maintainer to merge",
        workflows.join(", ")
    );
    Ok(Folded {
        base_branch,
        parent,
        tree,
        files,
        bumps,
    })
}

/// Merges Renovate's `head` onto the default branch's `parent` in git's object store and returns
/// the tree. Fails on a conflict outside the tool list, which is replaced whole.
fn merge(
    root: &Path,
    number: u64,
    base_branch: &str,
    parent: &str,
    head: &str,
) -> anyhow::Result<String> {
    let merged = Command::new("git")
        .args(["merge-tree", "--write-tree", "--name-only", "--no-messages"])
        .args(["--end-of-options", parent, head])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    let listed = String::from_utf8(merged.stdout).context("git printed text that is not UTF-8")?;
    let mut lines = listed.lines();
    let tree = lines.next().unwrap_or_default().trim().to_owned();
    match merged.status.code() {
        Some(0) => {}
        Some(1) => {
            // The tool list is replaced whole below, so a conflict there is no conflict.
            let conflicts: Vec<&str> = lines
                .filter(|path| !path.is_empty() && *path != MANIFEST)
                .collect();
            ensure!(
                conflicts.is_empty(),
                "#{number} conflicts with origin/{base_branch} in {}; Renovate rebases it on its \
                 next run, or fold it by hand",
                conflicts.join(", ")
            );
        }
        _ => bail!(
            "git merge-tree failed: {}",
            String::from_utf8_lossy(&merged.stderr).trim()
        ),
    }
    Ok(tree)
}

/// When Renovate's group changes the tool list from `fork` to `head`, sets it in `tree` to the
/// default branch's at `parent` with the bumps it does not have yet, those tools pinned with
/// downloads from `fetch`; returns those bumps.
fn retool(
    root: &Path,
    number: u64,
    base_branch: &str,
    [fork, parent, head]: [&str; 3],
    tree: &mut String,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>,
    out: &mut String,
) -> anyhow::Result<Vec<tools::Bump>> {
    let tool_list = |commit: &str| git::show(root, commit, MANIFEST);
    let mut bumps = Vec::new();
    let (before, after) = (tool_list(fork)?, tool_list(head)?);
    if before != after {
        let (Some(before), Some(after)) = (before, after) else {
            bail!("#{number} adds or removes {MANIFEST}; only a version bump is taken");
        };
        let current = tool_list(parent)?
            .with_context(|| format!("origin/{base_branch} has no {MANIFEST}"))?;
        let (to_apply, applied) = tools::outstanding(&current, tools::bumps(&before, &after)?)
            .with_context(|| format!("origin/{base_branch} moved a tool #{number} bumps"))?;
        for bump in &applied {
            say(
                out,
                &format!(
                    "pr: origin/{base_branch} already has {} {}",
                    bump.tool, bump.to
                ),
            );
        }
        let list = if to_apply.is_empty() {
            current
        } else {
            let moved: Vec<String> = to_apply.iter().map(ToString::to_string).collect();
            say(out, &format!("pr: #{number} bumps {}", moved.join(", ")));
            let names: Vec<String> = to_apply.iter().map(|bump| bump.tool.clone()).collect();
            let mut said = Vec::new();
            let pinned = tools::repin(
                &tools::bumped(&current, &to_apply)?,
                Some(&names),
                &mut said,
                &mut |url: &str| fetch(url),
            );
            out.push_str(&String::from_utf8_lossy(&said));
            pinned?
        };
        *tree = replace_file(root, tree, MANIFEST, &list)?;
        bumps = to_apply;
    }
    Ok(bumps)
}

/// Commits `folded` on its parent with `title` and pushes it to origin's
/// [`branch`]`(number)`, or takes up that branch when it already holds the same tree; returns the
/// commit there.
fn push_once(
    folded: &Folded,
    number: u64,
    root: &Path,
    title: &str,
    out: &mut String,
) -> anyhow::Result<String> {
    let branch = branch(number);
    let pushed = git::git(
        root,
        &[
            "ls-remote",
            "--heads",
            "origin",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    if let Some(theirs) = pushed.split_whitespace().next() {
        git::fetch(root, "origin", &branch, git::REMOTE_TIMEOUT)?;
        let held = git::git(
            root,
            &["rev-parse", "--verify", &format!("{theirs}^{{tree}}")],
        )?;
        ensure!(
            held.trim() == folded.tree,
            "origin already has {branch} at {}, with other files; delete that branch, or finish \
             it by hand",
            git::short(theirs)
        );
        say(
            out,
            &format!(
                "pr: {branch} already holds this change at {}",
                git::short(theirs)
            ),
        );
        return Ok(theirs.to_owned());
    }
    let message = format!(
        "{title}\n\nRenovate's #{number}, re-applied on origin/{} with any tool bump pinned, made by \
         `cargo xtask pr renovate {number}`.\n",
        folded.base_branch
    );
    let commit = with_input(
        root,
        &["commit-tree", &folded.tree, "-p", &folded.parent, "-F", "-"],
        &message,
        &[],
    )?
    .trim()
    .to_owned();
    git::push(root, "origin", &commit, &branch, git::REMOTE_TIMEOUT)?;
    say(
        out,
        &format!("pr: pushed {} to {branch}", git::short(&commit)),
    );
    Ok(commit)
}

/// The tree `tree` with the file at `path` set to `contents`, made with git's plumbing in an
/// index of its own, so neither the checkout's files, its index nor its branch change.
fn replace_file(root: &Path, tree: &str, path: &str, contents: &str) -> anyhow::Result<String> {
    let blob = with_input(root, &["hash-object", "-w", "--stdin"], contents, &[])?;
    let index = git::git(root, &["rev-parse", "--git-path", "abc-renovate-index"])?;
    let index = root.join(index.trim());
    let env = [("GIT_INDEX_FILE", index.as_os_str())];
    let result = (|| {
        with_input(root, &["read-tree", tree], "", &env)?;
        with_input(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},{path}", blob.trim()),
            ],
            "",
            &env,
        )?;
        with_input(root, &["write-tree"], "", &env)
    })();
    match std::fs::remove_file(&index) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("could not remove {}", index.display()));
        }
        _ => {}
    }
    Ok(result?.trim().to_owned())
}

/// Runs git with `args` in `root`, `input` on its stdin and `env` set; returns what it prints.
fn with_input(
    root: &Path,
    args: &[&str],
    input: &str,
    env: &[(&str, &std::ffi::OsStr)],
) -> anyhow::Result<String> {
    let mut child = Command::new("git")
        .args(args)
        .envs(env.iter().copied())
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run git")?;
    child
        .stdin
        .take()
        .context("git's stdin is not piped")?
        .write_all(input.as_bytes())
        .context("could not write to git")?;
    let output = child.wait_with_output().context("could not run git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git printed text that is not UTF-8")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};

    use super::run;
    use crate::github::{Repo, Request};
    use crate::testing::{clone_with_origin, git};
    use crate::tools::{MANIFEST, sha256_hex};

    const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const RELEASE: &[u8] = b"#!/bin/sh\necho 'fake 1.3.0'\n";

    /// A tool list with `fake` at `version`, whose release asset is its executable, pinned at
    /// `sha256` on every target; and `other`, which no bump moves, pinned at zeros.
    fn tool_list(version: &str, sha256: &str) -> String {
        format!(
            r#"# The tools.
[[tool]]
name = "other"
version = "9.9.9"
url = "https://example.com/other-{{version}}-{{platform}}"
attestations = "example/other"
targets.aarch64-apple-darwin = {{ platform = "mac", sha256 = "{ZEROS}", binary-sha256 = "{ZEROS}" }}
targets.x86_64-unknown-linux-gnu = {{ platform = "linux-x64", sha256 = "{ZEROS}", binary-sha256 = "{ZEROS}" }}
targets.aarch64-unknown-linux-gnu = {{ platform = "linux-arm64", sha256 = "{ZEROS}", binary-sha256 = "{ZEROS}" }}

[[tool]]
name = "fake"
version = "{version}"
url = "https://example.com/fake-{{version}}-{{platform}}"
attestations = "example/fake"
targets.aarch64-apple-darwin = {{ platform = "mac", sha256 = "{sha256}", binary-sha256 = "{sha256}" }}
targets.x86_64-unknown-linux-gnu = {{ platform = "linux-x64", sha256 = "{sha256}", binary-sha256 = "{sha256}" }}
targets.aarch64-unknown-linux-gnu = {{ platform = "linux-arm64", sha256 = "{sha256}", binary-sha256 = "{sha256}" }}
"#
        )
    }

    fn repo() -> Repo {
        Repo {
            owner: "o".to_owned(),
            name: "r".to_owned(),
        }
    }

    /// Commits every change in `work` with `message` and returns the commit.
    fn commit(work: &Path, message: &str) -> String {
        git(work, &["add", "--all"]);
        git(work, &["commit", "--quiet", "-m", message]);
        git(work, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// A recorded grouped Renovate pull request #30 in a checkout on `agent/work`.
    struct Grouped {
        work: PathBuf,
        /// Renovate's head: fake 1.2.3 -> 1.3.0 with Renovate's stale pins, and vite 8.3.5 ->
        /// 8.3.6 in `web/package.json`, with `group`'s files.
        head: String,
    }

    /// Renovate's weekly pull request as `refs/pull/30/head` on origin, with `group`'s files
    /// besides its updates, then main moving on `README.md`, which the group does not change, and
    /// on `main`'s files.
    fn recorded(temp: &Path, group: &[(&str, String)], main: &[(&str, String)]) -> Grouped {
        let work = clone_with_origin(temp);
        git(&work, &["config", "user.name", "agent"]);
        git(&work, &["config", "user.email", "agent@example.com"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        let write = |files: &[(&str, String)]| {
            for (path, text) in files {
                let path = work.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
            }
        };
        write(&[
            (MANIFEST, tool_list("1.2.3", ZEROS)),
            ("web/package.json", VITE_BEFORE.to_owned()),
            ("README.md", "abc\n".to_owned()),
        ]);
        let base = commit(&work, "main");
        write(&[
            (MANIFEST, tool_list("1.3.0", ZEROS)),
            ("web/package.json", VITE_AFTER.to_owned()),
        ]);
        write(group);
        let head = commit(&work, "Update all dependencies");
        git(
            &work,
            &["push", "--quiet", "origin", "HEAD:refs/pull/30/head"],
        );
        git(&work, &["reset", "--quiet", "--hard", &base]);
        write(&[("README.md", "abc, moved on\n".to_owned())]);
        write(main);
        commit(&work, "main moves on");
        git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
        git(&work, &["switch", "--quiet", "-c", "agent/work"]);
        fs::write(work.join("scratch.txt"), "uncommitted work\n").unwrap();
        Grouped { work, head }
    }

    const VITE_BEFORE: &str = "{\"vite\": \"8.3.5\"}\n";
    const VITE_AFTER: &str = "{\"vite\": \"8.3.6\"}\n";

    /// The open pull request from `branch`, numbered `number`.
    fn open_from(number: u64, branch: &str) -> Value {
        json!({
            "number": number,
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "head": {"ref": branch}
        })
    }

    /// GitHub as it answers for `grouped`'s pull request by `author`: `open` is the repository's
    /// open pull requests, `comments` the ones on #30, and opening one fails when `open_fails`.
    fn github(
        grouped: &Grouped,
        author: &'static str,
        open: Value,
        comments: Value,
        open_fails: bool,
    ) -> impl FnMut(&Request) -> anyhow::Result<Value> + use<> {
        let head = grouped.head.clone();
        move |request: &Request| {
            Ok(match (request.method, request.path.as_str()) {
                ("GET", "/repos/o/r/pulls/30") => json!({
                    "user": {"login": author}, "state": "open", "title": "Update all dependencies",
                    "head": {"sha": head}
                }),
                ("GET", "/repos/o/r/pulls?state=open&per_page=100&page=1") => open.clone(),
                ("GET", "/repos/o/r/issues/30/comments?per_page=100") => comments.clone(),
                ("POST", "/repos/o/r/pulls") if open_fails => anyhow::bail!("GitHub answered 502"),
                ("POST", "/repos/o/r/pulls") => open_from(47, "agent/renovate-30"),
                ("POST", "/repos/o/r/issues/30/comments") | ("PATCH", "/repos/o/r/pulls/30") => {
                    json!({})
                }
                other => anyhow::bail!("unexpected call {other:?}"),
            })
        }
    }

    /// Serves fake 1.3.0's release for every target, and an attestation for any digest; any
    /// other download, `other`'s included, fails.
    fn downloads(url: &str) -> anyhow::Result<Vec<u8>> {
        Ok(if url.contains("/attestations/") {
            br#"{"attestations":[{"repository_id":1}]}"#.to_vec()
        } else {
            anyhow::ensure!(url.contains("fake-1.3.0-"), "not the bumped release: {url}");
            RELEASE.to_vec()
        })
    }

    /// Every download fails: nothing may be pinned.
    fn offline(_: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("offline")
    }

    /// Renovate's tool bumps (#30, #31) always failed: Renovate cannot pin, and the App cannot
    /// push to `renovate/*`. Its weekly group is re-applied on main as main stands, its tool bump
    /// pinned, pushed on `agent/renovate-30` without touching the checkout, and the replacement
    /// opened as a draft, since the B1 reviews come first, and Renovate's closed with a link.
    #[test]
    fn folds_a_grouped_renovate_pull_request_onto_main() {
        let temp = tempfile::tempdir().unwrap();
        let grouped = recorded(temp.path(), &[], &[]);
        let work = &grouped.work;
        let calls = std::cell::RefCell::new(Vec::new());
        let mut answer = github(&grouped, "renovate[bot]", json!([]), json!([]), false);
        let mut send = |request: &Request| {
            calls.borrow_mut().push(request.clone());
            answer(request)
        };
        let mut out = String::new();
        assert!(run(30, work, &repo(), &mut send, &mut downloads, &mut out).unwrap());
        let folded = |path: &str| git(work, &["show", &format!("origin/agent/renovate-30:{path}")]);
        assert_eq!(
            folded(MANIFEST),
            tool_list("1.3.0", &sha256_hex(RELEASE)),
            "the bump pinned on main's tool list; `other` neither downloaded nor repinned"
        );
        assert_eq!(folded("web/package.json"), VITE_AFTER, "the whole group");
        assert_eq!(
            folded("README.md"),
            "abc, moved on\n",
            "on main as it stands"
        );
        assert_eq!(
            git(work, &["rev-parse", "origin/agent/renovate-30^"]),
            git(work, &["rev-parse", "origin/main"]),
            "one commit on main"
        );
        assert_eq!(
            git(work, &["symbolic-ref", "--short", "HEAD"]).trim(),
            "agent/work",
            "the checkout's branch is untouched"
        );
        assert_eq!(
            git(work, &["status", "--porcelain"]),
            "?? scratch.txt\n",
            "and so are its files and index"
        );
        let calls = calls.into_inner();
        let opened = calls
            .iter()
            .find(|call| call.method == "POST" && call.path == "/repos/o/r/pulls")
            .unwrap();
        let body = opened.body.as_ref().unwrap();
        assert_eq!(body["head"], "agent/renovate-30");
        assert_eq!(body["base"], "main");
        assert_eq!(body["draft"], true, "a draft until the B1 reviews are in");
        assert_eq!(
            body["title"],
            "Fold Renovate's #30: Update all dependencies"
        );
        let text = body["body"].as_str().unwrap();
        assert!(
            text.contains("Changed: web/package.json, xtask/tools.toml.")
                && text.contains("(fake 1.2.3 -> 1.3.0) are pinned")
                && text.contains("## Test changes\n\nNone\n")
                && text.contains(
                    "## Review findings\n\nNot run yet, so this pull request is a draft. Run \
                     `/code-review high` and `/security-review` on `agent/renovate-30`"
                ),
            "{text}"
        );
        let comment = calls
            .iter()
            .find(|call| call.method == "POST" && call.path.ends_with("/issues/30/comments"))
            .unwrap();
        assert!(
            comment.body.as_ref().unwrap()["body"]
                .as_str()
                .unwrap()
                .starts_with("Folded into #47, which carries"),
            "{comment:?}"
        );
        let closed = calls.last().unwrap();
        assert_eq!(
            (closed.method, closed.path.as_str(), closed.body.clone()),
            (
                "PATCH",
                "/repos/o/r/pulls/30",
                Some(json!({"state": "closed"}))
            ),
            "closed last, once the replacement is open"
        );
        let pushed = git(work, &["rev-parse", "origin/agent/renovate-30"]);
        assert!(
            out.starts_with("pr: #30 bumps fake 1.2.3 -> 1.3.0\n")
                && out.contains("changed  fake 1.3.0 aarch64-apple-darwin")
                && out.contains(&format!(
                    "pr: pushed {} to agent/renovate-30\n",
                    pushed.get(..12).unwrap()
                ))
                && out.ends_with(
                    "pr: opened #47 https://github.com/o/r/pull/47; it stays a draft until the B1 \
                     reviews are in its description\npr: closed #30 with a link to #47\n"
                ),
            "{out}"
        );
    }

    /// Main moved its tool list after the group left it: it bumped `other` and made the group's
    /// own bump by hand, pinned. The group's changes are read from where it left main, so neither
    /// reads as a bump back; main's tool list conflicts with Renovate's, which is no conflict,
    /// since main's replaces it; and a bump main has is neither made again nor downloaded.
    #[test]
    fn a_tool_list_main_moved_is_main_s() {
        let temp = tempfile::tempdir().unwrap();
        let mains = tool_list("1.3.0", &sha256_hex(RELEASE))
            .replace("version = \"9.9.9\"", "version = \"9.9.10\"");
        let grouped = recorded(temp.path(), &[], &[(MANIFEST, mains.clone())]);
        let work = &grouped.work;
        let mut answer = github(&grouped, "renovate[bot]", json!([]), json!([]), false);
        let mut out = String::new();
        assert!(run(30, work, &repo(), &mut answer, &mut offline, &mut out).unwrap());
        let folded = |path: &str| git(work, &["show", &format!("origin/agent/renovate-30:{path}")]);
        assert_eq!(folded(MANIFEST), mains, "main's tool list, whole");
        assert_eq!(
            folded("web/package.json"),
            VITE_AFTER,
            "the rest of the group"
        );
        assert!(
            out.starts_with("pr: origin/main already has fake 1.3.0\npr: pushed "),
            "{out}"
        );

        let moved_dir = tempfile::tempdir().unwrap();
        let elsewhere = tool_list("1.4.0", ZEROS);
        let moved = recorded(moved_dir.path(), &[], &[(MANIFEST, elsewhere)]);
        let mut moved_answer = github(&moved, "renovate[bot]", json!([]), json!([]), false);
        let error = run(
            30,
            &moved.work,
            &repo(),
            &mut moved_answer,
            &mut offline,
            &mut String::new(),
        )
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "origin/main moved a tool #30 bumps: fake is at 1.4.0 here, neither 1.2.3 nor 1.3.0, \
             which the bump moves between"
        );
    }

    /// Only Renovate's open pull requests are folded, at the head GitHub names, one group at a
    /// time, never one that changes workflows, and nothing is pushed when the group conflicts
    /// with main or a pin cannot be computed.
    #[test]
    fn refuses_what_it_cannot_fold_whole() {
        let temp = tempfile::tempdir().unwrap();
        let grouped = recorded(temp.path(), &[], &[]);
        let refused =
            |author: &'static str,
             pull: &Grouped,
             open: Value,
             fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>| {
                let mut answer = github(pull, author, open, json!([]), false);
                let mut out = String::new();
                format!(
                    "{:#}",
                    run(30, &pull.work, &repo(), &mut answer, fetch, &mut out).unwrap_err()
                )
            };
        assert_eq!(
            refused("someone", &grouped, json!([]), &mut downloads),
            "#30 is someone's, not Renovate's"
        );
        assert_eq!(
            refused(
                "renovate[bot]",
                &grouped,
                json!([
                    open_from(12, "agent/other"),
                    open_from(46, "agent/renovate-29")
                ]),
                &mut downloads
            ),
            "#46 (agent/renovate-29) still folds an earlier Renovate group, whose updates \
             Renovate repeats in #30: merge or close it first"
        );
        let offline = refused("renovate[bot]", &grouped, json!([]), &mut offline);
        assert!(
            offline.starts_with("could not pin 1 of the tools"),
            "{offline}"
        );
        let moved = Grouped {
            work: grouped.work.clone(),
            head: "1111111111111111111111111111111111111111".to_owned(),
        };
        let stale = refused("renovate[bot]", &moved, json!([]), &mut downloads);
        assert!(
            stale.starts_with("#30's head moved to ") && stale.ends_with("run this again"),
            "{stale}"
        );
        assert!(
            git(&grouped.work, &["ls-remote", "origin", "agent/renovate-30"]).is_empty(),
            "nothing pushed"
        );
        let clashing_dir = tempfile::tempdir().unwrap();
        let clashing = recorded(
            clashing_dir.path(),
            &[],
            &[("web/package.json", "{\"vite\": \"9.0.0\"}\n".to_owned())],
        );
        assert_eq!(
            refused("renovate[bot]", &clashing, json!([]), &mut downloads),
            "#30 conflicts with origin/main in web/package.json; Renovate rebases it on its next \
             run, or fold it by hand"
        );
        let actions_dir = tempfile::tempdir().unwrap();
        let actions = recorded(
            actions_dir.path(),
            &[(".github/workflows/ci.yml", "on: push\n".to_owned())],
            &[],
        );
        assert_eq!(
            refused("renovate[bot]", &actions, json!([]), &mut downloads),
            "#30 changes workflows (.github/workflows/ci.yml), which the agent App cannot push; \
             GitHub Actions updates come in a group of their own (renovate.json), for the \
             maintainer to merge"
        );
    }

    /// A full page of a hundred open pull requests is followed by the next, so an earlier
    /// replacement on page two still counts; a short page is the last one asked for.
    #[test]
    fn open_pulls_reads_every_page() {
        let mut asked = Vec::new();
        let mut answer = |request: &Request| {
            asked.push(request.path.clone());
            Ok(match request.path.as_str() {
                "/repos/o/r/pulls?state=open&per_page=100&page=1" => Value::Array(
                    (1..=100)
                        .map(|number| open_from(number, "agent/other"))
                        .collect(),
                ),
                "/repos/o/r/pulls?state=open&per_page=100&page=2" => {
                    json!([open_from(146, "agent/renovate-29")])
                }
                other => anyhow::bail!("unexpected call {other}"),
            })
        };
        let pulls = super::open_pulls(&mut answer, &repo()).unwrap();
        assert_eq!(pulls.len(), 101);
        assert_eq!(pulls[100], open_from(146, "agent/renovate-29"));
        assert_eq!(
            asked,
            [
                "/repos/o/r/pulls?state=open&per_page=100&page=1",
                "/repos/o/r/pulls?state=open&per_page=100&page=2"
            ]
        );
    }

    /// A run that failed after its push could not run again, since a new commit to the pushed
    /// branch was refused. A run again takes up the branch, the pull request and the linking
    /// comment when they hold the same change, and refuses a branch that holds another.
    #[test]
    fn a_second_run_takes_up_what_the_first_left() {
        let temp = tempfile::tempdir().unwrap();
        let grouped = recorded(temp.path(), &[], &[]);
        let work = &grouped.work;
        let mut failing = github(&grouped, "renovate[bot]", json!([]), json!([]), true);
        let failed = run(
            30,
            work,
            &repo(),
            &mut failing,
            &mut downloads,
            &mut String::new(),
        )
        .unwrap_err();
        assert!(format!("{failed:#}").contains("502"), "{failed:#}");
        let pushed = git(work, &["rev-parse", "origin/agent/renovate-30"]);
        let mut again = github(&grouped, "renovate[bot]", json!([]), json!([]), false);
        let mut taken = String::new();
        assert!(run(30, work, &repo(), &mut again, &mut downloads, &mut taken).unwrap());
        assert!(
            taken.contains("pr: agent/renovate-30 already holds this change at "),
            "{taken}"
        );
        assert_eq!(
            git(work, &["rev-parse", "origin/agent/renovate-30"]),
            pushed,
            "the branch as the first run left it"
        );
        let open = json!([open_from(47, "agent/renovate-30")]);
        let linked = json!([{"body": "Folded into #47, which carries these updates"}]);
        let calls = std::cell::RefCell::new(Vec::new());
        let mut found = github(&grouped, "renovate[bot]", open, linked, true);
        let mut send = |request: &Request| {
            calls.borrow_mut().push(request.clone());
            found(request)
        };
        let mut kept_pull = String::new();
        assert!(run(30, work, &repo(), &mut send, &mut downloads, &mut kept_pull).unwrap());
        assert!(kept_pull.contains("pr: found #47 "), "{kept_pull}");
        assert!(
            calls.into_inner().iter().all(|call| call.method != "POST"),
            "neither a second pull request nor a second comment"
        );
        let other_release = |url: &str| -> anyhow::Result<Vec<u8>> {
            if url.contains("/attestations/") {
                downloads(url)
            } else {
                Ok(b"another build".to_vec())
            }
        };
        let mut refusing = github(&grouped, "renovate[bot]", json!([]), json!([]), false);
        let error = run(
            30,
            work,
            &repo(),
            &mut refusing,
            &mut { other_release },
            &mut String::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("origin already has agent/renovate-30 at "),
            "{error:#}"
        );
    }

    /// The index `replace_file` builds in is its own and goes when it is done, whether the tree
    /// was made or not; a failure to remove it is said, and one that never existed is not.
    #[test]
    fn replacing_a_file_leaves_no_index_behind() {
        let temp = tempfile::tempdir().unwrap();
        let work = clone_with_origin(temp.path());
        let index =
            work.join(git(&work, &["rev-parse", "--git-path", "abc-renovate-index"]).trim());
        let tree = git(&work, &["rev-parse", "HEAD^{tree}"]);
        let made = super::replace_file(&work, tree.trim(), MANIFEST, "x\n").unwrap();
        assert_eq!(
            git(&work, &["show", &format!("{made}:{MANIFEST}")]),
            "x\n",
            "the file in the tree"
        );
        assert!(!index.exists(), "removed after the tree is written");
        let unknown = super::replace_file(
            &work,
            "0000000000000000000000000000000000000001",
            MANIFEST,
            "x\n",
        )
        .unwrap_err();
        assert!(
            format!("{unknown:#}").starts_with("git read-tree "),
            "the git error, not a missing index's: {unknown:#}"
        );
        fs::create_dir_all(&index).unwrap();
        let stuck = super::replace_file(&work, tree.trim(), MANIFEST, "x\n").unwrap_err();
        assert!(
            format!("{stuck:#}").starts_with("could not remove "),
            "{stuck:#}"
        );
    }
}
