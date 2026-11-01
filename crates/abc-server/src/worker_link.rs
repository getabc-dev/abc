//! The worker WebSocket: authentication before the upgrade, the abc-protocol handshake, sessions
//! in the log, heartbeats, and noticing revocation while connected.
//!
//! ```text
//! upgrade:  Authorization: Bearer abc_wk_…  -> store.query(Caller::Worker(key), WorkerSelf)
//!           401 invalid_worker_key | worker_revoked happens BEFORE the upgrade
//! session:  recv Hello within the handshake timeout       else close 4408
//!           negotiate(SUPPORTED_VERSIONS, hello.protocol)  else send Reject, close 1000
//!           one at a time: execute(StartSession { session, build }) (actor: the worker; online)
//!                          then sessions.attach(worker, session) (kicks an older socket: 4409)
//!           send Welcome
//!           loop select! {
//!             frame from worker -> a second Hello, binary or undecodable text: close 4400
//!             ping tick         -> ping; no pong for the idle timeout: close (timed out)
//!             kick              -> close 4409 (superseded) or 4503 (server shutdown)
//!             feed.changed()    -> query WorkerSelf; WorkerRevoked: close 4403
//!           }
//!           execute(&Caller::Server, EndSession { worker, session, reason })
//! ```
//!
//! The end is recorded by the server, so a revoked worker's last session still ends in the log.
//! If the process dies mid-session, the next start's boot sweep records it (`ServerRestart`).
//! The session keeps the worker's `Caller` for its lifetime and presents it on every store call;
//! each call re-authenticates in its own transaction.

use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::Response;
use bytes::Bytes;
use tracing::Instrument as _;

use abc_core::command::{EndSession, StartSession};
use abc_core::credential::WorkerKey;
use abc_core::decision::Origin;
use abc_core::event::SessionEndReason;
use abc_core::id::{CorrelationId, FromUuid as _, SessionId};
use abc_core::query::{WorkerIdentity, WorkerSelf};
use abc_core::secret::Secret;
use abc_protocol::api::ApiCode;
use abc_protocol::link::{CloseCode, HANDSHAKE_TIMEOUT, IDLE_TIMEOUT, PING_INTERVAL};
use abc_protocol::{
    Reject, RejectReason, SUPPORTED_VERSIONS, ServerMessage, Welcome, WorkerMessage, negotiate,
};
use abc_store::{AuthError, Caller, ExecuteError, QueryError};

use crate::api::SidecarState;
use crate::api::error::ApiError;
use crate::sessions::Kick;

/// The worker link's timings. The protocol's own by default; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkTimings {
    /// How long a worker has to send `Hello` after the upgrade.
    pub handshake: Duration,
    /// How often the server pings.
    pub ping: Duration,
    /// How long without a pong before the session is closed as timed out.
    pub idle: Duration,
}

impl Default for LinkTimings {
    fn default() -> Self {
        Self {
            handshake: HANDSHAKE_TIMEOUT,
            ping: PING_INTERVAL,
            idle: IDLE_TIMEOUT,
        }
    }
}

/// `Authorization: Bearer abc_wk_…`, parsed into a `Secret` at once. A missing or malformed header
/// is `invalid_worker_key` (401), and the message never echoes the header.
#[derive(Debug)]
pub(crate) struct WorkerBearer(pub(crate) Secret<WorkerKey>);

impl<S: Send + Sync> FromRequestParts<S> for WorkerBearer {
    type Rejection = ApiError;

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "axum's extractor trait is async; this one has nothing to wait for"
    )]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|key| WorkerKey::parse(key).ok())
            .map(Self)
            .ok_or_else(|| {
                ApiError::new(
                    ApiCode::InvalidWorkerKey,
                    "send the worker key as Authorization: Bearer abc_wk_…",
                )
            })
    }
}

/// `GET /api/worker/connect`.
pub(crate) async fn upgrade(
    State(app): State<SidecarState>,
    WorkerBearer(key): WorkerBearer,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let caller = Caller::Worker(key);
    let me = app.store.query(&caller, WorkerSelf).await?;
    tracing::Span::current().record("worker", tracing::field::display(me.worker));
    let upgrade = upgrade
        .ok()
        .ok_or_else(|| ApiError::malformed("expected a WebSocket upgrade"))?;
    Ok(upgrade.on_upgrade(move |socket| {
        let session = SessionId::from_uuid(uuid::Uuid::now_v7());
        let span = tracing::info_span!("worker_session", worker = %me.worker, session = %session);
        Link {
            app,
            caller,
            me,
            session,
            socket,
        }
        .run()
        .instrument(span)
    }))
}

/// What the worker sent while the server waited for its `Hello`.
enum Incoming {
    Text(String),
    Violation,
    Gone,
}

/// Why a session's loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Closed,
    TimedOut,
    Superseded,
    Shutdown,
    Revoked,
    Violation,
}

impl End {
    const fn reason(self) -> SessionEndReason {
        match self {
            Self::Closed => SessionEndReason::Closed,
            Self::TimedOut => SessionEndReason::TimedOut,
            Self::Superseded => SessionEndReason::Superseded,
            Self::Shutdown => SessionEndReason::ServerShutdown,
            Self::Revoked => SessionEndReason::Revoked,
            Self::Violation => SessionEndReason::ProtocolError,
        }
    }

    /// The close frame to send: an abc close code, or a normal close.
    fn frame(self) -> CloseFrame {
        let (code, reason) = match self {
            Self::Closed | Self::TimedOut => (1000, "closed"),
            Self::Superseded => (
                CloseCode::Superseded.code(),
                "superseded by a newer session",
            ),
            Self::Shutdown => (
                CloseCode::ServerShutdown.code(),
                "the server is shutting down",
            ),
            Self::Revoked => (CloseCode::Revoked.code(), "this worker was revoked"),
            Self::Violation => (CloseCode::ProtocolViolation.code(), "unexpected message"),
        };
        CloseFrame {
            code,
            reason: reason.into(),
        }
    }
}

/// One connected session, from handshake to close.
struct Link {
    app: SidecarState,
    caller: Caller,
    me: WorkerIdentity,
    session: SessionId,
    socket: WebSocket,
}

impl Link {
    fn origin() -> Origin {
        Origin {
            correlation: CorrelationId::from_uuid(uuid::Uuid::now_v7()),
            causation: None,
        }
    }

    async fn close(&mut self, code: u16, reason: &'static str) {
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        // The worker may already be gone; there is no one left to tell.
        let _unheard = self.socket.send(Message::Close(Some(frame))).await;
    }

    async fn send(&mut self, message: &ServerMessage) -> bool {
        match message.encode() {
            Ok(text) => self.socket.send(Message::Text(text.into())).await.is_ok(),
            Err(error) => {
                tracing::error!(error = %error, "could not encode a server message");
                false
            }
        }
    }

    /// The next text frame, skipping pings and pongs.
    async fn next_text(&mut self) -> Incoming {
        loop {
            match self.socket.recv().await {
                Some(Ok(Message::Text(text))) => return Incoming::Text(text.to_string()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Binary(_))) => return Incoming::Violation,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Incoming::Gone,
            }
        }
    }

    async fn run(mut self) {
        let hello = match tokio::time::timeout(self.app.link.handshake, self.next_text()).await {
            Err(_) => {
                return self
                    .close(CloseCode::HandshakeTimeout.code(), "send hello first")
                    .await;
            }
            Ok(Incoming::Gone) => return,
            Ok(Incoming::Violation) => {
                return self
                    .close(CloseCode::ProtocolViolation.code(), "expected hello")
                    .await;
            }
            Ok(Incoming::Text(text)) => match WorkerMessage::decode(&text) {
                Ok(WorkerMessage::Hello(hello)) => hello,
                Err(_) => {
                    return self
                        .close(CloseCode::ProtocolViolation.code(), "expected hello")
                        .await;
                }
            },
        };
        let Ok(protocol) = negotiate(SUPPORTED_VERSIONS, hello.protocol) else {
            let reject = ServerMessage::Reject(Reject {
                reason: RejectReason::UnsupportedVersion {
                    supported: SUPPORTED_VERSIONS,
                },
            });
            if self.send(&reject).await {
                self.close(1000, "no common protocol version").await;
            }
            return;
        };
        let sessions = self.app.sessions.clone();
        let admission = sessions.admission().await;
        let start = StartSession {
            session: self.session,
            build: hello.build,
        };
        let started = self
            .app
            .store
            .execute(&self.caller, &Self::origin(), start)
            .await;
        match started {
            Ok(()) => {}
            Err(ExecuteError::Auth(AuthError::WorkerRevoked)) => {
                drop(admission);
                return self
                    .close(CloseCode::Revoked.code(), "this worker was revoked")
                    .await;
            }
            Err(error) => {
                drop(admission);
                tracing::error!(error = %error, "could not record the session's start");
                return self.close(1011, "internal error").await;
            }
        }
        // Recorded first, attached second: the older session, kicked now, finds this one open.
        let (_attached, kicked) = sessions.attach(self.me.worker, self.session);
        drop(admission);
        tracing::info!(protocol = %protocol, "worker connected");
        let end = if self
            .send(&ServerMessage::Welcome(Welcome { protocol }))
            .await
        {
            self.serve(kicked).await
        } else {
            End::Closed
        };
        let _unheard = self.socket.send(Message::Close(Some(end.frame()))).await;
        let finish = EndSession {
            worker: self.me.worker,
            session: self.session,
            reason: end.reason(),
        };
        match self
            .app
            .store
            .execute(&Caller::Server, &Self::origin(), finish)
            .await
        {
            Ok(()) => tracing::info!(reason = ?end.reason(), "worker disconnected"),
            Err(error) => tracing::error!(error = %error, "could not record the session's end"),
        }
    }

    /// The connected loop: frames, heartbeats, kicks and revocation.
    async fn serve(&mut self, mut kicked: tokio::sync::oneshot::Receiver<Kick>) -> End {
        let mut feed = self.app.store.feed();
        let mut ping = tokio::time::interval(self.app.link.ping);
        ping.reset();
        let mut last_pong = Instant::now();
        loop {
            tokio::select! {
                frame = self.socket.recv() => match frame {
                    Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                    Some(Ok(Message::Ping(_))) => {}
                    Some(Ok(Message::Text(text))) => match WorkerMessage::decode(&text) {
                        Ok(WorkerMessage::Hello(_)) | Err(_) => return End::Violation,
                    },
                    Some(Ok(Message::Binary(_))) => return End::Violation,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return End::Closed,
                },
                _ = ping.tick() => {
                    if last_pong.elapsed() > self.app.link.idle {
                        return End::TimedOut;
                    }
                    if self.socket.send(Message::Ping(Bytes::new())).await.is_err() {
                        return End::Closed;
                    }
                }
                kick = &mut kicked => return match kick {
                    Ok(Kick::Superseded) => End::Superseded,
                    Ok(Kick::Shutdown) | Err(_) => End::Shutdown,
                },
                woke = feed.changed() => {
                    if woke.is_none() {
                        return End::Shutdown;
                    }
                    let me = self.app.store.query(&self.caller, WorkerSelf).await;
                    if let Err(QueryError::Auth(AuthError::WorkerRevoked)) = me {
                        return End::Revoked;
                    }
                }
            }
        }
    }
}
