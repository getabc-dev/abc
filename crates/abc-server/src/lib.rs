//! abc's server: two listeners, each with the trust it can carry.
//!
//! | Listener | Transport | Routes | Identity |
//! | --- | --- | --- | --- |
//! | sidecar | Unix socket shared only with the Tailscale sidecar ([`listen::SidecarSocket`]) | the whole API, the worker WebSocket, SSE | `Tailscale-User-Login`, trusted |
//! | ops | TCP, loopback by default | `/healthz`, `/readyz` | none, and no data |
//!
//! The public webhook listener (later) will be a third, with its own router and no identity.
//!
//! [`run`] starts the ops listener first (so probes answer during migration), migrates, opens the
//! store, ends the sessions a previous process left open (the boot sweep), logs a fixed hint if the
//! server is unclaimed, binds the sidecar socket, and serves until `shutdown` resolves. Then it
//! drains: SSE streams end, worker sessions close with 4503 and record `session_ended`.
//!
//! The owner setup code never passes through this process: `abc server setup-code` (the `abc`
//! binary, run from a shell on the host) mints it through the store and prints it to that shell.

mod api;
mod listen;
mod live;
mod ops;
mod sessions;
mod sidecar;
pub mod telemetry;
mod worker_link;

use std::net::SocketAddr;
use std::path::PathBuf;

pub use api::PublicUrl;

use abc_core::secret::Secret;
use abc_store::DatabaseUrl;
use thiserror::Error;

/// Everything the server needs, parsed by the binary from flags and `ABC_*` environment variables.
#[derive(Debug)]
pub struct ServerConfig {
    /// `ABC_DATABASE_URL`.
    pub database_url: Secret<DatabaseUrl>,
    /// `ABC_SIDECAR_SOCKET`, default `/run/abc-sidecar/abc.sock`. Its directory is the volume shared
    /// with the Tailscale container; the server refuses a directory others can reach.
    pub sidecar_socket: PathBuf,
    /// `ABC_OPS_ADDR`, default `127.0.0.1:7071`.
    pub ops_addr: SocketAddr,
    /// `ABC_PUBLIC_URL`, such as `https://abc.tail1234.ts.net`: the URL Serve answers on. Used by
    /// the identity extractor's Host and Origin checks and in the enrol command the web app shows.
    pub public_url: PublicUrl,
}

/// Runs the server until `shutdown` resolves.
///
/// # Errors
///
/// Returns [`ServeError`] when startup fails (bad socket directory, database unreachable,
/// migration failure) or a listener dies.
pub async fn run(
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    // let readiness = ops::Readiness::new();                        // Starting
    // let ops = listen::OpsListener::bind(config.ops_addr).await?; spawn ops::serve(ops, readiness.clone())
    // readiness.set(Migrating); let db = abc_store::connect(&config.database_url).await?;
    // abc_store::migrate(&db).await?;
    // let store = Store::open(db, StoreOptions::production()).await?;
    // boot sweep, before the socket exists so no session can be live:
    //   for open in store.query(&Caller::Server, OpenSessions).await? {
    //       store.execute(&Caller::Server, &origin, EndSession { .., reason: ServerRestart }).await?;
    //   }
    // if !store.is_claimed().await? {
    //     info!("server is unclaimed; run `abc server setup-code` on this host for a one-time code");
    // }
    // let socket = listen::SidecarSocket::bind(&config.sidecar_socket)?;   // permission checks
    // let app = api::sidecar_app(SidecarState { store, sessions, public_url });
    // readiness.set(Ready); listen::serve_sidecar(socket, app, shutdown).await?;
    // readiness.set(Draining); sessions.shutdown_all(); (each session records its end)
    let _ = (config, shutdown);
    unimplemented!()
}

/// Why the server could not start or keep serving. Each message tells a stranger what to fix (P4).
#[derive(Debug, Error)]
pub enum ServeError {
    /// The sidecar socket's directory is reachable by others, so anyone could forge identity.
    #[error(
        "{path} has mode {mode:o}; the sidecar socket directory must not be accessible to other users \
         (expected 0750 or 0770, shared only with the Tailscale container)"
    )]
    SocketDirTooOpen {
        /// The directory.
        path: PathBuf,
        /// Its mode.
        mode: u32,
    },
    /// `ABC_PUBLIC_URL` is not an https origin.
    #[error(
        "ABC_PUBLIC_URL must be the https URL Tailscale Serve answers on, such as https://abc.tail1234.ts.net ({reason})"
    )]
    PublicUrl {
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Binding a listener failed.
    #[error("could not listen on {what}")]
    Bind {
        /// Which listener.
        what: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The boot sweep could not end stale sessions.
    #[error("could not close the sessions left open by the previous run")]
    BootSweep(#[source] abc_store::StoreError),
    /// The store failed to open.
    #[error(transparent)]
    Store(#[from] abc_store::OpenError),
}
