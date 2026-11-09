//! abc's system of record and the only crate that links sqlx.
//!
//! You tell the store **who is calling** (a [`Caller`], an unverified claim) and **what they want**
//! (an abc-core command or query). [`Store::execute`] and [`Store::query`] authenticate, authorize,
//! decide and record (or read) inside one transaction and return a typed result. There is no other
//! way to write: the transaction type, the projections and the pool are private.
//!
//! ```ignore
//! let db = abc_store::connect(&database_url, Duration::from_secs(60)).await?;
//! abc_store::migrate(&db).await?;
//! let store = Store::open(db, StoreOptions::production()).await?;
//! let issued = store.execute(&Caller::Tailnet(login), &origin, IssueEnrolmentToken { .. }).await?;
//! let machines = store.query(&Caller::Tailnet(login), ListMachines { workspace }).await?;
//! let mut feed = store.feed(); // wakes when the log head advances
//! ```

mod error;
mod execute;
mod facts;
mod feed;
mod projection;
mod read;
#[cfg(feature = "testing")]
pub mod testing;

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use abc_core::command::{IssueSetupCode, Reservation, SetupCodeIssued};
use abc_core::credential::WorkerKey;
use abc_core::decision::{DecideCtx, Origin};
use abc_core::id::{FromUuid, WorkerId, WorkspaceId};
use abc_core::identity::TailnetLogin;
use abc_core::secret::Secret;
use abc_core::time::Clock;
use abc_platform::entropy::{ChaChaMint, Entropy, SystemEntropy};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub use error::{AuthError, ExecuteError, NotReady, OpenError, QueryError, StoreError};
pub use execute::Executable;
pub use feed::LogFeed;
pub use read::Fetchable;

/// The migrations, which [`migrate`] applies.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// Who is calling, as transport proved it. **Not trusted**: the store resolves it into an
/// `abc_core::actor::Principal` inside each request's transaction.
#[derive(Debug)]
pub enum Caller {
    /// A tailnet person, as the Tailscale sidecar asserted. Only abc-server's sidecar listener
    /// builds this.
    Tailnet(TailnetLogin),
    /// An enrolled worker presenting its key (`Authorization: Bearer abc_wk_…`).
    Worker(Secret<WorkerKey>),
    /// No identity. Used by enrolment (the token in the command is the authority) and by tagged
    /// devices.
    Anonymous,
    /// The installation itself: abc-server's own bookkeeping (ending sessions it saw close, the
    /// boot sweep) and the host operator's CLI. The policy matrix grants it only
    /// `IssueSetupCode` (while unclaimed), `RecordAsServer` and `ViewAsServer`, so using it from a
    /// request handler buys nothing else.
    Server,
}

/// A connection pool to abc's database. Opaque, so no sqlx type appears in this crate's public API
/// and abc-server never names one.
#[derive(Debug, Clone)]
pub struct Database(PgPool);

/// Postgres's SQLSTATE for a missing table.
const UNDEFINED_TABLE: &str = "42P01";

/// The advisory lock key one server process holds for its whole life: `abc_srvr` in ASCII.
const INSTALLATION_LOCK: i64 = 0x6162_635f_7372_7672;

/// Proof that this process is the one server for its database. Dropping it closes its connection,
/// which releases the lock.
pub struct InstallationLock {
    _conn: sqlx::PgConnection,
}

impl std::fmt::Debug for InstallationLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstallationLock")
    }
}

/// The most connections the store opens. Writes queue on the log head one at a time, so more would
/// only wait; reads and the SSE streams share the rest.
const MAX_CONNECTIONS: u32 = 16;

/// How long a request waits for a free connection before failing as unavailable.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The first pause between connection attempts; it doubles up to [`MAX_RETRY_PAUSE`].
const FIRST_RETRY_PAUSE: Duration = Duration::from_millis(250);

/// The longest pause between connection attempts.
const MAX_RETRY_PAUSE: Duration = Duration::from_secs(5);

/// Connects to Postgres, retrying with capped backoff for up to `patience` while the database
/// starts: launchd starts abc server and Postgres at once (P4). Logs each failed attempt without the URL.
///
/// # Errors
///
/// Returns [`OpenError::InvalidUrl`] for a URL that does not parse and [`OpenError::Connect`]
/// once `patience` runs out.
pub async fn connect(url: &Secret<DatabaseUrl>, patience: Duration) -> Result<Database, OpenError> {
    let options = PgConnectOptions::from_str(&url.expose_secret().0)
        .ok()
        .ok_or(OpenError::InvalidUrl)?;
    let mut last = None;
    let attempts = async {
        let mut pause = FIRST_RETRY_PAUSE;
        let mut attempt = 1_u32;
        loop {
            let result = PgPoolOptions::new()
                .max_connections(MAX_CONNECTIONS)
                .acquire_timeout(ACQUIRE_TIMEOUT)
                .connect_with(options.clone())
                .await;
            match result {
                Ok(pool) => return pool,
                Err(error) => {
                    tracing::warn!(attempt, "Postgres is not reachable yet; retrying");
                    last = Some(error);
                    tokio::time::sleep(pause).await;
                    pause = pause.saturating_mul(2).min(MAX_RETRY_PAUSE);
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    };
    match tokio::time::timeout(patience, attempts).await {
        Ok(pool) => Ok(Database(pool)),
        Err(_) => Err(OpenError::Connect(
            last.unwrap_or(sqlx::Error::PoolTimedOut),
        )),
    }
}

/// The Postgres URL. A secret: it usually holds a password.
#[derive(zeroize::Zeroize)]
pub struct DatabaseUrl(String);

impl std::fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DatabaseUrl(<redacted>)")
    }
}

impl DatabaseUrl {
    /// Wraps the raw URL. Validation happens when connecting, and its errors never echo the URL.
    #[must_use]
    pub fn new(raw: String) -> Secret<Self> {
        Secret::new(Self(raw))
    }
}

/// Applies pending migrations. Safe to run from several processes: sqlx takes an advisory lock.
/// The server calls it on every start (Postgres 18).
///
/// # Errors
///
/// Returns [`OpenError::Migrate`].
pub async fn migrate(db: &Database) -> Result<(), OpenError> {
    MIGRATOR.run(&db.0).await.map_err(OpenError::Migrate)
}

/// How a store reads time and makes fresh ids and secrets.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// The clock: the system clock in production, `testing::ManualClock` in tests.
    pub clock: Arc<dyn Clock>,
    /// Where each request's generator is seeded from: the operating system in production, a
    /// fixed sequence (`testing::SeededEntropy`) in tests.
    pub entropy: Arc<dyn Entropy>,
}

impl StoreOptions {
    /// The system clock and OS entropy.
    #[must_use]
    pub fn production() -> Self {
        Self {
            clock: Arc::new(execute::SystemClock),
            entropy: Arc::new(SystemEntropy),
        }
    }
}

/// The store. Cheap to clone; every clone shares one pool and one feed. It holds no domain state
/// in memory: everything it knows is in Postgres, so a restart changes nothing.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    pool: PgPool,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    feed: feed::FeedHandle,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Opens the store on a migrated database: it checks that exactly this build's migrations are
    /// applied and starts the log listener.
    ///
    /// `connect`, `migrate` and `open` are separate on purpose: the server reports each phase on
    /// `/readyz` (starting, migrating, ready).
    ///
    /// # Errors
    ///
    /// Returns [`OpenError::NotMigrated`] if the applied migrations differ from this build's, or
    /// [`OpenError::Database`].
    pub async fn open(db: Database, options: StoreOptions) -> Result<Self, OpenError> {
        let pool = db.0;
        let applied = sqlx::query_scalar!(
            "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version"
        )
        .fetch_all(&pool)
        .await;
        let applied: BTreeSet<i64> = match applied {
            Ok(versions) => versions.into_iter().collect(),
            // No migration table: never migrated.
            Err(sqlx::Error::Database(error))
                if error.code().as_deref() == Some(UNDEFINED_TABLE) =>
            {
                return Err(OpenError::NotMigrated);
            }
            Err(error) => return Err(StoreError::Database(error).into()),
        };
        let expected: BTreeSet<i64> = MIGRATOR
            .iter()
            .filter(|migration| migration.migration_type.is_up_migration())
            .map(|migration| migration.version)
            .collect();
        if applied != expected {
            return Err(OpenError::NotMigrated);
        }
        let feed = feed::FeedHandle::start(&pool).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                pool,
                clock: options.clock,
                entropy: options.entropy,
                feed,
            }),
        })
    }

    /// Runs `command` for `caller`: the only way to change anything. See `execute.rs` for the
    /// transaction it runs in.
    ///
    /// # Errors
    ///
    /// [`ExecuteError::Auth`] when the caller's credential does not resolve, [`ExecuteError::Denied`]
    /// when the policy refuses, [`ExecuteError::Rejected`] when the command's own rules refuse,
    /// [`ExecuteError::Store`] when the database fails.
    pub async fn execute<C: Executable>(
        &self,
        caller: &Caller,
        origin: &Origin,
        command: C,
    ) -> Result<C::Outcome, ExecuteError<C::Rejection>> {
        execute::run(&self.inner, caller, origin, command).await
    }

    /// Answers `query` for `caller`, after the same permission check. Runs in a REPEATABLE READ,
    /// READ ONLY transaction, so a multi-statement answer is one consistent snapshot. It does not
    /// take the head lock.
    ///
    /// # Errors
    ///
    /// [`QueryError::Auth`], [`QueryError::Denied`] or [`QueryError::Store`].
    pub async fn query<Q: Fetchable>(
        &self,
        caller: &Caller,
        query: Q,
    ) -> Result<Q::Answer, QueryError> {
        read::run(&self.inner, caller, query).await
    }

    /// Issues a fresh owner setup code for an unclaimed server: runs `IssueSetupCode` as
    /// `Caller::Server`, with candidate ids from this store's generator. Only the host operator's
    /// `abc server setup-code` calls it, which prints the code to the operator's terminal; the
    /// running server never holds a plaintext code. `IssueSetupCode` is not [`Executable`], so
    /// this is the only way to run it, and `clippy.toml` disallows this method everywhere but the
    /// one sanctioned call.
    ///
    /// # Errors
    ///
    /// [`ExecuteError::Denied`] with `ServerAlreadyClaimed` once claimed; [`ExecuteError::Store`].
    pub async fn issue_setup_code(&self) -> Result<SetupCodeIssued, ExecuteError<Infallible>> {
        let candidate = Reservation {
            owner: self.fresh_id()?,
            workspace: self.fresh_id()?,
        };
        let origin = Origin {
            correlation: self.fresh_id()?,
            causation: None,
        };
        execute::run(
            &self.inner,
            &Caller::Server,
            &origin,
            IssueSetupCode { candidate },
        )
        .await
    }

    /// A fresh id of any kind from this store's generator: a `UUIDv7` at the store's clock.
    ///
    /// # Errors
    ///
    /// [`StoreError::Entropy`] when no seed can be drawn.
    pub fn fresh_id<T: FromUuid>(&self) -> Result<T, StoreError> {
        let mint = ChaChaMint::from_seed(self.inner.entropy.seed()?);
        Ok(DecideCtx::new(self.inner.clock.now(), &mint).id())
    }

    /// Claims this database for one server process until the returned guard drops: a session-level
    /// advisory lock on a connection of its own. `None` when another process holds it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`].
    pub async fn lock_installation(&self) -> Result<Option<InstallationLock>, StoreError> {
        let mut conn = self.inner.pool.acquire().await?.detach();
        let held = sqlx::query_scalar!(
            r#"SELECT pg_try_advisory_lock($1) AS "held!""#,
            INSTALLATION_LOCK
        )
        .fetch_one(&mut conn)
        .await?;
        Ok(held.then_some(InstallationLock { _conn: conn }))
    }

    /// A handle that wakes whenever the log head advances. See [`LogFeed`].
    #[must_use]
    pub fn feed(&self) -> LogFeed {
        self.inner.feed.subscribe()
    }

    /// A handle that wakes only for commits in `workspace`.
    #[must_use]
    pub fn workspace_feed(&self, workspace: WorkspaceId) -> LogFeed {
        self.inner
            .feed
            .subscribe_to(feed::Scope::Workspace(workspace.as_uuid()))
    }

    /// A handle that wakes only for commits to `worker`'s stream.
    #[must_use]
    pub fn worker_feed(&self, worker: WorkerId) -> LogFeed {
        self.inner.feed.subscribe_to(feed::Scope::Stream(
            abc_core::event::StreamKind::Worker,
            worker.as_uuid(),
        ))
    }

    /// Readiness: the database answers within a second and the log listener is connected.
    ///
    /// # Errors
    ///
    /// Returns [`NotReady`] naming what is down.
    pub async fn ready(&self) -> Result<(), NotReady> {
        let ping = sqlx::query_scalar!(r#"SELECT 1 AS "one!""#).fetch_one(&self.inner.pool);
        match tokio::time::timeout(Duration::from_secs(1), ping).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => return Err(NotReady::Database),
        }
        if self.inner.feed.connected() {
            Ok(())
        } else {
            Err(NotReady::Listener)
        }
    }
}
