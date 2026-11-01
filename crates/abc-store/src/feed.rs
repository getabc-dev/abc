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
//! * **Reconnects are safe.** When the listener's connection drops, sqlx reconnects. Notifications
//!   during the gap are lost, so after reconnecting the task reads `event_log_head` and publishes
//!   it, which wakes everyone to re-read from their cursors.
//! * **Health.** The task's connection state feeds `Store::ready`.

use abc_core::event::Position;
use tokio::sync::watch;

/// Wakes when the log head advances. Clone one per subscriber.
#[derive(Debug, Clone)]
pub struct LogFeed {
    head: watch::Receiver<Position>,
}

impl LogFeed {
    /// Waits until the head moves past what this handle last saw, then returns the new head.
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
    /// Connects a `PgListener` on the store's pool, LISTENs on `abc_log`, seeds the watch from
    /// `event_log_head`, and spawns the task.
    pub(crate) async fn start(pool: &sqlx::PgPool) -> Result<Self, crate::error::StoreError> {
        let _ = pool;
        unimplemented!()
    }

    pub(crate) fn subscribe(&self) -> LogFeed {
        LogFeed {
            head: self.head.clone(),
        }
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
