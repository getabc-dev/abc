//! `abc server setup`: from an installed abc to a server that answers on the tailnet.
//!
//! Each step looks before it acts, so running setup again repairs what is missing or changed and
//! duplicates nothing:
//!
//! 1. the home and its private directories, and every socket path checked against the limit;
//! 2. `config.toml`, written from abc's install the first time, and its programs checked;
//! 3. Postgres: initdb unless a cluster is there, the service, the database;
//! 4. tailscaled: the service, then `tailscale up --hostname=abc` unless the node runs, with the
//!    login link opened in the browser, and the node's name read into the public URL;
//! 5. the root step ([`crate::root`]), only for what is missing, then checked;
//! 6. abc server's service, and its readiness.
//!
//! The claim page comes last, from the binary, which asks the store for a setup code.

use std::ffi::OsString;
use std::path::PathBuf;

use abc_protocol::url::PublicUrl;

use crate::config::Config;
use crate::ops;
use crate::postgres::Postgres;
use crate::process::{Invocation, find_on_path};
use crate::programs::Programs;
use crate::root::{Linger, RootStep};
use crate::services::{Part, Service, Services};
use crate::tailscale::Tailscale;
use crate::{Console, Host, HostError, Os, Server};

/// What setup was asked for beyond the host.
#[derive(Debug, Clone)]
pub struct SetupOptions {
    /// The running abc binary, which says where abc is installed.
    pub exe: PathBuf,
    /// A file holding a Tailscale auth key, to log abc's node in without a browser
    /// (decision 53's alternative).
    pub auth_key: Option<PathBuf>,
}

/// The stack setup leaves running.
#[derive(Debug, Clone)]
pub struct Stack {
    /// The https URL Serve answers on.
    pub public_url: PublicUrl,
    /// The URL abc server connects to Postgres with: a Unix socket, no password.
    pub database_url: String,
}

/// Sets up abc's server stack on `host`, or repairs it.
///
/// # Errors
///
/// [`HostError`], each naming its fix.
pub fn setup(
    server: &Server,
    options: &SetupOptions,
    console: &mut Console<'_>,
) -> Result<Stack, HostError> {
    let (host, home) = (&server.host, &server.home);
    home.check_paths(host.os)?;
    home.create()?;
    let mut config = match Config::load(home)? {
        Some(config) => config,
        None => Config::new(Programs::from_install(&options.exe)),
    };
    config.programs.check(&home.config())?;
    config.save(home)?;
    console.line(&format!("abc keeps its server in {}", home.dir().display()));
    let services = Services::new(host, home.logs());

    let postgres = Postgres::new(home, &config.programs);
    if postgres.init()? {
        console.line("Created abc's Postgres 18 cluster.");
    }
    services.install(&postgres_service(&postgres))?;
    stop_unless_ready(
        &services,
        Part::Postgres,
        postgres.wait_ready(
            &host.timing,
            &format!(
                "read its log ({}), then run abc server setup again",
                services.log_hint(Part::Postgres)
            ),
        ),
    )?;
    postgres.ensure_database()?;
    console.line(&format!(
        "abc's Postgres is running, on {} only.",
        home.postgres_socket().display()
    ));

    let tailscale = Tailscale::new(home, &config.programs);
    services.install(&tailscaled_service(home, &tailscale))?;
    let mut node = stop_unless_ready(
        &services,
        Part::Tailscaled,
        tailscale.wait_answering(&host.timing),
    )?;
    if !node.running() {
        node = tailscale.log_in(
            console,
            &host.timing,
            options.auth_key.as_deref(),
            &mut |said, url| open_in_browser(host, said, url),
        )?;
    }
    let (Some(dns_name), Some(public_url)) = (node.dns_name(), node.public_url()) else {
        return Err(HostError::TailscaleAnswer {
            command: tailscale.cli().args(["status", "--json"]).shown(),
            reason: "the running node has no DNS name; turn on MagicDNS for your tailnet"
                .to_owned(),
        });
    };
    console.line(&format!("abc's node is on your tailnet as {dns_name}."));
    config.public_url = Some(public_url.clone());
    config.save(home)?;

    root_step(server, &tailscale, dns_name, console)?;

    services.install(&server_service(home, &config, &public_url, &postgres))?;
    stop_unless_ready(
        &services,
        Part::Server,
        ops::wait_ready(
            config.ops_addr,
            &host.timing,
            &format!(
                "read its log ({}), then run abc server setup again",
                services.log_hint(Part::Server)
            ),
        ),
    )?;
    console.line(&format!("abc server is ready at {public_url}."));
    Ok(Stack {
        public_url,
        database_url: postgres.database_url(),
    })
}

/// `waited`, the wait for `part` to become ready; when it failed, `part`'s service is stopped first,
/// so launchd's `KeepAlive` or systemd's restarts do not run a failing program over and over unseen,
/// and the error says so. Setup starts it again.
fn stop_unless_ready<T>(
    services: &Services<'_>,
    part: Part,
    waited: Result<T, HostError>,
) -> Result<T, HostError> {
    waited.map_err(|error| {
        let stopped = services.stop(part).is_ok();
        error.with_service_stopped(part.title(), stopped)
    })
}

/// Runs the root step for whatever is missing, then checks it took.
fn root_step(
    server: &Server,
    tailscale: &Tailscale<'_>,
    dns_name: &str,
    console: &mut Console<'_>,
) -> Result<(), HostError> {
    let host = &server.host;
    let wanted = tailscale.wanted_target();
    let serve_set = tailscale.serve_target(dns_name)?.as_deref() == Some(wanted.as_str());
    let Some(step) = RootStep::of(
        missing_linger(host)?,
        (!serve_set).then(|| tailscale.serve_command()),
    ) else {
        return Ok(());
    };
    let command = run_as_root(host, &step, SETUP, console)?;
    if tailscale.serve_target(dns_name)?.as_deref() != Some(wanted.as_str()) {
        return Err(HostError::ServeNotSet {
            socket: server.home.abc_socket(),
            command: command.shown(),
        });
    }
    check_linger_took(&step, &command)?;
    console.line(&format!("Serve on abc's node points at {wanted}."));
    Ok(())
}

/// The command that sets the stack up, which a failed root step says to run again.
const SETUP: &str = "abc server setup";

/// Lingering for this host's user, when it is off on Linux: their services then stop at logout
/// and do not start at boot. macOS has no such thing.
pub(crate) fn missing_linger(host: &Host) -> Result<Option<Linger>, HostError> {
    match host.os {
        Os::MacOs => Ok(None),
        Os::Linux => {
            let loginctl = loginctl(host)?;
            Ok((!lingering(&loginctl, host.uid)).then_some(Linger {
                loginctl,
                uid: host.uid,
            }))
        }
    }
}

/// Says what `step` does, then runs it under `sudo`, where the person types their password.
/// Returns the command it ran. A failure says to run it by hand, then `again`.
pub(crate) fn run_as_root(
    host: &Host,
    step: &RootStep,
    again: &str,
    console: &mut Console<'_>,
) -> Result<Invocation, HostError> {
    let sudo = find_on_path(&host.path, "sudo").ok_or(HostError::SystemProgramMissing {
        program: "sudo",
        needed_for: "for the one step only root may take",
        fix: "install sudo, or run the command below as root yourself",
    })?;
    for line in step.explanation(&sudo) {
        console.line(&line);
    }
    let command = step.command(&sudo);
    command.interactive(&format!(
        "run `{}` yourself, then {again} again",
        command.shown()
    ))?;
    Ok(command)
}

/// Fails unless the lingering `step` enabled, if any, is on now that `command` ran.
pub(crate) fn check_linger_took(step: &RootStep, command: &Invocation) -> Result<(), HostError> {
    if let RootStep::Linger(linger) | RootStep::Both(linger, _) = step
        && !lingering(&linger.loginctl, linger.uid)
    {
        return Err(HostError::Failed {
            command: command.shown(),
            status: "lingering is still off".to_owned(),
            output: String::new(),
            fix: format!("run `sudo loginctl enable-linger {}` yourself", linger.uid),
        });
    }
    Ok(())
}

/// `loginctl`, which Linux's lingering needs.
pub(crate) fn loginctl(host: &Host) -> Result<PathBuf, HostError> {
    find_on_path(&host.path, "loginctl").ok_or(HostError::SystemProgramMissing {
        program: "loginctl",
        needed_for: "to keep its services running while you are logged out",
        fix: "abc runs as systemd user services on Linux; use a distribution with systemd",
    })
}

/// Whether `uid`'s services run without a login.
pub(crate) fn lingering(loginctl: &std::path::Path, uid: u32) -> bool {
    Invocation::new(loginctl)
        .args([
            "show-user",
            uid.to_string().as_str(),
            "--property=Linger",
            "--value",
        ])
        .run("")
        .is_ok_and(|said| said.trim() == "yes")
}

/// Opens `url` in the person's browser, and prints it for when that fails.
pub fn open_in_browser(host: &Host, console: &mut Console<'_>, url: &str) {
    let launcher = match host.os {
        Os::MacOs => "open",
        Os::Linux => "xdg-open",
    };
    let opened = find_on_path(&host.path, launcher).is_some_and(|program| {
        Invocation::new(program)
            .arg(url)
            .output("")
            .is_ok_and(|output| output.status.success())
    });
    if opened {
        console.line(&format!(
            "  {url}\n  (opened in your browser; if it did not open, visit it)"
        ));
    } else {
        console.line(&format!("  {url}\n  (open it in a browser)"));
    }
}

/// The three services, from the configuration.
pub(crate) fn definitions(server: &Server, config: &Config) -> Result<[Service; 3], HostError> {
    let home = &server.home;
    let public_url = config
        .public_url
        .as_ref()
        .ok_or_else(|| HostError::NotSetUp {
            path: home.config(),
        })?;
    let postgres = Postgres::new(home, &config.programs);
    let tailscale = Tailscale::new(home, &config.programs);
    Ok([
        postgres_service(&postgres),
        tailscaled_service(home, &tailscale),
        server_service(home, config, public_url, &postgres),
    ])
}

// Each service states the environment its program needs: launchd and systemd start it with next to
// none, and nothing of the person's own shell reaches it.

/// Postgres with `LC_ALL=C`: on macOS the postmaster refuses to start with no locale in its
/// environment ("postmaster became multithreaded during startup"). The cluster is initdb'd with
/// `--no-locale`, so C is its collation and its messages' language anyway.
fn postgres_service(postgres: &Postgres<'_>) -> Service {
    Service {
        part: Part::Postgres,
        command: postgres.server_command(),
        env: vec![("LC_ALL".to_owned(), "C".to_owned())],
    }
}

/// tailscaled with its logs in abc's own Tailscale directory: without `TS_LOGS_DIR`, it keeps them
/// in the system's (`/var/db/tailscale`, `/var/lib/tailscale`) when that exists, beside the
/// person's own Tailscale.
fn tailscaled_service(home: &crate::ServerHome, tailscale: &Tailscale<'_>) -> Service {
    Service {
        part: Part::Tailscaled,
        command: tailscale.daemon_command(),
        env: vec![(
            "TS_LOGS_DIR".to_owned(),
            home.tailscale().to_string_lossy().into_owned(),
        )],
    }
}

fn server_service(
    home: &crate::ServerHome,
    config: &Config,
    public_url: &PublicUrl,
    postgres: &Postgres<'_>,
) -> Service {
    let command: Vec<OsString> = vec![
        config.programs.abc.clone().into_os_string(),
        "server".into(),
        "--sidecar-socket".into(),
        home.abc_socket().into_os_string(),
        "--tailscaled-socket".into(),
        home.tailscaled_socket().into_os_string(),
        "--ops-addr".into(),
        config.ops_addr.to_string().into(),
        "--public-url".into(),
        public_url.origin().into(),
    ];
    Service {
        part: Part::Server,
        command,
        env: vec![("ABC_DATABASE_URL".to_owned(), postgres.database_url())],
    }
}
