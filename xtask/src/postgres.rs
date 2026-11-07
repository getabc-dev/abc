//! The warm Postgres container every machine's database tests share (D7), and the check that
//! each crate's committed sqlx query cache matches its queries.
//!
//! Database tests (`#[sqlx::test]`) create a fresh database per test on the server that
//! `ABC_TEST_DATABASE_URL` names. `.cargo/config.toml` points it at [`URL`], a Postgres 18 container that
//! [`warm`] starts once per machine and leaves running. CI runs the same image as a service on the
//! same port. An `ABC_TEST_DATABASE_URL` set to anything else is the caller's own server, and is
//! left alone.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};

/// The warm container's address on this machine.
pub(crate) const ADDRESS: &str = "127.0.0.1:55432";

/// The variable database tests read their server from. Not `DATABASE_URL`, so a developer's
/// own database can never receive test databases.
pub(crate) const TEST_DATABASE_URL: &str = "ABC_TEST_DATABASE_URL";

/// The `ABC_TEST_DATABASE_URL` `.cargo/config.toml` sets: the warm container's superuser, trusted
/// on loopback.
pub(crate) const URL: &str = "postgres://postgres@127.0.0.1:55432/postgres";

/// The warm container's name.
const CONTAINER: &str = "abc-postgres";

/// The image, pinned by digest. CI's service uses the same one.
const IMAGE: &str =
    "postgres:18.6@sha256:3a4264520e121176619f54ff867d12774bc81e95e7b84db58d63bccdb6144b81";

/// How long a starting container may take to accept connections.
const STARTUP: Duration = Duration::from_secs(60);

/// Makes sure the database tests have a server: the caller's own `ABC_TEST_DATABASE_URL`, or the warm
/// container, started if it is not running.
pub(crate) fn warm(out: &mut dyn Write) -> anyhow::Result<()> {
    let address: SocketAddr = ADDRESS.parse().context("the warm container's address")?;
    let database_url = std::env::var(TEST_DATABASE_URL).ok();
    ensure_running(
        Path::new("docker"),
        address,
        database_url.as_deref(),
        STARTUP,
        out,
    )
}

/// [`warm`], with the docker executable, the address, the configured URL and the patience given.
fn ensure_running(
    docker: &Path,
    address: SocketAddr,
    database_url: Option<&str>,
    patience: Duration,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if database_url.is_some_and(|url| url != URL) {
        writeln!(
            out,
            "xtask: database tests use the server ABC_TEST_DATABASE_URL names"
        )?;
        return Ok(());
    }
    if TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_ok() {
        writeln!(out, "xtask: database tests use the Postgres at {address}")?;
        return Ok(());
    }
    let inspect = Command::new(docker)
        .args([
            "container",
            "inspect",
            "--format",
            "{{.State.Running}}",
            CONTAINER,
        ])
        .stderr(Stdio::null())
        .output()
        .with_context(|| {
            "database tests need Postgres 18: install Docker so xtask can start its warm \
             container, or set ABC_TEST_DATABASE_URL to a Postgres 18 server"
        })?;
    let running = String::from_utf8_lossy(&inspect.stdout).trim() == "true";
    if !inspect.status.success() {
        writeln!(
            out,
            "xtask: starting the warm Postgres container {CONTAINER}"
        )?;
        docker_run(
            docker,
            &[
                "run",
                "--detach",
                "--name",
                CONTAINER,
                "--restart",
                "unless-stopped",
                "--publish",
                &format!("{address}:5432"),
                "--env",
                "POSTGRES_HOST_AUTH_METHOD=trust",
                "--tmpfs",
                "/var/lib/postgresql",
                IMAGE,
                // Test data is disposable: trade durability for speed, and allow many parallel
                // test databases.
                "-c",
                "fsync=off",
                "-c",
                "synchronous_commit=off",
                "-c",
                "full_page_writes=off",
                "-c",
                "max_connections=1000",
            ],
        )?;
    } else if !running {
        writeln!(
            out,
            "xtask: restarting the warm Postgres container {CONTAINER}"
        )?;
        docker_run(docker, &["start", CONTAINER])?;
    }
    let started = Instant::now();
    loop {
        let ready = Command::new(docker)
            .args([
                "exec",
                CONTAINER,
                "pg_isready",
                "--quiet",
                "--host",
                "127.0.0.1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if ready {
            return Ok(());
        }
        ensure!(
            started.elapsed() < patience,
            "the warm Postgres container {CONTAINER} did not accept connections within {}s; \
             see `docker logs {CONTAINER}`",
            patience.as_secs()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn docker_run(docker: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(docker)
        .args(args)
        .output()
        .context("could not run docker")?;
    if !output.status.success() {
        bail!(
            "docker {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Whether the workspace at `root` uses sqlx, so its tests need the database: its lockfile lists
/// the sqlx crate.
pub(crate) fn needed(root: &Path) -> bool {
    std::fs::read_to_string(root.join("Cargo.lock"))
        .is_ok_and(|lock| lock.contains("\nname = \"sqlx\"\n"))
}

/// `url` with its database replaced by `database`, keeping any query, such as the `host` of a
/// Unix-socket URL (`postgres://user@/db?host=/run/postgresql`).
pub(crate) fn with_database(url: &str, database: &str) -> anyhow::Result<String> {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(base, query)| (base, Some(query)));
    let authority = base
        .split_once("://")
        .map(|(scheme, rest)| {
            (
                scheme,
                rest.split_once('/')
                    .map_or(rest, |(authority, _)| authority),
            )
        })
        .with_context(|| {
            format!("{TEST_DATABASE_URL} must be a URL, such as postgres://host/postgres")
        })?;
    let (scheme, authority) = authority;
    Ok(match query {
        Some(query) => format!("{scheme}://{authority}/{database}?{query}"),
        None => format!("{scheme}://{authority}/{database}"),
    })
}

/// The commands that check the query cache of the crate in `dir`, with cargo-sqlx at `sqlx`: a
/// fresh database migrated from the crate's migrations, `sqlx prepare --check` over every target
/// and feature against it, then the database dropped.
pub(crate) fn query_cache_check(sqlx: &Path, dir: &Path, url: &str) -> [Command; 3] {
    let mut reset = Command::new(sqlx);
    reset
        .args(["sqlx", "database", "reset", "-y", "--source"])
        .arg(dir.join("migrations"))
        .args(["--database-url", url]);
    let mut prepare = Command::new(sqlx);
    prepare
        .args(["sqlx", "prepare", "--check", "--database-url", url])
        .args(["--", "--all-targets", "--all-features", "--locked"])
        .current_dir(dir);
    let mut drop = Command::new(sqlx);
    drop.args(["sqlx", "database", "drop", "-y", "--database-url", url]);
    [reset, prepare, drop]
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testing::script;

    /// An address nothing listens on.
    fn closed() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    /// A fake docker that logs its arguments to `log` and answers `inspect` with `state`
    /// (`missing`, `true` or `false`) and `exec` with `ready`'s exit status.
    fn docker(dir: &Path, state: &str, ready: bool) -> std::path::PathBuf {
        let path = dir.join("docker");
        let log = dir.join("log");
        let inspect = match state {
            "missing" => "exit 1".to_owned(),
            running => format!("echo {running}"),
        };
        let exec = if ready { "exit 0" } else { "exit 1" };
        script(
            &path,
            &format!(
                "echo \"$*\" >> {}\ncase \"$1\" in\n  container) {inspect} ;;\n  exec) {exec} ;;\n  *) exit 0 ;;\nesac\n",
                log.display()
            ),
        );
        path
    }

    fn calls(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split(' ').next().unwrap_or_default().to_owned())
            .collect()
    }

    fn run(docker: &Path, address: SocketAddr, url: Option<&str>) -> (anyhow::Result<()>, String) {
        let mut out = Vec::new();
        let result = ensure_running(docker, address, url, Duration::from_millis(300), &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn another_database_url_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let docker = docker(dir.path(), "missing", true);
        let (result, out) = run(&docker, closed(), Some("postgres://elsewhere/db"));
        result.unwrap();
        assert!(out.contains("ABC_TEST_DATABASE_URL names"), "{out}");
        assert!(calls(dir.path()).is_empty(), "docker is not touched");
    }

    #[test]
    fn warm_finds_the_test_server_this_checkout_configures() {
        let mut out = Vec::new();
        warm(&mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("database tests use"),
            "it says which server: {out}"
        );
    }

    #[test]
    fn a_listening_server_needs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let docker = docker(dir.path(), "missing", true);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (result, _) = run(&docker, listener.local_addr().unwrap(), Some(URL));
        result.unwrap();
        assert!(calls(dir.path()).is_empty(), "docker is not touched");
    }

    #[test]
    fn a_missing_container_is_created_and_awaited() {
        let dir = tempfile::tempdir().unwrap();
        let docker = docker(dir.path(), "missing", true);
        let address = closed();
        let (result, out) = run(&docker, address, None);
        result.unwrap();
        assert!(
            out.contains("starting the warm Postgres container"),
            "{out}"
        );
        assert_eq!(
            calls(dir.path()),
            ["container", "run", "exec"],
            "inspect, run, wait"
        );
        let log = fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(log.contains(&format!("--publish {address}:5432")), "{log}");
        assert!(log.contains(IMAGE), "the pinned image: {log}");
    }

    #[test]
    fn a_stopped_container_is_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let docker = docker(dir.path(), "false", true);
        let (result, out) = run(&docker, closed(), Some(URL));
        result.unwrap();
        assert!(out.contains("restarting"), "{out}");
        assert_eq!(
            calls(dir.path()),
            ["container", "start", "exec"],
            "inspect, start, wait"
        );
    }

    #[test]
    fn a_running_container_is_only_awaited() {
        let dir = tempfile::tempdir().unwrap();
        let docker = docker(dir.path(), "true", true);
        let (result, _) = run(&docker, closed(), Some(URL));
        result.unwrap();
        assert_eq!(calls(dir.path()), ["container", "exec"], "inspect, wait");
    }

    #[test]
    fn a_container_that_never_answers_fails_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let docker = docker(dir.path(), "true", false);
        let (result, _) = run(&docker, closed(), Some(URL));
        let error = result.unwrap_err().to_string();
        assert!(error.contains("docker logs abc-postgres"), "{error}");
        assert!(calls(dir.path()).len() > 2, "it kept waiting");
    }

    #[test]
    fn no_docker_explains_what_to_install() {
        let (result, _) = run(Path::new("/nonexistent/docker"), closed(), Some(URL));
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("install Docker"), "{error}");
    }

    #[test]
    fn a_failing_docker_command_reports_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        script(
            &docker,
            "case \"$1\" in\n  container) exit 1 ;;\n  *) echo 'no space left' >&2; exit 1 ;;\nesac\n",
        );
        let (result, _) = run(&docker, closed(), Some(URL));
        let error = result.unwrap_err().to_string();
        assert_eq!(
            error, "docker run failed: no space left",
            "the step and docker's words"
        );
    }

    #[test]
    fn a_workspace_needs_the_database_when_it_locks_sqlx() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!needed(dir.path()), "no lockfile");
        fs::write(
            dir.path().join("Cargo.lock"),
            "[[package]]\nname = \"sqlx-core\"\n",
        )
        .unwrap();
        assert!(!needed(dir.path()), "only a sibling crate");
        fs::write(
            dir.path().join("Cargo.lock"),
            "[[package]]\nname = \"sqlx\"\n",
        )
        .unwrap();
        assert!(needed(dir.path()), "sqlx itself");
        assert!(needed(&crate::testing::checkout()), "this workspace");
    }

    #[test]
    fn a_database_url_takes_another_database() {
        assert_eq!(
            with_database(URL, "abc_cache_1").unwrap(),
            "postgres://postgres@127.0.0.1:55432/abc_cache_1",
            "the server is kept"
        );
        assert!(
            with_database("postgres-no-path", "x").is_err(),
            "no database to replace"
        );
    }

    #[test]
    fn the_query_cache_check_resets_prepares_and_drops() {
        let commands =
            query_cache_check(Path::new("cargo-sqlx"), Path::new("crates/s"), "pg://x/d");
        let rendered: Vec<String> = commands
            .iter()
            .map(|command| {
                command
                    .get_args()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert_eq!(
            rendered,
            [
                "sqlx database reset -y --source crates/s/migrations --database-url pg://x/d",
                "sqlx prepare --check --database-url pg://x/d -- --all-targets --all-features --locked",
                "sqlx database drop -y --database-url pg://x/d",
            ],
            "fresh database, check, clean up"
        );
        assert_eq!(
            commands[1].get_current_dir(),
            Some(Path::new("crates/s")),
            "prepare runs in the crate"
        );
    }
}
