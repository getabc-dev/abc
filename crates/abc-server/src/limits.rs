//! Load limits: how many requests a source or a worker may make in a minute, and how many sockets
//! a worker may hold. Over a limit, a request is refused with `rate_limited` (429) before it
//! touches the store.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// The limits a server enforces. The defaults suit one person's machines; tests tighten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Enrolment attempts per minute from one source (the address Serve forwards).
    pub enrolments_per_minute: u32,
    /// Worker sessions one worker may start per minute.
    pub session_starts_per_minute: u32,
    /// `WebSocket`s one worker may hold at once, handshake or not.
    pub sockets_per_worker: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            enrolments_per_minute: 20,
            session_starts_per_minute: 30,
            sockets_per_worker: 4,
        }
    }
}

/// The length of a rate-limit window.
const WINDOW: Duration = Duration::from_secs(60);

/// How many keys a limiter tracks before it forgets the ones whose window ended.
const PRUNE_AT: usize = 1024;

/// Counts requests per key in fixed one-minute windows.
#[derive(Debug, Clone)]
pub(crate) struct RateLimiter<K: Ord> {
    per_minute: u32,
    windows: Arc<Mutex<BTreeMap<K, (Instant, u32)>>>,
}

impl<K: Ord> RateLimiter<K> {
    pub(crate) fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            windows: Arc::default(),
        }
    }

    /// Counts one request for `key`, and says whether it is within the limit.
    pub(crate) fn allow(&self, key: K) -> bool {
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        if windows.len() >= PRUNE_AT {
            windows.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
        }
        let (start, count) = windows.entry(key).or_insert((now, 0));
        if now.duration_since(*start) >= WINDOW {
            *start = now;
            *count = 0;
        }
        *count = count.saturating_add(1);
        *count <= self.per_minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_key_gets_its_quota_per_window() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.allow("a"), "first");
        assert!(limiter.allow("a"), "second");
        assert!(!limiter.allow("a"), "third is over");
        assert!(limiter.allow("b"), "another key has its own");
        tokio::time::advance(WINDOW).await;
        assert!(limiter.allow("a"), "a new window, a new quota");
    }

    #[tokio::test(start_paused = true)]
    async fn finished_windows_are_forgotten_when_many_keys_pile_up() {
        let limiter = RateLimiter::new(1);
        for key in 0..PRUNE_AT - 1 {
            assert!(limiter.allow(key), "{key}");
        }
        tokio::time::advance(WINDOW + Duration::from_secs(1)).await;
        assert!(limiter.allow(PRUNE_AT), "a fresh key");
        assert!(limiter.allow(PRUNE_AT + 1), "another, which prunes");
        assert_eq!(
            limiter.windows.lock().unwrap().len(),
            2,
            "only the live windows are kept"
        );
        assert_eq!(Limits::default().sockets_per_worker, 4, "the defaults");
    }
}
