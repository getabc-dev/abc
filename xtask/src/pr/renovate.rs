//! `cargo xtask pr renovate <number>`: carries a Renovate tool bump through `cargo xtask pin`.
//!
//! Renovate bumps a tool's version in `xtask/tools.toml`, but cannot run `cargo xtask pin`, and
//! `cargo xtask setup` refuses archives whose sha256 is not pinned, so every such pull request
//! failed (#30, #31). The agent App cannot push to Renovate's `renovate/*` branches either. So
//! this command takes the version bump from Renovate's pull request, applies it to the default
//! branch's tool list, pins it, commits that on a branch of its own (`agent/renovate-<number>`)
//! without touching the checkout, opens the replacement pull request, and closes Renovate's with
//! a link to it. Renovate then leaves that version alone.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::{say, text};
use crate::git;
use crate::github::{self, Repo, Send};
use crate::tools::{self, MANIFEST};

/// Renovate's login, as GitHub names the pull requests it opens.
const RENOVATE_LOGIN: &str = "renovate[bot]";

/// The branch the replacement for Renovate's pull request `number` comes from.
fn branch(number: u64) -> String {
    format!("agent/renovate-{number}")
}

/// Replaces Renovate's pull request `number` in `repo` with one whose tool list is pinned: reads
/// it through `send`, pins with downloads from `fetch`, commits and pushes from the checkout at
/// `root`, then opens the replacement and closes Renovate's. Says what it did in `out`.
pub(super) fn run(
    number: u64,
    root: &Path,
    repo: &Repo,
    send: &mut Send<'_>,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>,
    out: &mut String,
) -> anyhow::Result<bool> {
    let bumps = read_bumps(number, repo, send)?;
    let moved: Vec<String> = bumps.iter().map(ToString::to_string).collect();
    let moved = moved.join(", ");
    say(out, &format!("pr: #{number} bumps {moved}"));
    let title = format!("Pin {moved} (replaces #{number})");
    let base_branch = pin_and_push(number, root, &bumps, &title, fetch, out)?;
    let body = format!(
        "## What and why\n\nReplaces Renovate's #{number}: {moved}. Renovate cannot run `cargo \
         xtask pin`, and `cargo xtask setup` refuses an archive whose sha256 is not pinned, so \
         this pull request carries the same bump with every checksum in `{MANIFEST}` recomputed \
         and checked against what each publisher vouches for.\n\n## Plan\n\n`cargo xtask pr \
         renovate {number}`.\n\n## Test changes\n\nNone\n"
    );
    let opened = send(&github::Request {
        method: "POST",
        path: repo.path("pulls"),
        body: Some(
            json!({"title": title, "head": branch(number), "base": base_branch, "body": body}),
        ),
    })?;
    let replacement = opened
        .get("number")
        .and_then(Value::as_u64)
        .context("GitHub opened the pull request but gave no number")?;
    say(
        out,
        &format!("pr: opened #{replacement} {}", text(&opened, "/html_url")),
    );
    send(&github::Request {
        method: "POST",
        path: repo.path(&format!("issues/{number}/comments")),
        body: Some(json!({
            "body": format!(
                "Replaced by #{replacement}, which carries this bump with its checksums pinned \
                 (`cargo xtask pr renovate {number}`)."
            )
        })),
    })?;
    send(&github::Request {
        method: "PATCH",
        path: repo.path(&format!("pulls/{number}")),
        body: Some(json!({"state": "closed"})),
    })?;
    say(
        out,
        &format!("pr: closed #{number} with a link to #{replacement}"),
    );
    Ok(true)
}

/// The versions Renovate's open pull request `number` moves, read through `send`; fails unless
/// it is Renovate's and changes the tool list alone.
fn read_bumps(number: u64, repo: &Repo, send: &mut Send<'_>) -> anyhow::Result<Vec<tools::Bump>> {
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
    let files = github::get(
        send,
        repo.path(&format!("pulls/{number}/files?per_page=100")),
    )?;
    let changed: Vec<&str> = files
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| text(file, "/filename"))
        .collect();
    ensure!(
        changed == [MANIFEST],
        "#{number} changes {}; only a pull request that changes {MANIFEST} alone is replaced",
        changed.join(", ")
    );
    let mut tool_list = |commit: &str| -> anyhow::Result<String> {
        let file = github::get(
            send,
            repo.path(&format!("contents/{MANIFEST}?ref={commit}")),
        )?;
        ensure!(
            text(&file, "/encoding") == "base64",
            "GitHub gave {MANIFEST} at {commit} as {:?}, not base64",
            text(&file, "/encoding")
        );
        let encoded: String = text(&file, "/content")
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        String::from_utf8(
            STANDARD
                .decode(encoded)
                .with_context(|| format!("{MANIFEST} at {commit} is not base64"))?,
        )
        .with_context(|| format!("{MANIFEST} at {commit} is not UTF-8"))
    };
    let before = tool_list(text(&pull, "/base/sha"))?;
    let after = tool_list(text(&pull, "/head/sha"))?;
    let bumps = tools::bumps(&before, &after)?;
    ensure!(!bumps.is_empty(), "#{number} moves no tool's version");
    Ok(bumps)
}

/// Applies `bumps` to the default branch's tool list, freshly fetched, pins it with downloads
/// from `fetch`, commits it on that branch with `title`, and pushes the commit to
/// [`branch`]`(number)`; returns the default branch's name. Nothing is pushed unless every pin
/// was computed.
fn pin_and_push(
    number: u64,
    root: &Path,
    bumps: &[tools::Bump],
    title: &str,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>,
    out: &mut String,
) -> anyhow::Result<String> {
    let base = git::default_branch(root)?;
    let short = base.strip_prefix("refs/remotes/").unwrap_or(&base);
    let (remote, base_branch) = short
        .split_once('/')
        .with_context(|| format!("{base} is not a remote's branch"))?;
    git::fetch(root, remote, base_branch, git::FETCH_TIMEOUT)?;
    let parent = git::git(
        root,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )?
    .trim()
    .to_owned();
    let current = git::show(root, &parent, MANIFEST)?
        .with_context(|| format!("{short} has no {MANIFEST}"))?;
    let mut said = Vec::new();
    let pinned = tools::repin(
        &tools::bumped(&current, bumps)?,
        &mut said,
        &mut |url: &str| fetch(url),
    );
    out.push_str(&String::from_utf8_lossy(&said));
    let message = format!(
        "{title}\n\nRenovate's #{number} bumped {MANIFEST} but cannot run `cargo xtask pin`; this \
         is its bump with every checksum recomputed and checked against what each publisher \
         vouches for, made by `cargo xtask pr renovate {number}`.\n"
    );
    let commit = commit_file(root, &parent, MANIFEST, &pinned?, &message)?;
    let branch = branch(number);
    push(root, remote, &commit, &branch)?;
    say(
        out,
        &format!("pr: pushed {} to {branch}", git_short(&commit)),
    );
    Ok(base_branch.to_owned())
}

/// The first twelve characters of a commit's name.
fn git_short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// A commit on `parent` that sets the file at `path` to `contents`, with `message`, made with
/// git's plumbing in an index of its own, so neither the checkout's files, its index nor its
/// branch change.
fn commit_file(
    root: &Path,
    parent: &str,
    path: &str,
    contents: &str,
    message: &str,
) -> anyhow::Result<String> {
    let blob = with_input(root, &["hash-object", "-w", "--stdin"], contents, &[])?;
    let index = git::git(root, &["rev-parse", "--git-path", "abc-renovate-index"])?;
    let index = root.join(index.trim());
    let env = [("GIT_INDEX_FILE", index.as_os_str())];
    let result = (|| {
        with_input(root, &["read-tree", parent], "", &env)?;
        with_input(
            root,
            &[
                "update-index",
                "--cacheinfo",
                &format!("100644,{},{path}", blob.trim()),
            ],
            "",
            &env,
        )?;
        let tree = with_input(root, &["write-tree"], "", &env)?;
        with_input(
            root,
            &["commit-tree", tree.trim(), "-p", parent, "-F", "-"],
            message,
            &env,
        )
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

/// Pushes `commit` to `branch` on `remote`, never waiting on a person: no terminal prompt and no
/// askpass program, so a remote that wants credentials the configured helpers lack fails at once.
/// A branch that exists and does not lead to `commit` is refused, never forced.
fn push(root: &Path, remote: &str, commit: &str, branch: &str) -> anyhow::Result<()> {
    let output = Command::new("git")
        .args(["-c", "core.askPass=", "push", "--quiet", remote])
        .arg(format!("{commit}:refs/heads/{branch}"))
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env_remove("SSH_ASKPASS")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    ensure!(
        output.status.success(),
        "git push {remote} {branch} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};

    use super::run;
    use crate::github::{Repo, Request};
    use crate::testing::{clone_with_origin, git};
    use crate::tools::{MANIFEST, sha256_hex};

    const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const RELEASE: &[u8] = b"#!/bin/sh\necho 'fake 1.3.0'\n";

    /// A tool list with one tool, `fake` at `version`, whose release asset is its executable,
    /// pinned at `sha256` on every target.
    fn tool_list(version: &str, sha256: &str) -> String {
        format!(
            r#"# The tools.
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

    /// GitHub's contents API's answer for `text`, base64 in lines as it sends it.
    fn contents(text: &str) -> Value {
        let encoded = STANDARD.encode(text);
        let lines: Vec<String> = encoded
            .as_bytes()
            .chunks(60)
            .map(|line| String::from_utf8(line.to_vec()).unwrap())
            .collect();
        json!({"encoding": "base64", "content": lines.join("\n")})
    }

    /// A Renovate pull request #30 that moves fake from 1.2.3 to 1.3.0 in a tool list that
    /// `files` lists, answered as GitHub does; records every call.
    fn renovate_github(
        author: &'static str,
        files: Value,
    ) -> impl FnMut(&Request) -> anyhow::Result<Value> {
        move |request: &Request| {
            Ok(match (request.method, request.path.as_str()) {
                ("GET", "/repos/o/r/pulls/30") => json!({
                    "user": {"login": author}, "state": "open",
                    "base": {"sha": "b0"}, "head": {"sha": "h0"}
                }),
                ("GET", "/repos/o/r/pulls/30/files?per_page=100") => files.clone(),
                ("GET", "/repos/o/r/contents/xtask/tools.toml?ref=b0") => {
                    contents(&tool_list("1.2.3", ZEROS))
                }
                ("GET", "/repos/o/r/contents/xtask/tools.toml?ref=h0") => {
                    contents(&tool_list("1.3.0", ZEROS))
                }
                ("POST", "/repos/o/r/pulls") => {
                    json!({"number": 47, "html_url": "https://github.com/o/r/pull/47"})
                }
                ("POST", "/repos/o/r/issues/30/comments") | ("PATCH", "/repos/o/r/pulls/30") => {
                    json!({})
                }
                other => anyhow::bail!("unexpected call {other:?}"),
            })
        }
    }

    /// Serves the release for every target, and an attestation for any digest.
    fn downloads(url: &str) -> anyhow::Result<Vec<u8>> {
        Ok(if url.contains("/attestations/") {
            br#"{"attestations":[{"repository_id":1}]}"#.to_vec()
        } else {
            anyhow::ensure!(url.contains("fake-1.3.0-"), "not the bumped release: {url}");
            RELEASE.to_vec()
        })
    }

    /// A checkout on `agent/work` whose `origin/main` holds fake 1.2.3, pinned.
    fn checkout(temp: &Path) -> std::path::PathBuf {
        let work = clone_with_origin(temp);
        fs::create_dir_all(work.join("xtask")).unwrap();
        fs::write(work.join(MANIFEST), tool_list("1.2.3", ZEROS)).unwrap();
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "--quiet", "-m", "tools"]);
        git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
        git(&work, &["switch", "--quiet", "-c", "agent/work"]);
        git(&work, &["config", "user.name", "agent"]);
        git(&work, &["config", "user.email", "agent@example.com"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        fs::write(work.join("scratch.txt"), "uncommitted work\n").unwrap();
        work
    }

    /// Renovate's tool bumps (#30, #31) always failed: Renovate cannot pin, and the App cannot
    /// push to `renovate/*`. `pr renovate` carries the bump to main's tool list, pins it, pushes
    /// it on `agent/renovate-30` without touching the checkout, opens the replacement and
    /// closes Renovate's with a link.
    #[test]
    fn replaces_a_renovate_tool_bump_with_a_pinned_one() {
        let temp = tempfile::tempdir().unwrap();
        let work = checkout(temp.path());
        let calls = std::cell::RefCell::new(Vec::new());
        let mut github = renovate_github("renovate[bot]", json!([{"filename": MANIFEST}]));
        let mut send = |request: &Request| {
            calls.borrow_mut().push(request.clone());
            github(request)
        };
        let mut out = String::new();
        assert!(run(30, &work, &repo(), &mut send, &mut downloads, &mut out).unwrap());
        let pinned = sha256_hex(RELEASE);
        assert_eq!(
            git(
                &work,
                &["show", &format!("origin/agent/renovate-30:{MANIFEST}")]
            ),
            tool_list("1.3.0", &pinned),
            "the bump, pinned, on main's tool list"
        );
        assert_eq!(
            git(&work, &["rev-parse", "origin/agent/renovate-30^"]),
            git(&work, &["rev-parse", "origin/main"]),
            "on main"
        );
        assert_eq!(
            git(&work, &["symbolic-ref", "--short", "HEAD"]).trim(),
            "agent/work",
            "the checkout's branch is untouched"
        );
        assert_eq!(
            git(&work, &["status", "--porcelain"]),
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
        assert_eq!(body["title"], "Pin fake 1.2.3 -> 1.3.0 (replaces #30)");
        assert!(
            body["body"]
                .as_str()
                .unwrap()
                .contains("## Test changes\n\nNone\n"),
            "{body}"
        );
        let comment = calls
            .iter()
            .find(|call| call.path.ends_with("/issues/30/comments"))
            .unwrap();
        assert!(
            comment.body.as_ref().unwrap()["body"]
                .as_str()
                .unwrap()
                .starts_with("Replaced by #47"),
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
        assert!(out.contains("pr: #30 bumps fake 1.2.3 -> 1.3.0\n"), "{out}");
        assert!(
            out.contains("changed  fake 1.3.0 aarch64-apple-darwin"),
            "{out}"
        );
        assert!(
            out.ends_with(
                "pr: opened #47 https://github.com/o/r/pull/47\npr: closed #30 with a link to #47\n"
            ),
            "{out}"
        );
    }

    /// Only Renovate's pull requests that change the tool list alone are replaced, and nothing is
    /// pushed or opened when the pins cannot all be computed.
    #[test]
    fn refuses_what_it_cannot_replace_whole() {
        let temp = tempfile::tempdir().unwrap();
        let work = checkout(temp.path());
        let refused =
            |author: &'static str,
             files: Value,
             fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>| {
                let mut github = renovate_github(author, files);
                let mut out = String::new();
                format!(
                    "{:#}",
                    run(30, &work, &repo(), &mut github, fetch, &mut out).unwrap_err()
                )
            };
        let manifest = json!([{"filename": MANIFEST}]);
        assert_eq!(
            refused("someone", manifest.clone(), &mut downloads),
            "#30 is someone's, not Renovate's"
        );
        assert_eq!(
            refused(
                "renovate[bot]",
                json!([{"filename": MANIFEST}, {"filename": "Cargo.lock"}]),
                &mut downloads
            ),
            "#30 changes xtask/tools.toml, Cargo.lock; only a pull request that changes \
             xtask/tools.toml alone is replaced"
        );
        let offline = refused("renovate[bot]", manifest, &mut |_: &str| {
            anyhow::bail!("offline")
        });
        assert!(
            offline.contains("could not pin 1 of the tools"),
            "{offline}"
        );
        assert!(
            git(&work, &["ls-remote", "origin", "agent/renovate-30"]).is_empty(),
            "nothing pushed"
        );
    }
}
