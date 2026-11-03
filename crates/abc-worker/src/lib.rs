//! The abc worker: it enrols a machine once, keeps its key, and holds one dial-out link to the
//! server. The server never connects to a worker: workers sit behind NAT and go to sleep, so the
//! worker always dials.
//!
//! ```text
//! abc worker enrol <url>   lock the home -> save a new key (0600) -> POST /api/enrol with its
//!                          digest (the same request on every re-run) -> record who we are
//! abc worker               lock the home -> load the record -> loop { dial (TLS, bearer key) ->
//!                          Hello / Welcome -> hold the link until it ends -> policy: dial again
//!                          after a backoff, or stop because a person has to act }
//! ```
//!
//! The public surface is [`enrol()`], [`run`] and the values they take. Behind it: the key file and
//! its lock (`home`), TLS and the HTTP upgrade (`dial`), the handshake, liveness and sleep
//! detection (`session`), every reconnect decision (`policy`, pure) and the loop
//! (`supervisor`).

mod dial;
mod enrol;
mod home;
mod policy;
mod session;
mod supervisor;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use abc_core::id::{WorkerId, WorkspaceId};
use abc_core::identity::{Arch, DisplayName, Os};
use abc_platform::entropy::{Entropy as _, EntropyUnavailable, SystemEntropy};
use abc_protocol::link::{HANDSHAKE_TIMEOUT, IDLE_TIMEOUT, PING_INTERVAL};
use rand::rngs::ChaCha20Rng;
use rand::{RngExt as _, SeedableRng as _};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use dial::DialError;
pub use enrol::{EnrolError, Enrolment, enrol, machine};
pub use home::{HomeError, WorkerHome};
pub use policy::Terminal;

/// The operating system this build runs on.
#[cfg(target_os = "macos")]
pub(crate) const OS: Os = Os::Macos;
/// The operating system this build runs on.
#[cfg(target_os = "linux")]
pub(crate) const OS: Os = Os::Linux;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("abc workers run on macOS and Linux");

/// The CPU architecture this build runs on.
#[cfg(target_arch = "aarch64")]
pub(crate) const ARCH: Arch = Arch::Aarch64;
/// The CPU architecture this build runs on.
#[cfg(target_arch = "x86_64")]
pub(crate) const ARCH: Arch = Arch::X86_64;
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("abc workers run on aarch64 and x86_64");

/// Runs the worker until `shutdown` resolves (`Ok`) or the server says it must stop (`Err`).
/// Network trouble never ends it: every other failure is retried under the reconnect policy.
/// Holds the home's lock for its whole life, so one worker runs per home.
///
/// # Errors
///
/// [`RunError`]; [`RunError::exit_code`] tells the binary which code to exit with.
pub async fn run(
    home: &WorkerHome,
    env: &Env,
    shutdown: impl Future<Output = ()>,
) -> Result<(), RunError> {
    let held = home.lock()?;
    let record = held.load()?.ok_or(RunError::NotEnrolled)?;
    let enrolled = record.enrolled.ok_or(RunError::NotEnrolled)?;
    let span = tracing::info_span!(
        "worker",
        worker = %enrolled.worker,
        workspace = %enrolled.workspace,
        server = %record.server,
    );
    tracing::Instrument::instrument(
        async {
            tracing::info!(name = enrolled.name.as_str(), "starting");
            let result = supervisor::supervise(env, &record.server, &record.key, shutdown).await;
            if result.is_ok() {
                tracing::info!("stopped");
            }
            result
        },
        span,
    )
    .await?;
    drop(held);
    Ok(())
}

/// What to do when the home already holds an enrolment for another token or server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfEnrolled {
    /// Refuse with [`EnrolError::AlreadyEnrolled`].
    Refuse,
    /// Discard the old key and enrol as a new worker (`--replace`). The old worker stays on the
    /// server, offline, until its owner revokes it.
    Replace,
}

/// Who this machine is on the server, as enrolment answered; kept in the home's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enrolled {
    /// The worker's id.
    pub worker: WorkerId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Its display name.
    pub name: DisplayName,
}

/// Everything the worker takes from its surroundings: built once by the binary
/// ([`Env::production`]) or by a test (`testing::env`).
#[derive(Debug)]
pub struct Env {
    pub(crate) trust: Trust,
    pub(crate) timings: Timings,
    pub(crate) wall: Arc<dyn WallClock>,
    pub(crate) jitter: Jitter,
    pub(crate) build: semver::Version,
}

impl Env {
    /// The public web PKI's roots, the protocol's timings, the system clock and jitter seeded from
    /// the operating system, for a worker of release `build`.
    ///
    /// # Errors
    ///
    /// [`EnvError`] when the operating system cannot seed the jitter or TLS cannot be set up.
    pub fn production(build: semver::Version) -> Result<Self, EnvError> {
        Ok(Self {
            trust: Trust::public()?,
            timings: Timings::production(),
            wall: Arc::new(SystemClock),
            jitter: Jitter::from_seed(SystemEntropy.seed()?),
            build,
        })
    }
}

/// The worker's surroundings could not be set up.
#[derive(Debug, Error)]
pub enum EnvError {
    /// The operating system's random number generator failed.
    #[error(transparent)]
    Entropy(#[from] EntropyUnavailable),
    /// TLS could not be configured.
    #[error("could not set up TLS")]
    Tls(#[from] rustls::Error),
}

/// Which certificates may name the server. The worker trusts the public web PKI only: Tailscale
/// Serve presents a Let's Encrypt certificate for `<node>.<tailnet>.ts.net`, and verifying it for
/// the enrolled URL's host is how the worker knows it reached its own server before it sends its
/// key (threat model B3). There is deliberately no option to trust another CA or to skip TLS.
#[derive(Clone)]
pub struct Trust {
    config: Arc<rustls::ClientConfig>,
}

impl Trust {
    /// The public web PKI (`webpki-roots`).
    ///
    /// # Errors
    ///
    /// [`rustls::Error`] if the TLS configuration cannot be built.
    pub fn public() -> Result<Self, rustls::Error> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots)
    }

    fn with_roots(roots: rustls::RootCertStore) -> Result<Self, rustls::Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        // WebSocket upgrades are HTTP/1.1.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            config: Arc::new(config),
        })
    }

    pub(crate) fn config(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.config)
    }
}

impl fmt::Debug for Trust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Trust")
    }
}

/// Every duration the worker uses. [`Timings::production`] derives them from the protocol's;
/// tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// TCP, TLS and the upgrade, as one deadline.
    pub connect: Duration,
    /// The enrolment request, from connecting to the answer.
    pub request: Duration,
    /// From sending `Hello` to the server's answer.
    pub handshake: Duration,
    /// No frame for this long means the link is dead. The server pings every
    /// [`PING_INTERVAL`], so this allows two pings to go missing.
    pub silence: Duration,
    /// How often a session checks for silence and compares the clocks.
    pub watchdog_tick: Duration,
    /// Wall time ahead of monotonic time by more than this, between ticks, means the machine
    /// slept.
    pub wake_gap: Duration,
    /// The first retry's backoff ceiling.
    pub backoff_base: Duration,
    /// The largest backoff ceiling.
    pub backoff_cap: Duration,
    /// A link up for this long resets the backoff.
    pub stable_after: Duration,
    /// After `rate_limited`, wait at least this long.
    pub rate_limited_floor: Duration,
    /// How long a stopping worker waits for the server to answer its close.
    pub close_grace: Duration,
}

impl Timings {
    /// The timings `abc worker` runs with.
    #[must_use]
    pub const fn production() -> Self {
        Self {
            connect: Duration::from_secs(15),
            request: Duration::from_secs(30),
            handshake: HANDSHAKE_TIMEOUT,
            silence: Duration::from_secs(
                PING_INTERVAL.as_secs().saturating_mul(2).saturating_add(10),
            ),
            watchdog_tick: Duration::from_secs(5),
            wake_gap: Duration::from_secs(10),
            backoff_base: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(30),
            stable_after: IDLE_TIMEOUT,
            rate_limited_floor: Duration::from_secs(15),
            close_grace: Duration::from_secs(2),
        }
    }
}

/// Wall-clock time, which keeps running while the machine sleeps, unlike monotonic time. The
/// worker's only injected clock: tests move it to play a sleep. Monotonic time is tokio's.
pub trait WallClock: Send + Sync + fmt::Debug {
    /// Now.
    fn now(&self) -> SystemTime;
}

/// The operating system's wall clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock;

impl WallClock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The backoff's jitter: a `ChaCha20` generator seeded from the operating system, or from a fixed
/// seed in tests.
#[derive(Debug)]
pub(crate) struct Jitter(Mutex<ChaCha20Rng>);

impl Jitter {
    pub(crate) fn from_seed(seed: [u8; 32]) -> Self {
        Self(Mutex::new(ChaCha20Rng::from_seed(seed)))
    }

    /// A fraction drawn uniformly from `[0, 1)`.
    pub(crate) fn draw(&self) -> policy::Fraction {
        let value: f64 = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .random();
        policy::Fraction::new(value).unwrap_or(policy::Fraction::ZERO)
    }
}

/// Why [`run`] stopped. `Display` is the sentence the user reads.
#[derive(Debug, Error)]
pub enum RunError {
    /// The home holds no record, or only a pending one.
    #[error("this machine is not enrolled; run `abc worker enrol <url>` first")]
    NotEnrolled,
    /// The home is busy, insecure, damaged or unreadable.
    #[error(transparent)]
    Home(#[from] HomeError),
    /// The server said this worker must stop.
    #[error(transparent)]
    Terminal(#[from] Terminal),
}

impl RunError {
    /// 2 for configuration (fix it and run again), 3 when the server said stop: a person has to
    /// act, so a service manager should stop restarting it (`RestartPreventExitStatus=3`).
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::NotEnrolled | Self::Home(_) => 2,
            Self::Terminal(_) => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_production_timings_fit_the_protocol() {
        let t = Timings::production();
        assert!(
            PING_INTERVAL < t.silence && t.silence < IDLE_TIMEOUT,
            "a missed ping is tolerated, and the worker gives up before the server does"
        );
        assert!(
            t.watchdog_tick < t.wake_gap,
            "a tick is shorter than a sleep"
        );
        assert!(t.backoff_base < t.backoff_cap, "the backoff grows");
        assert!(t.close_grace < t.connect, "stopping is quick");
        assert_eq!(
            t.silence,
            Duration::from_secs(50),
            "two pings and ten seconds"
        );
    }

    #[test]
    fn exit_codes_say_who_has_to_act() {
        assert_eq!(RunError::NotEnrolled.exit_code(), 2, "configuration");
        assert_eq!(
            RunError::Home(HomeError::NoHomeDir).exit_code(),
            2,
            "configuration"
        );
        assert_eq!(
            RunError::Terminal(Terminal::Revoked).exit_code(),
            3,
            "a person"
        );
    }

    #[test]
    fn jitter_is_seeded_and_in_range() {
        let draws = |seed| {
            let jitter = Jitter::from_seed(seed);
            (0..100).map(|_| jitter.draw()).collect::<Vec<_>>()
        };
        assert_eq!(draws([1; 32]), draws([1; 32]), "a seed repeats its draws");
        assert_ne!(draws([1; 32]), draws([2; 32]), "and another seed does not");
        assert!(
            draws([3; 32])
                .windows(2)
                .any(|pair| pair.first() != pair.get(1)),
            "the draws vary"
        );
    }

    #[test]
    fn the_public_trust_builds() {
        let trust = Trust::public().unwrap();
        assert_eq!(
            trust.config().alpn_protocols,
            vec![b"http/1.1".to_vec()],
            "HTTP/1.1 only"
        );
        assert_eq!(format!("{trust:?}"), "Trust", "nothing to show");
    }
}
