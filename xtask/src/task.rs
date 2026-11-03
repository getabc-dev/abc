//! The task an agent is working on, recorded per worktree in `.abc/task.md` for the stop hook's
//! watchdog. Agents set it when they start a task (AGENTS.md says so).

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};

/// The task file, relative to the worktree root; `.abc/` is ignored by git.
pub(crate) const TASK: &str = ".abc/task.md";

/// The BLOCKED declaration, beside the task: what is missing and what was tried.
pub(crate) const BLOCKED: &str = ".abc/blocked.md";

/// How many stops in a row the stop hook has blocked, as a number.
const STOPS: &str = ".abc/stop-blocks";

/// Where the stop hook records, kept, each time its backstop let a stop through.
pub(crate) const BACKSTOP: &str = ".abc/stop-backstop.md";

/// The kept log of every BLOCKED declaration and what the stop hook did with it.
pub(crate) const BLOCKED_LOG: &str = ".abc/blocked-log.md";

/// The directory of the blocked-stop counts of subagents, one file per agent.
const SUBAGENT_STOPS: &str = ".abc/subagent-stops";

/// Creates the directory `path` is in.
fn create_parent(path: &Path) -> anyhow::Result<()> {
    match path.parent() {
        Some(dir) => {
            fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))
        }
        None => Ok(()),
    }
}

/// Writes `text` to `file` atomically: a temporary file beside it, renamed over it, so a reader
/// never sees half a file and two writers never mix theirs.
fn write(root: &Path, file: &str, text: &str) -> anyhow::Result<()> {
    let path = root.join(file);
    create_parent(&path)?;
    let temporary = root.join(format!("{file}.{}.tmp", std::process::id()));
    fs::write(&temporary, text)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not move {} into place", temporary.display()))
}

/// Appends `text` to `file`, creating it, as one write.
fn append(root: &Path, file: &str, text: &str) -> anyhow::Result<()> {
    let path = root.join(file);
    create_parent(&path)?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut log| log.write_all(text.as_bytes()))
        .with_context(|| format!("could not append to {}", path.display()))
}

/// A log entry's heading: the time in seconds since the Unix epoch and what happened.
fn entry(what: &str) -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    format!("\n## Unix time {seconds}: {what}\n\n")
}

/// Reads `file`, or `None` when it does not exist.
fn read_file(root: &Path, file: &str) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(root.join(file)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {file}")),
    }
}

/// Removes `file`; one that does not exist is already gone.
fn remove(root: &Path, file: &str) -> anyhow::Result<()> {
    match fs::remove_file(root.join(file)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {file}")),
    }
}

/// Records `text` as the current task. A new task starts with no BLOCKED declaration and no
/// blocked stops counted.
pub(crate) fn set(root: &Path, text: &str) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty(),
        "describe the task; an empty one guides nothing"
    );
    write(root, TASK, &format!("{}\n", text.trim()))?;
    remove(root, BLOCKED)?;
    reset_blocked_stops(root)?;
    match fs::remove_dir_all(root.join(SUBAGENT_STOPS)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {SUBAGENT_STOPS}")),
    }
}

/// Declares the task BLOCKED: what is missing and what was tried. The next stop is allowed and
/// prints this handoff.
pub(crate) fn block(root: &Path, text: &str) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty(),
        "say what is missing and what was tried; an empty declaration hands nothing over"
    );
    write(root, BLOCKED, &format!("{}\n", text.trim()))
}

/// Takes the BLOCKED declaration, so it covers one stop only.
pub(crate) fn take_blocked(root: &Path) -> anyhow::Result<Option<String>> {
    let declaration = read_file(root, BLOCKED)?.filter(|text| !text.trim().is_empty());
    remove(root, BLOCKED)?;
    Ok(declaration.map(|text| text.trim().to_owned()))
}

/// How many stops in a row `file` counts.
fn count_in(root: &Path, file: &str) -> anyhow::Result<u32> {
    match read_file(root, file)? {
        None => Ok(0),
        Some(text) => text
            .trim()
            .parse()
            .with_context(|| format!("{file} holds {:?}, not a count", text.trim())),
    }
}

/// Counts one more stop in `file` and returns the new count.
fn count_up(root: &Path, file: &str) -> anyhow::Result<u32> {
    let count = count_in(root, file)?.saturating_add(1);
    write(root, file, &format!("{count}\n"))?;
    Ok(count)
}

/// How many stops in a row the stop hook has blocked.
pub(crate) fn blocked_stops(root: &Path) -> anyhow::Result<u32> {
    count_in(root, STOPS)
}

/// Counts one more blocked stop and returns the new count.
pub(crate) fn count_blocked_stop(root: &Path) -> anyhow::Result<u32> {
    count_up(root, STOPS)
}

/// Forgets the blocked stops: `check` passed, a declaration was accepted, a new task began, or the
/// backstop fired.
pub(crate) fn reset_blocked_stops(root: &Path) -> anyhow::Result<()> {
    remove(root, STOPS)
}

/// The count file of the subagent `agent`. Every character but ASCII letters, digits, `-` and `_`
/// becomes `_`, so an id can name no other path; `stop.sh` makes the same name.
fn subagent_file(agent: &str) -> String {
    let name: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = if name.is_empty() { "subagent" } else { &name };
    format!("{SUBAGENT_STOPS}/{name}")
}

/// How many stops in a row the stop hook has blocked for the subagent `agent`.
pub(crate) fn subagent_stops(root: &Path, agent: &str) -> anyhow::Result<u32> {
    count_in(root, &subagent_file(agent))
}

/// Counts one more blocked stop of the subagent `agent` and returns the new count.
pub(crate) fn count_subagent_stop(root: &Path, agent: &str) -> anyhow::Result<u32> {
    count_up(root, &subagent_file(agent))
}

/// Forgets the blocked stops of the subagent `agent`: its check passed or its backstop fired.
pub(crate) fn reset_subagent_stops(root: &Path, agent: &str) -> anyhow::Result<()> {
    remove(root, &subagent_file(agent))
}

/// Appends a BLOCKED declaration and what the stop hook did with it to the kept log.
pub(crate) fn log_blocked(root: &Path, declaration: &str, outcome: &str) -> anyhow::Result<()> {
    append(
        root,
        BLOCKED_LOG,
        &format!("{}{declaration}\n", entry(outcome)),
    )
}

/// Appends to the kept record that the backstop let a stop through after `count` blocked ones.
pub(crate) fn record_backstop(root: &Path, count: u32) -> anyhow::Result<()> {
    append(
        root,
        BACKSTOP,
        &format!(
            "{}The stop hook let a stop through after {count} blocked stops in a row, with no BLOCKED declaration.\n",
            entry("backstop")
        ),
    )
}

/// The current task, if one is recorded.
pub(crate) fn read(root: &Path) -> anyhow::Result<Option<String>> {
    Ok(read_file(root, TASK)?
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty()))
}

/// Forgets the current task.
pub(crate) fn clear(root: &Path) -> anyhow::Result<()> {
    remove(root, TASK)
}

#[cfg(test)]
mod tests {
    use super::{
        BLOCKED, BLOCKED_LOG, TASK, block, blocked_stops, clear, count_blocked_stop,
        count_subagent_stop, log_blocked, read, record_backstop, reset_blocked_stops,
        reset_subagent_stops, set, subagent_stops, take_blocked,
    };

    #[test]
    fn the_count_is_replaced_whole_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..3 {
            count_blocked_stop(dir.path()).unwrap();
        }
        let files: Vec<_> = std::fs::read_dir(dir.path().join(".abc"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, ["stop-blocks"], "renamed into place");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".abc/stop-blocks")).unwrap(),
            "3\n"
        );
    }

    #[test]
    fn declarations_and_backstops_are_kept_in_order() {
        let dir = tempfile::tempdir().unwrap();
        log_blocked(dir.path(), "no network", "sent back: fix it").unwrap();
        log_blocked(dir.path(), "still no network", "stop allowed").unwrap();
        let log = std::fs::read_to_string(dir.path().join(BLOCKED_LOG)).unwrap();
        let first = log.find(": sent back: fix it\n\nno network\n").unwrap();
        let second = log.find(": stop allowed\n\nstill no network\n").unwrap();
        assert!(first < second, "{log}");
        assert!(log.starts_with("\n## Unix time "), "{log}");
        record_backstop(dir.path(), 5).unwrap();
        record_backstop(dir.path(), 6).unwrap();
        let note = std::fs::read_to_string(dir.path().join(super::BACKSTOP)).unwrap();
        assert!(
            note.contains("after 5 blocked stops") && note.contains("after 6 blocked stops"),
            "kept, not replaced: {note}"
        );
    }

    #[test]
    fn each_subagent_counts_apart_from_the_others_and_the_main_agent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(subagent_stops(dir.path(), "a1").unwrap(), 0);
        assert_eq!(count_subagent_stop(dir.path(), "a1").unwrap(), 1);
        assert_eq!(count_subagent_stop(dir.path(), "a1").unwrap(), 2);
        assert_eq!(count_subagent_stop(dir.path(), "b2").unwrap(), 1);
        assert_eq!(
            blocked_stops(dir.path()).unwrap(),
            0,
            "not the main agent's"
        );
        reset_subagent_stops(dir.path(), "a1").unwrap();
        reset_subagent_stops(dir.path(), "a1").unwrap();
        assert_eq!(subagent_stops(dir.path(), "a1").unwrap(), 0);
        assert_eq!(
            subagent_stops(dir.path(), "b2").unwrap(),
            1,
            "b2 is its own"
        );
        count_subagent_stop(dir.path(), "../../escape").unwrap();
        count_subagent_stop(dir.path(), "").unwrap();
        count_subagent_stop(dir.path(), "x-y_1").unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path().join(".abc/subagent-stops"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            names.contains(&"______escape".to_owned())
                && names.contains(&"subagent".to_owned())
                && names.contains(&"x-y_1".to_owned()),
            "an id names no other path: {names:?}"
        );
        assert!(!dir.path().join("escape").exists());
        set(dir.path(), "next").unwrap();
        assert!(
            !dir.path().join(".abc/subagent-stops").exists(),
            "a new task forgets them"
        );
        set(dir.path(), "and again").unwrap();
        std::fs::write(dir.path().join(".abc/subagent-stops"), "not a directory").unwrap();
        assert!(set(dir.path(), "cannot forget them").is_err());
    }

    #[test]
    fn a_blocked_declaration_covers_one_stop_and_a_new_task_forgets_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(take_blocked(dir.path()).unwrap(), None, "none yet");
        assert!(block(dir.path(), " ").is_err(), "an empty one is refused");
        block(dir.path(), " no network; tried twice \n").unwrap();
        assert_eq!(
            take_blocked(dir.path()).unwrap().as_deref(),
            Some("no network; tried twice")
        );
        assert_eq!(take_blocked(dir.path()).unwrap(), None, "taken once");
        block(dir.path(), "x").unwrap();
        count_blocked_stop(dir.path()).unwrap();
        set(dir.path(), "next").unwrap();
        assert!(!dir.path().join(BLOCKED).exists(), "a new task forgets it");
        assert_eq!(blocked_stops(dir.path()).unwrap(), 0, "and the count");
        std::fs::write(dir.path().join(BLOCKED), " \n").unwrap();
        assert_eq!(take_blocked(dir.path()).unwrap(), None, "blank is none");
    }

    #[test]
    fn blocked_stops_are_counted_and_reset() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(blocked_stops(dir.path()).unwrap(), 0);
        assert_eq!(count_blocked_stop(dir.path()).unwrap(), 1);
        assert_eq!(count_blocked_stop(dir.path()).unwrap(), 2);
        assert_eq!(blocked_stops(dir.path()).unwrap(), 2);
        reset_blocked_stops(dir.path()).unwrap();
        reset_blocked_stops(dir.path()).unwrap();
        assert_eq!(blocked_stops(dir.path()).unwrap(), 0);
        std::fs::create_dir_all(dir.path().join(".abc")).unwrap();
        std::fs::write(dir.path().join(".abc/stop-blocks"), "many").unwrap();
        assert!(blocked_stops(dir.path()).is_err(), "a garbled count");
    }

    #[test]
    fn records_reads_and_clears_the_task() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()).unwrap(), None, "no task yet");
        set(dir.path(), "  Add the coverage gate \n").unwrap();
        assert_eq!(
            read(dir.path()).unwrap().as_deref(),
            Some("Add the coverage gate"),
            "trimmed"
        );
        assert!(dir.path().join(TASK).is_file(), "in .abc/task.md");
        std::fs::write(dir.path().join(TASK), " \n").unwrap();
        assert_eq!(read(dir.path()).unwrap(), None, "a blank task is none");
        clear(dir.path()).unwrap();
        clear(dir.path()).unwrap();
        assert_eq!(
            read(dir.path()).unwrap(),
            None,
            "cleared, twice without error"
        );
        assert!(set(dir.path(), "   ").is_err(), "an empty task is refused");
        std::fs::create_dir_all(dir.path().join(TASK)).unwrap();
        assert!(
            read(dir.path()).is_err(),
            "an unreadable task file is an error"
        );
        assert!(
            clear(dir.path()).is_err(),
            "and cannot be removed as a file"
        );
    }
}
