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
mod renovate;

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
    /// merge-queue run's, with each failing check's annotations; warn first when the default
    /// branch moved on files it changes.
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
    /// Replace Renovate's pull request NUMBER, a tool bump in xtask/tools.toml, with one that
    /// pins it: the bump on the default branch, pinned with `cargo xtask pin`, on branch
    /// agent/renovate-NUMBER; then close Renovate's with a link. The checkout is not touched.
    Renovate { number: u64 },
}

/// What [`send`] does once [`prepare`] has checked it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Checks(u64),
    Renovate(u64),
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
        Action::Renovate { number } => Ok(Some(Plan::Renovate(number))),
        Action::TestChanges => {
            let commit = git::merge_base(root, &git::default_branch(root)?)?;
            out.push_str(&Review::of(root, &commit, None)?.skeleton());
            Ok(None)
        }
        Action::Open { title, body } => {
            let body = read_body(&body)?;
            let checked = preflight(root, &body, out)?;
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
            let checked = preflight(root, &body, out)?;
            Ok(Some(Plan::Update {
                number,
                body,
                branch: checked.branch,
                diff_bytes: checked.diff.len(),
            }))
        }
    }
}

/// Whether `body` has a `## Review findings` section with something in it besides comments: what
/// `/code-review` and `/security-review` found on the branch, or "None". #33 opened before they
/// ran, and they then found a data-loss bug and 8 more issues that every gate had passed.
/// The section is the lines after a line that is exactly that heading, up to the next heading of
/// any level, whatever the line endings.
fn reviewed(body: &str) -> bool {
    let mut lines = body.lines().map(str::trim_end);
    if !lines.any(|line| line == "## Review findings") {
        return false;
    }
    let section: Vec<&str> = lines.take_while(|line| !line.starts_with('#')).collect();
    let joined = section.join("\n");
    let mut text = joined.as_str();
    let mut kept = String::new();
    while let Some((before, after)) = text.split_once("<!--") {
        kept.push_str(before);
        text = after.split_once("-->").map_or("", |(_, after)| after);
    }
    kept.push_str(text);
    !kept.trim().is_empty()
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
/// changed. It warns in `out` when the default branch moved on files the branch changes.
fn preflight(root: &Path, body: &str, out: &mut String) -> anyhow::Result<Checked> {
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
    behind(root, &base, out)?;
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
    ensure!(
        reviewed(body),
        "the description has no `## Review findings` section saying what the B1 reviews found: \
         run `/code-review high` and `/security-review` on the branch before it opens, fix what \
         they confirm, and list each finding with what was done, or \"None\""
    );
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
    let (diff, _) = git::reviewable_diff(root, &commit, git::To::WorkingTree)?;
    Ok(Checked { branch, base, diff })
}

/// Carries out `plan` for the repository `repo` through `send`, with downloads from `fetch`,
/// writing what happened to `out`; whether all is well (for `checks`, whether every check passed).
pub(crate) fn send(
    plan: Plan,
    root: &Path,
    repo: &Repo,
    send: &mut Send<'_>,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>,
    out: &mut String,
) -> anyhow::Result<bool> {
    match plan {
        Plan::Renovate(number) => renovate::run(number, root, repo, send, fetch, out),
        Plan::Checks(number) => {
            // A warning only: a comparison GitHub cannot make never hides the checks.
            if let Err(error) = stale(repo, number, send, out) {
                say(
                    out,
                    &format!(
                        "pr: could not compare #{number} with its base branch, so whether it \
                         moved on is unknown: {error:#}"
                    ),
                );
            }
            checks::run(repo, number, send, out)
        }
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

/// GitHub's compare API lists at most this many files.
const COMPARE_FILES: usize = 300;

/// Warns in `out` when the default branch moved on, since pull request `number`'s branch left it,
/// files the pull request changes too, as GitHub's compare API shows them: the check before a
/// merge that `pr open` makes before a pull request opens. #33 broke on its merge with main once
/// #38 merged, though each passed alone.
fn stale(repo: &Repo, number: u64, send: &mut Send<'_>, out: &mut String) -> anyhow::Result<()> {
    let pull = github::get(send, repo.path(&format!("pulls/{number}")))?;
    if text(&pull, "/state") != "open" {
        return Ok(());
    }
    let base = text(&pull, "/base/ref").to_owned();
    let head = text(&pull, "/head/sha").to_owned();
    ensure!(
        !base.is_empty() && !head.is_empty(),
        "GitHub gave #{number} no base branch or head commit"
    );
    let files = |comparison: &Value| -> std::collections::BTreeSet<String> {
        comparison
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|file| {
                ["/filename", "/previous_filename"]
                    .into_iter()
                    .map(|field| text(file, field).to_owned())
            })
            .filter(|path| !path.is_empty())
            .collect()
    };
    let moved = github::get(send, repo.path(&format!("compare/{head}...{base}")))?;
    let behind = moved.get("ahead_by").and_then(Value::as_u64).unwrap_or(0);
    if behind == 0 {
        return Ok(());
    }
    let changed = github::get(send, repo.path(&format!("compare/{base}...{head}")))?;
    let both: Vec<String> = files(&changed)
        .intersection(&files(&moved))
        .cloned()
        .collect();
    let capped = [&moved, &changed].into_iter().any(|comparison| {
        comparison
            .get("files")
            .and_then(Value::as_array)
            .is_some_and(|listed| listed.len() >= COMPARE_FILES)
    });
    if both.is_empty() && !capped {
        return Ok(());
    }
    say(
        out,
        &format!(
            "pr: warning: {base} moved on by {behind} commit(s) since #{number}'s branch left it, \
             changing {} file(s) #{number} changes too{}:",
            both.len(),
            if capped {
                format!(", or more: GitHub lists at most {COMPARE_FILES} files of a comparison")
            } else {
                String::new()
            }
        ),
    );
    for path in &both {
        say(out, &format!("  {path}"));
    }
    say(
        out,
        &format!(
            "pr: merge it into the branch (`git merge origin/{base}`), check again and push \
             before #{number} merges; never rebase a pushed branch"
        ),
    );
    Ok(())
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
    let mut page = |path: &str| github::get(send, repo.path(path));
    let timeline = github::timeline(number, &mut page)?;
    let pusher = github::push_of(
        github::Head {
            sha: &head,
            repository: text(&pull, "/head/repo/full_name"),
            branch: head_ref,
        },
        &timeline,
        &mut page,
    )?
    .with_context(|| {
        format!(
            "no CI run for #{number}'s head {head} yet, so who pushed it is unknown; run this \
             again once CI has started"
        )
    })?
    .pusher;
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

/// Fetches the default branch `base` (a remote-tracking ref) and warns in `out` when it changed,
/// since HEAD left it, files that HEAD changes too. #33 and #38 were built at once, and #33
/// broke on its merge with main once #38 merged: the fix is to merge the default branch and
/// check again, never to rebase a pushed branch. A fetch that fails is said, and the last
/// fetched branch compared.
fn behind(root: &Path, base: &str, out: &mut String) -> anyhow::Result<()> {
    let short = base.strip_prefix("refs/remotes/").unwrap_or(base);
    if let Some((remote, branch)) = short.split_once('/')
        && let Err(error) = git::fetch(root, remote, branch, git::REMOTE_TIMEOUT)
    {
        say(
            out,
            &format!("pr: could not fetch {short}, so it is compared as last fetched: {error:#}"),
        );
    }
    let commit = git::merge_base(root, base)?;
    let names = |to: &str| -> anyhow::Result<std::collections::BTreeSet<String>> {
        Ok(git::git(
            root,
            &[
                "-c",
                "core.quotePath=false",
                "diff",
                "--no-renames",
                "--name-only",
                &commit,
                to,
                "--",
            ],
        )?
        .lines()
        .map(str::to_owned)
        .collect())
    };
    let moved = names(base)?;
    if moved.is_empty() {
        return Ok(());
    }
    let both: Vec<String> = names("HEAD")?.intersection(&moved).cloned().collect();
    if both.is_empty() {
        return Ok(());
    }
    say(
        out,
        &format!(
            "pr: warning: {short} moved on since this branch left it, changing {} file(s) this \
             branch changes too:",
            both.len()
        ),
    );
    for path in &both {
        say(out, &format!("  {path}"));
    }
    say(
        out,
        &format!(
            "pr: merge it (`git merge {short}`), check again and push; never rebase a pushed \
             branch"
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests;
