//! `GET /api/workspaces/{ws}/live`: the Machines screen's live feed over SSE.
//!
//! ```text
//! feed   = store.feed()                                         // 1. subscribe first: no gap
//! match Last-Event-ID {
//!   None    => send Snapshot from ListMachines                   // 2. 403 here, before streaming
//!   Some(n) => send Machines from MachinesSince { after: n }    //    resume: deltas since n
//! }
//! loop select! {
//!   feed.changed() => MachinesSince { after: cursor }
//!                       Ok(d)              => send Machines if d is not empty; cursor = d.head
//!                       Err(Denied | Auth) => send AccessEnded; end   // re-checked every batch (I4)
//!                       Err(Store)         => end (the browser reconnects with Last-Event-ID)
//!   drain          => end
//! }
//! ```
//!
//! The SSE `id:` of every message is its `head`, so `EventSource`'s automatic reconnect resumes
//! exactly where it left off: positions are gapless and commit-ordered, and every view row carries
//! `changed_at`, so "cards changed after N" is complete for any N. The payloads are read models
//! (`MachineCard`), never raw events. A comment every 15 seconds keeps proxies from closing an
//! idle stream.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;

use abc_core::event::Position;
use abc_core::id::WorkspaceId;
use abc_core::query::{ListMachines, MachinesSince, MachinesSnapshot};
use abc_store::{Caller, QueryError, Store};

use crate::api::error::ApiError;
use crate::api::types::LiveUpdate;
use crate::api::{ApiPath, Drain, SidecarState};
use crate::sidecar::TailnetCaller;

/// How often an idle stream carries a keep-alive comment.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The `Last-Event-ID` header, read as a log position. Absent means "send a snapshot".
#[derive(Debug, Clone, Copy)]
pub(crate) struct LastEventId(pub(crate) Option<Position>);

impl<S: Send + Sync> FromRequestParts<S> for LastEventId {
    type Rejection = ApiError;

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "axum's extractor trait is async; this one has nothing to wait for"
    )]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        let Some(value) = parts.headers.get("last-event-id") else {
            return Ok(Self(None));
        };
        value
            .to_str()
            .ok()
            .and_then(|text| text.parse::<i64>().ok())
            .and_then(|raw| Position::from_stored(raw).ok())
            .map(|position| Self(Some(position)))
            .ok_or_else(|| ApiError::malformed("Last-Event-ID must be a log position"))
    }
}

/// The SSE handler.
pub(crate) async fn stream(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    ApiPath(workspace): ApiPath<WorkspaceId>,
    LastEventId(last): LastEventId,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let feed = app.store.workspace_feed(workspace);
    let caller = Caller::from(caller);
    let first = match last {
        None => {
            let MachinesSnapshot { head, machines } =
                app.store.query(&caller, ListMachines { workspace }).await?;
            LiveUpdate::Snapshot { head, machines }
        }
        Some(after) => {
            let MachinesSnapshot { head, machines } = app
                .store
                .query(&caller, MachinesSince { workspace, after })
                .await?;
            LiveUpdate::Machines {
                head,
                changed: machines,
            }
        }
    };
    let (send, receive) = mpsc::channel(4);
    let follower = Follower {
        store: app.store,
        caller,
        workspace,
        cursor: first.head(),
        send,
    };
    if follower.send(&first).await {
        tokio::spawn(follower.follow(feed, app.drain));
    }
    Ok(Sse::new(ReceiverStream::new(receive).map(Ok))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}

/// One stream's state after its first message.
struct Follower {
    store: Store,
    caller: Caller,
    workspace: WorkspaceId,
    cursor: Position,
    send: mpsc::Sender<SseEvent>,
}

impl Follower {
    /// Sends `update`; false once the client has gone.
    async fn send(&self, update: &LiveUpdate) -> bool {
        match SseEvent::default()
            .event(update.name())
            .id(update.head().get().to_string())
            .json_data(update)
        {
            Ok(event) => self.send.send(event).await.is_ok(),
            Err(error) => {
                tracing::error!(error = %error, "could not encode a live update");
                false
            }
        }
    }

    /// Re-reads from the cursor on every wake-up until the client goes, access ends or the server
    /// drains.
    async fn follow(mut self, mut feed: abc_store::LogFeed, drain: Drain) {
        loop {
            tokio::select! {
                woke = feed.changed() => {
                    if woke.is_none() || !self.catch_up().await {
                        return;
                    }
                }
                () = drain.started() => return,
                () = self.send.closed() => return,
            }
        }
    }

    /// Sends what changed after the cursor; false when the stream should end.
    async fn catch_up(&mut self) -> bool {
        let since = MachinesSince {
            workspace: self.workspace,
            after: self.cursor,
        };
        match self.store.query(&self.caller, since).await {
            Ok(MachinesSnapshot { head, machines }) => {
                let sent = machines.is_empty()
                    || self
                        .send(&LiveUpdate::Machines {
                            head,
                            changed: machines,
                        })
                        .await;
                self.cursor = head;
                sent
            }
            Err(QueryError::Denied(_) | QueryError::Auth(_)) => {
                self.send(&LiveUpdate::AccessEnded { head: self.cursor })
                    .await;
                false
            }
            Err(QueryError::Store(error)) => {
                tracing::warn!(error = %error, "ending a live stream; the browser will resume it");
                false
            }
        }
    }
}
