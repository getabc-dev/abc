//! Health and readiness on the ops listener. The only routes that skip the permission check:
//! they serve no data and read no identity.
//!
//! * `GET /healthz` returns 200 `{"status":"alive"}` whenever the process can answer (liveness).
//! * `GET /readyz` returns 200 `{"status":"ready"}` only in `Ready` with `store.ready()` passing,
//!   else 503 `{"status":"starting"|"migrating"|"draining"|"degraded","reason":…}`. The phase
//!   tells a stranger why their container is not serving yet (P4).

use tokio::sync::watch;

/// The server's lifecycle phase, set by `run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Binding listeners, connecting to Postgres.
    Starting,
    /// Running migrations.
    Migrating,
    /// Serving.
    Ready,
    /// Shutting down.
    Draining,
}

/// Shared readiness. The store is attached once it is open.
#[derive(Debug, Clone)]
pub(crate) struct Readiness {
    phase: watch::Sender<Phase>,
    store: watch::Sender<Option<abc_store::Store>>,
}

impl Readiness {
    pub(crate) fn new() -> Self {
        unimplemented!()
    }
    pub(crate) fn set(&self, phase: Phase) {
        let _ = phase;
        unimplemented!()
    }
}

/// Serves `/healthz` and `/readyz` on the ops listener until shutdown.
pub(crate) async fn serve(
    listener: crate::listen::OpsListener,
    readiness: Readiness,
) -> Result<(), crate::ServeError> {
    let _ = (listener, readiness);
    unimplemented!()
}
