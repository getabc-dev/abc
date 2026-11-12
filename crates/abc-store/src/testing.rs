//! Test support (feature `testing`, dev-dependencies only). Every database test gets a fresh,
//! migrated database of its own from [`TestDatabase::migrated`], on the server
//! `ABC_TEST_DATABASE_URL` names (not `DATABASE_URL`, so a developer's own database is never used): the one warm Postgres container per machine (D7). Time and randomness are fixed, so
//! tests are deterministic (P2).
//!
//! `#[sqlx::test]` would do the same, but it names each database after its test alone, so two runs
//! of one test at the same time (cargo-mutants' parallel jobs, or two checkouts sharing the warm
//! container) drop each other's database. These names are unique per run.
#![expect(
    clippy::panic,
    reason = "test support: a failed setup step fails the calling test at that spot"
)]

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use abc_core::actor::Actor;
use abc_core::command::{
    ClaimServer, Claimed, EnrolWorker, Enrolled, IssueEnrolmentToken, TokenIssued, TokenTtl,
};
use abc_core::credential::{EnrolmentToken, SetupCode, WorkerKey};
use abc_core::decision::{Mint, Origin};
use abc_core::event::{Event, Position, Recorded, StreamKind, StreamRef};
use abc_core::id::{CorrelationId, EventId, FromUuid as _, WorkspaceId};
use abc_core::identity::{Arch, DisplayName, MachineInfo, Os, TailnetLogin, WorkspaceName};
use abc_core::secret::Secret;
use abc_core::time::{Clock, Timestamp};
use rand::rngs::ChaCha20Rng;
use rand::{Rng as _, SeedableRng as _};
use semver::Version;
use std::str::FromStr as _;

use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions as _, Connection as _, PgConnection, PgPool};
use zeroize::Zeroizing;

use crate::error::StoreError;
use crate::{Caller, Database, DatabaseUrl, ExecuteError, Store, StoreOptions, projection};
use abc_platform::entropy::{ChaChaMint, Entropy, EntropyUnavailable};

/// Wraps a test's pool.
#[must_use]
pub fn database(pool: PgPool) -> Database {
    Database(pool)
}

/// A database that exists for one test: created empty or migrated, and dropped when this value
/// is, even when the test fails.
#[derive(Debug)]
pub struct TestDatabase {
    pool: PgPool,
    name: String,
    server: PgConnectOptions,
}

/// The variable naming the server test databases are created on.
pub const TEST_DATABASE_URL: &str = "ABC_TEST_DATABASE_URL";

/// How long a test database may outlive its test (a killed process cannot drop it) before a later
/// test sweeps it away.
const STALE_AFTER_SECONDS: u64 = 24 * 60 * 60;

impl TestDatabase {
    /// A fresh database with every migration applied.
    ///
    /// # Panics
    ///
    /// When the server is unreachable or the migrations fail (test code).
    pub async fn migrated() -> Self {
        let database = Self::empty().await;
        crate::MIGRATOR
            .run(&database.pool)
            .await
            .unwrap_or_else(|error| panic!("migrating the test database: {error}"));
        database
    }

    /// A fresh database with no tables.
    ///
    /// # Panics
    ///
    /// When `ABC_TEST_DATABASE_URL` is unset or the server is unreachable (test code).
    pub async fn empty() -> Self {
        let url = std::env::var(TEST_DATABASE_URL).unwrap_or_else(|_| {
            panic!("database tests need {TEST_DATABASE_URL}; `cargo xtask check` sets up Postgres")
        });
        let server = PgConnectOptions::from_str(&url)
            .unwrap_or_else(|error| panic!("{TEST_DATABASE_URL} does not parse: {error}"));
        let mut admin = PgConnection::connect_with(&server)
            .await
            .unwrap_or_else(|error| panic!("connecting to the test server: {error}"));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        sweep(&mut admin, now).await;
        let name = format!("abc_test_{now}_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name:?}")))
            .execute(&mut admin)
            .await
            .unwrap_or_else(|error| panic!("creating {name}: {error}"));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_with(server.clone().database(&name))
            .await
            .unwrap_or_else(|error| panic!("connecting to {name}: {error}"));
        Self { pool, name, server }
    }

    /// The database's pool.
    #[must_use]
    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }
}

/// Whether a test database created at `created` is a leftover at `now`, both in seconds since the
/// Unix epoch.
const fn stale(created: u64, now: u64) -> bool {
    now.saturating_sub(created) > STALE_AFTER_SECONDS
}

/// The prefixes of the databases tests and `cargo xtask check` create, each followed by the
/// creation time in seconds since the Unix epoch and `_`.
const DISPOSABLE: [&str; 2] = ["abc_test_", "abc_query_cache_"];

/// When the disposable database `name` was created, if it is one.
fn created(name: &str) -> Option<u64> {
    DISPOSABLE
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .and_then(|rest| rest.split('_').next())
        .and_then(|seconds| seconds.parse().ok())
}

/// Drops the disposable databases older than [`STALE_AFTER_SECONDS`]: leftovers of killed runs.
async fn sweep(admin: &mut PgConnection, now: u64) {
    let names: Vec<String> = sqlx::query_scalar(
        r"SELECT datname FROM pg_database
          WHERE datname LIKE 'abc\_test\_%' OR datname LIKE 'abc\_query\_cache\_%'",
    )
    .fetch_all(&mut *admin)
    .await
    .unwrap_or_default();
    for name in names {
        if created(&name).is_some_and(|created| stale(created, now)) {
            let _dropped = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {name:?} WITH (FORCE)"
            )))
            .execute(&mut *admin)
            .await;
        }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        let server = self.server.clone();
        // Drop is synchronous and may run on the test's runtime; a thread with its own runtime
        // drops the database, forcing out the connections the test left open.
        let dropped = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(async {
                let mut admin = PgConnection::connect_with(&server).await.ok()?;
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS {name:?} WITH (FORCE)"
                )))
                .execute(&mut admin)
                .await
                .ok()
            })
        });
        let _finished = dropped.join();
    }
}

/// The URL of the database behind `pool`, with `password` if given: the one way tests build a
/// database URL. It keeps everything the pool connects with, a Unix-socket host and query
/// parameters included.
///
/// # Panics
///
/// When the password cannot be set on the URL (test code).
#[must_use]
pub fn database_url_text(pool: &PgPool, password: Option<&str>) -> String {
    let mut url = pool.connect_options().to_url_lossy();
    if let Some(password) = password
        && url.set_password(Some(password)).is_err()
    {
        panic!("the test database URL cannot carry a password");
    }
    url.to_string()
}

/// [`database_url_text`] without a password, as the secret the server takes.
#[must_use]
pub fn database_url(pool: &PgPool) -> Secret<DatabaseUrl> {
    DatabaseUrl::new(database_url_text(pool, None))
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
        Self {
            now: Arc::new(Mutex::new(Timestamp::from_utc(
                time::macros::datetime!(2026-01-01 00:00:00 UTC),
            ))),
        }
    }

    /// Moves time forward.
    pub fn advance(&self, by: time::Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now = now.checked_add(by).unwrap_or(*now);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A `ChaCha20`-seeded mint: the same seed gives the same ids and secrets.
#[derive(Debug)]
pub struct SeededMint(ChaChaMint);

/// 32 seed bytes drawn deterministically from `seed`.
fn expand(seed: u64) -> [u8; 32] {
    let mut bytes = [0; 32];
    ChaCha20Rng::seed_from_u64(seed).fill_bytes(&mut bytes);
    bytes
}

impl SeededMint {
    /// A mint seeded with `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(ChaChaMint::from_seed(expand(seed)))
    }
}

impl Mint for SeededMint {
    fn uuid(&self, now: Timestamp) -> uuid::Uuid {
        self.0.uuid(now)
    }

    fn secret(&self) -> Zeroizing<[u8; 32]> {
        self.0.secret()
    }
}

/// Seeds from a fixed sequence: every run of a test mints the same values.
#[derive(Debug)]
pub struct SeededEntropy(Mutex<ChaCha20Rng>);

impl SeededEntropy {
    /// The sequence that starts from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(Mutex::new(ChaCha20Rng::seed_from_u64(seed)))
    }
}

impl Entropy for SeededEntropy {
    fn seed(&self) -> Result<[u8; 32], EntropyUnavailable> {
        let mut bytes = [0; 32];
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fill_bytes(&mut bytes);
        Ok(bytes)
    }
}

/// Options for a deterministic store.
#[must_use]
pub fn options(clock: &ManualClock, seed: u64) -> StoreOptions {
    StoreOptions {
        clock: Arc::new(clock.clone()),
        entropy: Arc::new(SeededEntropy::new(seed)),
    }
}

/// The view tables, found from the catalogue rather than from a list kept by hand.
async fn view_tables(conn: &mut PgConnection) -> Result<Vec<String>, StoreError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT table_name AS "name!" FROM information_schema.tables
           WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
             AND table_name NOT IN ('event', 'event_log_head', '_sqlx_migrations')
           ORDER BY table_name"#
    )
    .fetch_all(conn)
    .await?)
}

/// Every view table's rows as JSON, in a stable order.
async fn dump(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<Vec<(String, serde_json::Value)>, StoreError> {
    let mut dumps = Vec::with_capacity(tables.len());
    for table in tables {
        let rows: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb)
             FROM {table:?} t"
        )))
        .fetch_one(&mut *conn)
        .await?;
        dumps.push((table.clone(), rows));
    }
    Ok(dumps)
}

/// Every event in the log, in order.
async fn log(conn: &mut PgConnection) -> Result<Vec<Recorded>, StoreError> {
    let rows = sqlx::query!(
        "SELECT position, id, recorded_at, workspace_id, stream_kind, stream_id, stream_seq,
                schema_version, actor_kind, actor_id, correlation_id, causation_id, payload
         FROM event ORDER BY position"
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            let kind = StreamKind::parse(&row.stream_kind)
                .ok_or(StoreError::Corrupt("event.stream_kind"))?;
            Ok(Recorded {
                position: Position::from_stored(row.position)
                    .ok()
                    .ok_or(StoreError::Corrupt("event.position"))?,
                id: EventId::from_uuid(row.id),
                recorded_at: Timestamp::from_utc(row.recorded_at),
                workspace: WorkspaceId::from_uuid(row.workspace_id),
                stream: StreamRef {
                    kind,
                    id: row.stream_id,
                },
                stream_seq: row.stream_seq,
                actor: Actor::from_stored(&row.actor_kind, row.actor_id)
                    .ok()
                    .ok_or(StoreError::Corrupt("event.actor"))?,
                correlation: CorrelationId::from_uuid(row.correlation_id),
                causation: row.causation_id.map(EventId::from_uuid),
                event: Event::decode(kind, row.schema_version, row.payload).map_err(|source| {
                    StoreError::CorruptEvent {
                        position: row.position,
                        source,
                    }
                })?,
            })
        })
        .collect()
}

/// Rebuilds every view from the log inside a transaction that is rolled back, and returns the
/// first view table whose rows differ from the live ones, with both versions. `None` means the
/// views are exactly what replaying the log produces.
///
/// The transaction first takes the event log's head lock, as every writer does, so no write
/// commits between the live read and the replay: both see one state.
///
/// # Errors
///
/// Returns [`StoreError`] when the database fails or the log cannot be decoded.
pub async fn views_replay_diff(pool: &PgPool) -> Result<Option<String>, StoreError> {
    views_replay_diff_during(pool, async {}).await
}

/// [`views_replay_diff`], running `during` after the live views are read and before the replay,
/// the moment a concurrent write would corrupt the comparison. Tests use it to put a write there.
///
/// # Errors
///
/// Returns [`StoreError`] when the database fails or the log cannot be decoded.
pub async fn views_replay_diff_during(
    pool: &PgPool,
    during: impl Future<Output = ()>,
) -> Result<Option<String>, StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query_scalar!("SELECT position FROM event_log_head FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    let tables = view_tables(&mut tx).await?;
    let live = dump(&mut tx, &tables).await?;
    during.await;
    let list = tables
        .iter()
        .map(|table| format!("{table:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query(sqlx::AssertSqlSafe(format!("TRUNCATE {list}")))
        .execute(&mut *tx)
        .await?;
    for recorded in log(&mut tx).await? {
        projection::apply(&mut tx, &recorded).await?;
    }
    let replayed = dump(&mut tx, &tables).await?;
    tx.rollback().await?;
    Ok(live
        .into_iter()
        .zip(replayed)
        .find(|(live, replayed)| live != replayed)
        .map(|((table, live), (_, replayed))| {
            format!(
                "view {table} differs from its replay:\n  live:     {live}\n  replayed: {replayed}"
            )
        }))
}

/// Asserts that the views are exactly what replaying the log produces. A write that bypasses the
/// event path, or a projection that reads `now()`, fails here.
///
/// # Panics
///
/// On a mismatch or a database error (test code).
pub async fn assert_views_replay(pool: &PgPool) {
    match views_replay_diff(pool).await {
        Ok(None) => {}
        Ok(Some(difference)) => panic!("{difference}"),
        Err(error) => panic!("could not replay the log: {error}"),
    }
}

/// Issues a setup code on `store`, as `abc server setup-code` does: the one place tests do.
///
/// # Errors
///
/// What [`Store::issue_setup_code`] returns.
#[expect(
    clippy::disallowed_methods,
    reason = "tests play the host operator's CLI, the one caller allowed to mint setup codes"
)]
pub async fn issue_setup_code(
    store: &Store,
) -> Result<abc_core::command::SetupCodeIssued, ExecuteError<Infallible>> {
    store.issue_setup_code().await
}

/// The seed every fixture's store uses.
pub const SEED: u64 = 20_261_031;

/// A store on a test database with a manual clock and seeded mints, and the steps most tests
/// start from.
#[derive(Debug)]
pub struct Fixture {
    /// The store under test.
    pub store: Store,
    /// Its clock.
    pub clock: ManualClock,
    /// The test's pool, for direct inspection.
    pub pool: PgPool,
    /// Generates worker keys, as a worker would, separately from the store's mint.
    keys: SeededMint,
    /// Numbers each request's correlation id.
    requests: AtomicU64,
}

impl Fixture {
    /// A fresh store on `pool`.
    ///
    /// # Panics
    ///
    /// When the store cannot open (test code).
    pub async fn new(pool: PgPool) -> Self {
        let clock = ManualClock::new();
        let store = Store::open(database(pool.clone()), options(&clock, SEED))
            .await
            .unwrap_or_else(|error| panic!("the test store opens: {error}"));
        Self {
            store,
            clock,
            pool,
            keys: SeededMint::new(SEED.wrapping_add(1)),
            requests: AtomicU64::new(0),
        }
    }

    /// A fresh store whose server `alice@example.com` has claimed, with the first workspace.
    ///
    /// # Panics
    ///
    /// When the claim fails (test code).
    pub async fn claimed(pool: PgPool) -> (Self, Claimed) {
        let fixture = Self::new(pool).await;
        let claimed = fixture
            .claim(&Self::alice())
            .await
            .unwrap_or_else(|error| panic!("alice claims the server: {error}"));
        (fixture, claimed)
    }

    /// The tailnet caller who claims the server in [`Fixture::claimed`].
    ///
    /// # Panics
    ///
    /// Never: the login is valid.
    #[must_use]
    pub fn alice() -> Caller {
        Self::tailnet("alice@example.com")
    }

    /// A tailnet caller with `login`.
    ///
    /// # Panics
    ///
    /// When `login` is not a valid login (test code).
    #[must_use]
    pub fn tailnet(login: &str) -> Caller {
        Caller::Tailnet(
            TailnetLogin::parse(login).unwrap_or_else(|error| panic!("{login}: {error}")),
        )
    }

    /// A fresh origin: correlation ids 1, 2, 3… for this fixture's requests.
    pub fn origin(&self) -> Origin {
        let n = self
            .requests
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        Origin {
            correlation: CorrelationId::from_uuid(uuid::Uuid::from_u128(u128::from(n))),
            causation: None,
        }
    }

    /// Issues a setup code and claims the server with it as `caller`.
    ///
    /// # Errors
    ///
    /// The claim's error.
    ///
    /// # Panics
    ///
    /// When no setup code can be issued (test code).
    pub async fn claim(&self, caller: &Caller) -> Result<Claimed, ExecuteError<Infallible>> {
        let issued = issue_setup_code(&self.store)
            .await
            .unwrap_or_else(|error| panic!("a setup code is issued: {error}"));
        self.claim_with(caller, &issued.code).await
    }

    /// Claims the server as `caller` with `code`.
    ///
    /// # Errors
    ///
    /// The claim's error.
    ///
    /// # Panics
    ///
    /// Never: the names are valid.
    pub async fn claim_with(
        &self,
        caller: &Caller,
        code: &Secret<SetupCode>,
    ) -> Result<Claimed, ExecuteError<Infallible>> {
        let command = ClaimServer {
            code: SetupCode::parse(&code.expose_secret().display_form())
                .unwrap_or_else(|error| panic!("{error}")),
            display_name: DisplayName::parse("Alice").unwrap_or_else(|error| panic!("{error}")),
            workspace_name: WorkspaceName::parse("Home").unwrap_or_else(|error| panic!("{error}")),
        };
        self.store.execute(caller, &self.origin(), command).await
    }

    /// Issues an enrolment token in `workspace` as alice, with the default lifetime.
    ///
    /// # Panics
    ///
    /// When issuing fails (test code).
    pub async fn issue_token(&self, workspace: WorkspaceId) -> TokenIssued {
        self.store
            .execute(
                &Self::alice(),
                &self.origin(),
                IssueEnrolmentToken {
                    workspace,
                    label: None,
                    ttl: TokenTtl::default(),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("alice issues a token: {error}"))
    }

    /// A fresh worker key, as `abc worker enrol` generates one.
    #[must_use]
    pub fn worker_key(&self) -> Secret<WorkerKey> {
        WorkerKey::generate(&self.keys)
    }

    /// Redeems `token` with `key` from a machine called `mbp`.
    ///
    /// # Errors
    ///
    /// The enrolment's error.
    ///
    /// # Panics
    ///
    /// Never: the inputs are valid.
    pub async fn enrol(
        &self,
        token: &Secret<EnrolmentToken>,
        key: &Secret<WorkerKey>,
    ) -> Result<Enrolled, ExecuteError<Infallible>> {
        let command = EnrolWorker {
            token: EnrolmentToken::parse(token.expose_secret().as_str())
                .unwrap_or_else(|error| panic!("{error}")),
            key: key.expose_secret().digest(),
            machine: machine(),
            build: Version::new(0, 1, 0),
        };
        self.store
            .execute(&Caller::Anonymous, &self.origin(), command)
            .await
    }

    /// A caller presenting `key`.
    ///
    /// # Panics
    ///
    /// Never: the key is well formed.
    #[must_use]
    pub fn worker(key: &Secret<WorkerKey>) -> Caller {
        Caller::Worker(
            WorkerKey::parse(key.expose_secret().as_str())
                .unwrap_or_else(|error| panic!("{error}")),
        )
    }

    /// How many events the log holds.
    ///
    /// # Panics
    ///
    /// On a database error (test code).
    pub async fn event_count(&self) -> i64 {
        sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM event"#)
            .fetch_one(&self.pool)
            .await
            .unwrap_or_else(|error| panic!("counting events: {error}"))
    }
}

/// The machine the fixture enrols.
///
/// # Panics
///
/// Never: the hostname is valid.
#[must_use]
pub fn machine() -> MachineInfo {
    MachineInfo {
        hostname: DisplayName::parse("mbp").unwrap_or_else(|error| panic!("{error}")),
        os: Os::Macos,
        arch: Arch::Aarch64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposable_databases_carry_their_creation_time() {
        assert_eq!(
            created("abc_test_1700000000_abcdef"),
            Some(1_700_000_000),
            "a test's"
        );
        assert_eq!(
            created("abc_query_cache_1700000000_42"),
            Some(1_700_000_000),
            "xtask's"
        );
        assert_eq!(created("abc_test_soon_x"), None, "no time");
        assert_eq!(created("abc"), None, "someone else's");
    }

    #[test]
    fn a_test_database_is_a_leftover_after_a_day() {
        let now = 1_000_000;
        assert!(!stale(now, now), "a database made now is not");
        assert!(!stale(now - 86_400, now), "nor one exactly a day old");
        assert!(stale(now - 86_401, now), "one a day and a second old is");
        assert!(
            !stale(now + 5, now),
            "a clock that went back is not a reason"
        );
    }

    #[test]
    fn the_manual_clock_starts_at_new_year_and_moves_on_demand() {
        let clock = ManualClock::default();
        let start = clock.now();
        assert_eq!(
            start.as_offset(),
            time::macros::datetime!(2026-01-01 00:00:00 UTC),
            "the fixed start"
        );
        clock.advance(time::Duration::minutes(5));
        assert_eq!(
            clock.now(),
            start.checked_add(time::Duration::minutes(5)).unwrap(),
            "five minutes later"
        );
    }

    #[test]
    fn seeded_mints_repeat_their_sequence() {
        let now = ManualClock::new().now();
        let (first, second) = (SeededMint::new(7), SeededMint::new(7));
        assert_eq!(
            first.uuid(now),
            second.uuid(now),
            "the same seed, the same id"
        );
        assert_eq!(*first.secret(), *second.secret(), "and the same secret");
        assert_ne!(
            *SeededMint::new(7).secret(),
            *SeededMint::new(8).secret(),
            "another seed starts another sequence"
        );
        assert_ne!(expand(7), [0; 32], "a seed expands to real bytes");
        assert_ne!(first.uuid(now), first.uuid(now), "each draw is new");
    }
}
