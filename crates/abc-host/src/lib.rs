//! abc server's own stack on its host, and the commands that set it up and look after it:
//! `abc server setup`, `status`, `upgrade`, `backup`, `restore` and `uninstall` (decision 51);
//! and a worker's service, which keeps `abc worker` running on its machine ([`worker`]).
//!
//! ```text
//! Browser ──tailnet──▶ abc's tailscaled (node `abc`, userspace, unprivileged)
//!                         │ Serve: https://abc.<tailnet>.ts.net/ → unix:<home>/run/abc.sock
//!                         ▼
//!                      abc server ──unix:<home>/run/.s.PGSQL.5432──▶ abc's private Postgres 18
//! ```
//!
//! Three services run the stack as the user who ran setup: launchd agents on macOS, systemd user
//! units on Linux ([`services`]). Nothing listens on TCP but abc's health and readiness, on
//! loopback ([`config::Config::ops_addr`]). The user's own Tailscale, app or daemon, is never
//! touched: every call to Tailscale names abc's own socket, and the programs come from abc's
//! install or its configuration, never from the `PATH` ([`programs`]).
//!
//! Root is used once, for the one thing only root may do: point Serve at a Unix socket. On Linux
//! the same step lets the user's services run while they are logged out (lingering). [`root`]
//! says what it runs before it asks.
//!
//! A [`Host`] is the machine as its user sees it: its service manager, its system programs and the
//! user's home. A [`Server`] is a host with abc server's home on it; the worker's commands need
//! only the host and the worker's own home, so a machine that only works needs no server home.
//!
//! Every function here runs other programs and waits for them; none is async. What only the
//! store knows (a setup code, whether the server is claimed) the binary asks the store itself.

pub mod config;
mod error;
pub mod home;
mod manage;
mod ops;
mod postgres;
mod process;
pub mod programs;
pub mod root;
pub mod services;
mod setup;
mod status;
mod tailscale;
mod worker;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub use error::HostError;
pub use home::ServerHome;
pub use manage::{Backup, backup, restore, uninstall, upgrade};
pub use process::quote as shell_word;
pub use setup::{SetupOptions, Stack, open_in_browser, setup};
pub use status::{Check, Report, status};
pub use worker::{WorkerService, WorkerStatus, install_worker, uninstall_worker, worker_status};

/// The URL abc server connects to its private Postgres with, from the configuration setup wrote.
///
/// # Errors
///
/// [`HostError::NotSetUp`] when there is no server here, and as [`config::Config::load`].
pub fn database_url(server: &Server) -> Result<String, HostError> {
    let config = config::Config::require(&server.home)?;
    Ok(postgres::Postgres::new(&server.home, &config.programs).database_url())
}

/// The operating systems abc's server and workers run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// macOS: launchd agents.
    MacOs,
    /// Linux: systemd user units, with lingering.
    Linux,
}

impl Os {
    /// The operating system this build runs on.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    /// The longest Unix socket path, in bytes, this system can bind: `sun_path` less its
    /// terminating NUL (104 bytes on macOS, 108 on Linux).
    #[must_use]
    pub const fn socket_path_limit(self) -> usize {
        match self {
            Self::MacOs => 103,
            Self::Linux => 107,
        }
    }

    /// Its name, as people know it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::MacOs => "macOS",
            Self::Linux => "Linux",
        }
    }
}

/// How long each wait lasts, and how often it looks.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Between two looks.
    pub poll: Duration,
    /// For Postgres to accept connections.
    pub postgres: Duration,
    /// For abc's tailscaled to answer on its socket.
    pub tailscaled: Duration,
    /// For the person to log abc's node in through the link.
    pub login: Duration,
    /// For abc server to report ready.
    pub server: Duration,
    /// For a service manager to finish unloading a service before it loads it again.
    pub service: Duration,
}

impl Timing {
    /// The waits a person sees.
    #[must_use]
    pub const fn production() -> Self {
        Self {
            poll: Duration::from_millis(250),
            postgres: Duration::from_secs(60),
            tailscaled: Duration::from_secs(30),
            login: Duration::from_mins(15),
            server: Duration::from_secs(120),
            service: Duration::from_secs(10),
        }
    }
}

/// The machine abc's services run on, as the person running a command sees it.
#[derive(Debug, Clone)]
pub struct Host {
    /// The operating system, which picks the service manager.
    pub os: Os,
    /// The user's home directory (`HOME`), which their services are given as theirs.
    pub user_home: PathBuf,
    /// Where service definitions go: `~/Library/LaunchAgents` on macOS, the systemd user unit
    /// directory on Linux.
    pub services_dir: PathBuf,
    /// The user's id: launchd's domain is `gui/<uid>`, and lingering is enabled for it.
    pub uid: u32,
    /// The directories searched for the system's own programs: `sudo`, `launchctl`,
    /// `systemctl`, `loginctl`, `open` and `xdg-open`. Never for Tailscale's or Postgres's.
    pub path: OsString,
    /// How long each wait lasts.
    pub timing: Timing,
}

impl Host {
    /// This machine, read through `lookup` (the process environment in the binary): the user's
    /// home from `HOME`, the service directory from it (and `XDG_CONFIG_HOME` on Linux), and
    /// `PATH`.
    ///
    /// # Errors
    ///
    /// [`HostError::NoUserHome`] when `HOME` is not set.
    pub fn discover(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, HostError> {
        let os = Os::current();
        let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let user_home = set("HOME")
            .map(PathBuf::from)
            .ok_or(HostError::NoUserHome)?;
        let services_dir = match os {
            Os::MacOs => user_home.join("Library/LaunchAgents"),
            Os::Linux => set("XDG_CONFIG_HOME")
                .map_or_else(|| user_home.join(".config"), PathBuf::from)
                .join("systemd/user"),
        };
        Ok(Self {
            os,
            user_home,
            services_dir,
            uid: rustix::process::getuid().as_raw(),
            path: set("PATH").unwrap_or_default(),
            timing: Timing::production(),
        })
    }
}

/// abc server's stack on a host: the machine and the server's home on it.
#[derive(Debug, Clone)]
pub struct Server {
    /// The machine.
    pub host: Host,
    /// abc server's home: its data, sockets and configuration.
    pub home: ServerHome,
}

impl Server {
    /// This machine and its server home, read through `lookup`: the host as
    /// [`Host::discover`] reads it, and the home from `ABC_SERVER_HOME` or this OS's place for it.
    ///
    /// # Errors
    ///
    /// As [`Host::discover`] and [`ServerHome::discover`].
    pub fn discover(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, HostError> {
        let host = Host::discover(&lookup)?;
        let home = ServerHome::discover(host.os, |name| {
            lookup(name).filter(|value| !value.is_empty())
        })?;
        Ok(Self { host, home })
    }
}

/// Where the commands write what the person reads: the terminal in the binary.
pub struct Console<'out> {
    out: &'out mut dyn Write,
}

impl std::fmt::Debug for Console<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Console")
    }
}

impl<'out> Console<'out> {
    /// Writes to `out`.
    pub fn new(out: &'out mut dyn Write) -> Self {
        Self { out }
    }

    /// Writes `text` and a line break. A terminal that is gone cannot be told anything, so a
    /// failed write is dropped.
    pub fn line(&mut self, text: &str) {
        let _unwritten = writeln!(self.out, "{text}").and_then(|()| self.out.flush());
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    use super::{Console, Host, HostError, Os, Server, Timing};

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn this_build_knows_its_system_and_its_socket_limit() {
        let os = Os::current();
        assert_eq!(os == Os::MacOs, cfg!(target_os = "macos"), "{os:?}");
        assert_eq!(Os::MacOs.name(), "macOS", "names");
        assert_eq!(Os::Linux.name(), "Linux", "names");
    }

    #[test]
    fn the_host_is_read_from_the_environment() {
        let host = Host::discover(env(&[
            ("HOME", "/u/ada"),
            ("PATH", "/bin:/usr/bin"),
            ("XDG_CONFIG_HOME", "/cfg"),
        ]))
        .unwrap();
        assert_eq!(host.os, Os::current(), "this system");
        assert_eq!(host.path, OsString::from("/bin:/usr/bin"), "PATH");
        assert_eq!(host.user_home, Path::new("/u/ada"), "HOME");
        assert_eq!(
            host.uid,
            rustix::process::getuid().as_raw(),
            "this process's user"
        );
        let services = match host.os {
            Os::MacOs => "/u/ada/Library/LaunchAgents",
            Os::Linux => "/cfg/systemd/user",
        };
        assert_eq!(host.services_dir, Path::new(services), "services");
        assert_eq!(
            host.timing.login,
            Duration::from_mins(15),
            "a person's pace"
        );
        let no_xdg = Host::discover(env(&[("HOME", "/u/ada"), ("XDG_CONFIG_HOME", "")])).unwrap();
        if no_xdg.os == Os::Linux {
            assert_eq!(
                no_xdg.services_dir,
                Path::new("/u/ada/.config/systemd/user"),
                "an empty XDG_CONFIG_HOME counts as unset"
            );
        }
        assert_eq!(no_xdg.path, OsString::new(), "no PATH, no system programs");
        assert!(
            matches!(Host::discover(env(&[])), Err(HostError::NoUserHome)),
            "no HOME"
        );
        assert!(
            matches!(
                Host::discover(env(&[("HOME", "")])),
                Err(HostError::NoUserHome)
            ),
            "an empty HOME counts as unset"
        );
    }

    #[test]
    fn a_server_is_the_host_with_the_servers_home_on_it() {
        let server = Server::discover(env(&[
            ("HOME", "/u/ada"),
            ("XDG_DATA_HOME", "/data"),
            ("ABC_SERVER_HOME", ""),
        ]))
        .unwrap();
        let home = match server.host.os {
            Os::MacOs => "/u/ada/Library/Application Support/abc/server",
            Os::Linux => "/data/abc/server",
        };
        assert_eq!(server.home.dir(), Path::new(home), "home");
        let set =
            Server::discover(env(&[("HOME", "/u/ada"), ("ABC_SERVER_HOME", "/srv/abc")])).unwrap();
        assert_eq!(set.home.dir(), Path::new("/srv/abc"), "ABC_SERVER_HOME");
        assert!(
            matches!(
                Server::discover(env(&[("ABC_SERVER_HOME", "/srv/abc")])),
                Err(HostError::NoUserHome)
            ),
            "the services still need HOME"
        );
    }

    #[test]
    fn production_waits_leave_a_person_time() {
        let timing = Timing::production();
        assert!(timing.poll < timing.service, "{timing:?}");
        assert!(timing.postgres >= Duration::from_secs(60), "{timing:?}");
        assert!(timing.server >= Duration::from_secs(120), "{timing:?}");
    }

    #[test]
    fn the_console_writes_lines() {
        let mut out = Vec::new();
        let mut console = Console::new(&mut out);
        assert_eq!(format!("{console:?}"), "Console", "debug");
        console.line("one");
        console.line("two");
        assert_eq!(out, b"one\ntwo\n", "a line each");
    }
}
