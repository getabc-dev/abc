//! abc's system of record and the only crate that links sqlx.
//!
//! You tell the store **who is calling** (a [`Caller`], an unverified claim) and **what they want**
//! (an abc-core command or query). [`Store::execute`] and [`Store::query`] authenticate, authorize,
//! decide and record (or read) inside one transaction and return a typed result. There is no other
//! way to write: the transaction type, the projections and the pool are private.
//!
//! ```ignore
//! let db = abc_store::connect(&database_url).await?;
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

use std::sync::Arc;

use std::convert::Infallible;

use abc_core::command::SetupCodeIssued;
use abc_core::credential::WorkerKey;
use abc_core::decision::Mint;
use abc_core::identity::TailnetLogin;
use abc_core::secret::Secret;
use abc_core::time::Clock;
use sqlx::PgPool;
use sqlx::migrate::Migrator;

pub use error::{AuthError, ExecuteError, NotReady, OpenError, QueryError, StoreError};
pub use execute::Executable;
pub use feed::LogFeed;
pub use read::Fetchable;

/// The migrations, for `#[sqlx::test(migrator = "abc_store::MIGRATOR")]` and for [`migrate`].
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// Who is calling, as transport proved it. **Not trusted**: the store resolves it into an
/// `abc_core::actor::Principal` inside each request's transaction.
#[derive(Debug)]
pub enum Caller {
    /// A tailnet person, as the Tailscale sidecar asserted. Only abc-server's sidecar listener
    /// builds this (see `abc_server::sidecar`).
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
/// and abc-server never names one (tests reach the pool `#[sqlx::test]` provides through
/// `testing::database`).
#[derive(Debug, Clone)]
pub struct Database(PgPool);

/// Connects to Postgres, retrying with capped backoff while the database starts (compose start
/// order, P4). Logs each attempt without the URL.
///
/// # Errors
///
/// Returns [`OpenError::Connect`] after the retry budget.
pub async fn connect(url: &Secret<DatabaseUrl>) -> Result<Database, OpenError> {
    let _ = url;
    unimplemented!(
        "PgPoolOptions with acquire_timeout; backoff 0.25 s doubling to 5 s, 60 s budget"
    )
}

/// The Postgres URL. A secret: it usually holds a password.
#[derive(Clone, zeroize::Zeroize)]
pub struct DatabaseUrl(String);

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
    /// The clock: `SystemClock` in production, `testing::ManualClock` in tests.
    pub clock: Arc<dyn Clock>,
    /// The mint: `OsMint` in production, `testing::SeededMint` in tests.
    pub mint: Arc<dyn Mint>,
}

impl StoreOptions {
    /// The system clock and OS entropy.
    #[must_use]
    pub fn production() -> Self {
        unimplemented!("SystemClock + OsMint (private types in execute.rs)")
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
    mint: Arc<dyn Mint>,
    feed: feed::FeedHandle,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Opens the store on a migrated database: it checks that every migration is applied and
    /// starts the log listener. Tests call this with `testing::database(pool)` on the pool
    /// `#[sqlx::test]` gives them.
    ///
    /// `connect`, `migrate` and `open` are separate on purpose: the server reports each phase on
    /// `/readyz` (starting, migrating, ready).
    ///
    /// # Errors
    ///
    /// Returns [`OpenError::NotMigrated`] if migrations are missing, or [`OpenError::Database`].
    pub async fn open(db: Database, options: StoreOptions) -> Result<Self, OpenError> {
        // 1. compare MIGRATOR.iter() with _sqlx_migrations; any missing -> NotMigrated.
        // 2. feed = feed::FeedHandle::start(&pool).await? (PgListener on 'abc_log').
        let _ = (db.0, options);
        unimplemented!()
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
        origin: &abc_core::decision::Origin,
        command: C,
    ) -> Result<C::Outcome, ExecuteError<C::Rejection>>
    where
        C::Facts: Send,
    {
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

    /// Issues a fresh owner setup code for an unclaimed server: `execute(&Caller::Server, ..,
    /// IssueSetupCode { candidate })` with candidate ids from this store's mint. Called only by the
    /// `abc server setup-code` CLI, which prints the code to the operator's terminal; the running
    /// server never holds a plaintext code.
    ///
    /// # Errors
    ///
    /// [`ExecuteError::Denied`] with `ServerAlreadyClaimed` once claimed; [`ExecuteError::Store`].
    pub async fn issue_setup_code(&self) -> Result<SetupCodeIssued, ExecuteError<Infallible>> {
        unimplemented!("mint a Reservation; execute IssueSetupCode with it as Caller::Server")
    }

    /// Whether the server has been claimed. Installation state, not tenant data: the server logs
    /// "unclaimed; run `abc server setup-code`" at start when this is false, and nothing else
    /// reads it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`].
    pub async fn is_claimed(&self) -> Result<bool, StoreError> {
        unimplemented!("SELECT EXISTS (SELECT 1 FROM installation)")
    }

    /// A handle that wakes whenever the log head advances. See [`LogFeed`].
    #[must_use]
    pub fn feed(&self) -> LogFeed {
        self.inner.feed.subscribe()
    }

    /// Readiness: the database answers within a second and the log listener is connected.
    ///
    /// # Errors
    ///
    /// Returns [`NotReady`] naming what is down.
    pub async fn ready(&self) -> Result<(), NotReady> {
        unimplemented!()
    }
}
