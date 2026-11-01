//! The worker WebSocket: authentication before the upgrade, the abc-protocol handshake, sessions
//! in the log, heartbeats, and noticing revocation while connected.
//!
//! ```text
//! upgrade:  Authorization: Bearer abc_wk_…  -> store.query(Caller::Worker(key), WorkerSelf)
//!           401 invalid_worker_key | worker_revoked happens BEFORE the upgrade (HTTP-level, clear)
//! session:  recv Hello within HANDSHAKE_TIMEOUT          else close 4408
//!           negotiate(SUPPORTED_VERSIONS, hello.protocol) else send Reject, close 1000
//!           sessions.attach(worker, session)              (kicks an older socket here: 4409)
//!           execute(StartSession { session, build })      (actor: the worker; it is now online)
//!           send Welcome
//!           loop select! {
//!             frame from worker   -> PR 4 messages; a second Hello or undecodable text: close 4400
//!             ping tick           -> ping; no pong within IDLE_TIMEOUT: close (TimedOut)
//!             kick                -> close 4409 (Superseded) or 4503 (ServerShutdown)
//!             feed.changed()      -> store.query(&caller, WorkerSelf); AuthError::WorkerRevoked: close 4403
//!           }
//!           execute(&Caller::Server, EndSession { worker, session, reason })
//!                                                         (actor: Server, so a revoked worker's end is
//!                                                          recorded too; a no-op if superseded)
//!           drop the Attached guard
//!
//! If the process dies mid-session, the end is recorded by the next start's boot sweep
//! (`ServerRestart`), before any socket is accepted.
//! ```
//!
//! The session keeps the worker's `Caller` (holding the `Secret` key) for its lifetime, and
//! presents it again on every store call. Each call re-authenticates in its own transaction, so
//! a revoked worker's next write fails whatever this task believes.

use axum::extract::FromRequestParts;
use axum::extract::State;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::response::Response;

use abc_core::credential::WorkerKey;
use abc_core::event::SessionEndReason;
use abc_core::query::{WorkerIdentity, WorkerSelf};
use abc_core::secret::Secret;
use abc_store::Caller;

use crate::api::SidecarState;
use crate::api::error::ApiError;

/// `Authorization: Bearer abc_wk_…`, parsed into a `Secret` at once. A missing or malformed header
/// is `invalid_worker_key` (401), and the message never echoes the header.
#[derive(Debug)]
pub(crate) struct WorkerBearer(pub(crate) Secret<WorkerKey>);

impl<S: Send + Sync> FromRequestParts<S> for WorkerBearer {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        let _ = parts;
        unimplemented!()
    }
}

/// `GET /api/worker/connect`.
pub(crate) async fn upgrade(
    State(app): State<SidecarState>,
    WorkerBearer(key): WorkerBearer,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let caller = Caller::Worker(key);
    let me = app.store.query(&caller, WorkerSelf).await?;
    Ok(ws.on_upgrade(move |socket| session(app, caller, me, socket)))
}

/// One connected session, from handshake to close. See the module doc for the state machine. The
/// end is recorded on every exit path, including a panic in the loop (the loop runs in a child
/// task whose `JoinError` is mapped to `ProtocolError`).
async fn session(app: SidecarState, caller: Caller, me: WorkerIdentity, socket: WebSocket) {
    let _ = (app, caller, me, socket, SessionEndReason::Closed);
    unimplemented!()
}
