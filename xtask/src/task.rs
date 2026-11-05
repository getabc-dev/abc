//! The task an agent is working on, recorded per worktree in `.abc/task.md` for the stop hook's
//! watchdog. Agents set it when they start a task (AGENTS.md says so).
//!
//! A new task and a BLOCKED declaration are events of the stop hook's state machine
//! (`crate::stop`), which keeps the declaration in its state.

use std::fs;
use std::path::Path;

use anyhow::{Context, bail, ensure};

use crate::stop;

/// The task file, relative to the worktree root; `.abc/` is ignored by git.
pub(crate) const TASK: &str = ".abc/task.md";

/// Writes `text` to `file` atomically: a temporary file beside it, renamed over it, so a reader
/// never sees half a file and two writers never mix theirs.
fn write(root: &Path, file: &str, text: &str) -> anyhow::Result<()> {
    let path = root.join(file);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    let temporary = root.join(format!("{file}.{}.tmp", std::process::id()));
    fs::write(&temporary, text)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not move {} into place", temporary.display()))
}

/// Records `text` as the current task. A new task starts every stop record afresh: no BLOCKED
/// declaration and no blocked stops counted. What the state's IO says goes to `note`.
pub(crate) fn set(root: &Path, text: &str, note: &mut dyn std::io::Write) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty(),
        "describe the task; an empty one guides nothing"
    );
    write(root, TASK, &format!("{}\n", text.trim()))?;
    stop::apply(root, stop::Event::TaskSet, note)?;
    Ok(())
}

/// Declares the task BLOCKED: what is missing and what was tried. The main agent's next stop
/// takes it to the judge. A declaration the stop state refuses, because one already waits, is an
/// error. This is the main agent's command; the hook's guard refuses a subagent's call of it.
pub(crate) fn block(root: &Path, text: &str, note: &mut dyn std::io::Write) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty(),
        "say what is missing and what was tried; an empty declaration hands nothing over"
    );
    let decision = stop::apply(
        root,
        stop::Event::Declared {
            agent: stop::Agent::Main,
            text: text.trim().to_owned(),
        },
        note,
    )?;
    if let stop::Decision::Refused(reason) = decision {
        bail!("{reason}");
    }
    Ok(())
}

/// The current task, if one is recorded.
pub(crate) fn read(root: &Path) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(root.join(TASK)) {
        Ok(text) => Ok(Some(text.trim().to_owned()).filter(|text| !text.is_empty())),
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
    use super::{TASK, block, clear, read, set};
    use crate::stop::{self, Agent, Decision, Event};

    fn stops(root: &std::path::Path) -> Decision {
        stop::apply(
            root,
            Event::Stop {
                agent: Agent::Main,
                stop_hook_active: false,
            },
            &mut std::io::sink(),
        )
        .unwrap()
    }

    #[test]
    fn a_declaration_goes_to_the_next_stop_and_a_new_task_forgets_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut note = Vec::new();
        assert!(
            block(dir.path(), " ", &mut note).is_err(),
            "an empty one is refused"
        );
        block(dir.path(), " no network; tried twice \n", &mut note).unwrap();
        assert_eq!(
            stops(dir.path()),
            Decision::Judge("no network; tried twice".to_owned()),
            "trimmed, and judged at the next stop"
        );
        let refused = block(dir.path(), "x", &mut note).unwrap_err();
        assert!(
            format!("{refused:#}").contains("already waits"),
            "one being judged is not replaced: {refused:#}"
        );
        set(dir.path(), "next", &mut note).unwrap();
        assert_eq!(
            stops(dir.path()),
            Decision::RunCheck,
            "a new task forgets it"
        );
        assert!(note.is_empty(), "nothing to note");
    }

    #[test]
    fn records_reads_and_clears_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut note = std::io::sink();
        assert_eq!(read(dir.path()).unwrap(), None, "no task yet");
        set(dir.path(), "  Add the coverage gate \n", &mut note).unwrap();
        assert_eq!(
            read(dir.path()).unwrap().as_deref(),
            Some("Add the coverage gate"),
            "trimmed"
        );
        assert!(dir.path().join(TASK).is_file(), "in .abc/task.md");
        let files: Vec<_> = std::fs::read_dir(dir.path().join(".abc"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| {
                std::path::Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext == "tmp")
            })
            .collect();
        assert!(files.is_empty(), "renamed into place: {files:?}");
        std::fs::write(dir.path().join(TASK), " \n").unwrap();
        assert_eq!(read(dir.path()).unwrap(), None, "a blank task is none");
        clear(dir.path()).unwrap();
        clear(dir.path()).unwrap();
        assert_eq!(
            read(dir.path()).unwrap(),
            None,
            "cleared, twice without error"
        );
        assert!(
            set(dir.path(), "   ", &mut note).is_err(),
            "an empty task is refused"
        );
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
