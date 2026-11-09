//! The agent App's token broker, `cargo xtask broker` (D3).
//!
//! Agents push and use gh as the getabc-dev-agent App, but must never read the App's private key,
//! which lasts until it is revoked. The broker is the one process that reads it. It runs on the
//! host from a build of `main` installed outside any checkout (an agent can change the xtask of a
//! checkout it works in; the check is a guard against mistakes, not a control), one broker per
//! agent VM: `cargo xtask vm start` starts it with the session's name, and it listens on that
//! session's sockets under [`DIR`]. The token socket is forwarded into the VM over vsock, where
//! [`ENV`] names the guest end, and `cargo xtask git-credential` and `cargo xtask gh` ask it for a
//! token per use. The control socket stays on the host: `cargo xtask vm stop` asks the broker
//! there to revoke every token it minted and to exit ([`shut_down`]). A broker that does not
//! answer gets SIGTERM through the pid file it writes beside its sockets. SIGTERM, SIGINT and
//! SIGHUP stop it directly, as `stop` does, through no socket: it hangs up on the clients it is
//! serving, revokes its tokens and removes its sockets and pid file. Each client has
//! [`PATIENCE`] in all to send its request and take its answer, so no client holds up a stop.
//! Each line the broker logs names its session, and it mints at most [`MAX_PER_HOUR`] tokens an
//! hour.
//!
//! The protocol is one line each way: the client sends a scope's name (`git` or `gh`), and the
//! broker answers `token <token>`, a fresh installation token for that scope alone that expires
//! within the hour, or `error <reason>`, and closes the connection. The key never leaves the
//! broker and no token is ever logged.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, anyhow, bail};

use crate::app::{Revoked, Scope};

/// The brokers' sockets, relative to the home directory.
pub(crate) const DIR: &str = ".config/abc/agent-broker";

/// The most tokens one session's broker mints in an hour, the life of a token.
pub(crate) const MAX_PER_HOUR: usize = 120;

/// How long `cargo xtask vm stop` waits for the broker to answer `stop`, revoking its tokens, and
/// then for a broker that did not answer to exit after SIGTERM, before it kills it.
pub(crate) const STOP_PATIENCE: Duration = Duration::from_secs(60);

/// The variable naming the broker's socket where a client runs: inside an agent VM, the guest end
/// of the vsock forward.
pub(crate) const ENV: &str = "ABC_AGENT_BROKER";

/// How long a client of the broker has, in all, to send its one-line request and take its answer,
/// however slowly it sends; and how long a client waits for the broker's answer.
const PATIENCE: Duration = Duration::from_secs(5);

/// How often the broker's listeners look for a client and for a stop.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// The signals that stop a broker as `stop` does.
const STOP_SIGNALS: [i32; 3] = [
    signal_hook::consts::SIGTERM,
    signal_hook::consts::SIGINT,
    signal_hook::consts::SIGHUP,
];

/// How many clients the broker serves at once; more are refused until one finishes, so clients
/// that hang can hold up at most this many connections, each for at most [`PATIENCE`].
const MAX_CLIENTS: usize = 8;

/// The longest request line a client can send; longer ones are refused.
const MAX_REQUEST: u64 = 64;

/// How many reads of a kilobyte the broker makes to drop what is left of a refused request.
const MAX_DRAIN_READS: usize = 64;

/// One session's broker files: its sockets and its pid file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Files {
    /// Where the session's VM asks for tokens, forwarded into it.
    pub(crate) tokens: PathBuf,
    /// Where the host stops the broker; never forwarded.
    pub(crate) control: PathBuf,
    /// The broker's process id, while it serves, for a stop it does not answer.
    pub(crate) pid: PathBuf,
}

impl Files {
    fn all(&self) -> [&Path; 3] {
        [&self.tokens, &self.control, &self.pid]
    }
}

/// The files of session `session`'s broker under the home directory `home`.
pub(crate) fn files(home: &Path, session: &str) -> Files {
    let dir = home.join(DIR);
    Files {
        tokens: dir.join(format!("{session}.sock")),
        control: dir.join(format!("{session}.ctl")),
        pid: dir.join(format!("{session}.pid")),
    }
}

/// Fails unless `session` is a session's name, fit for a socket's file name.
pub(crate) fn check_session(session: &str) -> anyhow::Result<()> {
    let rest = session.strip_prefix("abc-").unwrap_or_default();
    anyhow::ensure!(
        !rest.is_empty()
            && session.len() <= 64
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "{session:?} is not a session's name (abc- and lowercase letters, digits or dashes)"
    );
    Ok(())
}

/// A token for `scope` from the broker whose socket [`ENV`] names.
pub(crate) fn token(scope: Scope) -> anyhow::Result<String> {
    token_from(|name| std::env::var_os(name), scope)
}

/// A token for `scope` from the broker whose socket [`ENV`] names in the environment `env`.
pub(crate) fn token_from(
    env: impl Fn(&str) -> Option<OsString>,
    scope: Scope,
) -> anyhow::Result<String> {
    let socket = env(ENV).filter(|socket| !socket.is_empty()).with_context(|| {
        format!(
            "{ENV} is not set: agent App tokens come from the token broker, which only agent VMs \
             reach (see `cargo xtask vm --help`)"
        )
    })?;
    request(Path::new(&socket), scope)
}

/// A token for `scope` from the broker listening at `socket`.
pub(crate) fn request(socket: &Path, scope: Scope) -> anyhow::Result<String> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "could not reach the agent App's token broker at {}; the maintainer starts it on the \
             host (see `cargo xtask broker --help`)",
            socket.display()
        )
    })?;
    stream
        .set_read_timeout(Some(PATIENCE))
        .context("could not set a timeout on the broker's socket")?;
    exchange(stream, scope, std::thread::sleep)
}

/// How long a client waits before reading the broker's answer again when the socket says it would
/// block (`EAGAIN`), once per delay. A push once failed on one such read, and its retry worked.
const READ_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(200),
    Duration::from_millis(500),
];

/// Asks the broker on `stream` for a token for `scope`, reading its answer again after `wait`ing
/// whenever a read would block.
fn exchange(
    mut stream: impl Read + Write,
    scope: Scope,
    mut wait: impl FnMut(Duration),
) -> anyhow::Result<String> {
    stream
        .write_all(format!("{}\n", scope.name()).as_bytes())
        .context("could not send the broker a request")?;
    let mut reader = BufReader::new(stream);
    let mut answer = Vec::new();
    let mut delays = READ_RETRY_DELAYS.iter();
    loop {
        match reader.read_until(b'\n', &mut answer) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => match delays.next() {
                Some(&delay) => wait(delay),
                None => return Err(error).context("could not read the broker's answer"),
            },
            Err(error) => return Err(error).context("could not read the broker's answer"),
        }
    }
    let answer = String::from_utf8_lossy(&answer);
    let answer = answer.trim_end_matches('\n');
    match answer.split_once(' ') {
        Some(("token", token)) if !token.is_empty() => Ok(token.to_owned()),
        Some(("error", reason)) => bail!("the agent App's token broker refused: {reason}"),
        _ => bail!("the agent App's token broker gave no token"),
    }
}

/// How long an installation token lives: GitHub expires it an hour after it is minted.
const TOKEN_LIFE: Duration = Duration::from_secs(3600);

/// How many revocations a stopping broker sends at once.
const REVOKERS: usize = 8;

/// A token a broker minted, and when.
#[derive(Debug)]
struct Minted {
    token: String,
    at: Instant,
    /// When it expires, for the user.
    expires: SystemTime,
}

/// The tokens a broker minted: when, to keep to its hourly limit, and which, to revoke them.
#[derive(Debug, Default)]
struct Ledger {
    recent: VecDeque<Instant>,
    minted: Vec<Minted>,
}

impl Ledger {
    /// Admits one more token at `now` unless `limit` were minted in the hour before it.
    fn admit(&mut self, now: Instant, limit: usize) -> anyhow::Result<()> {
        while self
            .recent
            .front()
            .is_some_and(|&then| now.duration_since(then) >= Duration::from_secs(3600))
        {
            self.recent.pop_front();
        }
        anyhow::ensure!(
            self.recent.len() < limit,
            "{limit} tokens were minted in the last hour, the most a session gets"
        );
        self.recent.push_back(now);
        Ok(())
    }
}

/// What stops a broker.
enum Stop {
    /// A client of the control socket said `stop`; it is told what was revoked.
    Asked(UnixStream),
    /// The process got this signal.
    Signal(i32),
}

/// What a broker's threads share.
#[derive(Default)]
struct Shared {
    /// Set once the broker stops: from then on it accepts no client and mints nothing.
    stopping: AtomicBool,
    ledger: Mutex<Ledger>,
    /// The clients being served, by number, to hang up on when the broker stops.
    clients: Mutex<BTreeMap<u64, UnixStream>>,
    next: AtomicU64,
}

impl Shared {
    fn ledger(&self) -> anyhow::Result<MutexGuard<'_, Ledger>> {
        self.ledger
            .lock()
            .map_err(|poisoned| anyhow!("the broker's ledger is poisoned: {poisoned}"))
    }

    /// Records `client` as being served, to hang up on if the broker stops meanwhile; returns its
    /// number for [`Shared::leave`]. A client that comes as the broker stops is hung up on at once.
    fn enter(&self, client: &UnixStream) -> Option<u64> {
        let copy = client.try_clone().ok()?;
        let number = self.next.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut clients) = self.clients.lock() {
            clients.insert(number, copy);
        }
        // `stop` sets the flag before it hangs up, so a client it missed sees the flag here.
        if self.stopping.load(Ordering::SeqCst) {
            let _hung_up = client.shutdown(std::net::Shutdown::Both);
        }
        Some(number)
    }

    /// Forgets client `number`, which is done.
    fn leave(&self, number: u64) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.remove(&number);
        }
    }

    /// Stops the broker: no more clients or tokens, and every client being served is hung up on,
    /// which ends its reads and writes at once.
    fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Ok(mut clients) = self.clients.lock() {
            for (_, client) in std::mem::take(&mut *clients) {
                // A client that already went is fine.
                let _hung_up = client.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// Serves session `session`'s tokens from `mint`, at most `limit` an hour, on the sockets of
/// `files` until a client of the control socket says `stop` or the process gets one of
/// [`STOP_SIGNALS`]; then hangs up on the clients it is serving, revokes every token it minted
/// with `revoke`, removes its files, tells the client that asked how many it revoked, and
/// returns. While it serves, its pid file names this process. Each request is noted in `log`.
pub(crate) fn serve(
    session: &str,
    files: &Files,
    mint: impl Fn(Scope) -> anyhow::Result<String> + Sync,
    revoke: impl Fn(&str) -> anyhow::Result<Revoked> + Sync,
    limit: usize,
    log: &mut (impl Write + Send),
) -> anyhow::Result<()> {
    // Before anything else, so that a stop signal from here on stops the broker as `stop` does.
    let mut signals = signal_hook::iterator::Signals::new(STOP_SIGNALS)
        .context("could not handle the stop signals")?;
    let signal_thread = signals.handle();
    let tokens = bind(&files.tokens)?;
    let control = bind(&files.control)?;
    write_pid(&files.pid)?;
    writeln!(
        log,
        "broker {session}: serving agent App tokens on {}",
        files.tokens.display()
    )?;
    let log = Mutex::new(log);
    let shared = Shared::default();
    let metered = |scope: Scope| -> anyhow::Result<String> {
        anyhow::ensure!(
            !shared.stopping.load(Ordering::SeqCst),
            "the broker is stopping"
        );
        shared.ledger()?.admit(Instant::now(), limit)?;
        // A token minted while the broker stops is still recorded, and so revoked: the broker
        // revokes only once every client's thread has ended.
        let token = mint(scope)?;
        shared.ledger()?.minted.push(Minted {
            token: token.clone(),
            at: Instant::now(),
            expires: SystemTime::now()
                .checked_add(TOKEN_LIFE)
                .unwrap_or(SystemTime::UNIX_EPOCH),
        });
        Ok(token)
    };
    let (sender, stops) = mpsc::channel();
    let stop = std::thread::scope(|scope| {
        let shared = &shared;
        let (asked, log, metered) = (sender.clone(), &log, &metered);
        scope.spawn(move || {
            let clients = accepting(&tokens, &shared.stopping);
            answer_all(session, clients, metered, log, MAX_CLIENTS, shared);
        });
        scope.spawn(move || {
            if let Some(client) = await_stop(accepting(&control, &shared.stopping)) {
                let _sent = asked.send(Stop::Asked(client));
            }
        });
        scope.spawn(move || {
            // Ends once the handle is closed below.
            for signal in signals.forever() {
                if sender.send(Stop::Signal(signal)).is_err() {
                    break;
                }
            }
        });
        // The signal thread holds a sender until the handle is closed, so a stop always comes.
        let stop = stops.recv();
        shared.stop();
        signal_thread.close();
        stop
    })
    .context("the broker's stop channel closed")?;
    let minted = std::mem::take(&mut shared.ledger()?.minted);
    let summary = revoke_all(&minted, &revoke, Instant::now()).to_string();
    let how = match &stop {
        Stop::Asked(_) => String::new(),
        Stop::Signal(signal) => format!(
            " on {}",
            signal_hook::low_level::signal_name(*signal).unwrap_or("a signal")
        ),
    };
    note_line(&log, &format!("broker {session}: stopped{how}; {summary}"));
    for file in files.all() {
        let _removed = fs::remove_file(file);
    }
    // The client may be gone; the tokens are revoked either way. It is told last, so that once it
    // reads the answer the broker's files are gone.
    if let Stop::Asked(mut asker) = stop {
        let _told = asker
            .set_write_timeout(Some(PATIENCE))
            .and_then(|()| writeln!(asker, "stopped {summary}"));
    }
    Ok(())
}

/// What a stopping broker's revocations came to.
#[derive(Debug, Default, PartialEq, Eq)]
struct Revocations {
    /// The tokens minted.
    minted: usize,
    /// Revoked now.
    revoked: usize,
    /// Already revoked by the command that used them, or expired between the check and the call.
    already: usize,
    /// Expired an hour after they were minted, so not sent.
    expired: usize,
    /// Could not be revoked: the latest of their expiries, and the first error.
    failed: Vec<(SystemTime, String)>,
}

impl std::fmt::Display for Revocations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "revoked {} of {} tokens", self.revoked, self.minted)?;
        if self.already > 0 {
            write!(
                f,
                "; {} already revoked by the command that used them",
                self.already
            )?;
        }
        if self.expired > 0 {
            write!(f, "; {} expired", self.expired)?;
        }
        if let Some(latest) = self.failed.iter().map(|(expires, _)| *expires).max() {
            let first = self
                .failed
                .first()
                .map(|(_, error)| error.as_str())
                .unwrap_or_default();
            write!(
                f,
                "; WARNING: {} could not be revoked and stay valid until {} ({first})",
                self.failed.len(),
                clock(latest)
            )?;
        }
        Ok(())
    }
}

/// `when` as an hour and minute in UTC, rounded up, so a token is never said to expire before it
/// does.
fn clock(when: SystemTime) -> String {
    let seconds = when
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
        .saturating_add(59);
    let minutes = seconds / 60;
    format!("{:02}:{:02} UTC", (minutes / 60) % 24, minutes % 60)
}

/// Revokes every token of `minted` that has not expired by `now` with `revoke`, a few at a time.
fn revoke_all(
    minted: &[Minted],
    revoke: &(impl Fn(&str) -> anyhow::Result<Revoked> + Sync),
    now: Instant,
) -> Revocations {
    let (live, expired): (Vec<&Minted>, Vec<&Minted>) = minted
        .iter()
        .partition(|token| now.saturating_duration_since(token.at) < TOKEN_LIFE);
    let size = live.len().div_ceil(REVOKERS).max(1);
    let outcomes: Vec<(&Minted, anyhow::Result<Revoked>)> = std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every worker starts before the first is joined"
        )]
        let workers: Vec<_> = live
            .chunks(size)
            .map(|chunk| {
                let worker = scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|token| (*token, revoke(&token.token)))
                        .collect::<Vec<_>>()
                });
                (chunk, worker)
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|(chunk, worker)| {
                worker.join().unwrap_or_else(|_| {
                    chunk
                        .iter()
                        .map(|token| (*token, Err(anyhow!("the revocation panicked"))))
                        .collect()
                })
            })
            .collect()
    });
    let mut done = Revocations {
        minted: minted.len(),
        expired: expired.len(),
        ..Revocations::default()
    };
    for (token, outcome) in outcomes {
        match outcome {
            Ok(Revoked::Now) => done.revoked = done.revoked.saturating_add(1),
            Ok(Revoked::Already) => done.already = done.already.saturating_add(1),
            Err(error) => done.failed.push((token.expires, format!("{error:#}"))),
        }
    }
    done
}

/// The clients `listener` accepts until `stopping` is set.
fn accepting<'a>(
    listener: &'a UnixListener,
    stopping: &'a AtomicBool,
) -> impl Iterator<Item = std::io::Result<UnixStream>> + 'a {
    accepted(
        || {
            listener.accept().and_then(|(client, _)| {
                // On macOS an accepted socket inherits the listener's non-blocking mode.
                client.set_nonblocking(false).map(|()| client)
            })
        },
        stopping,
    )
}

/// The clients `accept` gives until `stopping` is set, which it looks at every [`ACCEPT_POLL`]:
/// the listener does not block, so a stop never waits on a client that does not come. "Would
/// block" means no client yet; any other error is passed on, after the same pause, so one that
/// repeats, such as too many open files, does not spin.
fn accepted<'a>(
    mut accept: impl FnMut() -> std::io::Result<UnixStream> + 'a,
    stopping: &'a AtomicBool,
) -> impl Iterator<Item = std::io::Result<UnixStream>> + 'a {
    std::iter::from_fn(move || {
        loop {
            if stopping.load(Ordering::SeqCst) {
                return None;
            }
            match accept() {
                Ok(client) => return Some(Ok(client)),
                Err(error) => {
                    std::thread::sleep(ACCEPT_POLL);
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        return Some(Err(error));
                    }
                }
            }
        }
    })
}

/// A stream whose every read and write waits at most a given time; a wait that runs out is the
/// operating system's "would block".
trait Patient {
    fn read_within(&mut self, buf: &mut [u8], patience: Duration) -> std::io::Result<usize>;
    fn write_within(&mut self, buf: &[u8], patience: Duration) -> std::io::Result<usize>;
}

impl Patient for &UnixStream {
    fn read_within(&mut self, buf: &mut [u8], patience: Duration) -> std::io::Result<usize> {
        self.set_read_timeout(Some(patience))?;
        self.read(buf)
    }

    fn write_within(&mut self, buf: &[u8], patience: Duration) -> std::io::Result<usize> {
        self.set_write_timeout(Some(patience))?;
        self.write(buf)
    }
}

/// A client's stream, read and written within one deadline for the whole exchange: each read and
/// write may take only what is left of it, so a client that sends a byte at a time gets no more.
/// Once the deadline has come, whether between reads or during one, every read and write is
/// [`timed_out`], never an end of input.
struct Timed<S, C = fn() -> Instant> {
    stream: S,
    deadline: Instant,
    /// The clock the deadline is read on.
    now: C,
}

impl<'a> Timed<&'a UnixStream> {
    /// `stream`, with [`PATIENCE`] from now.
    fn new(stream: &'a UnixStream) -> Self {
        Self {
            stream,
            // An instant past what the clock can show cannot come; then the time is up at once.
            deadline: Instant::now()
                .checked_add(PATIENCE)
                .unwrap_or_else(Instant::now),
            now: Instant::now,
        }
    }
}

impl<S, C: Fn() -> Instant> Timed<S, C> {
    /// What is left of the deadline; an error once it has passed.
    fn left(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since((self.now)())
            .filter(|left| !left.is_zero())
            .ok_or_else(timed_out)
    }
}

/// The error of a client whose time is up.
fn timed_out() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("the client took more than {} s", PATIENCE.as_secs()),
    )
}

/// `result` of one read or write given what was left of the deadline: the stream's own timeout,
/// which Linux and macOS report as "would block", means the deadline has come.
fn within_deadline(result: std::io::Result<usize>) -> std::io::Result<usize> {
    result.map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            timed_out()
        } else {
            error
        }
    })
}

impl<S: Patient, C: Fn() -> Instant> Read for Timed<S, C> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        within_deadline(self.stream.read_within(buf, left))
    }
}

impl<S: Patient, C: Fn() -> Instant> Write for Timed<S, C> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        within_deadline(self.stream.write_within(buf, left))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Writes this process's id to `path`, readable only by its owner.
fn write_pid(path: &Path) -> anyhow::Result<()> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| writeln!(file, "{}", std::process::id()))
        .with_context(|| format!("could not write {}", path.display()))
}

/// Waits among `clients` of the control socket for one that says `stop`, refusing anything else,
/// and returns it; none once there are no more.
fn await_stop(clients: impl Iterator<Item = std::io::Result<UnixStream>>) -> Option<UnixStream> {
    for client in clients {
        let Ok(mut client) = client else { continue };
        let mut line = String::new();
        let read = BufReader::new(Timed::new(&client).take(MAX_REQUEST)).read_line(&mut line);
        if read.is_ok() && line == "stop\n" {
            return Some(client);
        }
        refuse(&mut client, "the control socket understands only `stop`");
    }
    None
}

/// Stops the broker whose control socket is `control`, which revokes its tokens, waiting
/// `patience` for its answer; returns what it says it revoked.
fn stop_at(control: &Path, patience: Duration) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(control)
        .with_context(|| format!("no broker listens on {}", control.display()))?;
    stream.set_read_timeout(Some(patience))?;
    stream.write_all(b"stop\n")?;
    let mut answer = String::new();
    // A read that times out fails as one that would block (EAGAIN).
    BufReader::new(stream)
        .read_line(&mut answer)
        .with_context(|| {
            format!(
                "the broker did not answer `stop` within {} s",
                patience.as_secs()
            )
        })?;
    answer
        .trim_end()
        .strip_prefix("stopped ")
        .map(str::to_owned)
        .with_context(|| format!("the broker did not stop: {answer:?}"))
}

/// Stops session `session`'s broker, whose files are `files`, so that none is left running: asked
/// on its control socket first, which revokes its tokens; when it does not answer within
/// `patience`, SIGTERM to the pid its pid file names, once `ps` shows that process is still that
/// session's broker, which stops it the same way; when it is still that broker `patience` later,
/// SIGKILL, and its tokens expire within the hour. Its files are removed only once it is gone.
/// Returns what happened, for the user; fails, naming the pid, when the broker may still run.
pub(crate) fn shut_down(
    files: &Files,
    session: &str,
    patience: Duration,
) -> anyhow::Result<String> {
    let mut said = Vec::new();
    // A broker that answers has removed its files by then.
    let answered = files.control.exists()
        && match stop_at(&files.control, patience) {
            Ok(summary) => {
                said.push(format!("stopped the token broker; {summary}"));
                true
            }
            Err(error) => {
                said.push(format!("{error:#}"));
                false
            }
        };
    if !answered {
        match read_pid(&files.pid) {
            Some(pid) => match signal_broker(pid, session, patience) {
                Ok(done) => said.push(done),
                Err(error) => {
                    said.push(format!("{error:#}"));
                    bail!(
                        "{}; its files in {} are left as they are",
                        said.join("; "),
                        files.pid.parent().unwrap_or(&files.pid).display()
                    );
                }
            },
            None => anyhow::ensure!(
                !listening(files),
                "{}; a token broker listens on {} and left no pid file, so it may still be \
                 running: find it with `pgrep -f 'broker --session {session}'`, stop it, and run \
                 this again",
                said.join("; "),
                files.tokens.parent().unwrap_or(&files.tokens).display()
            ),
        }
    }
    let mut removed = Vec::new();
    for file in files.all() {
        if fs::remove_file(file).is_ok() {
            removed.push(
                file.file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            );
        }
    }
    if !removed.is_empty() {
        said.push(format!("removed its leftover {}", removed.join(", ")));
    }
    if said.is_empty() {
        said.push(String::from("no token broker was running"));
    }
    Ok(said.join("; "))
}

/// Whether a process accepts connections on either of the sockets of `files`.
fn listening(files: &Files) -> bool {
    [&files.tokens, &files.control]
        .into_iter()
        .any(|socket| UnixStream::connect(socket).is_ok())
}

/// The id of a process that may be signalled: a positive `pid_t` above 1. `kill` reads 0 as the
/// caller's own process group, -1 as every process the user may signal and any other negative
/// number as a whole process group, and 1 is init, never a broker; none of them can be made into
/// one, so no pid file, however written, makes [`signal`] reach past one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pid(i32);

impl std::str::FromStr for Pid {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let pid: i32 = text
            .parse()
            .with_context(|| format!("{text:?} is not a process id"))?;
        anyhow::ensure!(
            pid > 1,
            "{pid} names no single process that may be signalled"
        );
        Ok(Self(pid))
    }
}

impl std::fmt::Display for Pid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The process id in the pid file at `path`, if it holds one that may be signalled.
fn read_pid(path: &Path) -> Option<Pid> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Sends SIGTERM to `pid` once `ps` shows it is session `session`'s broker, then SIGKILL if it is
/// still that broker `patience` later, and waits `patience` again for it to go; says what it did.
/// Fails, naming the pid, when it cannot be signalled or is still that broker after SIGKILL.
fn signal_broker(pid: Pid, session: &str, patience: Duration) -> anyhow::Result<String> {
    if !is_broker(pid, session) {
        return Ok(format!(
            "process {pid} is no longer the token broker of {session}; left alone"
        ));
    }
    anyhow::ensure!(
        signal(pid, "TERM"),
        "could not send SIGTERM to the token broker (pid {pid}), which may still be running"
    );
    if within(patience, || !is_broker(pid, session)) {
        return Ok(format!("stopped the token broker (pid {pid}) with SIGTERM"));
    }
    // `within` looked last just now: the process is still the broker, not a program that took its
    // pid or that it became.
    anyhow::ensure!(
        signal(pid, "KILL"),
        "could not kill the token broker (pid {pid}), which outlived SIGTERM and may still be \
         running"
    );
    anyhow::ensure!(
        within(patience, || !is_broker(pid, session)),
        "the token broker (pid {pid}) still runs {} s after SIGKILL",
        patience.as_secs()
    );
    Ok(format!(
        "killed the token broker (pid {pid}), which outlived SIGTERM by {} s; its tokens were not \
         revoked and expire within the hour",
        patience.as_secs()
    ))
}

/// Whether `done` holds within `patience`, asked at once and then every [`POLL`].
fn within(patience: Duration, mut done: impl FnMut() -> bool) -> bool {
    let tries = patience
        .as_millis()
        .checked_div(POLL.as_millis())
        .unwrap_or_default();
    for _ in 0..tries {
        if done() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    done()
}

/// How often [`within`] asks: each ask runs one `ps`.
const POLL: Duration = Duration::from_millis(200);

/// Whether process `pid` is running `broker --session <session>`, by one `ps` showing its state
/// and command line: a process that is gone, a zombie (exited, not yet reaped) or one running
/// anything else is not.
fn is_broker(pid: Pid, session: &str) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-o", "command=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let shown = String::from_utf8_lossy(&output.stdout);
    let mut words = shown.split_whitespace();
    let running = words.next().is_some_and(|state| !state.starts_with('Z'));
    let words: Vec<&str> = words.collect();
    output.status.success()
        && running
        && words
            .windows(3)
            .any(|window| window == ["broker", "--session", session])
}

/// Sends `signal` to process `pid` alone with `kill`; whether it was delivered.
fn signal(pid: Pid, signal: &str) -> bool {
    Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Answers each of `clients` with a token from `mint`, each on its own thread and at most `max` at
/// once, noting each in `log`, and each recorded in `shared` while it is served, so a stop can
/// hang up on it. A client that fails or comes while `max` are being served is noted and refused.
fn answer_all(
    session: &str,
    clients: impl Iterator<Item = std::io::Result<UnixStream>>,
    mint: &(impl Fn(Scope) -> anyhow::Result<String> + Sync),
    log: &Mutex<&mut (impl Write + Send)>,
    max: usize,
    shared: &Shared,
) {
    let serving = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for client in clients {
            let mut client = match client {
                Ok(client) => client,
                Err(error) => {
                    note(
                        session,
                        log,
                        Err(anyhow!(error).context("could not accept a connection")),
                    );
                    continue;
                }
            };
            if serving.fetch_add(1, Ordering::SeqCst) >= max {
                serving.fetch_sub(1, Ordering::SeqCst);
                refuse(&mut client, "the broker is busy; try again");
                note(
                    session,
                    log,
                    Err(anyhow!("{max} clients are being served already")),
                );
                continue;
            }
            let serving = &serving;
            scope.spawn(move || {
                let number = shared.enter(&client);
                note(session, log, answer(client, mint));
                if let Some(number) = number {
                    shared.leave(number);
                }
                serving.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
}

/// Notes one client's `outcome` in `log`, naming the session.
fn note(session: &str, log: &Mutex<&mut (impl Write + Send)>, outcome: anyhow::Result<Scope>) {
    let line = match outcome {
        Ok(scope) => format!("broker {session}: minted a {} token", scope.name()),
        Err(error) => format!("broker {session}: refused a request: {error:#}"),
    };
    note_line(log, &line);
}

/// Writes `line` to `log`.
fn note_line(log: &Mutex<&mut (impl Write + Send)>, line: &str) {
    if let Ok(mut log) = log.lock() {
        // With the log gone there is nowhere left to note it; the client got its answer.
        let _noted = writeln!(log, "{line}");
    }
}

/// Refuses to serve from `exe` when it lies inside a git checkout, where agents may have changed
/// the code it was built from. The broker runs from a build of `main` installed outside any
/// checkout, as `cargo xtask broker --help` says.
pub(crate) fn refuse_checkout(exe: &Path) -> anyhow::Result<()> {
    if let Some(checkout) = exe.ancestors().find(|dir| dir.join(".git").exists()) {
        bail!(
            "the broker holds the App's key, so it runs only from a build of main installed \
             outside any checkout (see `cargo xtask broker --help`); {} is inside the checkout {}",
            exe.display(),
            checkout.display()
        );
    }
    Ok(())
}

/// Listens on `socket` without blocking, in a directory only the user can enter, replacing a
/// socket no broker listens on any longer.
fn bind(socket: &Path) -> anyhow::Result<UnixListener> {
    let dir = socket
        .parent()
        .context("the broker's socket has no directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not restrict {} to its owner", dir.display()))?;
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!("a broker is already listening on {}", socket.display());
        }
        fs::remove_file(socket)
            .with_context(|| format!("could not remove the stale {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("could not listen on {}", socket.display()))?;
    // [`accepting`] looks for clients without blocking, so that it sees a stop.
    listener
        .set_nonblocking(true)
        .with_context(|| format!("could not listen on {} without blocking", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict {} to its owner", socket.display()))?;
    Ok(listener)
}

/// Answers one client on `stream` with a token from `mint`, or with the reason there is none, and
/// returns the scope it minted for. The client has [`PATIENCE`] in all to send its request, and
/// again to take its answer, however long minting took.
fn answer(
    mut stream: UnixStream,
    mint: impl Fn(Scope) -> anyhow::Result<String>,
) -> anyhow::Result<Scope> {
    let mut request = String::new();
    let read = BufReader::new(Timed::new(&stream).take(MAX_REQUEST)).read_line(&mut request);
    let minted = read
        .context("could not read the request")
        .and_then(|_| {
            let name = request
                .strip_suffix('\n')
                .context("the request is not one line")?;
            Scope::parse(name).with_context(|| format!("no scope is named {name:?}"))
        })
        .and_then(|scope| Ok((scope, mint(scope)?)));
    match minted {
        Ok((scope, token)) => {
            writeln!(Timed::new(&stream), "token {token}").context("could not send the token")?;
            Ok(scope)
        }
        Err(error) => {
            refuse(&mut stream, &format!("{error:#}").replace('\n', " "));
            Err(error)
        }
    }
}

/// Tells `client` why it gets no token, then reads and drops whatever of its request is left
/// without waiting for more. On Linux, closing a Unix socket whose input was not all read resets
/// the connection, and the client would see that instead of the reason.
fn refuse(client: &mut UnixStream, reason: &str) {
    // The client may be gone; the refusal is noted either way.
    let _sent = writeln!(client, "error {reason}");
    if client.set_nonblocking(true).is_ok() {
        let mut rest = [0_u8; 1024];
        for _ in 0..MAX_DRAIN_READS {
            if !matches!(client.read(&mut rest), Ok(read) if read > 0) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime};

    use super::{
        DIR, Files, Ledger, Minted, STOP_PATIENCE, answer, answer_all, check_session, clock,
        exchange, files, request, revoke_all, serve, stop_at,
    };
    use crate::app::{Revoked, Scope};

    /// The broker's answer to `request`, with `mint` minting, and the scope it reports.
    fn ask(request: &[u8], mint: impl Fn(Scope) -> anyhow::Result<String>) -> (String, String) {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(request).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let outcome = answer(server, mint);
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        (
            reply,
            outcome.map_or_else(
                |error| format!("error: {error:#}"),
                |scope| scope.name().to_owned(),
            ),
        )
    }

    #[test]
    fn mints_a_token_for_the_scope_asked() {
        let mint = |scope: Scope| Ok(format!("ghs_{}", scope.name()));
        assert_eq!(
            ask(b"git\n", mint),
            ("token ghs_git\n".to_owned(), "git".to_owned())
        );
        assert_eq!(
            ask(b"gh\n", mint),
            ("token ghs_gh\n".to_owned(), "gh".to_owned())
        );
    }

    #[test]
    fn refuses_what_it_does_not_understand() {
        let mint = |_: Scope| -> anyhow::Result<String> { panic!("nothing is minted") };
        let (reply, outcome) = ask(b"admin\n", mint);
        assert_eq!(reply, "error no scope is named \"admin\"\n");
        assert!(outcome.starts_with("error"), "{outcome}");
        let (unfinished, _) = ask(b"git", mint);
        assert_eq!(
            unfinished, "error the request is not one line\n",
            "no newline"
        );
        let (long, _) = ask(&[b'g'; 100], mint);
        assert_eq!(
            long, "error the request is not one line\n",
            "too long to read"
        );
    }

    #[test]
    fn passes_on_why_minting_failed() {
        let (reply, outcome) = ask(b"gh\n", |_| {
            anyhow::bail!("GitHub refused the App's token request:\nhttp status: 401")
        });
        assert_eq!(
            reply, "error GitHub refused the App's token request: http status: 401\n",
            "one line"
        );
        assert!(outcome.contains("401"), "{outcome}");
    }

    /// A connected pair: the client's end, which has sent `request`, and the broker's.
    fn client(request: &'static [u8]) -> (UnixStream, UnixStream) {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(request).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        (client, server)
    }

    #[test]
    fn serves_every_client_and_logs_no_token() {
        let (mut first, first_server) = client(b"git\n");
        let (mut second, second_server) = client(b"root\n");
        let clients = vec![
            Ok(first_server),
            Err(std::io::Error::other("accept failed")),
            Ok(second_server),
        ];
        let mut log = Vec::new();
        answer_all(
            "abc-w",
            clients.into_iter(),
            &|_| Ok("ghs_secret".to_owned()),
            &Mutex::new(&mut log),
            2,
            &super::Shared::default(),
        );
        let mut reply = String::new();
        first.read_to_string(&mut reply).unwrap();
        assert_eq!(
            reply, "token ghs_secret\n",
            "the first client got its token"
        );
        reply.clear();
        second.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("error "), "the last was refused: {reply}");
        let log = String::from_utf8(log).unwrap();
        let mut lines: Vec<&str> = log.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "broker abc-w: minted a git token",
                "broker abc-w: refused a request: could not accept a connection: accept failed",
                "broker abc-w: refused a request: no scope is named \"root\"",
            ],
            "every request is noted"
        );
        assert!(!log.contains("ghs_secret"), "and no token ever");
    }

    #[test]
    fn a_hung_client_holds_up_only_its_own_connection() {
        // A client that connects and never sends its request.
        let (hung, hung_server) = UnixStream::pair().unwrap();
        let (mut waiting, waiting_server) = client(b"git\n");
        let (sender, served) = std::sync::mpsc::channel();
        let broker = std::thread::spawn(move || {
            let mut log = Vec::new();
            answer_all(
                "abc-w",
                vec![Ok(hung_server), Ok(waiting_server)].into_iter(),
                &|_| Ok("ghs_x".to_owned()),
                &Mutex::new(&mut log),
                1,
                &super::Shared::default(),
            );
            sender.send(String::from_utf8(log).unwrap()).unwrap();
        });
        let mut reply = String::new();
        waiting.read_to_string(&mut reply).unwrap();
        assert_eq!(
            reply, "error the broker is busy; try again\n",
            "past the limit a client is refused at once, not queued behind the hung one"
        );
        drop(hung);
        broker.join().unwrap();
        let log = served.recv().unwrap();
        assert!(
            log.contains("broker abc-w: refused a request: 1 clients are being served already"),
            "{log}"
        );
    }

    #[test]
    fn a_refusal_drops_what_is_left_of_the_request() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(&[b'x'; 5000]).unwrap();
        super::refuse(&mut server, "no scope is named \"xxx\"");
        let mut left = [0_u8; 1];
        server.set_nonblocking(true).unwrap();
        assert_eq!(
            server.read(&mut left).map_err(|error| error.kind()),
            Err(std::io::ErrorKind::WouldBlock),
            "nothing unread, so closing cannot reset the connection on Linux"
        );
        drop(server);
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        assert_eq!(
            reply, "error no scope is named \"xxx\"\n",
            "the reason arrives"
        );
    }

    #[test]
    fn refuses_to_run_from_a_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("install/bin/xtask");
        assert!(
            super::refuse_checkout(&installed).is_ok(),
            "an install root"
        );
        let checkout = temp.path().join("abc");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join(".git"), "gitdir: elsewhere\n").unwrap();
        let built = checkout.join("target/debug/xtask");
        let error = format!("{:#}", super::refuse_checkout(&built).unwrap_err());
        assert!(
            error.contains("installed outside any checkout") && error.contains("abc"),
            "a worktree's .git file counts too: {error}"
        );
    }

    #[test]
    fn a_client_reads_the_token_or_the_refusal() {
        let reply = |answer: &'static str, scope: Scope| {
            let (client, server) = UnixStream::pair().unwrap();
            let broker = std::thread::spawn(move || {
                let mut request = String::new();
                std::io::BufReader::new(&server)
                    .read_line(&mut request)
                    .unwrap();
                (&server).write_all(answer.as_bytes()).unwrap();
                request
            });
            let token = exchange(client, scope, |_| panic!("no read would block"))
                .map_err(|error| format!("{error:#}"));
            (broker.join().unwrap(), token)
        };
        assert_eq!(
            reply("token ghs_x\n", Scope::Gh),
            ("gh\n".to_owned(), Ok("ghs_x".to_owned()))
        );
        assert_eq!(
            reply("error no key\n", Scope::Git),
            (
                "git\n".to_owned(),
                Err("the agent App's token broker refused: no key".to_owned())
            )
        );
        for nothing in ["token \n", "", "gibberish\n"] {
            assert_eq!(
                reply(nothing, Scope::Git).1,
                Err("the agent App's token broker gave no token".to_owned()),
                "{nothing:?}"
            );
        }
    }

    /// A broker's end whose first `blocked` reads fail with `error`, then answers `answer`.
    struct Hesitant {
        blocked: usize,
        error: std::io::ErrorKind,
        answer: std::io::Cursor<&'static [u8]>,
    }

    impl std::io::Read for Hesitant {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.blocked > 0 {
                self.blocked = self.blocked.saturating_sub(1);
                return Err(self.error.into());
            }
            self.answer.read(buf)
        }
    }

    impl std::io::Write for Hesitant {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_client_reads_again_when_the_socket_would_block() {
        let ask_with = |blocked: usize, kind| {
            let stream = Hesitant {
                blocked,
                error: kind,
                answer: std::io::Cursor::new(b"token ghs_x\n"),
            };
            let mut waits = Vec::new();
            let token = exchange(stream, Scope::Git, |delay| waits.push(delay))
                .map_err(|error| format!("{error:#}"));
            (token, waits)
        };
        let ask = |blocked| ask_with(blocked, std::io::ErrorKind::WouldBlock);
        // `could not read the broker's answer: Resource temporarily unavailable (os error 11)`
        // failed a push once; the retry by hand worked.
        assert_eq!(
            ask(2),
            (
                Ok("ghs_x".to_owned()),
                super::READ_RETRY_DELAYS[..2].to_vec()
            ),
            "two reads would block, then the answer comes, after a short wait each time"
        );
        assert_eq!(ask(0), (Ok("ghs_x".to_owned()), Vec::new()), "no wait");
        let (token, waits) = ask(4);
        assert_eq!(
            waits,
            super::READ_RETRY_DELAYS.to_vec(),
            "it waits a few times"
        );
        let error = token.unwrap_err();
        assert!(
            error.starts_with("could not read the broker's answer"),
            "then gives up: {error}"
        );
        let (reset, reset_waits) = ask_with(1, std::io::ErrorKind::ConnectionReset);
        assert!(
            reset_waits.is_empty() && reset.is_err(),
            "any other error fails at once"
        );
    }

    #[test]
    fn a_missing_broker_says_how_to_start_one() {
        let dir = tempfile::tempdir().unwrap();
        let error = format!(
            "{:#}",
            request(&dir.path().join("broker.sock"), Scope::Git).unwrap_err()
        );
        assert!(error.contains("cargo xtask broker --help"), "{error}");
    }

    #[test]
    fn each_session_has_its_own_files() {
        assert_eq!(
            files(Path::new("/home/a"), "abc-w"),
            Files {
                tokens: Path::new("/home/a").join(DIR).join("abc-w.sock"),
                control: Path::new("/home/a").join(DIR).join("abc-w.ctl"),
                pid: Path::new("/home/a").join(DIR).join("abc-w.pid"),
            }
        );
        check_session("abc-s0-microvm").unwrap();
        for bad in [
            "abc-",
            "w",
            "abc-W",
            "abc-a/b",
            "abc-a.sock",
            &format!("abc-{}", "a".repeat(61)),
        ] {
            assert!(check_session(bad).is_err(), "{bad}");
        }
        check_session(&format!("abc-{}", "a".repeat(60))).unwrap();
    }

    #[test]
    fn keeps_to_its_hourly_limit() {
        let mut ledger = Ledger::default();
        let start = Instant::now();
        ledger.admit(start, 2).unwrap();
        ledger.admit(start, 2).unwrap();
        let error = format!("{:#}", ledger.admit(start, 2).unwrap_err());
        assert_eq!(
            error,
            "2 tokens were minted in the last hour, the most a session gets"
        );
        let almost = start + Duration::from_secs(3599);
        assert!(ledger.admit(almost, 2).is_err(), "not yet an hour");
        let later = start + Duration::from_secs(3600);
        ledger.admit(later, 2).unwrap();
        assert_eq!(ledger.recent.len(), 1, "the old ones aged out");
    }

    /// A minted token for [`revoke_all`]: minted `ago` before `now`, expiring an hour after.
    fn minted(token: &str, now: Instant, ago: Duration) -> Minted {
        Minted {
            token: token.to_owned(),
            at: now.checked_sub(ago).unwrap(),
            // 15:30 UTC, less `ago`.
            expires: SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_mins(930))
                .and_then(|at| at.checked_sub(ago))
                .unwrap(),
        }
    }

    /// `vm stop` said "revoked 0 of 20 tokens" though every token was harmless: `cargo xtask gh`
    /// and `pr` had revoked their own, so GitHub answered 401 to the broker's second revocation,
    /// which counted as a failure. Now a token GitHub no longer takes is already revoked, an
    /// expired one is not sent, and only a real failure is a warning naming how long the tokens
    /// it left stay valid.
    #[test]
    fn revocation_tells_revoked_already_revoked_expired_and_failed_apart() {
        let now = Instant::now();
        let tokens = [
            minted("ghs_live", now, Duration::from_secs(60)),
            minted("ghs_used", now, Duration::from_secs(120)),
            minted("ghs_old", now, Duration::from_secs(3_600)),
            minted("ghs_down", now, Duration::from_secs(30)),
            minted("ghs_down2", now, Duration::from_secs(600)),
        ];
        let asked = Mutex::new(Vec::new());
        let done = revoke_all(
            &tokens,
            &|token: &str| {
                asked.lock().unwrap().push(token.to_owned());
                match token {
                    "ghs_live" => Ok(Revoked::Now),
                    "ghs_used" => Ok(Revoked::Already),
                    _ => anyhow::bail!("GitHub answered 502"),
                }
            },
            now,
        );
        let mut asked = asked.into_inner().unwrap();
        asked.sort();
        assert_eq!(
            asked,
            ["ghs_down", "ghs_down2", "ghs_live", "ghs_used"],
            "an expired token is not sent"
        );
        assert_eq!(
            (
                done.minted,
                done.revoked,
                done.already,
                done.expired,
                done.failed.len()
            ),
            (5, 1, 1, 1, 2)
        );
        assert_eq!(
            done.to_string(),
            "revoked 1 of 5 tokens; 1 already revoked by the command that used them; 1 expired; \
             WARNING: 2 could not be revoked and stay valid until 15:30 UTC (GitHub answered 502)",
            "the latest expiry of the failed ones, rounded up to the minute"
        );
        let clean = revoke_all(&tokens[..1], &|_: &str| Ok(Revoked::Now), now);
        assert_eq!(clean.to_string(), "revoked 1 of 1 tokens");
        assert_eq!(
            revoke_all(&[], &|_: &str| Ok(Revoked::Now), now).to_string(),
            "revoked 0 of 0 tokens"
        );
    }

    #[test]
    fn a_time_is_said_in_utc_rounded_up_to_the_minute() {
        let at = |seconds: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(clock(at(0)), "00:00 UTC");
        assert_eq!(clock(at(1)), "00:01 UTC");
        assert_eq!(clock(at(60)), "00:01 UTC");
        assert_eq!(clock(at(86_400 + 3_600 * 23 + 59 * 60 + 1)), "00:00 UTC");
        assert_eq!(clock(at(3_600 * 13 + 60 * 7)), "13:07 UTC");
    }

    /// A broker for session `abc-w` under `dir`, minting `ghs_<scope><n>` at most `limit` an
    /// hour and recording what it revokes; returns its sockets, the revocations and its thread.
    fn broker(
        dir: &Path,
        limit: usize,
    ) -> (
        Files,
        std::sync::Arc<Mutex<Vec<String>>>,
        std::thread::JoinHandle<String>,
    ) {
        let sockets = files(dir, "abc-w");
        let paths = sockets.clone();
        let revoked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let record = std::sync::Arc::clone(&revoked);
        let thread = std::thread::spawn(move || {
            let count = std::sync::atomic::AtomicUsize::new(0);
            let mut log = Vec::new();
            serve(
                "abc-w",
                &paths,
                |scope| {
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    match scope {
                        Scope::Git => Ok(format!("ghs_git{n}")),
                        Scope::Gh => anyhow::bail!("no gh today"),
                    }
                },
                |token| {
                    record.lock().unwrap().push(token.to_owned());
                    Ok(if token == "ghs_git1" {
                        Revoked::Already
                    } else {
                        Revoked::Now
                    })
                },
                limit,
                &mut log,
            )
            .unwrap();
            String::from_utf8(log).unwrap()
        });
        while UnixStream::connect(&sockets.control).is_err() {
            std::thread::sleep(Duration::from_millis(10));
        }
        (sockets, revoked, thread)
    }

    #[test]
    fn serves_a_session_then_revokes_its_tokens_on_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (sockets, revoked, thread) = broker(dir.path(), 2);
        assert_eq!(request(&sockets.tokens, Scope::Git).unwrap(), "ghs_git0");
        assert_eq!(request(&sockets.tokens, Scope::Git).unwrap(), "ghs_git1");
        let limited = format!("{:#}", request(&sockets.tokens, Scope::Git).unwrap_err());
        assert!(
            limited.contains("2 tokens were minted in the last hour"),
            "{limited}"
        );
        let refused = format!("{:#}", request(&sockets.tokens, Scope::Gh).unwrap_err());
        assert!(
            refused.contains("tokens were minted"),
            "the limit counts every ask: {refused}"
        );
        let mode = |file: &Path| std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&sockets.tokens), 0o600, "the token socket");
        assert_eq!(mode(&sockets.control), 0o600, "the control socket");
        assert_eq!(mode(&sockets.pid), 0o600, "the pid file");
        assert_eq!(
            std::fs::read_to_string(&sockets.pid).unwrap(),
            format!("{}\n", std::process::id()),
            "naming this process"
        );
        assert_eq!(
            mode(sockets.tokens.parent().unwrap()),
            0o700,
            "their directory"
        );
        let mut wrong = UnixStream::connect(&sockets.control).unwrap();
        wrong.write_all(b"mint\n").unwrap();
        let mut said = String::new();
        wrong.read_to_string(&mut said).unwrap();
        assert!(said.starts_with("error "), "{said}");
        assert_eq!(
            stop_at(&sockets.control, STOP_PATIENCE).unwrap(),
            "revoked 1 of 2 tokens; 1 already revoked by the command that used them"
        );
        let log = thread.join().unwrap();
        let mut asked = revoked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, ["ghs_git0", "ghs_git1"]);
        assert!(log.contains("broker abc-w: minted a git token"), "{log}");
        assert!(
            log.contains("broker abc-w: stopped; revoked 1 of 2 tokens; 1 already revoked"),
            "{log}"
        );
        assert!(!log.contains("ghs_"), "no token is logged: {log}");
        assert!(
            !sockets.tokens.exists() && !sockets.control.exists() && !sockets.pid.exists(),
            "sockets and pid file removed"
        );
        let gone = format!(
            "{:#}",
            stop_at(&sockets.control, STOP_PATIENCE).unwrap_err()
        );
        assert!(gone.contains("no broker listens"), "{gone}");
    }

    #[test]
    fn refuses_a_second_broker_and_replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let stale = files(dir.path(), "abc-w");
        std::fs::create_dir_all(stale.tokens.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&stale.tokens).unwrap());
        let (sockets, _, thread) = broker(dir.path(), 5);
        assert_eq!(request(&sockets.tokens, Scope::Git).unwrap(), "ghs_git0");
        let again = format!(
            "{:#}",
            serve(
                "abc-w",
                &sockets,
                |_| Ok(String::new()),
                |_| Ok(Revoked::Now),
                1,
                &mut std::io::sink()
            )
            .unwrap_err()
        );
        assert!(again.contains("already listening"), "{again}");
        stop_at(&sockets.control, STOP_PATIENCE).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn a_client_finds_the_broker_through_the_variable() {
        let unset = format!("{:#}", super::token_from(|_| None, Scope::Git).unwrap_err());
        assert!(unset.contains("ABC_AGENT_BROKER is not set"), "{unset}");
        let empty = format!(
            "{:#}",
            super::token_from(|_| Some("".into()), Scope::Git).unwrap_err()
        );
        assert!(
            empty.contains("is not set"),
            "an empty value counts as unset: {empty}"
        );
        let dir = tempfile::tempdir().unwrap();
        let (sockets, _, thread) = broker(dir.path(), 5);
        let path = sockets.tokens.clone();
        let named = |name: &str| (name == super::ENV).then(|| path.clone().into_os_string());
        assert_eq!(super::token_from(named, Scope::Git).unwrap(), "ghs_git0");
        stop_at(&sockets.control, STOP_PATIENCE).unwrap();
        thread.join().unwrap();
    }

    /// A stop signal stops the broker as `stop` does: tokens revoked, files removed.
    #[test]
    fn sigterm_stops_the_broker_as_stop_does() {
        let dir = tempfile::tempdir().unwrap();
        let (sockets, revoked, thread) = broker(dir.path(), 5);
        assert_eq!(request(&sockets.tokens, Scope::Git).unwrap(), "ghs_git0");
        // nextest runs each test in a process of its own, so this signals this test alone.
        let sent = std::process::Command::new("kill")
            .args(["-s", "TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let log = thread.join().unwrap();
        assert!(
            log.contains("broker abc-w: stopped on SIGTERM; revoked 1 of 1 tokens"),
            "{log}"
        );
        assert_eq!(*revoked.lock().unwrap(), ["ghs_git0"]);
        for file in [&sockets.tokens, &sockets.control, &sockets.pid] {
            assert!(!file.exists(), "{} is left behind", file.display());
        }
    }

    /// Waits up to `patience` for the broker thread `thread` to end, returning its log; fails the
    /// test when it does not.
    fn ended(thread: std::thread::JoinHandle<String>, patience: Duration) -> String {
        let (sender, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(thread.join().unwrap()));
        ended
            .recv_timeout(patience)
            .expect("the broker did not stop in time")
    }

    /// A client of `tokens` that sends one byte a second, each within the broker's patience for a
    /// read, and never finishes its request; it stops when the broker hangs up.
    fn trickle(tokens: &Path) -> std::thread::JoinHandle<()> {
        let mut client = UnixStream::connect(tokens).unwrap();
        std::thread::spawn(move || {
            for _ in 0..60 {
                if client.write_all(b"g").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
    }

    /// The broker's stop waited for every client being served, and a client that sends a byte
    /// within each read's patience held it for as long as it liked: up to 64 bytes at almost 5 s
    /// each. Stopping ends the clients in flight.
    #[test]
    fn a_trickling_client_cannot_hold_up_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (sockets, revoked, thread) = broker(dir.path(), 5);
        assert_eq!(request(&sockets.tokens, Scope::Git).unwrap(), "ghs_git0");
        let trickler = trickle(&sockets.tokens);
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        assert_eq!(
            stop_at(&sockets.control, Duration::from_secs(3)).unwrap(),
            "revoked 1 of 1 tokens"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        ended(thread, Duration::from_secs(2));
        assert_eq!(*revoked.lock().unwrap(), ["ghs_git0"]);
        trickler.join().unwrap();
    }

    /// The listeners look again while no client comes, pass on any other error, and end once the
    /// broker stops.
    #[test]
    fn listening_waits_out_would_block_and_passes_other_errors_on() {
        let stopping = std::sync::atomic::AtomicBool::new(false);
        let (client, _other) = UnixStream::pair().unwrap();
        let mut answers = vec![
            Ok(client),
            Err(std::io::Error::other("too many open files")),
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
        ];
        let mut clients =
            super::accepted(move || answers.pop().expect("asked too often"), &stopping);
        let first = clients.next().unwrap();
        assert_eq!(
            first.map(|_| ()).map_err(|error| error.to_string()),
            Err("too many open files".to_owned()),
            "would block is no client yet, and is not passed on"
        );
        assert!(clients.next().unwrap().is_ok(), "then the client");
        drop(clients);
        stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut stopped = super::accepted(|| panic!("not asked once stopping"), &stopping);
        assert!(stopped.next().is_none());
    }

    /// A client that sends nothing until the deadline gets the deadline's error, not the
    /// operating system's "would block" from a read whose timeout ran out (CI on #33 saw it).
    #[test]
    fn a_silent_client_times_out_with_the_deadlines_error() {
        let (_client, server) = UnixStream::pair().unwrap();
        let mut timed = super::Timed {
            stream: &server,
            deadline: Instant::now() + Duration::from_millis(100),
            now: Instant::now,
        };
        let read = std::io::Read::read(&mut timed, &mut [0; 8]);
        assert_eq!(
            read.map_err(|error| (error.kind(), error.to_string())),
            Err((
                std::io::ErrorKind::TimedOut,
                format!("the client took more than {} s", super::PATIENCE.as_secs())
            ))
        );
    }

    /// A client sending a byte every 50 ms, on a clock the test drives: each read waits only what
    /// is left of the deadline, and once it has come the read is `TimedOut`, whether it came
    /// between two reads (400 ms) or during one (420 ms). CI's runner once saw the stream's own
    /// timeout, "would block", come out instead.
    #[test]
    fn a_client_has_its_patience_in_all_not_per_read() {
        /// A client that sends one byte 50 ms after each read starts, on `clock`.
        struct Trickler<'a> {
            clock: &'a std::cell::Cell<Instant>,
            waits: Vec<Duration>,
        }
        impl super::Patient for &mut Trickler<'_> {
            fn read_within(
                &mut self,
                buf: &mut [u8],
                patience: Duration,
            ) -> std::io::Result<usize> {
                self.waits.push(patience);
                let gap = Duration::from_millis(50);
                if patience < gap {
                    self.clock.set(self.clock.get() + patience);
                    return Err(std::io::ErrorKind::WouldBlock.into());
                }
                self.clock.set(self.clock.get() + gap);
                buf[0] = b'g';
                Ok(1)
            }

            fn write_within(&mut self, _: &[u8], _: Duration) -> std::io::Result<usize> {
                Err(std::io::Error::other("the test only reads"))
            }
        }

        for (millis, bytes, last_wait) in [(400, 8, None), (420, 8, Some(20))] {
            let started = Instant::now();
            let clock = std::cell::Cell::new(started);
            let mut trickler = Trickler {
                clock: &clock,
                waits: Vec::new(),
            };
            let timed = super::Timed {
                stream: &mut trickler,
                deadline: started + Duration::from_millis(millis),
                now: || clock.get(),
            };
            let mut line = String::new();
            let read =
                std::io::BufReader::new(std::io::Read::take(timed, 1000)).read_line(&mut line);
            assert_eq!(
                read.map_err(|error| error.kind()),
                Err(std::io::ErrorKind::TimedOut),
                "deadline {millis} ms"
            );
            assert_eq!(line, "g".repeat(bytes), "deadline {millis} ms");
            let mut waits: Vec<Duration> = (0..bytes)
                .map(|byte| Duration::from_millis(millis - 50 * u64::try_from(byte).unwrap()))
                .collect();
            waits.extend(last_wait.map(Duration::from_millis));
            assert_eq!(trickler.waits, waits, "deadline {millis} ms");
            assert_eq!(
                clock.get() - started,
                Duration::from_millis(millis),
                "deadline {millis} ms"
            );
        }
    }

    /// The broker stopped on SIGTERM by sending `stop` to its own control socket: a broker that
    /// did not answer there was never revoked, and one whose control socket was gone never exited
    /// on SIGTERM. It now stops directly.
    #[test]
    fn sigterm_stops_the_broker_without_its_control_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (sockets, revoked, thread) = broker(dir.path(), 5);
        assert_eq!(request(&sockets.tokens, Scope::Git).unwrap(), "ghs_git0");
        std::fs::remove_file(&sockets.control).unwrap();
        let trickler = trickle(&sockets.tokens);
        std::thread::sleep(Duration::from_millis(200));
        // nextest runs each test in a process of its own, so this signals this test alone.
        let sent = std::process::Command::new("kill")
            .args(["-s", "TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let log = ended(thread, Duration::from_secs(3));
        assert!(
            log.contains("broker abc-w: stopped on SIGTERM; revoked 1 of 1 tokens"),
            "{log}"
        );
        assert_eq!(*revoked.lock().unwrap(), ["ghs_git0"]);
        for file in [&sockets.tokens, &sockets.pid] {
            assert!(!file.exists(), "{} is left behind", file.display());
        }
        trickler.join().unwrap();
    }

    /// A broker that listens but does not answer and left no pid file may still be running and
    /// minting: its files were deleted and `vm stop` went on as if it were gone.
    #[test]
    fn a_broker_that_may_still_run_keeps_its_files_and_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let (files, _control) = hung(dir.path(), None);
        let said = format!(
            "{:?}",
            super::shut_down(&files, "abc-w", Duration::from_millis(300))
        );
        assert!(said.contains("may still be running"), "{said}");
        assert!(files.control.exists() && files.tokens.exists(), "{said}");
    }

    /// `kill` reads 0 as the caller's process group, -1 as every process the user may signal,
    /// other negative numbers as groups, and 1 is init: no such pid file is read as a pid.
    #[test]
    fn only_a_single_process_other_than_init_is_a_pid() {
        for text in [
            "0",
            "1",
            "-1",
            "-2",
            "+0",
            "2147483648",
            "4294967295",
            "",
            "x",
        ] {
            assert!(text.parse::<super::Pid>().is_err(), "{text:?}");
        }
        assert_eq!("2".parse::<super::Pid>().unwrap(), super::Pid(2));
        assert_eq!(
            "2147483647".parse::<super::Pid>().unwrap().to_string(),
            "2147483647"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc-w.pid");
        for text in ["0\n", "1\n", "-1\n"] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(super::read_pid(&path), None, "{text:?}");
        }
    }

    /// A broker that does not answer and whose pid file names 0, 1 or -1 is never signalled
    /// through it: `kill 0` would stop this test's own process group, `kill -1` every process it
    /// may signal.
    #[test]
    fn a_pid_file_naming_a_group_or_init_is_never_signalled() {
        for pid in ["0", "1", "-1"] {
            let dir = tempfile::tempdir().unwrap();
            let (files, _control) = hung(dir.path(), None);
            std::fs::write(&files.pid, format!("{pid}\n")).unwrap();
            let said = format!(
                "{:?}",
                super::shut_down(&files, "abc-w", Duration::from_millis(300))
            );
            assert!(said.contains("may still be running"), "{pid}: {said}");
            assert!(!said.contains("SIGTERM"), "{pid}: {said}");
        }
    }

    /// A broker that, on SIGTERM, became another program at the same pid got SIGKILL: the
    /// identity check ran only before SIGTERM.
    #[test]
    fn sigkill_goes_only_to_a_process_that_is_still_the_broker() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "trap 'exec sleep 30' TERM; while :; do sleep 0.05; done",
                "sh",
                "broker",
                "--session",
                "abc-w",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let (files, _control) = hung(dir.path(), Some(child.id()));
        let said = format!(
            "{:?}",
            super::shut_down(&files, "abc-w", Duration::from_millis(500))
        );
        assert!(child.try_wait().unwrap().is_none(), "never killed: {said}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// A process that looks like session abc-w's broker to `ps`: it exits on SIGTERM unless
    /// `stubborn`, and never answers on a control socket.
    fn fake_broker(stubborn: bool) -> std::process::Child {
        let trap = if stubborn { "''" } else { "'exit 0'" };
        std::process::Command::new("sh")
            .args([
                "-c",
                &format!("trap {trap} TERM; while :; do sleep 0.05; done"),
                "sh",
                "broker",
                "--session",
                "abc-w",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// The files of a broker that hung, as on 2026-11-06: a control socket that takes the
    /// connection and never answers, its token socket and its pid file naming `pid`.
    /// Session `abc-w`'s files in `dir` for a broker that listens on both sockets and never
    /// answers, with a pid file naming `pid` when there is one.
    fn hung(dir: &Path, pid: Option<u32>) -> (Files, std::os::unix::net::UnixListener) {
        let files = files(dir, "abc-w");
        std::fs::create_dir_all(files.control.parent().unwrap()).unwrap();
        let control = std::os::unix::net::UnixListener::bind(&files.control).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&files.tokens).unwrap());
        if let Some(pid) = pid {
            std::fs::write(&files.pid, format!("{pid}\n")).unwrap();
        }
        (files, control)
    }

    /// 2026-11-06: `vm stop` failed with `Resource temporarily unavailable`, because the broker
    /// never answered `stop`, and it had to be stopped and its files deleted by hand. Now it gets
    /// SIGTERM through its pid file, and nothing is left behind.
    #[test]
    fn a_broker_that_does_not_answer_is_stopped_with_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = fake_broker(false);
        let (files, _control) = hung(dir.path(), Some(child.id()));
        let said = super::shut_down(&files, "abc-w", Duration::from_secs(1)).unwrap();
        assert!(
            said.starts_with("the broker did not answer `stop` within 1 s: ")
                && said.contains(&format!(
                    "stopped the token broker (pid {}) with SIGTERM",
                    child.id()
                ))
                && said.ends_with("removed its leftover abc-w.sock, abc-w.ctl, abc-w.pid"),
            "{said}"
        );
        assert!(child.wait().unwrap().success(), "it exited on SIGTERM");
        for file in files.all() {
            assert!(!file.exists(), "{} is left behind", file.display());
        }
    }

    #[test]
    fn a_broker_that_outlives_sigterm_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = fake_broker(true);
        let (files, _control) = hung(dir.path(), Some(child.id()));
        let said = super::shut_down(&files, "abc-w", Duration::from_millis(500)).unwrap();
        assert!(
            said.contains(&format!(
                "killed the token broker (pid {}), which outlived SIGTERM",
                child.id()
            )),
            "{said}"
        );
        assert!(!child.wait().unwrap().success(), "killed");
        assert!(files.all().iter().all(|file| !file.exists()));
    }

    #[test]
    fn waits_as_long_as_its_patience_and_no_longer() {
        let started = Instant::now();
        let mut asked = 0;
        assert!(!super::within(Duration::from_millis(600), || {
            asked += 1;
            false
        }));
        assert_eq!(
            asked, 4,
            "at once, then every 200 ms: one ps a poll, not two every 50 ms"
        );
        assert!(started.elapsed() >= Duration::from_millis(600));
        let mut late = 0;
        assert!(super::within(Duration::from_millis(600), || {
            late += 1;
            late == 4
        }));
        assert!(super::within(Duration::ZERO, || true), "asked once");
    }

    #[test]
    fn a_pid_that_is_not_the_broker_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        // Another session's broker, by its command line.
        let mut other = std::process::Command::new("sh")
            .args(["-c", "sleep 30", "sh", "broker", "--session", "abc-other"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let (files, control) = hung(dir.path(), Some(other.id()));
        drop(control);
        let said = super::shut_down(&files, "abc-w", Duration::from_secs(1)).unwrap();
        assert!(
            said.starts_with("no broker listens on ")
                && said.contains(&format!(
                    "process {} is no longer the token broker of abc-w; left alone",
                    other.id()
                ))
                && said.ends_with("removed its leftover abc-w.sock, abc-w.ctl, abc-w.pid"),
            "a stale socket and a reused pid: {said}"
        );
        assert!(other.try_wait().unwrap().is_none(), "never signalled");
        other.kill().unwrap();
        other.wait().unwrap();
        assert_eq!(
            super::shut_down(&files, "abc-w", Duration::from_secs(1)).unwrap(),
            "no token broker was running"
        );
    }

    #[test]
    fn shutting_down_a_broker_that_answers_asks_it_and_waits_for_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let (sockets, revoked, thread) = broker(dir.path(), 5);
        request(&sockets.tokens, Scope::Git).unwrap();
        assert_eq!(
            super::shut_down(&sockets, "abc-w", STOP_PATIENCE).unwrap(),
            "stopped the token broker; revoked 1 of 1 tokens"
        );
        thread.join().unwrap();
        assert_eq!(revoked.lock().unwrap().len(), 1);
    }
}
