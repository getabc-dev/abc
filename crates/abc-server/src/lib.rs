//! abc's server: two listeners, each with the trust it can carry.
//!
//! | Listener | Transport | Routes | Identity |
//! | --- | --- | --- | --- |
//! | sidecar | Unix socket that only abc's own tailscaled, through Serve, reaches | the whole API, the worker WebSocket, SSE, the web app | `Tailscale-User-Login`, trusted |
//! | ops | TCP, loopback by default | `/healthz`, `/readyz` | none, and no data |
//!
//! The public webhook listener (later) will be a third, with its own router and no identity.
//!
//! [`run`] starts the ops listener first (so probes answer during migration), connects to Postgres,
//! migrates, opens the store, ends the sessions a previous process left open (the boot sweep),
//! logs a fixed hint if the server is unclaimed, binds the sidecar socket, and serves until
//! `shutdown` resolves. Then it drains: SSE streams end, and worker sessions close with 4503 and
//! record their end.
//!
//! The owner setup code never passes through this process: `abc server setup-code` (the `abc`
//! binary, run from a shell on the host) mints it through the store and prints it to that shell.

mod api;
mod limits;
mod listen;
mod live;
mod ops;
mod sessions;
mod sidecar;
mod web;
mod worker_link;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use abc_core::command::EndSession;
use abc_core::decision::Origin;
use abc_core::event::SessionEndReason;
use abc_core::query::{InstallationState, InstallationStatus, OpenSessions};
use abc_core::secret::Secret;
use abc_protocol::url::PublicUrl;
use abc_store::{Caller, DatabaseUrl, Store, StoreOptions};
use thiserror::Error;
use tokio::sync::oneshot;

pub use limits::Limits;
pub use web::{WebApp, WebAppError};
pub use worker_link::LinkTimings;

/// Everything the server needs, parsed by the binary from flags and `ABC_*` environment variables.
#[derive(Debug)]
pub struct ServerConfig {
    /// `ABC_DATABASE_URL`.
    pub database_url: Secret<DatabaseUrl>,
    /// How long to keep retrying while Postgres starts.
    pub database_wait: Duration,
    /// `ABC_SIDECAR_SOCKET`: the socket Tailscale Serve proxies to, `<home>/run/abc.sock` under
    /// `abc server setup`. The server refuses a directory others can reach.
    pub sidecar_socket: PathBuf,
    /// `ABC_OPS_ADDR`, default `127.0.0.1:7071`.
    pub ops_addr: SocketAddr,
    /// `ABC_PUBLIC_URL`, such as `https://abc.tail1234.ts.net`: the URL Serve answers on. The
    /// identity extractor's forwarded-host and Origin checks use it.
    pub public_url: PublicUrl,
    /// The worker link's handshake, ping and idle timings: the protocol's own unless a test
    /// shortens them.
    pub link: LinkTimings,
    /// Load limits.
    pub limits: Limits,
    /// The web app, served on the sidecar listener beside the API.
    pub web: WebApp,
}

/// How long a draining server waits for live worker sessions to record their end.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// Runs the server until `shutdown` resolves.
///
/// # Errors
///
/// Returns [`ServeError`] when startup fails (bad socket directory, database unreachable,
/// migration failure) or a listener dies.
pub async fn run(
    config: ServerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let readiness = ops::Readiness::new();
    let ops_listener = listen::OpsListener::bind(config.ops_addr).await?;
    tracing::info!(addr = %ops_listener.local_addr()?, "health and readiness listening");
    let (stop_ops, ops_stopped) = oneshot::channel::<()>();
    let ops = tokio::spawn(ops::serve(ops_listener, readiness.clone(), async move {
        // A dropped sender also stops the listener.
        let _stopped = ops_stopped.await;
    }));
    let served = serve(config, shutdown, &readiness).await;
    drop(stop_ops);
    let ops_result = ops
        .await
        .map_err(|error| ServeError::Task(error.to_string()))?;
    served.and(ops_result)
}

/// Everything after the ops listener is up.
async fn serve(
    config: ServerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
    readiness: &ops::Readiness,
) -> Result<(), ServeError> {
    let db = abc_store::connect(&config.database_url, config.database_wait).await?;
    readiness.set(ops::Phase::Migrating);
    abc_store::migrate(&db).await?;
    let store = Store::open(db, StoreOptions::production()).await?;
    // One server per installation: in-process supersession and the boot sweep assume it.
    let _installation = store
        .lock_installation()
        .await
        .map_err(ServeError::Installation)?
        .ok_or(ServeError::AlreadyRunning)?;
    readiness.attach(store.clone());
    boot_sweep(&store).await?;
    let state = store
        .query(&Caller::Server, InstallationStatus)
        .await
        .map_err(|error| ServeError::Sweep(Box::new(error)))?;
    if state == InstallationState::Unclaimed {
        tracing::info!(
            "this server is unclaimed; run `abc server setup-code` on this host for a one-time code"
        );
    }
    let socket = listen::SidecarSocket::bind(&config.sidecar_socket)?;
    let sessions = sessions::Sessions::default();
    let drain = api::Drain::new();
    let app = api::sidecar_app(api::SidecarState {
        store,
        sessions: sessions.clone(),
        public_url: config.public_url,
        link: config.link,
        drain: drain.clone(),
        limits: config.limits,
        enrolments: limits::RateLimiter::new(config.limits.enrolments_per_minute),
        session_starts: limits::RateLimiter::new(config.limits.session_starts_per_minute),
        web: config.web,
    });
    readiness.set(ops::Phase::Ready);
    tracing::info!(socket = %config.sidecar_socket.display(), "serving the tailnet API");
    let drain_readiness = readiness.clone();
    let drain_sessions = sessions.clone();
    listen::serve_sidecar(socket, app, async move {
        shutdown.await;
        drain_readiness.set(ops::Phase::Draining);
        tracing::info!("draining: closing live streams and worker sessions");
        drain.start();
        drain_sessions.shutdown_all();
    })
    .await?;
    if tokio::time::timeout(DRAIN_GRACE, sessions.idle())
        .await
        .is_err()
    {
        tracing::warn!(
            "worker sessions did not record their end in time; the next start's boot sweep will"
        );
    }
    Ok(())
}

/// Ends every session a previous process left open. It runs before the sidecar socket is bound, so
/// no session can be live: every open one is stale.
async fn boot_sweep(store: &Store) -> Result<(), ServeError> {
    let open = store
        .query(&Caller::Server, OpenSessions)
        .await
        .map_err(|error| ServeError::Sweep(Box::new(error)))?;
    let origin = Origin {
        correlation: store.fresh_id().map_err(ServeError::Installation)?,
        causation: None,
    };
    for session in open
        .iter()
        .filter_map(|open| open.session.map(|id| (open.worker, id)))
    {
        let (worker, session) = session;
        store
            .execute(
                &Caller::Server,
                &origin,
                EndSession {
                    worker,
                    session,
                    reason: SessionEndReason::ServerRestart,
                },
            )
            .await
            .map_err(|error| ServeError::Sweep(Box::new(error)))?;
        tracing::info!(%worker, %session, "ended a session the previous run left open");
    }
    Ok(())
}

/// Why the server could not start or keep serving. Each message tells a stranger what to fix (P4).
#[derive(Debug, Error)]
pub enum ServeError {
    /// The sidecar socket's directory is reachable by others, so anyone could forge identity.
    #[error(
        "{path} has mode {mode:o}, but anyone who can reach the sidecar socket can claim any \
         tailnet identity: chmod 700 it, as abc server setup does (abc's tailscaled runs as you)"
    )]
    SocketDirTooOpen {
        /// The directory.
        path: PathBuf,
        /// Its mode.
        mode: u32,
    },
    /// Another server process already serves this database.
    #[error(
        "another abc server is already running on this database; stop it first (one server \
         process per installation)"
    )]
    AlreadyRunning,
    /// The socket's directory belongs to another user.
    #[error(
        "{path} belongs to uid {owner}, but this server runs as uid {server}; the sidecar socket \
         directory must belong to the server's user"
    )]
    SocketDirNotOwned {
        /// The directory.
        path: PathBuf,
        /// Its owner.
        owner: u32,
        /// The server's user.
        server: u32,
    },
    /// The socket path or its directory cannot be read.
    #[error("cannot inspect {path} for the sidecar socket")]
    SocketAccess {
        /// The path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The sidecar socket path cannot be used.
    #[error("cannot use {path} as the sidecar socket: {reason}")]
    SocketPath {
        /// The path.
        path: PathBuf,
        /// What is wrong.
        reason: &'static str,
    },
    /// Binding or serving a listener failed.
    #[error("the {what} listener failed")]
    Listener {
        /// Which listener.
        what: &'static str,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The installation's state or lock could not be read.
    #[error("could not read the installation's state")]
    Installation(#[source] abc_store::StoreError),
    /// The boot sweep could not end stale sessions.
    #[error("could not close the sessions left open by the previous run")]
    Sweep(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// A server task ended abnormally.
    #[error("a server task failed: {0}")]
    Task(String),
    /// The store failed to open.
    #[error(transparent)]
    Store(#[from] abc_store::OpenError),
}
