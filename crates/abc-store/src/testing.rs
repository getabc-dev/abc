//! Test support (feature `testing`, dev-dependencies only). Every database test gets a fresh,
//! migrated database from `#[sqlx::test(migrator = "abc_store::MIGRATOR")]`, which runs against the
//! one warm Postgres container per machine (D7). Time and randomness are fixed, so tests are
//! deterministic (P2).

use std::sync::{Arc, Mutex};

use abc_core::decision::Mint;
use abc_core::time::{Clock, Timestamp};
use sqlx::PgPool;

use crate::{Database, StoreOptions};

/// Wraps the pool `#[sqlx::test]` hands a test (already migrated by `MIGRATOR`).
#[must_use]
pub fn database(pool: PgPool) -> Database {
    Database(pool)
}

/// A clock tests move by hand, for expiry.
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<Timestamp>>,
}

impl ManualClock {
    /// Starts at a fixed instant (2026-01-01T00:00:00Z).
    #[must_use]
    pub fn new() -> Self {
        unimplemented!()
    }

    /// Moves time forward.
    pub fn advance(&self, by: time::Duration) {
        let _ = by;
        unimplemented!()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        unimplemented!()
    }
}

/// A ChaCha-seeded mint: the same seed gives the same ids and secrets.
#[derive(Debug)]
pub struct SeededMint {
    rng: Mutex<rand_chacha::ChaCha20Rng>,
}

impl SeededMint {
    /// A mint seeded with `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let _ = seed;
        unimplemented!()
    }
}

impl Mint for SeededMint {
    fn uuid(&self, now: Timestamp) -> uuid::Uuid {
        let _ = now;
        unimplemented!("UUIDv7 with `now` as the time part and seeded random bits")
    }
    fn secret(&self) -> zeroize::Zeroizing<[u8; 32]> {
        unimplemented!()
    }
}

/// Options for a deterministic store.
#[must_use]
pub fn options(clock: &ManualClock, seed: u64) -> StoreOptions {
    StoreOptions {
        clock: Arc::new(clock.clone()),
        mint: Arc::new(SeededMint::new(seed)),
    }
}

/// Asserts that the views are exactly what replaying the log produces.
///
/// 1. Lists view tables from `information_schema.tables` (schema `public`, excluding `event`,
///    `event_log_head` and `_sqlx_migrations`), so a new view is covered without editing a list.
/// 2. Dumps each as `jsonb_agg(to_jsonb(t) ORDER BY <primary key>)`.
/// 3. In one transaction: `TRUNCATE` every view table, replay `SELECT * FROM event ORDER BY
///    position` through `projection::apply`, dump again, and roll back.
/// 4. Asserts the two dumps are equal, naming the first table that differs.
///
/// A write that bypasses the event path, or a projection that reads `now()`, fails here.
///
/// # Panics
///
/// On mismatch or database error (test code).
pub async fn assert_views_replay(pool: &PgPool) {
    let _ = pool;
    unimplemented!()
}
