//! The agent App's token broker, `cargo xtask broker`.
//!
//! Agents need App tokens to push and to use gh, but must never read the App's private key, which
//! lasts until it is revoked. The broker is the one process that reads it. The user runs it
//! outside Claude Code's sandbox, from a build of `main` (never from a checkout agents work in,
//! whose xtask they can change), and it listens on a Unix socket in `~/.config/abc/agent-broker/`
//! that `.claude/settings.json` lets sandboxed commands reach. Each request names a [`Scope`], and
//! the broker answers with a fresh installation token for that scope alone, valid for an hour. The
//! key never leaves it, and no token is ever logged.
//!
//! The protocol is one line each way: the client sends a scope's name (`git` or `gh`), and the
//! broker answers `token <token>` or `error <reason>` and closes the connection.

use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};

use crate::app::Scope;

/// The broker's socket, relative to the home directory. `.claude/settings.json` allows sandboxed
/// commands to connect to this path and to no other socket.
pub(crate) const SOCKET: &str = ".config/abc/agent-broker/broker.sock";

/// How long the broker waits for a client to send its one-line request or take its answer.
const PATIENCE: Duration = Duration::from_secs(2);

/// How many clients the broker serves at once; more are refused until one finishes, so clients
/// that hang can hold up at most this many connections, each for at most [`PATIENCE`].
const MAX_CLIENTS: usize = 8;

/// The longest request line a client can send; longer ones are refused.
const MAX_REQUEST: u64 = 64;

/// The broker's socket for the home directory in `HOME`.
pub(crate) fn socket() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .context("HOME is not set")?;
    Ok(PathBuf::from(home).join(SOCKET))
}

/// A token for `scope` from the broker listening at `socket`.
pub(crate) fn request(socket: &Path, scope: Scope) -> anyhow::Result<String> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "could not reach the agent App's token broker at {}; the user starts it outside \
             Claude Code (see `cargo xtask broker --help`)",
            socket.display()
        )
    })?;
    stream
        .set_read_timeout(Some(PATIENCE))
        .context("could not set a timeout on the broker's socket")?;
    exchange(stream, scope)
}

/// Asks the broker on `stream` for a token for `scope`.
fn exchange(mut stream: impl Read + Write, scope: Scope) -> anyhow::Result<String> {
    stream
        .write_all(format!("{}\n", scope.name()).as_bytes())
        .context("could not send the broker a request")?;
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .context("could not read the broker's answer")?;
    let answer = answer.trim_end_matches('\n');
    match answer.split_once(' ') {
        Some(("token", token)) if !token.is_empty() => Ok(token.to_owned()),
        Some(("error", reason)) => bail!("the agent App's token broker refused: {reason}"),
        _ => bail!("the agent App's token broker gave no token"),
    }
}

/// Serves tokens from `mint` on `socket` until the process ends, noting each request in `log`.
pub(crate) fn serve(
    socket: &Path,
    mint: impl Fn(Scope) -> anyhow::Result<String> + Sync,
    log: &mut (impl Write + Send),
) -> anyhow::Result<()> {
    let listener = bind(socket)?;
    writeln!(
        log,
        "broker: serving agent App tokens on {}",
        socket.display()
    )?;
    let clients = listener.incoming().map(|client| {
        let client = client?;
        client.set_read_timeout(Some(PATIENCE))?;
        client.set_write_timeout(Some(PATIENCE))?;
        Ok(client)
    });
    answer_all(clients, mint, log, MAX_CLIENTS);
    Ok(())
}

/// Answers each of `clients` with a token from `mint`, each on its own thread and at most `max` at
/// once, noting each in `log`. A client that fails or comes while `max` are being served is noted
/// and refused.
fn answer_all<S: Read + Write + Send>(
    clients: impl Iterator<Item = std::io::Result<S>>,
    mint: impl Fn(Scope) -> anyhow::Result<String> + Sync,
    log: &mut (impl Write + Send),
    max: usize,
) {
    let log = Mutex::new(log);
    let serving = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for client in clients {
            let mut client = match client {
                Ok(client) => client,
                Err(error) => {
                    note(
                        &log,
                        Err(anyhow!(error).context("could not accept a connection")),
                    );
                    continue;
                }
            };
            if serving.fetch_add(1, Ordering::SeqCst) >= max {
                serving.fetch_sub(1, Ordering::SeqCst);
                // The client may be gone; the refusal is noted either way.
                let _sent = writeln!(client, "error the broker is busy; try again");
                note(&log, Err(anyhow!("{max} clients are being served already")));
                continue;
            }
            let (mint, log, serving) = (&mint, &log, &serving);
            scope.spawn(move || {
                note(log, answer(client, mint));
                serving.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
}

/// Notes one client's `outcome` in `log`.
fn note(log: &Mutex<&mut (impl Write + Send)>, outcome: anyhow::Result<Scope>) {
    let line = match outcome {
        Ok(scope) => format!("broker: minted a {} token\n", scope.name()),
        Err(error) => format!("broker: refused a request: {error:#}\n"),
    };
    if let Ok(mut log) = log.lock() {
        // With the log gone there is nowhere left to note it; the client got its answer.
        let _noted = log.write_all(line.as_bytes());
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
    mut stream: impl Read + Write,
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
            let reason = format!("{error:#}").replace('\n', " ");
            // The client may be gone; the refusal is logged either way.
            let _sent = writeln!(stream, "error {reason}");
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    use super::{SOCKET, answer, answer_all, exchange, request, serve, socket};
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
            clients.into_iter(),
            |_| Ok("ghs_secret".to_owned()),
            &mut log,
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
                "broker: minted a git token",
                "broker: refused a request: could not accept a connection: accept failed",
                "broker: refused a request: no scope is named \"root\"",
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
                vec![Ok(hung_server), Ok(waiting_server)].into_iter(),
                |_| Ok("ghs_x".to_owned()),
                &mut log,
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
            log.contains("broker: refused a request: 1 clients are being served already"),
            "{log}"
        );
        assert!(
            log.contains("the request is not one line"),
            "the hung client: {log}"
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
            let token = exchange(client, scope).map_err(|error| format!("{error:#}"));
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
    fn listens_where_the_settings_let_agents_connect() {
        assert!(
            socket().unwrap().ends_with(Path::new(SOCKET)),
            "under the home directory"
        );
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(crate::repo_root().unwrap().join(".claude/settings.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["sandbox"]["network"]["allowUnixSockets"],
            serde_json::json!([format!("~/{SOCKET}")]),
            "the broker's socket and no other"
        );
    }

    /// A log another thread can read while the broker writes it.
    #[derive(Clone, Default)]
    struct SharedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn serves_tokens_on_its_socket_or_the_sandbox_refuses_the_socket() {
        use std::os::unix::fs::PermissionsExt as _;

        // A short path: a socket's must fit in 104 bytes, and the jail's temporary directory is
        // long already, so binding there fails on the sandbox, not on the length.
        let dir = tempfile::Builder::new().prefix("s").tempdir().unwrap();
        let socket = dir.path().join("broker.sock");
        // A socket left behind by a broker that is gone.
        let stale = std::os::unix::net::UnixListener::bind(&socket);
        if let Err(error) = stale {
            // Claude Code's sandbox and the hooks' jail forbid binding a socket: so does the
            // broker, which never runs there.
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{error}"
            );
            let refused = serve(&socket, |_| Ok(String::new()), &mut Vec::new()).unwrap_err();
            assert!(
                format!("{refused:#}").contains("could not listen"),
                "{refused:#}"
            );
            return;
        }
        drop(stale);
        let log = SharedLog::default();
        let mut writer = log.clone();
        let path = socket.clone();
        std::thread::spawn(move || {
            serve(
                &path,
                |scope| Ok(format!("ghs_{}", scope.name())),
                &mut writer,
            )
        });
        let started = std::time::Instant::now();
        while !log.0.lock().unwrap().starts_with(b"broker: serving") {
            assert!(
                started.elapsed().as_secs() < 10,
                "the broker never listened"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(request(&socket, Scope::Git).unwrap(), "ghs_git");
        assert_eq!(request(&socket, Scope::Gh).unwrap(), "ghs_gh");
        let mode = |file: &Path| std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(socket.parent().unwrap()),
            0o700,
            "only its owner enters"
        );
        assert_eq!(mode(&socket), 0o600, "only its owner connects");
        // The broker notes a request after answering it.
        let noted = b"broker: minted a git token\nbroker: minted a gh token\n";
        while !log.0.lock().unwrap().ends_with(noted) {
            assert!(started.elapsed().as_secs() < 10, "both requests noted");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let second = serve(&socket, |_| Ok(String::new()), &mut Vec::new()).unwrap_err();
        assert!(
            format!("{second:#}").contains("already listening"),
            "one broker per socket: {second:#}"
        );
    }
}
