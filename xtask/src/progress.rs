//! The checkpoint of an agent's work in a worktree, `.abc/progress.md`, which xtask's hooks keep
//! so that a fresh session can carry on from it and the brief alone, after a usage limit, a
//! network outage, a reboot or anything else that ends a session without a handoff.
//!
//! What it holds is recorded in `.abc/progress.json` and rendered whole into the Markdown file
//! every time it changes:
//!
//! - each gate's last pass, with the commit it passed at and whether the tree was clean
//!   ([`gate_passed`], from every lock gate that passes on its own checkout, the stop hook's
//!   check included);
//! - the agent's next step ([`set_next`], `cargo xtask task next`);
//! - the message the agent last stopped with ([`stopped`], from the `Stop` and `SubagentStop`
//!   hooks);
//! - the task, the commits since the merge base with the default branch and the uncommitted
//!   files, read at rendering.
//!
//! [`observed`], from the `PostToolUse` hook on Bash, renders again when HEAD moved, so a commit
//! is checkpointed even when no stop follows it. `.abc/` is ignored by git.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::{git, task};

/// The checkpoint a fresh session reads, relative to the worktree root.
pub(crate) const PROGRESS: &str = ".abc/progress.md";

/// What the checkpoint is rendered from, relative to the worktree root.
const STATE: &str = ".abc/progress.json";

/// The lock every rendering holds, so that hooks rendering at once never lose each other's
/// change.
const LOCK: &str = ".abc/progress.lock";

/// The most of the agent's last message the checkpoint keeps, in characters.
const MESSAGE_CHARS: usize = 4000;

/// The most commits the checkpoint lists.
const COMMITS: usize = 50;

/// What the checkpoint records between renderings.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    /// Each gate's last pass, by its command (`check --changed`).
    #[serde(default)]
    gates: BTreeMap<String, Pass>,
    /// Each mutants run's last result, by its command (`mutants --base origin/main`), pass or
    /// fail: a fresh session re-tests the missed mutants and never repeats a full pass.
    #[serde(default)]
    mutants: BTreeMap<String, MutantsRun>,
    /// The agent's next step, in its words.
    #[serde(default)]
    next: Option<String>,
    /// The message the agent last stopped with.
    #[serde(default)]
    last_message: Option<String>,
    /// The commit HEAD was at when the checkpoint was last rendered.
    #[serde(default)]
    head: Option<String>,
}

/// A gate's pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pass {
    commit: String,
    /// Whether the tree had no uncommitted changes when it passed.
    clean: bool,
    /// When it passed, in seconds since the Unix epoch.
    at: i64,
}

/// A mutants run, whatever its result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutantsRun {
    commit: String,
    /// Whether the tree had no uncommitted changes when it ran.
    clean: bool,
    /// When it ended, in seconds since the Unix epoch.
    at: i64,
    /// The mutants no test noticed, as cargo-mutants lists them.
    missed: Vec<String>,
}

/// The commit HEAD is at, if any.
fn head(root: &Path) -> Option<String> {
    git::git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .ok()
        .map(|head| head.trim().to_owned())
        .filter(|head| !head.is_empty())
}

/// What a rendering reads from the checkout.
struct Tree {
    head: Option<String>,
    branch: Option<String>,
    /// The commits since the merge base with the default branch, newest first, one line each.
    commits: Result<Vec<String>, String>,
    /// `git status --short`'s lines.
    uncommitted: Vec<String>,
}

impl Tree {
    fn read(root: &Path) -> Self {
        let lines = |text: String| -> Vec<String> {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect()
        };
        let head = head(root);
        let branch = git::git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .map(|branch| branch.trim().to_owned());
        let commits = git::default_branch(root)
            .and_then(|base| git::merge_base(root, &base).map(|commit| (base, commit)))
            .and_then(|(base, commit)| {
                let range = format!("{commit}..HEAD");
                let count = format!("--max-count={COMMITS}");
                let log = git::git(root, &["log", "--format=%h %s", &count, &range])?;
                let base = base.strip_prefix("refs/remotes/").unwrap_or(&base);
                Ok(std::iter::once(format!("since {base}"))
                    .chain(lines(log))
                    .collect())
            })
            .map_err(|error| format!("{error:#}"));
        let uncommitted = git::git(root, &["status", "--short", "--untracked-files=all"])
            .map(lines)
            .unwrap_or_default();
        Self {
            head,
            branch,
            commits,
            uncommitted,
        }
    }
}

/// Records that `gate` (its command after `cargo xtask`, such as `check --changed`) passed on the
/// checkout at `root` at `now`, and renders the checkpoint.
pub(crate) fn gate_passed(root: &Path, gate: &str, now: i64) -> anyhow::Result<()> {
    update(root, now, |state, tree| {
        if let Some(commit) = &tree.head {
            state.gates.insert(
                gate.to_owned(),
                Pass {
                    commit: commit.clone(),
                    clean: tree.uncommitted.is_empty(),
                    at: now,
                },
            );
        }
    })
}

/// Records that `gate` (its command after `cargo xtask`, such as `mutants --base origin/main`)
/// ran on the checkout at `root` and ended at `now` with these `missed` mutants, none when every
/// one was caught, and renders the checkpoint. A failing run is recorded too: a fresh session
/// re-tests only the missed mutants, and never repeats a full pass.
pub(crate) fn mutants_ran(
    root: &Path,
    gate: &str,
    missed: &[String],
    now: i64,
) -> anyhow::Result<()> {
    update(root, now, |state, tree| {
        if let Some(commit) = &tree.head {
            state.mutants.insert(
                gate.to_owned(),
                MutantsRun {
                    commit: commit.clone(),
                    clean: tree.uncommitted.is_empty(),
                    at: now,
                    missed: missed.to_vec(),
                },
            );
        }
    })
}

/// Records the agent's next step and renders the checkpoint.
pub(crate) fn set_next(root: &Path, text: &str, now: i64) -> anyhow::Result<()> {
    let text = text.trim();
    anyhow::ensure!(
        !text.is_empty(),
        "say what the next step is; an empty one guides nothing"
    );
    update(root, now, |state, _| state.next = Some(text.to_owned()))
}

/// Starts the checkpoint afresh for a new task: no next step and no last message. Gate passes
/// stay, since they say what passed at which commit whatever the task.
pub(crate) fn new_task(root: &Path, now: i64) -> anyhow::Result<()> {
    update(root, now, |state, _| {
        state.next = None;
        state.last_message = None;
    })
}

/// Records that the agent stopped, with `message`, the last thing it said, and renders the
/// checkpoint.
pub(crate) fn stopped(root: &Path, message: Option<&str>, now: i64) -> anyhow::Result<()> {
    update(root, now, |state, _| {
        if let Some(message) = message.map(str::trim).filter(|text| !text.is_empty()) {
            state.last_message = Some(message.chars().take(MESSAGE_CHARS).collect());
        }
    })
}

/// Renders the checkpoint again when HEAD moved since it was last rendered: a commit, a merge or
/// a reset. Says whether it did.
pub(crate) fn observed(root: &Path, now: i64) -> anyhow::Result<bool> {
    let state = load(root)?;
    // One git call: most Bash calls move nothing, and a full read of the tree is for rendering.
    if head(root) == state.head && root.join(PROGRESS).is_file() {
        return Ok(false);
    }
    update(root, now, |_, _| {})?;
    Ok(true)
}

/// Loads the state, applies `change` and writes the state and the checkpoint.
fn update(root: &Path, now: i64, change: impl FnOnce(&mut State, &Tree)) -> anyhow::Result<()> {
    let _lock = crate::files::lock(&root.join(LOCK))?;
    let mut state = load(root)?;
    let tree = Tree::read(root);
    change(&mut state, &tree);
    state.head.clone_from(&tree.head);
    let task = task::read(root)?;
    crate::files::replace(
        &root.join(STATE),
        &format!("{}\n", serde_json::to_string_pretty(&state)?),
    )?;
    crate::files::replace(
        &root.join(PROGRESS),
        &render(&state, &tree, task.as_deref(), now)?,
    )
}

/// The state recorded in `root`; none yet, or one that cannot be read, is a fresh one, since the
/// checkpoint is a summary that the next writer rebuilds.
fn load(root: &Path) -> anyhow::Result<State> {
    match fs::read_to_string(root.join(STATE)) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).with_context(|| format!("could not read {STATE}")),
    }
}

/// The section of mutants runs: each one's result and the mutants it missed.
fn render_mutants(state: &State, tree: &Tree, out: &mut String) -> Result<(), std::fmt::Error> {
    if state.mutants.is_empty() {
        return Ok(());
    }
    out.push_str(
        "\n## Mutants runs\n\nThe last full run of each, with the mutants it missed. Re-test \
         only those, with `cargo xtask tool cargo-mutants mutants --iterate` or `-F <regex>` \
         on each (`--in-diff` as `cargo xtask mutants` passes it); never run a second full \
         pass, since CI's shards do the full pass.\n\n",
    );
    let mut runs: Vec<_> = state.mutants.iter().collect();
    runs.sort_by_key(|(_, run)| std::cmp::Reverse(run.at));
    for (gate, run) in runs {
        let at = if tree.head.as_deref() == Some(run.commit.as_str()) {
            "HEAD"
        } else {
            "an earlier commit"
        };
        let result = match run.missed.len() {
            0 => "caught every mutant".to_owned(),
            1 => "missed 1 mutant".to_owned(),
            n => format!("missed {n} mutants"),
        };
        writeln!(
            out,
            "- `cargo xtask {gate}` ran at {} ({at}){}, {}, and {result}",
            short(&run.commit),
            if run.clean {
                ""
            } else {
                " with uncommitted changes"
            },
            utc(run.at)
        )?;
        for line in &run.missed {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

/// `seconds` since the Unix epoch as a UTC date and time to the minute.
pub(crate) fn utc(seconds: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(seconds) {
        Ok(at) => format!("{} {:02}:{:02} UTC", at.date(), at.hour(), at.minute()),
        Err(_) => format!("Unix time {seconds}"),
    }
}

/// The current time in seconds since the Unix epoch.
pub(crate) fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The checkpoint's Markdown.
fn render(
    state: &State,
    tree: &Tree,
    task: Option<&str>,
    now: i64,
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(
        out,
        "# Progress checkpoint\n\nWritten by xtask's hooks at {}. A fresh session carries on from \
         this file and the brief: read both, then do the next step. Record a new next step with \
         `cargo xtask task next \"<step>\"` at each milestone.\n",
        utc(now)
    )?;
    writeln!(
        out,
        "## Task\n\n{}\n",
        task.unwrap_or("None recorded (`cargo xtask task set \"<the task>\"`).")
    )?;
    writeln!(
        out,
        "## Next step\n\n{}\n",
        state
            .next
            .as_deref()
            .unwrap_or("None recorded (`cargo xtask task next \"<step>\"`).")
    )?;
    out.push_str("## Gates passed\n\n");
    if state.gates.is_empty() {
        out.push_str("None yet.\n");
    }
    let mut passes: Vec<_> = state.gates.iter().collect();
    passes.sort_by_key(|(_, pass)| std::cmp::Reverse(pass.at));
    for (gate, pass) in passes {
        let at = if tree.head.as_deref() == Some(pass.commit.as_str()) {
            "HEAD"
        } else {
            "an earlier commit"
        };
        writeln!(
            out,
            "- `cargo xtask {gate}` passed at {} ({at}){}, {}",
            short(&pass.commit),
            if pass.clean {
                ""
            } else {
                " with uncommitted changes"
            },
            utc(pass.at)
        )?;
    }
    render_mutants(state, tree, &mut out)?;
    writeln!(
        out,
        "\n## Commits\n\nHEAD: {} on {}.",
        tree.head.as_deref().map_or("no commit", short),
        tree.branch.as_deref().unwrap_or("no branch")
    )?;
    match &tree.commits {
        Ok(lines) => {
            let mut lines = lines.iter();
            if let Some(since) = lines.next() {
                writeln!(out, "Commits {since}, newest first:\n")?;
            }
            let mut any = false;
            for line in lines {
                any = true;
                writeln!(out, "- {line}")?;
            }
            if !any {
                out.push_str("- none\n");
            }
        }
        Err(error) => {
            writeln!(out, "The commits could not be listed: {error}")?;
        }
    }
    out.push_str("\n## Uncommitted\n\n");
    if tree.uncommitted.is_empty() {
        out.push_str("Nothing.\n");
    }
    for line in &tree.uncommitted {
        writeln!(out, "    {line}")?;
    }
    if let Some(message) = &state.last_message {
        out.push_str("\n## Last message\n\nThe agent last stopped saying:\n\n");
        for line in message.lines() {
            writeln!(out, "> {line}")?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        PROGRESS, STATE, gate_passed, load, mutants_ran, new_task, now, observed, set_next,
        stopped, utc,
    };
    use crate::testing::{clone_with_origin, git};

    fn progress(root: &Path) -> String {
        std::fs::read_to_string(root.join(PROGRESS)).unwrap()
    }

    #[test]
    fn the_checkpoint_carries_the_task_next_step_gates_commits_and_last_message() {
        let temp = tempfile::tempdir().unwrap();
        let root = clone_with_origin(temp.path());
        // As in this repository, where `.abc/` is ignored.
        std::fs::write(root.join(".git/info/exclude"), ".abc/\n").unwrap();
        crate::task::set(&root, "Ship the supervisor", &mut std::io::sink()).unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-qm", "Add a"]);
        let head = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        assert!(observed(&root, 100).unwrap(), "the first look renders");
        assert!(!observed(&root, 101).unwrap(), "HEAD did not move");

        gate_passed(&root, "check --changed", 1_762_700_000).unwrap();
        set_next(&root, "  write the queue  ", 1_762_700_060).unwrap();
        assert!(set_next(&root, " ", 0).is_err(), "an empty step is refused");
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        gate_passed(&root, "coverage --base origin/main", 1_762_700_120).unwrap();
        stopped(&root, Some("**PASS**\nall good"), 1_762_700_180).unwrap();
        stopped(&root, Some("  "), 1_762_700_190).unwrap();

        let text = progress(&root);
        for expected in [
            "## Task\n\nShip the supervisor\n",
            "## Next step\n\nwrite the queue\n",
            &format!(
                "- `cargo xtask check --changed` passed at {} (HEAD), 2025-11-09 14:53 UTC",
                head.get(..12).unwrap()
            ),
            "- `cargo xtask coverage --base origin/main` passed at ",
            "(HEAD) with uncommitted changes, ",
            "Commits since origin/main, newest first:\n\n- ",
            " Add a\n",
            "    ?? b.txt\n",
            "> **PASS**\n> all good\n",
        ] {
            assert!(text.contains(expected), "{expected:?} in:\n{text}");
        }
        assert!(
            text.find("coverage").unwrap() < text.find("check --changed").unwrap(),
            "newest pass first:\n{text}"
        );

        std::fs::remove_file(root.join("b.txt")).unwrap();
        git(&root, &["commit", "-qm", "Empty", "--allow-empty"]);
        assert!(observed(&root, 200).unwrap(), "a commit renders again");
        let after = progress(&root);
        assert!(
            after.contains("(an earlier commit), 2025-11-09 14:53 UTC"),
            "{after}"
        );
        assert!(after.contains(" Empty\n"), "{after}");
        assert!(after.contains("## Uncommitted\n\nNothing.\n"), "{after}");

        new_task(&root, 300).unwrap();
        let fresh = progress(&root);
        assert!(
            fresh.contains("None recorded (`cargo xtask task next"),
            "{fresh}"
        );
        assert!(!fresh.contains("## Last message"), "{fresh}");
        assert!(fresh.contains("check --changed"), "passes stay:\n{fresh}");
    }

    /// The maintainer's request after I3's run: a mutants run that misses is recorded with when it
    /// ran, on which commit and the missed list, so a fresh session re-tests only those.
    #[test]
    fn a_mutants_run_that_misses_is_recorded_with_its_missed_mutants() {
        let temp = tempfile::tempdir().unwrap();
        let root = clone_with_origin(temp.path());
        std::fs::write(root.join(".git/info/exclude"), ".abc/\n").unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-qm", "Add a"]);
        let head = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        let gate = "mutants --base origin/main";
        let missed = [
            "xtask/src/a.rs:1:18: replace a -> bool with false".to_owned(),
            "xtask/src/b.rs:2:3: delete !".to_owned(),
        ];
        observed(&root, 100).unwrap();
        assert!(!progress(&root).contains("Mutants runs"), "none yet");

        mutants_ran(&root, gate, &missed, 1_762_700_000).unwrap();

        let text = progress(&root);
        for expected in [
            "## Mutants runs\n\nThe last full run of each, with the mutants it missed. Re-test only those",
            "never run a second full pass",
            &format!(
                "- `cargo xtask {gate}` ran at {} (HEAD), 2025-11-09 14:53 UTC, and missed 2 mutants\n",
                head.get(..12).unwrap()
            ),
            "    xtask/src/a.rs:1:18: replace a -> bool with false\n    xtask/src/b.rs:2:3: delete !\n",
        ] {
            assert!(text.contains(expected), "{expected:?} in:\n{text}");
        }
        assert!(
            !text.contains("## Gates passed\n\n- "),
            "a miss is no pass:\n{text}"
        );

        git(&root, &["commit", "-qm", "Fix", "--allow-empty"]);
        mutants_ran(
            &root,
            "mutants --base origin/main --shard 1/8",
            &missed[..1],
            1_762_700_100,
        )
        .unwrap();
        mutants_ran(&root, "mutants --base other", &[], 1_762_700_200).unwrap();
        std::fs::write(root.join("c.txt"), "c\n").unwrap();
        mutants_ran(&root, "mutants --base third", &[], 1_762_700_300).unwrap();
        let later = progress(&root);
        for expected in [
            "(an earlier commit), 2025-11-09 14:53 UTC, and missed 2 mutants",
            "--shard 1/8` ran at ",
            "and missed 1 mutant\n",
            "`cargo xtask mutants --base other` ran at ",
            "and caught every mutant\n",
            "(HEAD) with uncommitted changes, ",
        ] {
            assert!(later.contains(expected), "{expected:?} in:\n{later}");
        }
        assert!(
            later.find("--base third").unwrap() < later.find("--base other").unwrap(),
            "newest first:\n{later}"
        );
        let many: Vec<String> = (0..300).map(|n| format!("x.rs:{n}:1: delete !")).collect();
        mutants_ran(&root, "mutants --base many", &many, 1_762_700_400).unwrap();
        let long = progress(&root);
        assert!(
            long.contains("    x.rs:299:1: delete !\n"),
            "every missed mutant is listed, since a fresh session re-tests only those"
        );
    }

    #[test]
    fn a_broken_state_starts_afresh_and_a_checkout_without_commits_still_renders() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::create_dir_all(dir.path().join(".abc")).unwrap();
        std::fs::write(dir.path().join(STATE), "{ not json").unwrap();
        gate_passed(dir.path(), "check", 5).unwrap();
        let text = progress(dir.path());
        assert!(text.contains("## Gates passed\n\nNone yet.\n"), "{text}");
        assert!(text.contains("HEAD: no commit on "), "{text}");
        assert!(text.contains("The commits could not be listed: "), "{text}");
        assert!(
            text.contains("None recorded (`cargo xtask task set"),
            "{text}"
        );
        std::fs::remove_file(dir.path().join(STATE)).unwrap();
        std::fs::create_dir(dir.path().join(STATE)).unwrap();
        assert!(gate_passed(dir.path(), "check", 5).is_err(), "unreadable");
    }

    #[test]
    fn a_long_last_message_is_cut() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        stopped(dir.path(), Some(&"x".repeat(5000)), 0).unwrap();
        let state = std::fs::read_to_string(dir.path().join(STATE)).unwrap();
        assert!(state.contains(&"x".repeat(4000)), "kept");
        assert!(!state.contains(&"x".repeat(4001)), "cut");
    }

    #[test]
    fn a_branch_without_commits_of_its_own_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let root = clone_with_origin(temp.path());
        assert!(observed(&root, 0).unwrap());
        let text = progress(&root);
        assert!(
            text.contains("Commits since origin/main, newest first:\n\n- none\n"),
            "{text}"
        );
    }

    #[test]
    fn only_a_missing_state_is_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), super::State::default());
        std::fs::create_dir_all(dir.path().join(STATE)).unwrap();
        assert!(load(dir.path()).is_err(), "a state that cannot be read");
    }

    #[test]
    fn now_is_the_system_clock() {
        let system = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let now = u64::try_from(now()).unwrap();
        assert!(now.abs_diff(system) <= 5, "{now} vs {system}");
    }

    /// Review finding: two hooks rendering the checkpoint at once each wrote the state they had
    /// read, so one's change was lost. Rendering holds the checkpoint's lock.
    #[test]
    fn rendering_holds_the_checkpoints_lock() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        let held = crate::files::lock(&dir.path().join(super::LOCK)).unwrap();
        let root = dir.path().to_path_buf();
        let writer = std::thread::spawn(move || set_next(&root, "after the lock", 0).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !dir.path().join(PROGRESS).exists(),
            "it waited for the lock"
        );
        drop(held);
        writer.join().unwrap();
        assert!(progress(dir.path()).contains("after the lock"));
    }

    #[test]
    fn times_read_in_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(i64::MAX), format!("Unix time {}", i64::MAX));
    }
}
