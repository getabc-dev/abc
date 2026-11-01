//! The task an agent is working on, recorded per worktree in `.abc/task.md` for the stop hook's
//! watchdog. Agents set it when they start a task (AGENTS.md says so).

use std::fs;
use std::path::Path;

use anyhow::{Context, ensure};

/// The task file, relative to the worktree root; `.abc/` is ignored by git.
pub(crate) const TASK: &str = ".abc/task.md";

/// Records `text` as the current task.
pub(crate) fn set(root: &Path, text: &str) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty(),
        "describe the task; an empty one guides nothing"
    );
    let path = root.join(TASK);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    fs::write(&path, format!("{}\n", text.trim()))
        .with_context(|| format!("could not write {}", path.display()))
}

/// The current task, if one is recorded.
pub(crate) fn read(root: &Path) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(root.join(TASK)) {
        Ok(text) if !text.trim().is_empty() => Ok(Some(text.trim().to_owned())),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {TASK}")),
    }
}

/// Forgets the current task.
pub(crate) fn clear(root: &Path) -> anyhow::Result<()> {
    match fs::remove_file(root.join(TASK)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {TASK}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{TASK, clear, read, set};

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
