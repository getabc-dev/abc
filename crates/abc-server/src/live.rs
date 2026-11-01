//! `GET /api/workspaces/{ws}/live`: the Machines screen's live feed over SSE.
//!
//! ```text
//! feed   = store.feed()                                         // 1. subscribe first: no gap
//! match Last-Event-ID header {
//!   None    => snap = store.query(caller, ListMachines { ws })? // 2. 403 here, before streaming
//!              send Snapshot { head, machines }; cursor = head
//!   Some(n) => cursor = n                                       //    resume: deltas since n
//!              send Machines from store.query(caller, MachinesSince { ws, after: n })?
//! }
//! loop select! {
//!   feed.changed() => store.query(caller, MachinesSince { ws, after: cursor })
//!                       Ok(d)              => if !d.machines.is_empty() send Machines; cursor = d.head
//!                       Err(Denied | Auth) => send AccessEnded; end   // re-checked every batch (I4)
//!                       Err(Store)         => end (the browser reconnects with Last-Event-ID)
//!   every 15 s     => SSE comment (keeps proxies from timing out idle streams)
//!   shutdown       => end
//! }
//! ```
//!
//! The SSE `id:` of every message is its `head`, so `EventSource`'s automatic reconnect resumes
//! exactly where it left off: positions are gapless and commit-ordered, and every view row carries
//! `changed_at`, so "cards changed after N" is complete for any N. Online-ness is a view column
//! like any other, so there is no second source to merge. The payloads are read models
//! (`MachineCard`), never raw events.

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::response::sse::{Event as SseEvent, Sse};
use futures::Stream;

use abc_core::id::WorkspaceId;

use crate::api::SidecarState;
use crate::api::error::ApiError;
use crate::sidecar::TailnetCaller;

/// The `Last-Event-ID` header, parsed as a log position. A malformed value is
/// `malformed_request`; an absent one means "send a snapshot".
#[derive(Debug, Clone, Copy)]
pub(crate) struct LastEventId(pub(crate) Option<abc_core::event::Position>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for LastEventId {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, ApiError> {
        let _ = parts;
        unimplemented!("absent -> None; a non-negative integer -> Position; else malformed_request")
    }
}

/// The SSE handler.
pub(crate) async fn stream(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    Path(workspace): Path<WorkspaceId>,
    last_event_id: LastEventId,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let _ = (app, caller, workspace, last_event_id);
    unimplemented!();
    #[expect(
        unreachable_code,
        reason = "sketch: gives the opaque return type a concrete stream"
    )]
    Ok(Sse::new(futures::stream::empty()))
}
