//! The abc binary. Thin: parse, set up logging, call the library, map errors to exit codes.
//!
//! Exit codes: 0 on success, 1 when running fails, 2 for a configuration error, and 3 when the
//! server told a worker to stop (revoked, unknown key, superseded, no common protocol version):
//! a person has to act, so a service manager should not restart it.

use std::error::Error as StdError;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use abc_core::credential::EnrolmentToken;
use abc_core::secret::Secret;
use abc_platform::telemetry::chain;
use abc_protocol::url::PublicUrl;
use abc_server::{Limits, LinkTimings, ServerConfig};
use abc_store::{DatabaseUrl, ExecuteError, Store, StoreOptions};
use abc_worker::{Env, IfEnrolled, WorkerHome};
use clap::{Parser, Subcommand};
use zeroize::Zeroizing;

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
    /// Run this machine's worker, or enrol it with a server first.
    Worker(WorkerCommand),
}

#[derive(Debug, clap::Args)]
struct WorkerCommand {
    #[command(subcommand)]
    action: Option<WorkerAction>,
    /// The directory holding the worker's key [default: `ABC_WORKER_HOME`, else
    /// `~/Library/Application Support/abc/worker` on macOS and `$XDG_CONFIG_HOME/abc/worker` or
    /// `~/.config/abc/worker` on Linux].
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    /// Log filter, such as `info` or `abc_worker=debug`.
    #[arg(long, env = "ABC_LOG")]
    log: Option<String>,
}

#[derive(Debug, Subcommand)]
enum WorkerAction {
    /// Enrol this machine with a server, using a one-time token from the server's Machines
    /// screen. The token is read from `ABC_ENROLMENT_TOKEN`, else from standard input; it is never
    /// a command-line argument, where other users of this machine could read it and use it first.
    /// Running the same command again is safe: the same token gets the same answer.
    Enrol(EnrolArgs),
}

#[derive(Debug, clap::Args)]
struct EnrolArgs {
    /// The server's tailnet URL, such as `https://abc.tail1234.ts.net`.
    url: String,
    /// Enrol as a new worker even if this machine is already enrolled. The old worker stays on
    /// the server, offline, until its owner revokes it.
    #[arg(long)]
    replace: bool,
    /// The name this machine introduces itself with [default: its hostname].
    #[arg(long)]
    hostname: Option<String>,
}

/// The variable a token can be passed in.
const ENROLMENT_TOKEN: &str = "ABC_ENROLMENT_TOKEN";

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
    /// The server told the worker to stop; a person has to act.
    Stop(Box<dyn StdError>),
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
    let result = match cli.command {
        Command::Server(server) => match server.action {
            Some(ServerAction::SetupCode) => runtime.block_on(setup_code(&server.database)),
            None => runtime.block_on(serve(server)),
        },
        Command::Worker(worker) => runtime.block_on(work(worker)),
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
        Failure::Stop(error) => (error, 3),
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
    let public_url = PublicUrl::parse(&raw_url).map_err(|error| {
        config(
            format!(
                "ABC_PUBLIC_URL must be the https URL Tailscale Serve answers on, such as \
                 https://abc.tail1234.ts.net ({error})"
            )
            .into(),
        )
    })?;
    abc_platform::telemetry::init(server.serve.log.as_deref())
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
    abc_server::run(settings, shutdown_signal()?)
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

/// `abc worker` and `abc worker enrol`.
async fn work(worker: WorkerCommand) -> Result<(), Failure> {
    let config = |error: Box<dyn StdError>| Failure::Config(error);
    let home = match worker.home {
        Some(dir) => WorkerHome::at(dir),
        None => WorkerHome::discover(|name| std::env::var_os(name))
            .map_err(|error| config(Box::new(error)))?,
    };
    let build = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| Failure::Run(Box::new(error)))?;
    let env = Env::production(build).map_err(|error| Failure::Run(Box::new(error)))?;
    match worker.action {
        Some(WorkerAction::Enrol(args)) => enrol(&home, &env, args).await,
        None => {
            abc_platform::telemetry::init(worker.log.as_deref())
                .map_err(|error| config(Box::new(error)))?;
            abc_worker::run(&home, &env, shutdown_signal()?)
                .await
                .map_err(|error| worker_failure(&error))
        }
    }
}

/// How a stopped worker is reported: its own exit code, with the cause chain.
fn worker_failure(error: &abc_worker::RunError) -> Failure {
    let shown: Box<dyn StdError> = chain(error).into();
    match error.exit_code() {
        3 => Failure::Stop(shown),
        2 => Failure::Config(shown),
        _ => Failure::Run(shown),
    }
}

/// Enrols this machine and says who it became. No logger is installed: the token must not reach
/// a log, and the person running this reads plain sentences.
async fn enrol(home: &WorkerHome, env: &Env, args: EnrolArgs) -> Result<(), Failure> {
    let config = |error: Box<dyn StdError>| Failure::Config(error);
    let server = PublicUrl::parse(&args.url).map_err(|error| {
        config(format!("the URL must be the server's tailnet URL, such as https://abc.tail1234.ts.net ({error})").into())
    })?;
    let token = enrolment_token().map_err(config)?;
    let machine =
        abc_worker::machine(args.hostname.as_deref()).map_err(|error| config(Box::new(error)))?;
    let if_enrolled = if args.replace {
        IfEnrolled::Replace
    } else {
        IfEnrolled::Refuse
    };
    let enrolment = abc_worker::enrol(home, env, &server, token, machine, if_enrolled)
        .await
        .map_err(|error| {
            let fix_the_command = matches!(
                error,
                abc_worker::EnrolError::AlreadyEnrolled(_)
                    | abc_worker::EnrolError::Hostname
                    | abc_worker::EnrolError::Home(_)
            );
            let shown: Box<dyn StdError> = chain(&error).into();
            if fix_the_command {
                Failure::Config(shown)
            } else {
                Failure::Run(shown)
            }
        })?;
    let enrolled = &enrolment.enrolled;
    let opening = if enrolment.again {
        "This machine was already enrolled with this token"
    } else {
        "Enrolled this machine"
    };
    writeln!(
        std::io::stdout().lock(),
        "{opening} as {} (worker {}) with {server}.\nStart the worker with: abc worker",
        enrolled.name.as_str(),
        enrolled.worker,
    )
    .map_err(|error| Failure::Run(Box::new(error)))
}

/// The enrolment token, from [`ENROLMENT_TOKEN`] or else one line of standard input.
fn enrolment_token() -> Result<Secret<EnrolmentToken>, Box<dyn StdError>> {
    let text = match std::env::var(ENROLMENT_TOKEN) {
        Ok(text) => Zeroizing::new(text),
        Err(_unset) => {
            let stdin = std::io::stdin();
            if stdin.is_terminal() {
                write!(std::io::stderr().lock(), "Paste the enrolment token: ")?;
            }
            let mut line = Zeroizing::new(String::new());
            stdin.lock().read_line(&mut line)?;
            line
        }
    };
    EnrolmentToken::parse(text.trim()).map_err(|error| {
        format!(
            "{error}: pass the abc_enr_… token from the server's Machines screen in \
             {ENROLMENT_TOKEN} or on standard input"
        )
        .into()
    })
}

/// Listens for SIGTERM and SIGINT at once, and returns a future that resolves on either. Listening
/// starts here, not on the future's first poll, so a signal that arrives while the process is
/// still starting is a clean stop rather than the default action, which kills it.
fn shutdown_signal() -> Result<impl Future<Output = ()>, Failure> {
    use tokio::signal::unix::{SignalKind, signal};
    let listen = |kind| signal(kind).map_err(|error| Failure::Run(Box::new(error)));
    let mut terminate = listen(SignalKind::terminate())?;
    let mut interrupt = listen(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use abc_worker::{HomeError, RunError, Terminal};

    use super::{Failure, worker_failure};

    #[test]
    fn a_stopped_worker_exits_with_its_own_code() {
        let code = |error: &RunError| match worker_failure(error) {
            Failure::Config(_) => 2,
            Failure::Run(_) => 1,
            Failure::Stop(_) => 3,
        };
        assert_eq!(code(&RunError::NotEnrolled), 2, "configuration");
        assert_eq!(
            code(&RunError::Home(HomeError::NoHomeDir)),
            2,
            "configuration"
        );
        assert_eq!(
            code(&RunError::Terminal(Terminal::Superseded)),
            3,
            "the server said stop"
        );
    }
}
