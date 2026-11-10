//! Supervised agent runs: `cargo xtask vm run`, `vm queue`, `vm runs` and `vm cancel`.
//!
//! A run carries out one brief in one worktree's session until the agent hands off PASS, FAIL or
//! BLOCKED, across usage limits, network outages and reboots of the host. Its supervisor runs
//! `claude -p` in the session's VM, reads how each attempt ended from its stream-json `result`
//! ([`outcome`]), waits out a limit until its reset or an outage while probing the API, and
//! restarts as a **fresh** `claude -p` on the brief and the checkpoint the hooks keep
//! (`.abc/progress.md`, [`crate::progress`]), never `--resume`, which re-reads the whole
//! transcript and burns a usage window. It is bounded ([`outcome::MAX_ATTEMPTS`],
//! [`outcome::MAX_FAILURES`], [`outcome::MAX_OUTAGE`]) and always ends in a final state.
//!
//! Each run lives in its own directory under the host's state directory
//! (`~/.local/state/abc/runs/<run>/`), outside the VM and `/tmp`: `state.json`, which `vm runs
//! --json` prints, and `log`, every attempt's output. The supervisor is a host service
//! ([`service`]), so a reboot starts it again, and it carries on: it marks the attempt the reboot
//! cut off as interrupted, starts the VM, and restarts fresh from the checkpoint.
//!
//! One run is active at a time, since every session runs on the same subscription and so shares
//! its usage window: `vm run` refuses while another is active, and `vm queue add` queues a brief
//! for when the active run ends. The supervisor never reads, stores or logs the Claude token
//! (I1): starting a VM pipes it from the keychain into the VM as `vm start` does, and only
//! `claude` in the VM reads it.

pub(crate) mod outcome;
pub(crate) mod service;

use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, bail, ensure};
use serde::{Deserialize, Serialize};

use outcome::{Cause, Next, Outage, Outcome, Verdict};
use service::Services;

use super::session::Session;
use super::{Backend, Host};

/// A run's state file, in its directory.
const STATE: &str = "state.json";

/// A run's log, every attempt's output, in its directory.
pub(crate) const LOG: &str = "log";

/// The lock that serialises the decisions of which run is active.
const LOCK: &str = "queue.lock";

/// Where a run stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "status")]
pub(crate) enum Status {
    /// Waiting for the active run to end.
    Queued,
    /// Active: the next attempt starts at `until`, in seconds since the Unix epoch.
    Waiting { until: i64, cause: Cause },
    /// Active: an attempt runs.
    Running,
    /// The agent handed off.
    Done { verdict: Verdict },
    /// The supervisor gave up, for this reason.
    GaveUp { reason: String },
    /// The maintainer cancelled it.
    Cancelled,
}

impl Status {
    /// Whether the run is the active one: its supervisor runs.
    const fn active(&self) -> bool {
        matches!(self, Self::Waiting { .. } | Self::Running)
    }

    /// The status in a few words.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Queued => "queued".to_owned(),
            Self::Waiting {
                until,
                cause: Cause::Start,
            } => format!("starting at {}", crate::progress::utc(*until)),
            Self::Waiting {
                until,
                cause: Cause::Retry,
            } => format!("retrying at {}", crate::progress::utc(*until)),
            Self::Waiting {
                until,
                cause: Cause::Limit,
            } => format!("usage limit until {}", crate::progress::utc(*until)),
            Self::Waiting {
                until,
                cause: Cause::Network,
            } => format!("network down; probing at {}", crate::progress::utc(*until)),
            Self::Waiting {
                until,
                cause: Cause::Unavailable,
            } => format!(
                "the API is unavailable; trying again at {}",
                crate::progress::utc(*until)
            ),
            Self::Waiting {
                until,
                cause: Cause::Unstarted,
            } => format!(
                "the session could not start; starting it again at {}",
                crate::progress::utc(*until)
            ),
            Self::Running => "running".to_owned(),
            Self::Done { verdict } => format!("done: {}", verdict.word()),
            Self::GaveUp { reason } => format!("gave up: {reason}"),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// One `claude -p` session of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attempt {
    /// When it started, in seconds since the Unix epoch.
    pub(crate) started: i64,
    pub(crate) ended: Option<i64>,
    /// The Claude Code session ids its stream named.
    pub(crate) sessions: Vec<String>,
    pub(crate) outcome: Option<Outcome>,
}

/// A run, as its state file holds it. Its status's fields sit beside the others (serde cannot
/// refuse unknown fields beside a flattened one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Run {
    /// `<session>-<UTC start, yyyymmddThhmmss>`, and its directory's name.
    pub(crate) id: String,
    pub(crate) worktree: PathBuf,
    /// The brief, relative to the worktree.
    pub(crate) brief: String,
    /// When it was created, in seconds since the Unix epoch.
    pub(crate) created: i64,
    #[serde(flatten)]
    pub(crate) status: Status,
    pub(crate) attempts: Vec<Attempt>,
    /// The network outage it is waiting out, if any.
    pub(crate) outage: Option<Outage>,
}

impl Run {
    /// Takes `next` as the run's status.
    fn follow(&mut self, next: Next) {
        self.status = match next {
            Next::Wait {
                until,
                cause,
                outage,
            } => {
                self.outage = outage;
                Status::Waiting { until, cause }
            }
            Next::Done(verdict) => {
                self.outage = None;
                Status::Done { verdict }
            }
            Next::GiveUp(reason) => {
                self.outage = None;
                Status::GaveUp { reason }
            }
        };
    }

    /// Ends the open attempt with `outcome` at `now` and decides what follows.
    fn end_attempt(&mut self, outcome: Outcome, now: i64) {
        let earlier: Vec<Outcome> = self
            .attempts
            .iter()
            .filter_map(|attempt| attempt.outcome.clone())
            .collect();
        let started = self.attempts.last().map_or(now, |attempt| attempt.started);
        let next = outcome::after_attempt(&earlier, &outcome, started, self.outage, now);
        if let Some(attempt) = self.attempts.last_mut() {
            attempt.ended = Some(now);
            attempt.outcome = Some(outcome);
        }
        self.follow(next);
    }

    /// The prompt of the next attempt: the brief, and after the first attempt the checkpoint.
    pub(crate) fn prompt(&self) -> String {
        let keep = "Keep the checkpoint: at each milestone record your next step with `cargo \
                    xtask task next \"<step>\"`; the hooks write it to .abc/progress.md with the \
                    gates passed and the commits, and commit and push your work as you go. Begin \
                    your final message, the handoff, with its verdict as its first word: PASS, \
                    FAIL or BLOCKED.";
        let last = self
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| attempt.outcome.as_ref());
        match last {
            None => format!(
                "Read {} and do the brief to its handoff. {keep}",
                self.brief
            ),
            Some(outcome) => format!(
                "This is a fresh session of a supervised run (attempt {}); the last session \
                 ended with {}. Read {}, then .abc/progress.md, the checkpoint of the work so \
                 far, and carry on from its next step to the brief's handoff; do not redo what it \
                 shows done. {keep}",
                self.attempts.len().saturating_add(1),
                outcome.describe(),
                self.brief
            ),
        }
    }
}

/// The runs directory.
#[derive(Debug)]
pub(crate) struct Runs {
    pub(crate) dir: PathBuf,
}

impl Runs {
    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    /// Run `id`'s log.
    pub(crate) fn log(&self, id: &str) -> PathBuf {
        self.path(id).join(LOG)
    }

    pub(crate) fn load(&self, id: &str) -> anyhow::Result<Run> {
        ensure!(
            plain_name(id),
            "{id} is not a run; `cargo xtask vm runs` lists them"
        );
        let path = self.path(id).join(STATE);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("no run {id}: could not read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("could not read {}", path.display()))
    }

    /// Writes `run`'s state atomically.
    pub(crate) fn save(&self, run: &Run) -> anyhow::Result<()> {
        let dir = self.path(&run.id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        let path = dir.join(STATE);
        let partial = dir.join(format!(".{STATE}.{}", std::process::id()));
        std::fs::write(
            &partial,
            format!("{}\n", serde_json::to_string_pretty(run)?),
        )
        .with_context(|| format!("could not write {}", partial.display()))?;
        std::fs::rename(&partial, &path)
            .with_context(|| format!("could not move {} into place", path.display()))
    }

    /// Every run, oldest first.
    pub(crate) fn list(&self) -> anyhow::Result<Vec<Run>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not read {}", self.dir.display()));
            }
        };
        let mut runs = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.path().join(STATE).is_file()
                && let Some(id) = entry.file_name().to_str()
            {
                runs.push(self.load(id)?);
            }
        }
        runs.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
        Ok(runs)
    }

    /// Takes the lock that serialises which run is active, until the returned file is dropped.
    fn lock(&self) -> anyhow::Result<File> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("could not create {}", self.dir.display()))?;
        let path = self.dir.join(LOCK);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("could not lock {}", path.display()))?;
        Ok(file)
    }

    /// A new run of `brief` in `session`, created at `now`, with `status`.
    fn create(
        &self,
        session: &Session,
        brief: &str,
        now: i64,
        status: Status,
    ) -> anyhow::Result<Run> {
        let brief = brief_in(&session.worktree, brief)?;
        let at = time::OffsetDateTime::from_unix_timestamp(now)?;
        let stamp = format!(
            "{}-{:04}{:02}{:02}T{:02}{:02}{:02}",
            session.name,
            at.year(),
            u8::from(at.month()),
            at.day(),
            at.hour(),
            at.minute(),
            at.second()
        );
        let mut id = stamp.clone();
        let mut n = 1_u32;
        while self.path(&id).exists() {
            n = n.saturating_add(1);
            id = format!("{stamp}-{n}");
        }
        let run = Run {
            id,
            worktree: session.worktree.clone(),
            brief,
            created: now,
            status,
            attempts: Vec::new(),
            outage: None,
        };
        self.save(&run)?;
        Ok(run)
    }
}

/// Whether `name` is a plain file name: no separators and not `.` or `..`.
fn plain_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// `brief` as a path relative to `worktree`, which must hold it as a file. The VM reads it; the
/// host only names it in the prompt.
fn brief_in(worktree: &Path, brief: &str) -> anyhow::Result<String> {
    let path = Path::new(brief);
    let relative = if path.is_absolute() {
        path.strip_prefix(worktree)
            .with_context(|| format!("the brief {brief} is not in {}", worktree.display()))?
    } else {
        path
    };
    ensure!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            && relative.components().next().is_some(),
        "the brief {brief} must be a plain path inside the worktree"
    );
    ensure!(
        worktree.join(relative).is_file(),
        "the brief {brief} is not a file in {}",
        worktree.display()
    );
    relative
        .to_str()
        .map(str::to_owned)
        .with_context(|| format!("{brief} is not UTF-8"))
}

/// The run that is active, if any, among `runs`.
fn active(runs: &[Run]) -> Option<&Run> {
    runs.iter().find(|run| run.status.active())
}

/// `vm run`: starts a run of `brief` in `session` now, refusing while another run is active.
pub(crate) fn start(
    runs: &Runs,
    services: &dyn Services,
    session: &Session,
    brief: &str,
    now: i64,
) -> anyhow::Result<Run> {
    let _lock = runs.lock()?;
    if let Some(other) = active(&runs.list()?) {
        bail!(
            "run {} is active, and runs share one usage window; queue this brief with `cargo \
             xtask vm queue add`, or cancel that run with `cargo xtask vm cancel {}`",
            other.id,
            other.id
        );
    }
    let run = runs.create(
        session,
        brief,
        now,
        Status::Waiting {
            until: now,
            cause: Cause::Start,
        },
    )?;
    activate(runs, services, run)
}

/// Installs `run`'s service, or records why it could not be.
fn activate(runs: &Runs, services: &dyn Services, mut run: Run) -> anyhow::Result<Run> {
    if let Err(error) = services.install(&run.id) {
        run.status = Status::GaveUp {
            reason: format!("its service could not be installed: {error:#}"),
        };
        runs.save(&run)?;
        return Err(error.context(format!("run {} did not start", run.id)));
    }
    Ok(run)
}

/// `vm queue add`: queues a run of `brief` in `session`, which starts at once when no run is
/// active.
pub(crate) fn enqueue(
    runs: &Runs,
    services: &dyn Services,
    session: &Session,
    brief: &str,
    now: i64,
) -> anyhow::Result<Run> {
    let run = {
        let _lock = runs.lock()?;
        runs.create(session, brief, now, Status::Queued)?
    };
    Ok(start_next(runs, services, now)?
        .filter(|started| started.id == run.id)
        .unwrap_or(run))
}

/// Starts the oldest queued run when no run is active, and returns it.
pub(crate) fn start_next(
    runs: &Runs,
    services: &dyn Services,
    now: i64,
) -> anyhow::Result<Option<Run>> {
    let _lock = runs.lock()?;
    let all = runs.list()?;
    if active(&all).is_some() {
        return Ok(None);
    }
    let Some(mut next) = all.into_iter().find(|run| run.status == Status::Queued) else {
        return Ok(None);
    };
    next.status = Status::Waiting {
        until: now,
        cause: Cause::Start,
    };
    runs.save(&next)?;
    activate(runs, services, next).map(Some)
}

/// `vm cancel`: stops run `id`'s supervisor and removes its service, stops `claude` in its VM
/// when one runs, marks it cancelled and starts the next queued run.
pub(crate) fn cancel(
    runs: &Runs,
    services: &dyn Services,
    backend: &dyn Backend,
    host: &Host,
    id: &str,
    now: i64,
) -> anyhow::Result<String> {
    let mut run = runs.load(id)?;
    {
        let _lock = runs.lock()?;
        ensure!(
            run.status.active() || run.status == Status::Queued,
            "run {id} has ended: {}",
            run.status.describe()
        );
        services.remove(id)?;
        // The supervisor is gone; the state is the one it left.
        run = runs.load(id)?;
        if run.status == Status::Running
            && let Some(attempt) = run.attempts.last_mut()
        {
            attempt.ended = Some(now);
            attempt.outcome = Some(Outcome::Interrupted);
        }
        run.status = Status::Cancelled;
        runs.save(&run)?;
    }
    let session = Session::open(&run.worktree, &host.repo, &host.state)?;
    if backend.is_running(&session.name)? {
        stop_claude(backend, &session.name)?;
    }
    let next = start_next(runs, services, now)?;
    Ok(match next {
        Some(next) => format!("cancelled {id}; started {}", next.id),
        None => format!("cancelled {id}"),
    })
}

/// `vm runs`: every run, oldest first, as a table or as JSON.
pub(crate) fn list(runs: &Runs, json: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let all = runs.list()?;
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&all)?)?;
        return Ok(());
    }
    if all.is_empty() {
        writeln!(out, "vm runs: none")?;
    }
    for run in &all {
        writeln!(
            out,
            "{}\t{}\t{} attempts\t{}\t{}",
            run.id,
            run.status.describe(),
            run.attempts.len(),
            run.worktree.join(&run.brief).display(),
            runs.log(&run.id).display()
        )?;
    }
    Ok(())
}

/// `vm queue`: the queued runs, in the order they start.
pub(crate) fn queued(runs: &Runs, out: &mut dyn Write) -> anyhow::Result<()> {
    let all = runs.list()?;
    if let Some(run) = active(&all) {
        writeln!(out, "active\t{}\t{}", run.id, run.status.describe())?;
    }
    let mut any = false;
    for (place, run) in all
        .iter()
        .filter(|run| run.status == Status::Queued)
        .enumerate()
    {
        any = true;
        writeln!(
            out,
            "{}\t{}\t{}",
            place.saturating_add(1),
            run.id,
            run.worktree.join(&run.brief).display()
        )?;
    }
    if !any {
        writeln!(out, "vm queue: nothing queued")?;
    }
    Ok(())
}

/// The supervisor's clock.
pub(crate) trait Clock {
    /// Seconds since the Unix epoch.
    fn now(&self) -> i64;
    /// Returns at `at` or later.
    fn sleep_until(&self, at: i64);
}

/// The system clock.
#[derive(Debug)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        crate::progress::now()
    }

    fn sleep_until(&self, at: i64) {
        // A sleep of at most a minute at a time, so a clock change or a suspend is noticed.
        loop {
            let left = at.saturating_sub(self.now());
            if left <= 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(
                u64::try_from(left.min(60)).unwrap_or(60),
            ));
        }
    }
}

/// The URL the network probe reaches from inside the VM.
const PROBE_URL: &str = "https://api.anthropic.com/";

/// Whether the API answers from inside VM `name`: any HTTP answer at all, as the agent, who gets
/// no token for anything but `claude`.
fn probe(backend: &dyn Backend, name: &str) -> bool {
    let command = super::words(&[
        "agent-exec",
        "curl",
        "--silent",
        "--output",
        "/dev/null",
        "--max-time",
        "20",
        PROBE_URL,
    ]);
    backend
        .exec(name, &command, b"")
        .is_ok_and(|output| output.status.success())
}

/// Kills every Claude Code process of the agent in VM `name`, such as one a killed supervisor
/// left running, so two sessions never work on the worktree at once.
fn stop_claude(backend: &dyn Backend, name: &str) -> anyhow::Result<()> {
    let command = super::words(&[
        "pkill",
        "-KILL",
        "-u",
        "agent",
        "-f",
        "@anthropic-ai/claude-code",
    ]);
    let output = backend.exec(name, &command, b"")?;
    // pkill exits 1 when no process matched.
    ensure!(
        matches!(output.status.code(), Some(0 | 1)),
        "could not stop claude in {name}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// The command of an attempt: a fresh, headless Claude Code session as the agent, whose
/// stream-json output the supervisor reads. Permission prompts would wait forever with no one to
/// answer them, so it bypasses them; the VM is the sandbox (D37).
fn command(prompt: &str) -> Vec<std::ffi::OsString> {
    super::words(&[
        "agent-exec",
        "claude",
        "-p",
        prompt,
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "bypassPermissions",
    ])
}

/// What the supervisor works with.
pub(crate) struct Supervisor<'a> {
    pub(crate) runs: &'a Runs,
    pub(crate) host: &'a Host,
    pub(crate) backend: &'a dyn Backend,
    pub(crate) services: &'a dyn Services,
    pub(crate) clock: &'a dyn Clock,
}

impl std::fmt::Debug for Supervisor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("runs", &self.runs)
            .finish_non_exhaustive()
    }
}

impl Supervisor<'_> {
    /// `vm supervise`: carries run `id` on to a final state, then retires its service and starts
    /// the next queued run. Returns the final status.
    pub(crate) fn supervise(&self, id: &str) -> anyhow::Result<Status> {
        let mut run = self.runs.load(id)?;
        let session = match Session::open(&run.worktree, &self.host.repo, &self.host.state) {
            Ok(session) => session,
            Err(error) => {
                // Its worktree is gone or no longer a worktree of the main clone: nothing a
                // restart of the supervisor would change, so the run ends.
                run.status = Status::GaveUp {
                    reason: format!("its session cannot be opened: {error:#}"),
                };
                self.runs.save(&run)?;
                self.services.retire(id)?;
                start_next(self.runs, self.services, self.clock.now())?;
                return Ok(run.status);
            }
        };
        let mut log = File::options()
            .create(true)
            .append(true)
            .open(self.runs.log(id))
            .with_context(|| format!("could not open {}", self.runs.log(id).display()))?;
        loop {
            match run.status.clone() {
                Status::Queued => {
                    run.status = Status::Waiting {
                        until: self.clock.now(),
                        cause: Cause::Start,
                    };
                }
                Status::Running => {
                    // This supervisor's predecessor stopped mid-attempt: killed, or the host
                    // rebooted. The attempt it ran is over.
                    writeln!(
                        log,
                        "--- abc run {id}: the supervisor restarted mid-attempt"
                    )?;
                    run.end_attempt(Outcome::Interrupted, self.clock.now());
                }
                Status::Waiting { until, cause } => {
                    self.clock.sleep_until(until);
                    // The probe runs in the VM; a VM that is gone, as after a reboot, is started
                    // by the attempt instead.
                    if cause == Cause::Network
                        && self.backend.is_running(&session.name)?
                        && !probe(self.backend, &session.name)
                    {
                        let outage = run.outage.unwrap_or(Outage {
                            since: until,
                            tries: 0,
                        });
                        let now = self.clock.now();
                        writeln!(
                            log,
                            "--- abc run {id}: the API is still unreachable at {}",
                            crate::progress::utc(now)
                        )?;
                        run.follow(outcome::down(outage, now, Cause::Network));
                    } else {
                        self.attempt(&mut run, &session, &mut log)?;
                    }
                }
                Status::Done { .. } | Status::GaveUp { .. } | Status::Cancelled => {
                    writeln!(log, "--- abc run {id}: {}", run.status.describe())?;
                    self.services.retire(id)?;
                    start_next(self.runs, self.services, self.clock.now())?;
                    return Ok(run.status);
                }
            }
            self.runs.save(&run)?;
        }
    }

    /// Runs one attempt of `run` in `session`, writing its output to `log`, and records how it
    /// ended and what follows.
    fn attempt(&self, run: &mut Run, session: &Session, log: &mut File) -> anyhow::Result<()> {
        let prompt = run.prompt();
        run.attempts.push(Attempt {
            started: self.clock.now(),
            ended: None,
            sessions: Vec::new(),
            outcome: None,
        });
        run.status = Status::Running;
        self.runs.save(run)?;
        writeln!(
            log,
            "--- abc run {}: attempt {} at {}",
            run.id,
            run.attempts.len(),
            crate::progress::utc(self.clock.now())
        )?;
        let outcome = if let Err(error) = self.prepare(session, log) {
            Outcome::Unstarted {
                message: format!("{error:#}"),
            }
        } else {
            let runs = self.runs;
            let mut seen = |session_id: &str| {
                if let Some(attempt) = run.attempts.last_mut()
                    && !attempt.sessions.iter().any(|known| known == session_id)
                {
                    attempt.sessions.push(session_id.to_owned());
                    // Kept at once, so a reboot mid-attempt still names the session.
                    runs.save(run)
                } else {
                    Ok(())
                }
            };
            let mut stream = Stream::new(log, &mut seen);
            let streamed = self
                .backend
                .stream(&session.name, &command(&prompt), &mut stream);
            let result = stream.finish()?;
            match streamed {
                Ok(_) => outcome::classify(result.as_ref(), self.clock.now()),
                Err(error) => Outcome::Error {
                    message: format!("could not run claude in the VM: {error:#}"),
                },
            }
        };
        writeln!(
            log,
            "--- abc run {}: attempt {} ended with {}",
            run.id,
            run.attempts.len(),
            outcome.describe()
        )?;
        run.end_attempt(outcome, self.clock.now());
        Ok(())
    }

    /// Readies `session` for an attempt: starts its VM when it is not running, handing it the
    /// subscription token as `vm start` does, or else makes sure its token broker listens; and
    /// stops any `claude` left running in it.
    fn prepare(&self, session: &Session, log: &mut File) -> anyhow::Result<()> {
        if self.backend.is_running(&session.name)? {
            // The broker may have stopped with an earlier supervisor while the VM ran on.
            super::ensure_broker(
                self.host,
                &session.name,
                &crate::broker::files(&self.host.home, &session.name),
            )?;
        } else {
            super::start(session, self.host, self.backend, log)?;
        }
        stop_claude(self.backend, &session.name)
    }
}

/// A writer that passes an attempt's output on to the log, and reads each stream-json line as it
/// goes: the session ids it names, and the last `result`.
struct Stream<'a> {
    log: &'a mut dyn Write,
    seen: &'a mut dyn FnMut(&str) -> anyhow::Result<()>,
    line: Vec<u8>,
    result: Option<serde_json::Value>,
    /// A failure to keep a session id, reported when the stream ends.
    failed: Option<anyhow::Error>,
}

impl<'a> Stream<'a> {
    fn new(log: &'a mut dyn Write, seen: &'a mut dyn FnMut(&str) -> anyhow::Result<()>) -> Self {
        Self {
            log,
            seen,
            line: Vec::new(),
            result: None,
            failed: None,
        }
    }

    fn take_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
            return;
        };
        if let Some(id) = value.get("session_id").and_then(serde_json::Value::as_str)
            && self.failed.is_none()
            && let Err(error) = (self.seen)(id)
        {
            self.failed = Some(error);
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("result") {
            self.result = Some(value);
        }
    }

    /// The last `result` the stream printed.
    fn finish(mut self) -> anyhow::Result<Option<serde_json::Value>> {
        if !self.line.is_empty() {
            self.take_line();
        }
        match self.failed {
            Some(error) => Err(error),
            None => Ok(self.result),
        }
    }
}

impl Write for Stream<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.log.write(bytes)?;
        for byte in bytes.iter().take(written) {
            if *byte == b'\n' {
                self.take_line();
            } else {
                self.line.push(*byte);
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.log.flush()
    }
}

#[cfg(test)]
mod tests;
