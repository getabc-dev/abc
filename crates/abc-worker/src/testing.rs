//! Test support, for this crate's tests and its integration tests (`testing` feature): a worker
//! environment that trusts one test CA, short timings, a wall clock the test moves and
//! deterministic keys.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use abc_core::credential::WorkerKey;
use abc_core::secret::Secret;
use abc_platform::entropy::ChaChaMint;

use crate::{Env, Jitter, Timings, Trust, WallClock};

/// An environment for tests: `trust`, `timings`, `wall`, jitter from a fixed seed, and release
/// 0.1.0.
#[must_use]
pub fn env(trust: Trust, timings: Timings, wall: Arc<dyn WallClock>) -> Env {
    Env {
        trust,
        timings,
        wall,
        jitter: Jitter::from_seed([7; 32]),
        build: semver::Version::new(0, 1, 0),
    }
}

/// Trust in `root` alone, the CA of a test's stand-in for Tailscale Serve.
///
/// # Errors
///
/// [`rustls::Error`] when `root` is not a usable CA certificate.
pub fn trust(root: rustls::pki_types::CertificateDer<'static>) -> Result<Trust, rustls::Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root)?;
    Trust::verified_by(|builder| Ok(builder.with_root_certificates(roots)))
}

/// Timings short enough for tests: the same shape as production's, in tens of milliseconds.
#[must_use]
pub const fn fast() -> Timings {
    Timings {
        connect: Duration::from_secs(5),
        request: Duration::from_secs(5),
        handshake: Duration::from_secs(2),
        silence: Duration::from_millis(600),
        watchdog_tick: Duration::from_millis(50),
        wake_gap: Duration::from_secs(10),
        backoff_base: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(100),
        stable_after: Duration::from_secs(60),
        rate_limited_floor: Duration::from_millis(100),
        close_grace: Duration::from_secs(1),
    }
}

/// A wall clock that runs with the system clock, plus whatever the test adds to play a sleep.
#[derive(Debug, Default)]
pub struct ManualWallClock {
    ahead: Mutex<Duration>,
}

impl ManualWallClock {
    /// Moves the clock forward by `by`, as if the machine had slept that long.
    pub fn sleep(&self, by: Duration) {
        let mut ahead = self.ahead.lock().unwrap_or_else(PoisonError::into_inner);
        *ahead = ahead.saturating_add(by);
    }
}

impl WallClock for ManualWallClock {
    fn now(&self) -> SystemTime {
        let ahead = *self.ahead.lock().unwrap_or_else(PoisonError::into_inner);
        SystemTime::now()
            .checked_add(ahead)
            .unwrap_or_else(SystemTime::now)
    }
}

/// A worker key that is the same on every run for the same `seed`.
#[must_use]
pub fn key(seed: u8) -> Secret<WorkerKey> {
    WorkerKey::generate(&ChaChaMint::from_seed([seed; 32]))
}
