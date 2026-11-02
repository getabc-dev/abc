//! The agent App's token broker, `cargo xtask broker` (D3).
//!
//! Agents push and use gh as the getabc-dev-agent App, but must never read the App's private key,
//! which lasts until it is revoked. The broker is the one process that reads it. The maintainer
//! runs it on the host from a build of `main` installed outside any checkout (an agent can change
//! the xtask of a checkout it works in), and it listens on [`SOCKET`] under the home directory.
//! `cargo xtask vm` forwards that socket into each agent VM over vsock, where [`ENV`] names the
//! guest end; `cargo xtask git-credential` and `cargo xtask gh` ask it for a token per use.
//!
//! The protocol is one line each way: the client sends a scope's name (`git` or `gh`), and the
//! broker answers `token <token>`, a fresh installation token for that scope alone that expires
//! within the hour, or `error <reason>`, and closes the connection. The key never leaves the
//! broker and no token is ever logged.

use std::ffi::OsString;
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

/// The broker's socket, relative to the home directory.
pub(crate) const SOCKET: &str = ".config/abc/agent-broker/broker.sock";

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

/// The broker's socket for the home directory `home`.
pub(crate) fn socket(home: &Path) -> PathBuf {
    home.join(SOCKET)
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
    answer_all(clients, &mint, &Mutex::new(log), MAX_CLIENTS);
    Ok(())
}

/// Answers each of `clients` with a token from `mint`, each on its own thread and at most `max` at
/// once, noting each in `log`. A client that fails or comes while `max` are being served is noted
/// and refused.
fn answer_all(
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
                        log,
                        Err(anyhow!(error).context("could not accept a connection")),
                    );
                    continue;
                }
            };
            if serving.fetch_add(1, Ordering::SeqCst) >= max {
                serving.fetch_sub(1, Ordering::SeqCst);
                refuse(&mut client, "the broker is busy; try again");
                note(log, Err(anyhow!("{max} clients are being served already")));
                continue;
            }
            let serving = &serving;
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
        Ok(scope) => format!("broker: minted a {} token", scope.name()),
        Err(error) => format!("broker: refused a request: {error:#}"),
    };
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
            log.contains("broker: refused a request: 1 clients are being served already"),
            "{log}"
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
    fn listens_under_the_home_directory() {
        assert_eq!(
            socket(Path::new("/home/a")),
            Path::new("/home/a").join(SOCKET)
        );
    }

    #[test]
    fn serves_on_a_socket_only_its_owner_reaches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-broker/b.sock");
        let served = path.clone();
        std::thread::spawn(move || {
            serve(
                &served,
                |scope| match scope {
                    Scope::Git => Ok("ghs_git".to_owned()),
                    Scope::Gh => anyhow::bail!("no gh today"),
                },
                &mut std::io::sink(),
            )
        });
        while UnixStream::connect(&path).is_err() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(request(&path, Scope::Git).unwrap(), "ghs_git");
        let refused = format!("{:#}", request(&path, Scope::Gh).unwrap_err());
        assert!(refused.contains("no gh today"), "{refused}");
        let mode = |file: &Path| std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "the socket");
        assert_eq!(mode(path.parent().unwrap()), 0o700, "its directory");
        let again = format!(
            "{:#}",
            serve(&path, |_| Ok(String::new()), &mut std::io::sink()).unwrap_err()
        );
        assert!(again.contains("already listening"), "{again}");
    }

    #[test]
    fn replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let served = path.clone();
        std::thread::spawn(move || {
            serve(&served, |_| Ok("ghs_x".to_owned()), &mut std::io::sink())
        });
        let token = loop {
            if let Ok(token) = request(&path, Scope::Gh) {
                break token;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(token, "ghs_x");
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
        let path = dir.path().join("b.sock");
        let served = path.clone();
        std::thread::spawn(move || {
            serve(&served, |_| Ok("ghs_v".to_owned()), &mut std::io::sink())
        });
        while UnixStream::connect(&path).is_err() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let named = |name: &str| (name == super::ENV).then(|| path.clone().into_os_string());
        assert_eq!(super::token_from(named, Scope::Gh).unwrap(), "ghs_v");
    }
}
