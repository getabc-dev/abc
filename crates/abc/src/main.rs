//! The abc binary. Thin: parse, set up logging, call the library, map errors to exit codes.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

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
    /// Run the server (migrates Postgres on start), or administer it from its host.
    Server(ServerCommand),
    // Worker(WorkerArgs): PR 4. `abc worker enrol <url> --token …` and `abc worker`.
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
    /// Print a one-time code that claims this unclaimed server (valid one hour). Run it from a
    /// shell on the server host, such as `docker compose exec abc abc server setup-code`. The code
    /// is printed to this terminal only: never logged, never stored (only its digest).
    SetupCode,
}

#[derive(Debug, clap::Args)]
struct DatabaseArgs {
    /// Postgres URL. Read as a raw string so clap never echoes it in an error; validated when
    /// connecting, with errors that do not contain it.
    #[arg(long, env = "ABC_DATABASE_URL", hide_env_values = true)]
    database_url: String,
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
    /// Health and readiness.
    #[arg(long, env = "ABC_OPS_ADDR", default_value = "127.0.0.1:7071")]
    ops_addr: SocketAddr,
    /// The https URL Tailscale Serve answers on, such as `https://abc.tail1234.ts.net`.
    #[arg(long, env = "ABC_PUBLIC_URL")]
    public_url: Option<String>,
    /// Log filter, such as `info` or `abc_server=debug`.
    #[arg(long, env = "ABC_LOG")]
    log: Option<String>,
}

/// Why the process failed. Each message tells the user what to do.
#[derive(Debug, thiserror::Error)]
enum RunError {
    #[error(transparent)]
    Telemetry(#[from] abc_server::telemetry::TelemetryError),
    #[error(transparent)]
    Serve(#[from] abc_server::ServeError),
    #[error(transparent)]
    Store(#[from] abc_store::OpenError),
    #[error(transparent)]
    SetupCode(#[from] abc_store::ExecuteError<std::convert::Infallible>),
    #[error("ABC_PUBLIC_URL is required to run the server")]
    MissingPublicUrl,
    #[error("could not start the async runtime")]
    Runtime(#[source] std::io::Error),
}

fn main() -> ExitCode {
    // Parse; build a multi-thread tokio runtime; dispatch.
    // Server: telemetry::init(args.log), then abc_server::run(ServerConfig { database_url:
    //   abc_store::DatabaseUrl::new(args.database_url), .. }, shutdown_signal()), where
    //   shutdown_signal is SIGTERM or SIGINT.
    // Server + SetupCode: connect, Store::open (refuses an unmigrated database with a clear
    //   message), store.issue_setup_code(), then writeln!(io::stdout().lock(), …) the code's
    //   display form and expiry to the operator's own terminal. No subscriber is installed, so
    //   nothing reaches the server's log; `ServerAlreadyClaimed` prints "already claimed".
    // Errors: the chain is written to stderr through the JSON logger (server) or plainly
    //   (setup-code); exit 1 for runtime failures, 2 for configuration errors.
    let _ = Cli::parse;
    unimplemented!()
}

async fn shutdown_signal() {
    unimplemented!()
}
