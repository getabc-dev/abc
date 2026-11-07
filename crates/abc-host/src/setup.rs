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
use crate::{Console, Host, HostError, Os};

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
    host: &Host,
    options: &SetupOptions,
    console: &mut Console<'_>,
) -> Result<Stack, HostError> {
    let home = &host.home;
    home.check_paths(host.os)?;
    home.create()?;
    let mut config = match Config::load(home)? {
        Some(config) => config,
        None => Config::new(Programs::from_install(&options.exe)),
    };
    config.programs.check(&home.config())?;
    config.save(home)?;
    console.line(&format!("abc keeps its server in {}", home.dir().display()));
    let services = Services::new(host);

    let postgres = Postgres::new(home, &config.programs);
    if postgres.init()? {
        console.line("Created abc's Postgres 18 cluster.");
    }
    services.install(&postgres_service(&postgres))?;
    postgres.wait_ready(
        &host.timing,
        &format!(
            "read its log ({}), then run abc server setup again",
            services.log_hint(Part::Postgres)
        ),
    )?;
    postgres.ensure_database()?;
    console.line(&format!(
        "abc's Postgres is running, on {} only.",
        home.postgres_socket().display()
    ));

    let tailscale = Tailscale::new(home, &config.programs);
    services.install(&tailscaled_service(&tailscale))?;
    let mut node = tailscale.wait_answering(&host.timing)?;
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

    root_step(host, &tailscale, dns_name, console)?;

    services.install(&server_service(home, &config, &public_url, &postgres))?;
    ops::wait_ready(
        config.ops_addr,
        &host.timing,
        &format!(
            "read its log ({}), then run abc server setup again",
            services.log_hint(Part::Server)
        ),
    )?;
    console.line(&format!("abc server is ready at {public_url}."));
    Ok(Stack {
        public_url,
        database_url: postgres.database_url(),
    })
}

/// Runs the root step for whatever is missing, then checks it took.
fn root_step(
    host: &Host,
    tailscale: &Tailscale<'_>,
    dns_name: &str,
    console: &mut Console<'_>,
) -> Result<(), HostError> {
    let wanted = tailscale.wanted_target();
    let serve_set = tailscale.serve_target(dns_name)?.as_deref() == Some(wanted.as_str());
    let missing_linger = match host.os {
        Os::MacOs => None,
        Os::Linux => {
            let loginctl = loginctl(host)?;
            (!lingering(&loginctl, host.uid)).then_some(Linger {
                loginctl,
                uid: host.uid,
            })
        }
    };
    let Some(step) = RootStep::of(
        missing_linger,
        (!serve_set).then(|| tailscale.serve_command()),
    ) else {
        return Ok(());
    };
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
        "run `{}` yourself, then abc server setup again",
        command.shown()
    ))?;
    if tailscale.serve_target(dns_name)?.as_deref() != Some(wanted.as_str()) {
        return Err(HostError::ServeNotSet {
            socket: host.home.abc_socket(),
            command: command.shown(),
        });
    }
    if let RootStep::Linger(linger) | RootStep::Both(linger, _) = &step
        && !lingering(&linger.loginctl, linger.uid)
    {
        return Err(HostError::Failed {
            command: command.shown(),
            status: "lingering is still off".to_owned(),
            output: String::new(),
            fix: format!("run `sudo loginctl enable-linger {}` yourself", linger.uid),
        });
    }
    console.line(&format!("Serve on abc's node points at {wanted}."));
    Ok(())
}

/// `loginctl`, which Linux's lingering needs.
pub(crate) fn loginctl(host: &Host) -> Result<PathBuf, HostError> {
    find_on_path(&host.path, "loginctl").ok_or(HostError::SystemProgramMissing {
        program: "loginctl",
        needed_for: "to keep its services running while you are logged out",
        fix: "abc's server runs as systemd user services on Linux; use a distribution with \
              systemd",
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
pub(crate) fn definitions(host: &Host, config: &Config) -> Result<[Service; 3], HostError> {
    let public_url = config
        .public_url
        .as_ref()
        .ok_or_else(|| HostError::NotSetUp {
            path: host.home.config(),
        })?;
    let postgres = Postgres::new(&host.home, &config.programs);
    let tailscale = Tailscale::new(&host.home, &config.programs);
    Ok([
        postgres_service(&postgres),
        tailscaled_service(&tailscale),
        server_service(&host.home, config, public_url, &postgres),
    ])
}

fn postgres_service(postgres: &Postgres<'_>) -> Service {
    Service {
        part: Part::Postgres,
        command: postgres.server_command(),
        env: Vec::new(),
    }
}

fn tailscaled_service(tailscale: &Tailscale<'_>) -> Service {
    Service {
        part: Part::Tailscaled,
        command: tailscale.daemon_command(),
        env: Vec::new(),
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
