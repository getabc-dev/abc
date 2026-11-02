//! [`LogFeed`]: how consumers learn that the log moved, without the notifier carrying events.
//!
//! `append` runs `pg_notify('abc_log', …)` inside its transaction, and Postgres delivers the
//! notification only if and when the transaction commits. The payload names the new head, the
//! workspace the commit belongs to and the streams it touched. One `PgListener` task per store
//! publishes each head into `tokio::sync::watch` channels: one for the whole log, and one per
//! workspace or stream that someone subscribed to. Every subscriber then follows the same loop:
//!
//! ```text
//! loop { feed.changed().await; read from MY cursor (an authorized query); advance my cursor }
//! ```
//!
//! What this buys:
//! * **No lost events.** The notifier is only a wake-up. Data always comes from the subscriber's
//!   own cursor, and positions are gapless and commit-ordered, so `position > cursor` misses
//!   nothing.
//! * **Only relevant wake-ups.** A live stream follows its workspace and a worker session follows
//!   its worker's stream, so a commit elsewhere wakes neither.
//! * **No unbounded buffers.** `watch` keeps one value. A burst of commits, or a slow subscriber,
//!   coalesces into one wake-up.
//! * **Reconnects are safe.** When the listener's connection drops, the task reconnects with
//!   backoff. Notifications during the gap are lost, so after reconnecting it reads
//!   `event_log_head` and wakes every subscriber to re-read from its cursor.
//! * **Health.** The task's connection state feeds `Store::ready`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use abc_core::event::{Position, StreamKind, StreamRef};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::StoreError;

/// The notification channel `append` sends commits on.
pub(crate) const CHANNEL: &str = "abc_log";

/// The longest pause between reconnection attempts.
const MAX_RECONNECT_PAUSE: Duration = Duration::from_secs(5);

/// What one commit changed: the notification's payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Commit {
    /// The head after the commit.
    pub(crate) head: i64,
    /// The workspace its events belong to.
    pub(crate) workspace: Uuid,
    /// The streams it appended to, as (kind, id).
    pub(crate) streams: Vec<(String, Uuid)>,
}

/// Wakes when the log head advances in what it follows. Clone one per subscriber.
#[derive(Debug, Clone)]
pub struct LogFeed {
    head: watch::Receiver<Position>,
}

impl LogFeed {
    /// Waits until a commit this feed follows moves the head past what this handle last saw (at
    /// first, the head when it was subscribed), then returns the new head. Returns `None` when the
    /// store has shut down.
    pub async fn changed(&mut self) -> Option<Position> {
        self.head.changed().await.ok()?;
        Some(*self.head.borrow_and_update())
    }

    /// The latest head this feed was woken for.
    #[must_use]
    pub fn head(&self) -> Position {
        *self.head.borrow()
    }
}

/// What a scoped feed follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Scope {
    /// Commits in one workspace.
    Workspace(Uuid),
    /// Commits to one stream.
    Stream(StreamKind, Uuid),
}

/// The channels the listener publishes into.
#[derive(Debug)]
struct Hub {
    head: watch::Sender<Position>,
    scoped: Mutex<BTreeMap<Scope, watch::Sender<Position>>>,
}

impl Hub {
    fn scoped(&self) -> std::sync::MutexGuard<'_, BTreeMap<Scope, watch::Sender<Position>>> {
        self.scoped.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publishes `commit` to the log's channel and to the channels following what it touched,
    /// dropping channels no one follows any more.
    fn publish(&self, commit: &Commit) {
        let Ok(head) = Position::from_stored(commit.head) else {
            tracing::error!("ignored a log notification with a negative head");
            return;
        };
        advance(&self.head, head);
        let mut scoped = self.scoped();
        scoped.retain(|_, sender| sender.receiver_count() > 0);
        let touched = std::iter::once(Scope::Workspace(commit.workspace)).chain(
            commit.streams.iter().filter_map(|(kind, id)| {
                StreamKind::parse(kind).map(|kind| Scope::Stream(kind, *id))
            }),
        );
        for scope in touched {
            if let Some(sender) = scoped.get(&scope) {
                advance(sender, head);
            }
        }
    }

    /// Wakes every channel at `head`: after a reconnect, when notifications may have been lost.
    fn publish_everywhere(&self, head: Position) {
        advance(&self.head, head);
        for sender in self.scoped().values() {
            advance(sender, head);
        }
    }

    fn subscribe(&self, scope: Option<Scope>) -> LogFeed {
        let head = match scope {
            None => self.head.subscribe(),
            Some(scope) => {
                let current = *self.head.borrow();
                self.scoped()
                    .entry(scope)
                    .or_insert_with(|| watch::Sender::new(current))
                    .subscribe()
            }
        };
        LogFeed { head }
    }
}

/// Moves `sender` to `head` if that is ahead of it.
fn advance(sender: &watch::Sender<Position>, head: Position) {
    sender.send_if_modified(|current| {
        let ahead = head > *current;
        if ahead {
            *current = head;
        }
        ahead
    });
}

/// Owns the listener task. It is aborted when the store's last clone drops.
#[derive(Debug)]
pub(crate) struct FeedHandle {
    hub: Arc<Hub>,
    connected: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl FeedHandle {
    /// Listens on the store's pool, seeds the channels from `event_log_head`, and spawns the task.
    pub(crate) async fn start(pool: &PgPool) -> Result<Self, StoreError> {
        let listener = listen(pool).await?;
        let hub = Arc::new(Hub {
            head: watch::Sender::new(current_head(pool).await?),
            scoped: Mutex::default(),
        });
        let (connected_tx, connected) = watch::channel(true);
        let task = tokio::spawn(follow(
            pool.clone(),
            listener,
            Arc::clone(&hub),
            connected_tx,
        ));
        Ok(Self {
            hub,
            connected,
            task,
        })
    }

    /// A feed of every commit, whose first wake-up is the first advance after this call.
    pub(crate) fn subscribe(&self) -> LogFeed {
        self.hub.subscribe(None)
    }

    /// A feed of the commits in `scope` only.
    pub(crate) fn subscribe_to(&self, scope: Scope) -> LogFeed {
        self.hub.subscribe(Some(scope))
    }

    pub(crate) fn connected(&self) -> bool {
        *self.connected.borrow()
    }
}

impl Drop for FeedHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The payload `append` sends for a commit of `streams` in `workspace` at `head`.
pub(crate) fn payload(
    head: Position,
    workspace: Uuid,
    streams: &[StreamRef],
) -> Result<String, StoreError> {
    let mut touched: Vec<(String, Uuid)> = streams
        .iter()
        .map(|stream| (stream.kind.as_str().to_owned(), stream.id))
        .collect();
    touched.sort();
    touched.dedup();
    serde_json::to_string(&Commit {
        head: head.get(),
        workspace,
        streams: touched,
    })
    .map_err(StoreError::Encode)
}

/// A listener on [`CHANNEL`].
async fn listen(pool: &PgPool) -> Result<PgListener, StoreError> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}

/// The head as the database has it.
async fn current_head(pool: &PgPool) -> Result<Position, StoreError> {
    let head = sqlx::query_scalar!("SELECT position FROM event_log_head")
        .fetch_one(pool)
        .await?;
    Position::from_stored(head)
        .ok()
        .ok_or(StoreError::Corrupt("event_log_head.position"))
}

/// The listener task: publish every commit; on a lost connection, reconnect with backoff and
/// wake everyone at the database's head, since notifications sent meanwhile are gone.
async fn follow(
    pool: PgPool,
    mut listener: PgListener,
    hub: Arc<Hub>,
    connected: watch::Sender<bool>,
) {
    loop {
        if let Ok(Some(notification)) = listener.try_recv().await {
            if let Ok(commit) = serde_json::from_str::<Commit>(notification.payload()) {
                hub.publish(&commit);
            } else {
                tracing::error!("ignored a malformed log notification");
            }
        } else {
            connected.send_replace(false);
            tracing::warn!("lost the log listener's connection; reconnecting");
            let (fresh, head) = resume(&pool).await;
            listener = fresh;
            hub.publish_everywhere(head);
            connected.send_replace(true);
            tracing::info!("log listener reconnected");
        }
    }
}

/// Listens again and reads the head, retrying with capped backoff until both succeed. The head is
/// read after listening, so a commit in between is either notified or included.
async fn resume(pool: &PgPool) -> (PgListener, Position) {
    let mut pause = Duration::from_millis(100);
    loop {
        let attempt =
            async { Ok::<_, StoreError>((listen(pool).await?, current_head(pool).await?)) };
        match attempt.await {
            Ok(resumed) => return resumed,
            Err(error) => {
                tracing::warn!(error = %error, "could not reconnect the log listener");
                tokio::time::sleep(pause).await;
                pause = pause.saturating_mul(2).min(MAX_RECONNECT_PAUSE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> Hub {
        Hub {
            head: watch::Sender::new(Position::ORIGIN),
            scoped: Mutex::default(),
        }
    }

    fn commit(head: i64, workspace: u128, worker: u128) -> Commit {
        Commit {
            head,
            workspace: Uuid::from_u128(workspace),
            streams: vec![("worker".to_owned(), Uuid::from_u128(worker))],
        }
    }

    fn handle(connected: bool, task: tokio::task::JoinHandle<()>) -> FeedHandle {
        let (_state, connected) = watch::channel(connected);
        FeedHandle {
            hub: Arc::new(hub()),
            connected,
            task,
        }
    }

    #[tokio::test]
    async fn the_handle_reports_the_listeners_connection() {
        assert!(
            handle(true, tokio::spawn(async {})).connected(),
            "connected"
        );
        assert!(
            !handle(false, tokio::spawn(async {})).connected(),
            "reconnecting"
        );
    }

    #[tokio::test]
    async fn dropping_the_handle_stops_the_listener_task() {
        let (alive, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _alive = alive;
            std::future::pending::<()>().await;
        });
        drop(handle(true, task));
        assert!(
            stopped.await.is_err(),
            "the task was aborted, dropping what it held"
        );
    }

    #[tokio::test]
    async fn scoped_feeds_wake_only_for_what_they_follow() {
        let hub = hub();
        let mut all = hub.subscribe(None);
        let mut home = hub.subscribe(Some(Scope::Workspace(Uuid::from_u128(1))));
        let mut worker = hub.subscribe(Some(Scope::Stream(StreamKind::Worker, Uuid::from_u128(9))));
        hub.publish(&commit(1, 2, 8));
        assert_eq!(
            all.changed().await.map(Position::get),
            Some(1),
            "the log moved"
        );
        assert!(
            !home.head.has_changed().unwrap(),
            "another workspace wakes no one here"
        );
        assert!(!worker.head.has_changed().unwrap(), "nor another worker");
        hub.publish(&commit(2, 1, 9));
        assert_eq!(
            home.changed().await.map(Position::get),
            Some(2),
            "this workspace"
        );
        assert_eq!(
            worker.changed().await.map(Position::get),
            Some(2),
            "this worker"
        );
        hub.publish(&commit(2, 1, 9));
        assert!(
            !worker.head.has_changed().unwrap(),
            "the same head wakes no one"
        );
        hub.publish(&Commit {
            head: 3,
            workspace: Uuid::from_u128(1),
            streams: vec![("unknown".to_owned(), Uuid::from_u128(9))],
        });
        assert!(
            !worker.head.has_changed().unwrap(),
            "an unknown stream kind is no one's"
        );
        hub.publish(&Commit {
            head: -1,
            workspace: Uuid::from_u128(1),
            streams: Vec::new(),
        });
        assert_eq!(
            home.changed().await.map(Position::get),
            Some(3),
            "a negative head is dropped"
        );
        hub.publish_everywhere(Position::from_stored(5).unwrap());
        assert_eq!(
            worker.changed().await.map(Position::get),
            Some(5),
            "a reconnect wakes all"
        );
        assert_eq!(home.head().get(), 5, "home too");
    }

    #[tokio::test]
    async fn channels_no_one_follows_are_dropped() {
        let hub = hub();
        let feed = hub.subscribe(Some(Scope::Workspace(Uuid::from_u128(1))));
        assert_eq!(hub.scoped().len(), 1, "one followed scope");
        drop(feed);
        hub.publish(&commit(1, 2, 3));
        assert!(hub.scoped().is_empty(), "dropped once no one follows it");
        drop(hub);
    }

    #[test]
    fn the_payload_names_each_stream_once() {
        let stream = StreamRef {
            kind: StreamKind::Worker,
            id: Uuid::from_u128(3),
        };
        let text = payload(
            Position::from_stored(4).unwrap(),
            Uuid::from_u128(1),
            &[stream, stream],
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Commit>(&text).unwrap(),
            commit(4, 1, 3),
            "head, workspace, and the worker stream once"
        );
    }

    #[tokio::test]
    async fn a_closed_feed_ends() {
        let hub = hub();
        let mut feed = hub.subscribe(None);
        drop(hub);
        assert_eq!(feed.changed().await, None, "no store, no more heads");
    }
}
