//! The task an agent is working on, recorded per worktree in `.abc/task.md` for the stop hook's
//! watchdog. Agents set it when they start a task (AGENTS.md says so).

use std::fs;
use std::path::Path;

use anyhow::{Context, ensure};

/// The task file, relative to the worktree root; `.abc/` is ignored by git.
pub(crate) const TASK: &str = ".abc/task.md";

/// The BLOCKED declaration, beside the task: what is missing and what was tried.
pub(crate) const BLOCKED: &str = ".abc/blocked.md";

/// How many stops in a row the stop hook has blocked, as a number.
const STOPS: &str = ".abc/stop-blocks";

/// Where the stop hook records that its backstop let a stop through.
pub(crate) const BACKSTOP: &str = ".abc/stop-backstop.md";

/// Writes `text` to `.abc/<file>`, creating `.abc/`.
fn write(root: &Path, file: &str, text: &str) -> anyhow::Result<()> {
    let path = root.join(file);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))
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
    reset_blocked_stops(root)
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

/// How many stops in a row the stop hook has blocked.
pub(crate) fn blocked_stops(root: &Path) -> anyhow::Result<u32> {
    match read_file(root, STOPS)? {
        None => Ok(0),
        Some(text) => text
            .trim()
            .parse()
            .with_context(|| format!("{STOPS} holds {:?}, not a count", text.trim())),
    }
}

/// Counts one more blocked stop and returns the new count.
pub(crate) fn count_blocked_stop(root: &Path) -> anyhow::Result<u32> {
    let count = blocked_stops(root)?.saturating_add(1);
    write(root, STOPS, &format!("{count}\n"))?;
    Ok(count)
}

/// Forgets the blocked stops: `check` passed, a new task began, or the backstop fired.
pub(crate) fn reset_blocked_stops(root: &Path) -> anyhow::Result<()> {
    remove(root, STOPS)
}

/// Records that the backstop let a stop through after `count` blocked ones.
pub(crate) fn record_backstop(root: &Path, count: u32) -> anyhow::Result<()> {
    write(
        root,
        BACKSTOP,
        &format!(
            "The stop hook let a stop through after {count} blocked stops in a row, with no BLOCKED declaration.\n"
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
        BLOCKED, TASK, block, blocked_stops, clear, count_blocked_stop, read, record_backstop,
        reset_blocked_stops, set, take_blocked,
    };

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
        record_backstop(dir.path(), 5).unwrap();
        let note = std::fs::read_to_string(dir.path().join(super::BACKSTOP)).unwrap();
        assert!(note.contains("after 5 blocked stops"), "{note}");
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
