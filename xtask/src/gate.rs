//! The gate lock: one heavy gate at a time in a checkout.
//!
//! `check`, `coverage`, `mutants`, `fuzz` and `e2e` each build and test the whole tree, and two at
//! once do not fit in an agent VM's memory: in S0 PR 5a the stop hook's `check` started while the
//! agent's `coverage`, and later its `mutants`, ran, and the out-of-memory killer took the linker
//! both times. In-place xtask mutants also edit xtask's sources while they run, which a `check`
//! beside them would build. So each of these gates takes one exclusive lock, [`LOCK`] under the
//! checkout's own git directory (`.git`, or `.git/worktrees/<name>` for a linked worktree), for
//! the whole run. A second gate waits for it and says once who holds it;
//! the stop hook waits only so long (`check --wait-at-most`, `hook::GATE_WAIT`), and then sends
//! the stop back naming the gate to wait for.
//!
//! The lock is the operating system's (`flock` on Unix), so it dies with the process that holds
//! it: a gate that was killed never blocks the next. It is held on an open file, so the file
//! must outlive the gate: in `target/`, `cargo clean` or a hand deleted it, the holder kept the
//! unlinked file and the next gate locked a new one. Nothing deletes a checkout's git
//! directory while it is in use, and no build writes there. The file records the holder, its gate, pid
//! and start, only to say who holds the lock; the record never decides anything.

use std::fs::{self, File, TryLockError};
use std::io::{Read as _, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The lock's file, in the checkout's git directory ([`path`]).
pub(crate) const LOCK: &str = "gate.lock";

/// The gate lock's file of the checkout at `root`: [`LOCK`] in its own git directory.
pub(crate) fn path(root: &Path) -> anyhow::Result<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--absolute-git-dir"])
        .stdin(std::process::Stdio::null())
        .output()
        .context("could not run git")?;
    anyhow::ensure!(
        output.status.success(),
        "the gate lock lives in the checkout's git directory, but {} has none: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let dir = String::from_utf8(output.stdout).context("git printed a path that is not UTF-8")?;
    Ok(Path::new(dir.trim_end()).join(LOCK))
}

/// The exit status of `check --wait-at-most` when another gate still holds the lock at the end of
/// the wait (`EX_TEMPFAIL`).
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
    Spec,
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
            Self::Spec => "spec",
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
    /// Another gate held the lock all the while the caller would wait: who, in words.
    Busy(String),
}

/// How long [`take`] waits for a lock another gate holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    /// Until the other gate ends.
    Yes,
    /// At most this long; no time at all is not waiting.
    AtMost(Duration),
}

/// How often a wait with a limit tries the lock again.
const RETRY: Duration = Duration::from_millis(500);

/// Takes the gate lock of the checkout at `root` for `gate`. When another gate holds it, says so
/// once in `out` and waits for it, or with [`Wait::AtMost`] returns who holds it once that time
/// has passed.
pub(crate) fn take(
    root: &Path,
    gate: Gate,
    wait: Wait,
    out: &mut impl Write,
) -> anyhow::Result<Taken> {
    take_at(&path(root)?, gate, wait, now(), out)
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
            match wait {
                Wait::AtMost(limit) if limit.is_zero() => return Ok(Taken::Busy(holder)),
                Wait::AtMost(limit) => {
                    writeln!(
                        out,
                        "xtask: waiting at most {} s for {holder} to finish: one gate runs at a \
                         time ({})",
                        limit.as_secs(),
                        path.display()
                    )?;
                    out.flush()?;
                    if !retry(&file, path, limit)? {
                        return Ok(Taken::Busy(describe(&mut file, self::now())));
                    }
                }
                Wait::Yes => {
                    writeln!(
                        out,
                        "xtask: waiting for {holder} to finish: one gate runs at a time ({})",
                        path.display()
                    )?;
                    out.flush()?;
                    file.lock()
                        .with_context(|| format!("could not lock {}", path.display()))?;
                }
            }
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

/// Tries to lock `file`, at `path`, every [`RETRY`] for at most `limit`; whether it got the lock.
fn retry(file: &File, path: &Path, limit: Duration) -> anyhow::Result<bool> {
    let started = Instant::now();
    loop {
        let left = limit.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(left.min(RETRY));
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("could not lock {}", path.display()));
            }
        }
    }
}

/// Opens the lock's file, creating it in its existing directory.
fn open(path: &Path) -> anyhow::Result<File> {
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

    /// A checkout in a fresh directory, whose own repository the lock lives in.
    fn checkout() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::testing::repository(&dir.path().join("checkout"));
        (dir, root)
    }

    fn busy(taken: Taken) -> String {
        match taken {
            Taken::Busy(holder) => holder,
            Taken::Held(_) => panic!("the lock was free"),
        }
    }

    /// The stop hook's check waits only so long for another gate, then reports it busy; one that
    /// ends within the wait lets the check start.
    #[test]
    fn a_wait_with_a_limit_takes_the_lock_if_it_comes_in_time() {
        let (_dir, root) = checkout();
        let held = take(&root, Gate::Coverage, Wait::Yes, &mut Vec::new()).unwrap();
        let started = std::time::Instant::now();
        let mut said = Vec::new();
        let holder = busy(
            take(
                &root,
                Gate::Check,
                Wait::AtMost(Duration::from_millis(700)),
                &mut said,
            )
            .unwrap(),
        );
        assert!(started.elapsed() >= Duration::from_millis(700));
        assert!(holder.contains("`cargo xtask coverage`"), "{holder}");
        let said = String::from_utf8(said).unwrap();
        assert!(
            said.starts_with("xtask: waiting at most 0 s for `cargo xtask coverage`"),
            "{said}"
        );
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let taken = take(
            &root,
            Gate::Check,
            Wait::AtMost(Duration::from_secs(10)),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(taken, Taken::Held(_)), "{taken:?}");
        releaser.join().unwrap();
    }

    #[test]
    fn a_second_gate_is_told_which_gate_holds_the_lock() {
        let (_dir, root) = checkout();
        let held = take(
            &root,
            Gate::Coverage,
            Wait::AtMost(Duration::ZERO),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(held, Taken::Held(_)));
        let mut said = Vec::new();
        let holder =
            busy(take(&root, Gate::Check, Wait::AtMost(Duration::ZERO), &mut said).unwrap());
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
                take(
                    &root,
                    Gate::Check,
                    Wait::AtMost(Duration::ZERO),
                    &mut Vec::new()
                )
                .unwrap(),
                Taken::Held(_)
            ),
            "released when dropped, and free for the next gate"
        );
        assert!(
            super::path(&root).unwrap().is_file(),
            "in the checkout's own repository"
        );
        assert_eq!(
            super::path(&root).unwrap(),
            root.join(".git").join(LOCK),
            "a main clone's own"
        );
    }

    /// PR 5a: the stop hook's `check` started while the agent's `coverage` ran, and the VM's
    /// out-of-memory killer took the linker. Now `check` waits, saying once what for.
    #[test]
    fn a_gate_waits_for_the_one_running_and_says_so_once() {
        let (_dir, root) = checkout();
        let held = take(&root, Gate::Coverage, Wait::Yes, &mut Vec::new()).unwrap();
        let (done, finished) = mpsc::channel();
        let dir = root.clone();
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
                take(
                    &root,
                    Gate::Fuzz,
                    Wait::AtMost(Duration::ZERO),
                    &mut Vec::new()
                )
                .unwrap(),
                Taken::Held(_)
            ),
            "the waiter released it at its end"
        );
    }

    /// The lock lived in `target/`, which `cargo clean` or a hand deletes: the holder kept the
    /// unlinked file, the next gate locked a fresh one, and two heavy gates ran at once.
    #[test]
    fn deleting_the_build_output_does_not_free_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::testing::repository(&dir.path().join("checkout"));
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let held = take(&root, Gate::Coverage, Wait::Yes, &mut Vec::new()).unwrap();
        fs::remove_dir_all(root.join("target")).unwrap();
        let holder = busy(
            take(
                &root,
                Gate::Check,
                Wait::AtMost(Duration::ZERO),
                &mut Vec::new(),
            )
            .unwrap(),
        );
        assert!(holder.contains("`cargo xtask coverage`"), "{holder}");
        drop(held);
    }

    #[test]
    fn a_lock_whose_holder_was_killed_does_not_block() {
        let (_dir, root) = checkout();
        let path = super::path(&root).unwrap();
        let file = open(&path).unwrap();
        file.lock().unwrap();
        fs::write(&path, "{\"gate\":\"mutants\",\"pid\":1,\"since\":0}\n").unwrap();
        // The child shares the locked file and holds the lock once this process lets go of it.
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::from(file))
            .spawn()
            .unwrap();
        let holder = busy(
            take(
                &root,
                Gate::Check,
                Wait::AtMost(Duration::ZERO),
                &mut Vec::new(),
            )
            .unwrap(),
        );
        assert!(
            holder.starts_with("`cargo xtask mutants` (pid 1, running for "),
            "{holder}"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            matches!(
                take(
                    &root,
                    Gate::Check,
                    Wait::AtMost(Duration::ZERO),
                    &mut Vec::new()
                )
                .unwrap(),
                Taken::Held(_)
            ),
            "the lock died with its holder"
        );
    }

    #[test]
    fn a_record_that_cannot_be_read_is_another_gate() {
        let (_dir, root) = checkout();
        let path = root.join("lock");
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
        let (_dir, root) = checkout();
        let path = root.join("lock");
        let _held = take_at(
            &path,
            Gate::E2e,
            Wait::AtMost(Duration::ZERO),
            0,
            &mut Vec::new(),
        )
        .unwrap();
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
