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
//! there to revoke every token it minted and to exit. Each line the broker logs names its
//! session, and it mints at most [`MAX_PER_HOUR`] tokens an hour.
//!
//! The protocol is one line each way: the client sends a scope's name (`git` or `gh`), and the
//! broker answers `token <token>`, a fresh installation token for that scope alone that expires
//! within the hour, or `error <reason>`, and closes the connection. The key never leaves the
//! broker and no token is ever logged.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};

use crate::app::Scope;

/// The brokers' sockets, relative to the home directory.
pub(crate) const DIR: &str = ".config/abc/agent-broker";

/// The most tokens one session's broker mints in an hour, the life of a token.
pub(crate) const MAX_PER_HOUR: usize = 120;

/// How long a client waits for the broker to stop, revoking its tokens.
const STOP_PATIENCE: Duration = Duration::from_secs(60);

/// The variable naming the broker's socket where a client runs: inside an agent VM, the guest end
/// of the vsock forward.
pub(crate) const ENV: &str = "ABC_AGENT_BROKER";

/// How long the broker waits for a client to send its one-line request or take its answer, and a
/// client for the broker's answer.
const PATIENCE: Duration = Duration::from_secs(5);

/// How many clients the broker serves at once; more are refused until one finishes, so clients
/// that hang can hold up at most this many connections, each for at most [`PATIENCE`].
const MAX_CLIENTS: usize = 8;

/// The longest request line a client can send; longer ones are refused.
const MAX_REQUEST: u64 = 64;

/// How many reads of a kilobyte the broker makes to drop what is left of a refused request.
const MAX_DRAIN_READS: usize = 64;

/// One session's broker sockets.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Sockets {
    /// Where the session's VM asks for tokens, forwarded into it.
    pub(crate) tokens: PathBuf,
    /// Where the host stops the broker; never forwarded.
    pub(crate) control: PathBuf,
}

/// The sockets of session `session`'s broker under the home directory `home`.
pub(crate) fn sockets(home: &Path, session: &str) -> Sockets {
    let dir = home.join(DIR);
    Sockets {
        tokens: dir.join(format!("{session}.sock")),
        control: dir.join(format!("{session}.ctl")),
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
fn token_from(env: impl Fn(&str) -> Option<OsString>, scope: Scope) -> anyhow::Result<String> {
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

/// The tokens a broker minted: when, to keep to its hourly limit, and which, to revoke them.
#[derive(Debug, Default)]
struct Ledger {
    recent: VecDeque<Instant>,
    minted: Vec<String>,
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

/// Serves session `session`'s tokens from `mint`, at most `limit` an hour, on `sockets` until a
/// client of the control socket says `stop`; then revokes every token it minted with `revoke`,
/// tells that client how many, and returns. Each request is noted in `log`.
pub(crate) fn serve(
    session: &str,
    sockets: &Sockets,
    mint: impl Fn(Scope) -> anyhow::Result<String> + Sync,
    revoke: impl Fn(&str) -> anyhow::Result<()>,
    limit: usize,
    log: &mut (impl Write + Send),
) -> anyhow::Result<()> {
    let tokens = bind(&sockets.tokens)?;
    let control = bind(&sockets.control)?;
    writeln!(
        log,
        "broker {session}: serving agent App tokens on {}",
        sockets.tokens.display()
    )?;
    let log = Mutex::new(log);
    let ledger = Mutex::new(Ledger::default());
    let stopping = AtomicBool::new(false);
    let metered = |scope: Scope| -> anyhow::Result<String> {
        ledger
            .lock()
            .map_err(|poisoned| anyhow!("the broker's ledger is poisoned: {poisoned}"))?
            .admit(Instant::now(), limit)?;
        let token = mint(scope)?;
        ledger
            .lock()
            .map_err(|poisoned| anyhow!("the broker's ledger is poisoned: {poisoned}"))?
            .minted
            .push(token.clone());
        Ok(token)
    };
    let mut asker = std::thread::scope(|scope| {
        scope.spawn(|| {
            let clients = tokens
                .incoming()
                .take_while(|_| !stopping.load(Ordering::SeqCst))
                .map(|client| {
                    let client = client?;
                    client.set_read_timeout(Some(PATIENCE))?;
                    client.set_write_timeout(Some(PATIENCE))?;
                    Ok(client)
                });
            answer_all(session, clients, &metered, &log, MAX_CLIENTS);
        });
        let asker = await_stop(&control);
        stopping.store(true, Ordering::SeqCst);
        // Wakes the token loop, which then sees it is stopping.
        let _woken = UnixStream::connect(&sockets.tokens);
        asker
    })?;
    let minted = std::mem::take(
        &mut ledger
            .lock()
            .map_err(|poisoned| anyhow!("the broker's ledger is poisoned: {poisoned}"))?
            .minted,
    );
    let revoked = minted.iter().filter(|token| revoke(token).is_ok()).count();
    let summary = format!("revoked {revoked} of {} tokens", minted.len());
    note_line(&log, &format!("broker {session}: stopped; {summary}"));
    // The client may be gone; the tokens are revoked either way.
    let _told = writeln!(asker, "stopped {summary}");
    for socket in [&sockets.tokens, &sockets.control] {
        let _removed = fs::remove_file(socket);
    }
    Ok(())
}

/// Waits on `control` for a client that says `stop`, refusing anything else, and returns it.
fn await_stop(control: &UnixListener) -> anyhow::Result<UnixStream> {
    for client in control.incoming() {
        let Ok(mut client) = client else { continue };
        if client.set_read_timeout(Some(PATIENCE)).is_err() {
            continue;
        }
        let mut line = String::new();
        let read = BufReader::new((&mut client).take(MAX_REQUEST)).read_line(&mut line);
        if read.is_ok() && line == "stop\n" {
            return Ok(client);
        }
        refuse(&mut client, "the control socket understands only `stop`");
    }
    bail!("the broker's control socket closed")
}

/// Stops the broker whose control socket is `control`, which revokes its tokens; returns what it
/// says it revoked.
pub(crate) fn stop(control: &Path) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(control)
        .with_context(|| format!("no broker listens on {}", control.display()))?;
    stream.set_read_timeout(Some(STOP_PATIENCE))?;
    stream.write_all(b"stop\n")?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    answer
        .trim_end()
        .strip_prefix("stopped ")
        .map(str::to_owned)
        .with_context(|| format!("the broker did not stop: {answer:?}"))
}

/// Answers each of `clients` with a token from `mint`, each on its own thread and at most `max` at
/// once, noting each in `log`. A client that fails or comes while `max` are being served is noted
/// and refused.
fn answer_all(
    session: &str,
    clients: impl Iterator<Item = std::io::Result<UnixStream>>,
    mint: &(impl Fn(Scope) -> anyhow::Result<String> + Sync),
    log: &Mutex<&mut (impl Write + Send)>,
    max: usize,
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
                note(session, log, answer(client, mint));
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

/// Listens on `socket`, in a directory only the user can enter, replacing a socket no broker
/// listens on any longer.
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
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict {} to its owner", socket.display()))?;
    Ok(listener)
}

/// Answers one client on `stream` with a token from `mint`, or with the reason there is none, and
/// returns the scope it minted for.
fn answer(
    mut stream: UnixStream,
    mint: impl Fn(Scope) -> anyhow::Result<String>,
) -> anyhow::Result<Scope> {
    let mut request = String::new();
    let read = BufReader::new((&mut stream).take(MAX_REQUEST)).read_line(&mut request);
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
            writeln!(stream, "token {token}").context("could not send the token")?;
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

    use super::{
        DIR, Ledger, Sockets, answer, answer_all, check_session, exchange, request, serve, sockets,
        stop,
    };
    use crate::app::Scope;

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
    fn each_session_has_its_own_sockets() {
        assert_eq!(
            sockets(Path::new("/home/a"), "abc-w"),
            Sockets {
                tokens: Path::new("/home/a").join(DIR).join("abc-w.sock"),
                control: Path::new("/home/a").join(DIR).join("abc-w.ctl"),
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
        let start = std::time::Instant::now();
        ledger.admit(start, 2).unwrap();
        ledger.admit(start, 2).unwrap();
        let error = format!("{:#}", ledger.admit(start, 2).unwrap_err());
        assert_eq!(
            error,
            "2 tokens were minted in the last hour, the most a session gets"
        );
        let almost = start + std::time::Duration::from_secs(3599);
        assert!(ledger.admit(almost, 2).is_err(), "not yet an hour");
        let later = start + std::time::Duration::from_secs(3600);
        ledger.admit(later, 2).unwrap();
        assert_eq!(ledger.recent.len(), 1, "the old ones aged out");
    }

    /// A broker for session `abc-w` under `dir`, minting `ghs_<scope><n>` at most `limit` an
    /// hour and recording what it revokes; returns its sockets, the revocations and its thread.
    fn broker(
        dir: &Path,
        limit: usize,
    ) -> (
        Sockets,
        std::sync::Arc<Mutex<Vec<String>>>,
        std::thread::JoinHandle<String>,
    ) {
        let sockets = sockets(dir, "abc-w");
        let paths = Sockets {
            tokens: sockets.tokens.clone(),
            control: sockets.control.clone(),
        };
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
                    anyhow::ensure!(token != "ghs_git1", "already gone");
                    Ok(())
                },
                limit,
                &mut log,
            )
            .unwrap();
            String::from_utf8(log).unwrap()
        });
        while UnixStream::connect(&sockets.control).is_err() {
            std::thread::sleep(std::time::Duration::from_millis(10));
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
        assert_eq!(stop(&sockets.control).unwrap(), "revoked 1 of 2 tokens");
        let log = thread.join().unwrap();
        assert_eq!(*revoked.lock().unwrap(), ["ghs_git0", "ghs_git1"]);
        assert!(log.contains("broker abc-w: minted a git token"), "{log}");
        assert!(
            log.contains("broker abc-w: stopped; revoked 1 of 2 tokens"),
            "{log}"
        );
        assert!(!log.contains("ghs_"), "no token is logged: {log}");
        assert!(
            !sockets.tokens.exists() && !sockets.control.exists(),
            "sockets removed"
        );
        let gone = format!("{:#}", stop(&sockets.control).unwrap_err());
        assert!(gone.contains("no broker listens"), "{gone}");
    }

    #[test]
    fn refuses_a_second_broker_and_replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let stale = sockets(dir.path(), "abc-w");
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
                |_| Ok(()),
                1,
                &mut std::io::sink()
            )
            .unwrap_err()
        );
        assert!(again.contains("already listening"), "{again}");
        stop(&sockets.control).unwrap();
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
        stop(&sockets.control).unwrap();
        thread.join().unwrap();
    }
}
