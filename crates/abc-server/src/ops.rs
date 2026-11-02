//! Health and readiness on the ops listener. The only routes that skip the permission check:
//! they serve no data and read no identity.
//!
//! * `GET /healthz` returns 200 `{"status":"alive"}` whenever the process can answer (liveness).
//! * `GET /readyz` returns 200 `{"status":"ready"}` only when serving and `Store::ready` passes,
//!   else 503 with `{"status":"starting"|"migrating"|"draining"|"degraded","reason":…}`. The phase
//!   tells a stranger why their container is not serving yet (P4).

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::sync::watch;

use crate::ServeError;
use crate::listen::OpsListener;

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

/// Shared readiness: the phase, and the store once it is open.
#[derive(Debug, Clone)]
pub(crate) struct Readiness {
    phase: Arc<watch::Sender<Phase>>,
    store: Arc<watch::Sender<Option<abc_store::Store>>>,
}

impl Readiness {
    pub(crate) fn new() -> Self {
        Self {
            phase: Arc::new(watch::Sender::new(Phase::Starting)),
            store: Arc::new(watch::Sender::new(None)),
        }
    }

    pub(crate) fn set(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }

    pub(crate) fn attach(&self, store: abc_store::Store) {
        self.store.send_replace(Some(store));
    }
}

/// A probe's answer.
#[derive(Debug, Serialize)]
struct Status {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

fn answer(code: StatusCode, status: &'static str, reason: Option<String>) -> Response {
    (code, Json(Status { status, reason })).into_response()
}

async fn healthz() -> Response {
    answer(StatusCode::OK, "alive", None)
}

async fn readyz(State(readiness): State<Readiness>) -> Response {
    let phase = *readiness.phase.borrow();
    let unavailable = |status| answer(StatusCode::SERVICE_UNAVAILABLE, status, None);
    match phase {
        Phase::Starting => unavailable("starting"),
        Phase::Migrating => unavailable("migrating"),
        Phase::Draining => unavailable("draining"),
        Phase::Ready => {
            let store = readiness.store.borrow().clone();
            match store {
                Some(store) => match store.ready().await {
                    Ok(()) => answer(StatusCode::OK, "ready", None),
                    Err(not_ready) => answer(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "degraded",
                        Some(not_ready.to_string()),
                    ),
                },
                None => unavailable("starting"),
            }
        }
    }
}

/// Serves `/healthz` and `/readyz` on the ops listener until `shutdown` resolves.
pub(crate) async fn serve(
    listener: OpsListener,
    readiness: Readiness,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(readiness);
    listener.serve(router, shutdown).await
}
