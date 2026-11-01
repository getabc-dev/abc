//! [`LogFeed`]: how consumers learn that the log moved, without the notifier carrying events.
//!
//! `append` runs `pg_notify('abc_log', <new head>)` inside its transaction, and Postgres delivers
//! the notification only if and when the transaction commits. One `PgListener` task per store
//! publishes each head into a `tokio::sync::watch`. Every subscriber (SSE stream, worker session,
//! the S1 runner later) then follows the same loop:
//!
//! ```text
//! loop { feed.changed().await; read from MY cursor (an authorized query); advance my cursor }
//! ```
//!
//! What this buys:
//! * **No lost events.** The notifier is only a wake-up. Data always comes from the subscriber's
//!   own cursor, and positions are gapless and commit-ordered, so `position > cursor` misses
//!   nothing.
//! * **No unbounded buffers.** `watch` keeps one value. A burst of commits, or a slow subscriber,
//!   coalesces into one wake-up.
//! * **Reconnects are safe.** When the listener's connection drops, the task reconnects with
//!   backoff. Notifications during the gap are lost, so after reconnecting it reads
//!   `event_log_head` and publishes it, which wakes everyone to re-read from their cursors.
//! * **Health.** The task's connection state feeds `Store::ready`.

use std::time::Duration;

use abc_core::event::Position;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::watch;

use crate::error::StoreError;

/// The notification channel `append` sends the new head on.
const CHANNEL: &str = "abc_log";

/// The longest pause between reconnection attempts.
const MAX_RECONNECT_PAUSE: Duration = Duration::from_secs(5);

/// Wakes when the log head advances. Clone one per subscriber.
#[derive(Debug, Clone)]
pub struct LogFeed {
    head: watch::Receiver<Position>,
}

impl LogFeed {
    /// Waits until the head moves past what this handle last saw (at first, the head when it was
    /// subscribed), then returns the new head.
    /// Returns `None` when the store has shut down.
    pub async fn changed(&mut self) -> Option<Position> {
        self.head.changed().await.ok()?;
        Some(*self.head.borrow_and_update())
    }

    /// The latest head this process knows of.
    #[must_use]
    pub fn head(&self) -> Position {
        *self.head.borrow()
    }
}

/// Owns the listener task. It is aborted when the store's last clone drops.
#[derive(Debug)]
pub(crate) struct FeedHandle {
    head: watch::Receiver<Position>,
    connected: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl FeedHandle {
    /// Listens on the store's pool, seeds the watch from `event_log_head`, and spawns the task.
    pub(crate) async fn start(pool: &PgPool) -> Result<Self, StoreError> {
        let listener = listen(pool).await?;
        let head = current_head(pool).await?;
        let (head_tx, head_rx) = watch::channel(head);
        let (connected_tx, connected_rx) = watch::channel(true);
        let task = tokio::spawn(follow(pool.clone(), listener, head_tx, connected_tx));
        Ok(Self {
            head: head_rx,
            connected: connected_rx,
            task,
        })
    }

    /// A feed whose first wake-up is the first advance after this call.
    pub(crate) fn subscribe(&self) -> LogFeed {
        let mut head = self.head.clone();
        head.borrow_and_update();
        LogFeed { head }
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

/// Publishes `head` if it is ahead of what subscribers have.
fn publish(heads: &watch::Sender<Position>, head: Position) {
    heads.send_if_modified(|current| {
        let ahead = head > *current;
        if ahead {
            *current = head;
        }
        ahead
    });
}

/// The listener task: publish every notified head; on a lost connection, reconnect with backoff
/// and publish the database's head, since notifications sent meanwhile are gone.
async fn follow(
    pool: PgPool,
    mut listener: PgListener,
    heads: watch::Sender<Position>,
    connected: watch::Sender<bool>,
) {
    loop {
        if let Ok(Some(notification)) = listener.try_recv().await {
            let head = notification
                .payload()
                .parse()
                .ok()
                .map(Position::from_stored);
            if let Some(Ok(head)) = head {
                publish(&heads, head);
            } else {
                tracing::error!("ignored a malformed log notification");
            }
        } else {
            connected.send_replace(false);
            tracing::warn!("lost the log listener's connection; reconnecting");
            let (fresh, head) = resume(&pool).await;
            listener = fresh;
            publish(&heads, head);
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

    /// A handle around `task`, with the listener's connection state `connected`.
    fn handle(connected: bool, task: tokio::task::JoinHandle<()>) -> FeedHandle {
        let (_heads, head) = watch::channel(Position::ORIGIN);
        let (_state, connected) = watch::channel(connected);
        FeedHandle {
            head,
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
    async fn a_new_feed_wakes_only_for_heads_published_after_it() {
        let (heads, head) = watch::channel(Position::ORIGIN);
        let (_state, connected) = watch::channel(true);
        let feed = FeedHandle {
            head,
            connected,
            task: tokio::spawn(async {}),
        };
        publish(&heads, Position::from_stored(3).unwrap());
        let mut subscriber = feed.subscribe();
        assert_eq!(subscriber.head().get(), 3, "it starts at the head");
        publish(&heads, Position::from_stored(3).unwrap());
        assert!(
            !subscriber.head.has_changed().unwrap(),
            "the same head wakes no one"
        );
        publish(&heads, Position::from_stored(2).unwrap());
        publish(&heads, Position::from_stored(4).unwrap());
        assert_eq!(
            subscriber.changed().await.map(Position::get),
            Some(4),
            "an older head is ignored; a newer one wakes it"
        );
        drop(heads);
        assert_eq!(subscriber.changed().await, None, "a closed feed ends");
    }
}
