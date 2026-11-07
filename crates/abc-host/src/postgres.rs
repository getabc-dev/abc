//! abc's private Postgres 18: one cluster in the home, reachable only through its Unix socket.
//!
//! initdb makes the cluster with one role, `abc`, trusted over the socket and refused over TCP;
//! the service starts `postgres` with no TCP listener and its socket in the home's `run/`, a
//! directory only this user can enter. So whoever reaches the socket is this user, which is what
//! `trust` means here.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::process::Invocation;
use crate::programs::{POSTGRES_MAJOR, Programs};
use crate::{HostError, ServerHome, Timing};

/// The role abc connects as.
pub(crate) const ROLE: &str = "abc";
/// abc's database.
pub(crate) const DATABASE: &str = "abc";
/// The database a restore loads into before it takes [`DATABASE`]'s place.
const RESTORING: &str = "abc_restoring";
/// The port, which names the socket file; nothing listens on it over TCP.
const PORT: &str = "5432";

/// abc's Postgres on this host.
#[derive(Debug)]
pub(crate) struct Postgres<'a> {
    home: &'a ServerHome,
    programs: &'a Programs,
}

impl<'a> Postgres<'a> {
    pub(crate) const fn new(home: &'a ServerHome, programs: &'a Programs) -> Self {
        Self { home, programs }
    }

    fn program(&self, name: &str) -> Invocation {
        Invocation::new(self.programs.postgres_program(name))
    }

    /// The service's command line: the cluster, no TCP, the socket in `run/`, private to us.
    pub(crate) fn server_command(&self) -> Vec<OsString> {
        let mut sockets = OsString::from("unix_socket_directories=\"");
        sockets.push(self.home.run());
        sockets.push("\"");
        [
            self.programs.postgres_program("postgres").into_os_string(),
            "-D".into(),
            self.home.postgres().into_os_string(),
            "-p".into(),
            PORT.into(),
            "-c".into(),
            "listen_addresses=".into(),
            "-c".into(),
            sockets,
            "-c".into(),
            "unix_socket_permissions=0700".into(),
        ]
        .into()
    }

    /// Creates the cluster unless one is there; refuses one of another major version.
    /// Returns whether it created one.
    pub(crate) fn init(&self) -> Result<bool, HostError> {
        let cluster = self.home.postgres();
        let version_file = cluster.join("PG_VERSION");
        match fs::read_to_string(&version_file) {
            Ok(found) if found.trim() == POSTGRES_MAJOR => return Ok(false),
            Ok(found) => {
                return Err(HostError::ClusterVersion {
                    path: cluster,
                    found: found.trim().to_owned(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(HostError::io(&version_file, source)),
        }
        self.program("initdb")
            .arg("--pgdata")
            .arg(&cluster)
            .args([
                "--username",
                ROLE,
                "--auth-local=trust",
                "--auth-host=reject",
                "--encoding=UTF8",
                "--no-locale",
            ])
            .run(&format!(
                "read initdb's output above; if {} is left half made, delete it and run abc \
                 server setup again",
                cluster.display()
            ))?;
        Ok(true)
    }

    /// Whether it accepts connections on its socket.
    pub(crate) fn ready(&self) -> bool {
        self.program("pg_isready")
            .arg("--host")
            .arg(self.home.run())
            .args([
                "--port",
                PORT,
                "--username",
                ROLE,
                "--dbname",
                "postgres",
                "--timeout=1",
            ])
            .output("")
            .is_ok_and(|output| output.status.success())
    }

    /// Waits until it accepts connections.
    pub(crate) fn wait_ready(&self, timing: &Timing, fix: &str) -> Result<(), HostError> {
        let start = Instant::now();
        loop {
            if self.ready() {
                return Ok(());
            }
            if start.elapsed() >= timing.postgres {
                return Err(HostError::Timeout {
                    what: "abc's Postgres accepting connections".to_owned(),
                    seconds: timing.postgres.as_secs(),
                    fix: fix.to_owned(),
                });
            }
            std::thread::sleep(timing.poll);
        }
    }

    /// `psql` on `database`, running `sql` and returning what it prints, unaligned.
    fn psql(&self, database: &str, sql: &str, fix: &str) -> Result<String, HostError> {
        self.program("psql")
            .args(["--no-psqlrc", "--no-align", "--tuples-only", "--quiet"])
            .args(["--set", "ON_ERROR_STOP=1"])
            .arg("--host")
            .arg(self.home.run())
            .args(["--port", PORT, "--username", ROLE, "--dbname", database])
            .args(["--command", sql])
            .run(fix)
    }

    fn database_exists(&self, name: &str) -> Result<bool, HostError> {
        let found = self.psql(
            "postgres",
            &format!("SELECT 1 FROM pg_database WHERE datname = '{name}'"),
            "check that abc's Postgres is running: abc server status",
        )?;
        Ok(found.trim() == "1")
    }

    /// Creates abc's database unless it is there. Returns whether it created it.
    pub(crate) fn ensure_database(&self) -> Result<bool, HostError> {
        if self.database_exists(DATABASE)? {
            return Ok(false);
        }
        self.psql(
            "postgres",
            &format!("CREATE DATABASE {DATABASE}"),
            "check that abc's Postgres is running: abc server status",
        )?;
        Ok(true)
    }

    /// The URL abc server connects with: the socket's directory as `host`, percent-encoded.
    pub(crate) fn database_url(&self) -> String {
        format!(
            "postgres://{ROLE}@localhost/{DATABASE}?host={}&port={PORT}",
            percent_encode(&self.home.run())
        )
    }

    /// Dumps abc's database to `file`, in `pg_dump`'s custom format.
    pub(crate) fn dump(&self, file: &Path) -> Result<(), HostError> {
        self.program("pg_dump")
            .args(["--format=custom", "--no-owner"])
            .arg("--file")
            .arg(file)
            .arg("--host")
            .arg(self.home.run())
            .args(["--port", PORT, "--username", ROLE, "--dbname", DATABASE])
            .run("check that abc's Postgres is running (abc server status), then try again")
            .map(|_printed| ())
    }

    /// Checks that `file` is a dump `pg_restore` can read.
    pub(crate) fn check_dump(&self, file: &Path) -> Result<(), HostError> {
        self.program("pg_restore")
            .arg("--list")
            .arg(file)
            .run("name a file abc server backup wrote")
            .map(|_listing| ())
    }

    /// Loads `file` into a fresh database, then puts it in place of abc's. A failed load leaves
    /// abc's database as it was.
    pub(crate) fn restore(&self, file: &Path) -> Result<(), HostError> {
        let fix = "check that abc's Postgres is running: abc server status";
        self.psql(
            "postgres",
            &format!("DROP DATABASE IF EXISTS {RESTORING} WITH (FORCE)"),
            fix,
        )?;
        self.psql("postgres", &format!("CREATE DATABASE {RESTORING}"), fix)?;
        let loaded = self
            .program("pg_restore")
            .args(["--exit-on-error", "--single-transaction", "--no-owner"])
            .arg("--host")
            .arg(self.home.run())
            .args(["--port", PORT, "--username", ROLE, "--dbname", RESTORING])
            .arg(file)
            .run("abc's database is unchanged; check that the file is a whole backup of abc");
        if let Err(error) = loaded {
            // The half-loaded copy is of no use; abc's database was never touched.
            let _dropped = self.psql(
                "postgres",
                &format!("DROP DATABASE IF EXISTS {RESTORING}"),
                fix,
            );
            return Err(error);
        }
        self.psql(
            "postgres",
            &format!("DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)"),
            fix,
        )?;
        self.psql(
            "postgres",
            &format!("ALTER DATABASE {RESTORING} RENAME TO {DATABASE}"),
            fix,
        )
        .map(|_printed| ())
    }
}

/// `path` percent-encoded for a URL's query: every byte but the unreserved ones and `/`.
fn percent_encode(path: &Path) -> String {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut encoded = String::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _infallible = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// The default name of a backup taken at `now`.
pub(crate) fn backup_name(now: time::OffsetDateTime) -> PathBuf {
    let utc = now.to_offset(time::UtcOffset::UTC);
    PathBuf::from(format!(
        "abc-{:04}{:02}{:02}-{:02}{:02}{:02}.dump",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second()
    ))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Postgres, backup_name, percent_encode};
    use crate::ServerHome;
    use crate::programs::Programs;

    fn programs() -> Programs {
        Programs {
            abc: PathBuf::from("/x/abc"),
            tailscaled: PathBuf::from("/x/tailscaled"),
            tailscale: PathBuf::from("/x/tailscale"),
            postgres: PathBuf::from("/pg/bin"),
        }
    }

    #[test]
    fn the_server_runs_with_no_tcp_and_its_socket_in_the_home() {
        let home = ServerHome::at(PathBuf::from(
            "/Users/ada/Library/Application Support/abc/server",
        ));
        let programs = programs();
        let command = Postgres::new(&home, &programs).server_command();
        let shown: Vec<String> = command
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            shown,
            [
                "/pg/bin/postgres",
                "-D",
                "/Users/ada/Library/Application Support/abc/server/postgres",
                "-p",
                "5432",
                "-c",
                "listen_addresses=",
                "-c",
                "unix_socket_directories=\"/Users/ada/Library/Application Support/abc/server/run\"",
                "-c",
                "unix_socket_permissions=0700",
            ],
            "the cluster, no TCP, the quoted socket directory"
        );
    }

    #[test]
    fn the_database_url_names_the_socket_directory_encoded() {
        let home = ServerHome::at(PathBuf::from(
            "/Users/ada/Library/Application Support/abc/server",
        ));
        let programs = programs();
        assert_eq!(
            Postgres::new(&home, &programs).database_url(),
            "postgres://abc@localhost/abc?host=%2FUsers%2Fada%2FLibrary%2FApplication%20Support%2Fabc%2Fserver%2Frun&port=5432",
            "a Unix socket, through host"
        );
        assert_eq!(
            percent_encode(Path::new("a-b.c_d~e&f")),
            "a-b.c_d~e%26f",
            "unreserved stay"
        );
    }

    #[test]
    fn backups_are_named_by_their_utc_time() {
        let at = time::OffsetDateTime::from_unix_timestamp(1_793_000_000)
            .unwrap()
            .to_offset(time::UtcOffset::from_hms(5, 30, 0).unwrap());
        assert_eq!(
            backup_name(at),
            PathBuf::from("abc-20261026-073320.dump"),
            "UTC, sortable"
        );
    }
}
