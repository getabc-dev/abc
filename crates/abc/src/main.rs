//! The abc binary. Thin: parse, set up logging, call the library, map errors to exit codes.
//!
//! Exit codes: 0 on success, 1 when running fails, 2 for a configuration error.

use std::error::Error as StdError;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use abc_core::secret::Secret;
use abc_server::telemetry::chain;
use abc_server::{Limits, LinkTimings, PublicUrl, ServerConfig};
use abc_store::{DatabaseUrl, ExecuteError, Store, StoreOptions};
use clap::{Parser, Subcommand};

/// abc: a self-hosted software factory.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server (it migrates Postgres on start), or administer it from its host.
    Server(ServerCommand),
}

#[derive(Debug, clap::Args)]
struct ServerCommand {
    #[command(subcommand)]
    action: Option<ServerAction>,
    #[command(flatten)]
    database: DatabaseArgs,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Debug, Subcommand)]
enum ServerAction {
    /// Print a one-time code that claims this unclaimed server (valid for one hour). Run it from a
    /// shell on the server host, such as `docker compose exec abc abc server setup-code`. The code
    /// is printed to this terminal only: never logged, and only its digest is stored.
    SetupCode,
}

/// The variable holding the Postgres URL. It is read from the environment only: a command-line
/// flag would show the password to anyone who can list processes, and clap would keep a copy.
const DATABASE_URL: &str = "ABC_DATABASE_URL";

/// The Postgres URL from [`DATABASE_URL`], moved straight into a [`Secret`].
fn database_url() -> Result<Secret<DatabaseUrl>, Failure> {
    std::env::var(DATABASE_URL)
        .ok()
        .map(DatabaseUrl::new)
        .ok_or_else(|| {
            Failure::Config(
                format!("{DATABASE_URL} must hold the Postgres URL, such as postgres://abc@db/abc")
                    .into(),
            )
        })
}

/// Database settings. The URL itself comes from `ABC_DATABASE_URL` in the environment.
#[derive(Debug, clap::Args)]
struct DatabaseArgs {
    /// How many seconds to keep retrying while Postgres starts.
    #[arg(long, env = "ABC_DATABASE_WAIT", default_value_t = 60)]
    database_wait: u64,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// The Unix socket the Tailscale sidecar proxies to.
    #[arg(
        long,
        env = "ABC_SIDECAR_SOCKET",
        default_value = "/run/abc-sidecar/abc.sock"
    )]
    sidecar_socket: PathBuf,
    /// Where health and readiness listen.
    #[arg(long, env = "ABC_OPS_ADDR", default_value = "127.0.0.1:7071")]
    ops_addr: SocketAddr,
    /// The https URL Tailscale Serve answers on, such as `https://abc.tail1234.ts.net`.
    #[arg(long, env = "ABC_PUBLIC_URL")]
    public_url: Option<String>,
    /// Log filter, such as `info` or `abc_server=debug`.
    #[arg(long, env = "ABC_LOG")]
    log: Option<String>,
}

/// Why the process failed, and with which exit code.
enum Failure {
    /// Fix the configuration and run again.
    Config(Box<dyn StdError>),
    /// Running failed.
    Run(Box<dyn StdError>),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return report(&Failure::Run(Box::new(error))),
    };
    let Command::Server(server) = cli.command;
    let result = match server.action {
        Some(ServerAction::SetupCode) => runtime.block_on(setup_code(&server.database)),
        None => runtime.block_on(serve(server)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => report(&failure),
    }
}

/// Writes the failure to stderr and returns its exit code.
fn report(failure: &Failure) -> ExitCode {
    let (error, code) = match failure {
        Failure::Config(error) => (error, 2),
        Failure::Run(error) => (error, 1),
    };
    // Nothing more can be done if stderr is gone.
    let _unwritten = writeln!(std::io::stderr().lock(), "abc: {}", chain(error.as_ref()));
    ExitCode::from(code)
}

async fn serve(server: ServerCommand) -> Result<(), Failure> {
    let config = |error: Box<dyn StdError>| Failure::Config(error);
    let raw_url = server
        .serve
        .public_url
        .ok_or_else(|| config("ABC_PUBLIC_URL is required to run the server".into()))?;
    let public_url = PublicUrl::parse(&raw_url).map_err(|error| config(Box::new(error)))?;
    abc_server::telemetry::init(server.serve.log.as_deref())
        .map_err(|error| config(Box::new(error)))?;
    let settings = ServerConfig {
        database_url: database_url()?,
        database_wait: Duration::from_secs(server.database.database_wait),
        sidecar_socket: server.serve.sidecar_socket,
        ops_addr: server.serve.ops_addr,
        public_url,
        link: LinkTimings::default(),
        limits: Limits::default(),
    };
    abc_server::run(settings, shutdown_signal())
        .await
        .map_err(|error| {
            tracing::error!(error = %chain(&error), "the server stopped");
            Failure::Run(Box::new(error))
        })
}

/// Issues a setup code and prints it to this terminal. No logger is installed, so nothing about
/// the code reaches the server's log.
async fn setup_code(database: &DatabaseArgs) -> Result<(), Failure> {
    let run = |error: Box<dyn StdError>| Failure::Run(error);
    let url = database_url()?;
    let db = abc_store::connect(&url, Duration::from_secs(database.database_wait))
        .await
        .map_err(|error| run(Box::new(error)))?;
    let store = Store::open(db, StoreOptions::production())
        .await
        .map_err(|error| run(Box::new(error)))?;
    #[expect(
        clippy::disallowed_methods,
        reason = "the host operator's CLI is the one place that may mint a setup code"
    )]
    let issued = store.issue_setup_code().await;
    let issued = match issued {
        Ok(issued) => issued,
        // Only an unclaimed server issues setup codes; the reason says so.
        Err(ExecuteError::Denied(denied)) => {
            return Err(run(format!("no setup code: {}", denied.reason).into()));
        }
        Err(error) => return Err(run(Box::new(error))),
    };
    let expires = issued.expires_at.as_offset();
    let shown = issued.code.expose_secret().display_form();
    writeln!(
        std::io::stdout().lock(),
        "Setup code (valid until {:02}:{:02} UTC): {}\nOpen the server's tailnet URL and enter it to become its owner.",
        expires.hour(),
        expires.minute(),
        shown.as_str()
    )
    .map_err(|error| run(Box::new(error)))
}

/// Resolves on SIGTERM or SIGINT.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(mut terminate), Ok(mut interrupt)) => {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
        }
        (Err(error), _) | (_, Err(error)) => {
            tracing::error!(error = %error, "cannot listen for shutdown signals; stopping");
        }
    }
}
