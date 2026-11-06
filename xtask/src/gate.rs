//! The gate lock: one heavy gate at a time in a checkout.
//!
//! `check`, `coverage`, `mutants`, `fuzz` and `e2e` each build and test the whole tree, and two at
//! once do not fit in an agent VM's memory: in S0 PR 5a the stop hook's `check` started while the
//! agent's `coverage`, and later its `mutants`, ran, and the out-of-memory killer took the linker
//! both times. In-place xtask mutants also edit xtask's sources while they run, which a `check`
//! beside them would build. So each of these gates takes one exclusive lock, [`LOCK`] under the
//! checkout's `target/`, for the whole run. A second gate waits for it and says once who holds it;
//! the stop hook does not wait (`check --no-wait`), and sends the stop back naming the gate to
//! wait for.
//!
//! The lock is the operating system's (`flock` on Unix), so it dies with the process that holds
//! it: a gate that was killed never blocks the next. The file records the holder, its gate, pid
//! and start, only to say who holds the lock; the record never decides anything.

use std::fs::{self, File, TryLockError};
use std::io::{Read as _, Seek as _, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The lock's file, relative to the checkout.
pub(crate) const LOCK: &str = "target/gate.lock";

/// The exit status of `check --no-wait` when another gate holds the lock (`EX_TEMPFAIL`).
pub(crate) const BUSY_STATUS: u8 = 75;

/// The gates that take the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Gate {
    Check,
    Coverage,
    Mutants,
    Fuzz,
    E2e,
}

impl Gate {
    /// The gate's command, `cargo xtask <name>`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Coverage => "coverage",
            Self::Mutants => "mutants",
            Self::Fuzz => "fuzz",
            Self::E2e => "e2e",
        }
    }
}

/// Who holds the lock, as its file records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Holder {
    gate: Gate,
    pid: u32,
    /// When it took the lock, in seconds since the Unix epoch.
    since: u64,
}

/// The lock, held until this is dropped or the process ends.
#[derive(Debug)]
pub(crate) struct Held {
    /// The locked file; closing it releases the lock.
    _file: File,
}

/// What [`take`] got.
#[derive(Debug)]
pub(crate) enum Taken {
    Held(Held),
    /// Another gate holds the lock and the caller would not wait: who, in words.
    Busy(String),
}

/// Whether [`take`] waits for a lock another gate holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    Yes,
    No,
}

/// Takes the gate lock of the checkout at `root` for `gate`. When another gate holds it, says so
/// once in `out` and waits for it, or with [`Wait::No`] returns who holds it.
pub(crate) fn take(
    root: &Path,
    gate: Gate,
    wait: Wait,
    out: &mut impl Write,
) -> anyhow::Result<Taken> {
    take_at(&root.join(LOCK), gate, wait, now(), out)
}

fn take_at(
    path: &Path,
    gate: Gate,
    wait: Wait,
    now: u64,
    out: &mut impl Write,
) -> anyhow::Result<Taken> {
    let mut file = open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let holder = describe(&mut file, now);
            if wait == Wait::No {
                return Ok(Taken::Busy(holder));
            }
            writeln!(
                out,
                "xtask: waiting for {holder} to finish: one gate runs at a time ({LOCK})"
            )?;
            out.flush()?;
            file.lock()
                .with_context(|| format!("could not lock {}", path.display()))?;
        }
        Err(TryLockError::Error(error)) => {
            return Err(error).with_context(|| format!("could not lock {}", path.display()));
        }
    }
    let record = serde_json::to_string(&Holder {
        gate,
        pid: std::process::id(),
        since: self::now(),
    })
    .context("could not encode the gate lock's holder")?;
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .and_then(|_| writeln!(file, "{record}"))
        .and_then(|()| file.flush())
        .with_context(|| format!("could not record the holder in {}", path.display()))?;
    Ok(Taken::Held(Held { _file: file }))
}

/// Opens the lock's file, creating it and its directory.
fn open(path: &Path) -> anyhow::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))
}

/// The holder `file` records, in words, at `now`; "another gate" when the record cannot be read,
/// as when its holder is still writing it.
fn describe(file: &mut File, now: u64) -> String {
    let mut text = String::new();
    let read = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_string(&mut text));
    match read
        .ok()
        .and_then(|_| serde_json::from_str::<Holder>(text.trim()).ok())
    {
        Some(holder) => format!(
            "`cargo xtask {}` (pid {}, running for {})",
            holder.gate.name(),
            holder.pid,
            minutes(now.saturating_sub(holder.since))
        ),
        None => String::from("another gate"),
    }
}

/// `seconds` in whole minutes, in words.
fn minutes(seconds: u64) -> String {
    match seconds / 60 {
        0 => String::from("less than a minute"),
        1 => String::from("1 minute"),
        count => format!("{count} minutes"),
    }
}

/// Seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{Gate, LOCK, Taken, Wait, describe, minutes, open, take, take_at};

    fn busy(taken: Taken) -> String {
        match taken {
            Taken::Busy(holder) => holder,
            Taken::Held(_) => panic!("the lock was free"),
        }
    }

    #[test]
    fn a_second_gate_is_told_which_gate_holds_the_lock() {
        let root = tempfile::tempdir().unwrap();
        let held = take(root.path(), Gate::Coverage, Wait::No, &mut Vec::new()).unwrap();
        assert!(matches!(held, Taken::Held(_)));
        let mut said = Vec::new();
        let holder = busy(take(root.path(), Gate::Check, Wait::No, &mut said).unwrap());
        assert_eq!(
            holder,
            format!(
                "`cargo xtask coverage` (pid {}, running for less than a minute)",
                std::process::id()
            )
        );
        assert!(
            said.is_empty(),
            "a caller that does not wait is told nothing"
        );
        drop(held);
        assert!(
            matches!(
                take(root.path(), Gate::Check, Wait::No, &mut Vec::new()).unwrap(),
                Taken::Held(_)
            ),
            "released when dropped, and free for the next gate"
        );
        assert!(root.path().join(LOCK).is_file(), "under target/");
    }

    /// PR 5a: the stop hook's `check` started while the agent's `coverage` ran, and the VM's
    /// out-of-memory killer took the linker. Now `check` waits, saying once what for.
    #[test]
    fn a_gate_waits_for_the_one_running_and_says_so_once() {
        let root = tempfile::tempdir().unwrap();
        let held = take(root.path(), Gate::Coverage, Wait::Yes, &mut Vec::new()).unwrap();
        let (done, finished) = mpsc::channel();
        let dir = root.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            let mut said = Vec::new();
            let taken = take(&dir, Gate::Check, Wait::Yes, &mut said).unwrap();
            done.send(()).unwrap();
            (
                matches!(taken, Taken::Held(_)),
                String::from_utf8(said).unwrap(),
            )
        });
        assert!(
            finished.recv_timeout(Duration::from_millis(500)).is_err(),
            "check does not start while coverage holds the lock"
        );
        drop(held);
        let (got, said) = waiter.join().unwrap();
        assert!(got, "it gets the lock once coverage is done");
        assert_eq!(said.matches("waiting for").count(), 1, "said once: {said}");
        assert!(said.contains("`cargo xtask coverage`"), "{said}");
        assert!(
            matches!(
                take(root.path(), Gate::Fuzz, Wait::No, &mut Vec::new()).unwrap(),
                Taken::Held(_)
            ),
            "the waiter released it at its end"
        );
    }

    #[test]
    fn a_lock_whose_holder_was_killed_does_not_block() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(LOCK);
        let file = open(&path).unwrap();
        file.lock().unwrap();
        fs::write(&path, "{\"gate\":\"mutants\",\"pid\":1,\"since\":0}\n").unwrap();
        // The child shares the locked file and holds the lock once this process lets go of it.
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::from(file))
            .spawn()
            .unwrap();
        let holder = busy(take(root.path(), Gate::Check, Wait::No, &mut Vec::new()).unwrap());
        assert!(
            holder.starts_with("`cargo xtask mutants` (pid 1, running for "),
            "{holder}"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            matches!(
                take(root.path(), Gate::Check, Wait::No, &mut Vec::new()).unwrap(),
                Taken::Held(_)
            ),
            "the lock died with its holder"
        );
    }

    #[test]
    fn a_record_that_cannot_be_read_is_another_gate() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        fs::write(&path, "half a rec").unwrap();
        assert_eq!(describe(&mut open(&path).unwrap(), 0), "another gate");
        fs::write(&path, "{\"gate\":\"fuzz\",\"pid\":7,\"since\":100}\n").unwrap();
        assert_eq!(
            describe(&mut open(&path).unwrap(), 100 + 125 * 60),
            "`cargo xtask fuzz` (pid 7, running for 125 minutes)"
        );
    }

    #[test]
    fn the_holder_is_recorded_for_the_next_gate() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/lock");
        let _held = take_at(&path, Gate::E2e, Wait::No, 0, &mut Vec::new()).unwrap();
        let record: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(record["gate"], "e2e");
        assert_eq!(record["pid"], std::process::id());
        let since = record["since"].as_u64().unwrap();
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            clock.abs_diff(since) < 60,
            "the real time, in seconds since the epoch: {record}"
        );
    }

    #[test]
    fn minutes_read_as_words() {
        assert_eq!(minutes(59), "less than a minute");
        assert_eq!(minutes(60), "1 minute");
        assert_eq!(minutes(3 * 60 + 59), "3 minutes");
        for gate in [
            Gate::Check,
            Gate::Coverage,
            Gate::Mutants,
            Gate::Fuzz,
            Gate::E2e,
        ] {
            assert_eq!(
                serde_json::to_value(gate).unwrap(),
                gate.name(),
                "the record names the gate as its command does"
            );
        }
    }
}
