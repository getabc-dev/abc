//! `cargo xtask pr`: pull requests as the agent App, checked before they are sent.
//!
//! `gh pr create` and `gh pr edit` sent descriptions whose `## Test changes` had drifted from what
//! integrity flags (#21 missed an entry, #25 kept three stale ones), and `gh pr edit` breaks on
//! GitHub's retired Projects classic. So `pr open` and `pr update` refuse a description integrity
//! would fail, a change over the review's size limit counting the title and description, and
//! rulesets that no longer match CI, before one call goes out; then they call the REST API.
//!
//! Each command runs in two phases: [`prepare`] checks everything it can locally and spends no
//! token; [`send`] then makes its calls with one App token ([`crate::github`]).

mod checks;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail, ensure};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::git;
use crate::github::{self, Repo, Send};
use crate::integrity::Review;

/// The agent App's login, as GitHub names the pushes and runs it makes.
pub(crate) const APP_LOGIN: &str = "getabc-dev-agent[bot]";

/// What `cargo xtask pr` does.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// List pull request NUMBER's failing, cancelled and running checks, and its latest
    /// merge-queue run's, with each failing check's annotations.
    Checks { number: u64 },
    /// Open a pull request from the current branch, pushed, into the default branch.
    Open {
        #[arg(long)]
        title: String,
        /// The file holding the description.
        #[arg(long)]
        body: PathBuf,
    },
    /// Replace pull request NUMBER's description, and say whether the agent App's push is
    /// the latest, as the maintainer's approval needs.
    Update {
        number: u64,
        /// The file holding the description.
        #[arg(long)]
        body: PathBuf,
    },
    /// Print a "## Test changes" section naming every test integrity flags; only the reasons
    /// are left to write.
    TestChanges,
}

/// What [`send`] does once [`prepare`] has checked it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Checks(u64),
    Open {
        title: String,
        body: String,
        branch: String,
        base: String,
    },
    Update {
        number: u64,
        body: String,
        branch: String,
        /// The size of the diff the review sends; the pull request's title adds to it.
        diff_bytes: usize,
    },
}

/// Checks what it can of `action` in the checkout at `root` without GitHub, writing what it finds
/// to `out`: the plan for [`send`], or none when there is nothing to send.
pub(crate) fn prepare(
    action: Action,
    root: &Path,
    out: &mut String,
) -> anyhow::Result<Option<Plan>> {
    match action {
        Action::Checks { number } => Ok(Some(Plan::Checks(number))),
        Action::TestChanges => {
            let commit = git::merge_base(root, &git::default_branch(root)?)?;
            out.push_str(&Review::of(root, &commit, None)?.skeleton());
            Ok(None)
        }
        Action::Open { title, body } => {
            let body = read_body(&body)?;
            let checked = preflight(root, &body)?;
            let head = git::git(root, &["rev-parse", "HEAD"])?;
            let pushed = git::git(
                root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/origin/{}", checked.branch),
                ],
            )
            .unwrap_or_default();
            ensure!(
                pushed.trim() == head.trim(),
                "HEAD is not what origin/{0} holds; push it first (`git push origin {0}`), so the \
                 pull request opens on the commits that were checked",
                checked.branch
            );
            let size = crate::review::size(&title, &body, &checked.diff);
            check_size(size)?;
            let base = checked
                .base
                .rsplit_once("refs/remotes/")
                .and_then(|(_, rest)| rest.split_once('/'))
                .map_or(checked.base.as_str(), |(_, name)| name)
                .to_owned();
            Ok(Some(Plan::Open {
                title,
                body,
                branch: checked.branch,
                base,
            }))
        }
        Action::Update { number, body } => {
            let body = read_body(&body)?;
            let checked = preflight(root, &body)?;
            Ok(Some(Plan::Update {
                number,
                body,
                branch: checked.branch,
                diff_bytes: checked.diff.len(),
            }))
        }
    }
}

/// The review size check, as `review` makes it.
fn check_size(bytes: usize) -> anyhow::Result<()> {
    crate::review::check_size("the diff with the title and description", bytes)
}

fn read_body(path: &Path) -> anyhow::Result<String> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("could not read the description in {}", path.display()))?;
    ensure!(
        !body.trim().is_empty(),
        "{} is empty; a pull request needs a description",
        path.display()
    );
    Ok(body)
}

/// What [`preflight`] found.
#[derive(Debug)]
struct Checked {
    /// The branch checked out.
    branch: String,
    /// The default branch's ref.
    base: String,
    /// The diff the review sends.
    diff: String,
}

/// The checks every description goes through before it is sent: a clean tree on a branch, a
/// `## Test changes` section integrity accepts, and rulesets that match CI when `.github/`
/// changed.
fn preflight(root: &Path, body: &str) -> anyhow::Result<Checked> {
    let branch = git::git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("HEAD is on no branch; a pull request comes from a branch")?
        .trim()
        .to_owned();
    let status = git::git(root, &["status", "--porcelain"])?;
    ensure!(
        status.trim().is_empty(),
        "the tree has changes that are not committed, which the pull request would not carry; \
         commit them first:\n{status}"
    );
    let base = git::default_branch(root)?;
    let commit = git::merge_base(root, &base)?;
    let review = Review::of(root, &commit, Some(body))?;
    if !review.explained() {
        let mut why = Vec::new();
        review.report(&mut why)?;
        bail!(
            "{}the description does not match what integrity flags; `cargo xtask pr \
             test-changes` prints the section to fill in",
            String::from_utf8_lossy(&why)
        );
    }
    let workflows = git::changes(root, &commit)?.into_iter().any(|change| {
        change
            .old
            .iter()
            .chain(change.new.iter())
            .any(|path| path.starts_with(".github/"))
    });
    if workflows {
        crate::rulesets::check(root).context(".github/ changed and `rulesets check` fails")?;
    }
    let (diff, _) = git::reviewable_diff(root, &commit, 3)?;
    Ok(Checked { branch, base, diff })
}

/// Carries out `plan` for the repository `repo` through `send`, writing what happened to `out`;
/// whether all is well (for `checks`, whether every check passed).
pub(crate) fn send(
    plan: Plan,
    root: &Path,
    repo: &Repo,
    send: &mut Send<'_>,
    out: &mut String,
) -> anyhow::Result<bool> {
    match plan {
        Plan::Checks(number) => checks::run(repo, number, send, out),
        Plan::Open {
            title,
            body,
            branch,
            base,
        } => {
            let opened = send(&github::Request {
                method: "POST",
                path: repo.path("pulls"),
                body: Some(json!({"title": title, "head": branch, "base": base, "body": body})),
            })?;
            say(
                out,
                &format!(
                    "pr: opened #{} {}",
                    opened
                        .get("number")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    text(&opened, "/html_url")
                ),
            );
            Ok(true)
        }
        Plan::Update {
            number,
            body,
            branch,
            diff_bytes,
        } => update(root, repo, send, (number, &body, &branch, diff_bytes), out),
    }
}

/// A string field of `value`, or nothing.
fn text<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn say(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

/// `pr update`: checks the size with the pull request's title and whether the hand-over can be
/// done, then replaces the description.
fn update(
    root: &Path,
    repo: &Repo,
    send: &mut Send<'_>,
    (number, body, branch, diff_bytes): (u64, &str, &str, usize),
    out: &mut String,
) -> anyhow::Result<bool> {
    let pull = github::get(send, repo.path(&format!("pulls/{number}")))?;
    let head_ref = text(&pull, "/head/ref");
    ensure!(
        head_ref == branch,
        "#{number} comes from {head_ref}, but HEAD is on {branch}"
    );
    let title = text(&pull, "/title");
    check_size(
        diff_bytes
            .saturating_add(title.len())
            .saturating_add(body.len()),
    )?;
    let head = text(&pull, "/head/sha").to_owned();
    let runs = github::get(
        send,
        repo.path(&format!(
            "actions/runs?head_sha={head}&event=pull_request&per_page=1"
        )),
    )?;
    let pusher = runs
        .pointer("/workflow_runs/0/actor/login")
        .and_then(Value::as_str)
        .with_context(|| {
            format!(
                "no CI run for #{number}'s head {head} yet, so who pushed it is unknown; run this \
                 again once CI has started"
            )
        })?
        .to_owned();
    let handover = if pusher == APP_LOGIN {
        format!("pr: the latest push to #{number} is the agent App's; the hand-over is done")
    } else {
        handover(root, &head, &pusher, number)?
    };
    send(&github::Request {
        method: "PATCH",
        path: repo.path(&format!("pulls/{number}")),
        body: Some(json!({"body": body})),
    })?;
    say(out, &format!("pr: updated the description of #{number}"));
    say(out, &handover);
    Ok(pusher == APP_LOGIN)
}

/// When `pusher`, not the App, pushed `head` last (the maintainer pushes workflow changes), the
/// maintainer can approve only after the App pushes again, and GitHub counts only a push that
/// changes the diff (#25 lost a round to an empty commit). Refuses unless HEAD's tree differs from
/// `head`'s; otherwise says to push it.
fn handover(root: &Path, head: &str, pusher: &str, number: u64) -> anyhow::Result<String> {
    ensure!(
        git::git(root, &["cat-file", "-e", &format!("{head}^{{commit}}")]).is_ok(),
        "{pusher} pushed {head} to #{number} last, and this checkout does not have it; fetch \
         and build on it first"
    );
    let changed = std::process::Command::new("git")
        .args(["diff", "--quiet", head, "HEAD", "--"])
        .current_dir(root)
        .status()
        .context("could not run git")?;
    match changed.code() {
        Some(1) => Ok(format!(
            "pr: {pusher} pushed #{number}'s head last; push HEAD now: it changes the diff, so \
             GitHub counts it as a reviewable push and the maintainer can approve"
        )),
        Some(0) => bail!(
            "{pusher} pushed #{number}'s head last, and HEAD changes nothing in it: GitHub does \
             not count a push that leaves the diff as it is, so the maintainer could still not \
             approve. Commit a change that alters the diff, then run this again"
        ),
        _ => bail!("git diff {head} HEAD failed"),
    }
}

#[cfg(test)]
mod tests;
