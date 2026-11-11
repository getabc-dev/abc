//! Writing the small state files xtask keeps beside a checkout or under the host's state
//! directory: whole or not at all, and one writer at a time.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;

/// Replaces the file at `path` with `text` atomically, creating its directory: a temporary file
/// beside it, renamed over it, so a reader never sees half a file and two writers never mix
/// theirs. The file is synced before the rename and the directory after it, so a crash leaves
/// the old contents or the new, never an empty file.
pub(crate) fn replace(path: &Path, text: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} names no file in a directory", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("{} names no file", path.display()))?;
    let temporary = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut file = File::create(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("could not write {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, path)
        .with_context(|| format!("could not move {} into place", temporary.display()))?;
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("could not sync {}", dir.display()))
}

/// Takes the lock file at `path`, creating it and its directory, and waits for it while another
/// process holds it; the lock is the operating system's, held until the returned file is dropped
/// or its process ends.
pub(crate) fn lock(path: &Path) -> anyhow::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("could not lock {}", path.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::{lock, replace};

    #[test]
    fn a_replaced_file_is_whole_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/state.json");
        replace(&path, "one").unwrap();
        replace(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let names: Vec<_> = std::fs::read_dir(dir.path().join("a/b"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"], "no temporary file left");
        replace(std::path::Path::new("/"), "x").unwrap_err();
        std::fs::create_dir(dir.path().join("a/b/dir")).unwrap();
        replace(&dir.path().join("a/b/dir"), "x").unwrap_err();
    }

    #[test]
    fn a_lock_is_one_holders_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x/state.lock");
        let held = lock(&path).unwrap();
        let other = std::fs::File::open(&path).unwrap();
        assert!(
            matches!(other.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "held"
        );
        drop(held);
        other.try_lock().unwrap();
        lock(&dir.path().join("x")).unwrap_err();
    }
}
