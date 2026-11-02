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
use std::time::Duration;

use anyhow::{Context as _, bail};

use crate::app::Scope;

/// The broker's socket, relative to the home directory. `.claude/settings.json` allows sandboxed
/// commands to connect to this path and to no other socket.
pub(crate) const SOCKET: &str = ".config/abc/agent-broker/broker.sock";

/// How long the broker waits for a client to send its request or take its answer.
const PATIENCE: Duration = Duration::from_secs(10);

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
    mint: impl Fn(Scope) -> anyhow::Result<String>,
    log: &mut impl Write,
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
    answer_all(clients, mint, log)
}

/// Answers each of `clients` in turn with a token from `mint`, noting each in `log`. A client that
/// fails is noted and the next one served.
fn answer_all<S: Read + Write>(
    clients: impl Iterator<Item = std::io::Result<S>>,
    mint: impl Fn(Scope) -> anyhow::Result<String>,
    log: &mut impl Write,
) -> anyhow::Result<()> {
    for client in clients {
        let outcome = client
            .context("could not accept a connection")
            .and_then(|client| answer(client, &mint));
        match outcome {
            Ok(scope) => writeln!(log, "broker: minted a {} token", scope.name())?,
            Err(error) => writeln!(log, "broker: refused a request: {error:#}")?,
        }
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

    use super::{SOCKET, answer, answer_all, exchange, request, socket};
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

    #[test]
    fn serves_one_client_after_another_and_logs_no_token() {
        let client = |request: &'static [u8]| {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(request).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            (client, server)
        };
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
        )
        .unwrap();
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
        assert_eq!(
            log,
            "broker: minted a git token\n\
             broker: refused a request: could not accept a connection: accept failed\n\
             broker: refused a request: no scope is named \"root\"\n",
            "every request is noted"
        );
        assert!(!log.contains("ghs_secret"), "and no token ever");
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
}
