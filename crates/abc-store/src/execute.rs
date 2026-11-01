//! The write pipeline: one transaction per command, from credential to commit.
//!
//! ```text
//! BEGIN (READ COMMITTED)
//!   SELECT position FROM event_log_head FOR UPDATE   -- first statement; all writers queue here
//!   authenticate(caller)      -> Principal            -- in-txn: revocation is seen immediately
//!   C::load(principal)        -> C::Facts             -- in-txn: the facts the check rests on
//!   abc_core::authorize(..)   -> Authorized<C>        -- the one permission check
//!   C::decide(..)             -> Decision<Outcome>    -- pure
//!   for each event: INSERT event (position = head+i, stream_seq = next) ; projection::apply
//!   UPDATE event_log_head ; SELECT pg_notify('abc_log', head)
//! COMMIT                                               -- the notification is delivered here
//! ```
//!
//! **Why READ COMMITTED.** Each statement takes a fresh snapshot, so everything after the lock sees
//! every transaction that committed before the lock was granted. Under REPEATABLE READ the
//! snapshot would be taken at the first statement, before the lock was granted, so the
//! transaction would read stale facts or fail with a serialization error.
//!
//! **Why time of check = time of use.** The principal, the facts, the decision and the append all
//! run under the head lock, so no other write can commit between the check and the write.
//! Revoking a worker, redeeming a token, claiming: each is decided against state no one else can
//! change until this transaction ends.

use std::future::Future;

use abc_core::actor::Principal;
use abc_core::command::Command;
use abc_core::decision::{DecideCtx, Decision, Origin};
use abc_core::event::Position;
use abc_core::policy::authorize;
use abc_core::time::Timestamp;
use sqlx::{PgConnection, Postgres, Transaction};

use crate::error::{AuthError, ExecuteError, StoreError};
use crate::{Caller, Inner};

mod sealed {
    pub trait Sealed {}
}

/// A command the store knows how to load facts for. Sealed and implemented here for every
/// abc-core command. A command without an impl cannot be passed to `Store::execute`, so adding a
/// command fails the build until its loader exists (per make-the-build-fail).
pub trait Executable: Command + Send + sealed::Sealed
where
    Self::Facts: Send,
{
    /// Loads the facts `authorize` and `decide` need, from the open write transaction.
    /// Read-only SQL, by convention and by test: a write here would make views diverge from the
    /// log, and `assert_views_replay` would fail.
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        principal: &'c Principal,
    ) -> impl Future<Output = Result<Self::Facts, StoreError>> + Send + 'c;
}

// Implementations live in facts.rs, one per command (IssueSetupCode, ClaimServer,
// IssueEnrolmentToken, RevokeEnrolmentToken, EnrolWorker, RevokeWorker, StartSession, EndSession),
// each also implementing `Sealed`.
pub(crate) use sealed::Sealed;

/// The whole pipeline for one command.
pub(crate) async fn run<C: Executable>(
    inner: &Inner,
    caller: &Caller,
    origin: &Origin,
    command: C,
) -> Result<C::Outcome, ExecuteError<C::Rejection>>
where
    C::Facts: Send,
{
    let mut tx = WriteTx::begin(&inner.pool).await?;
    let now = inner.clock.now();
    let principal = authenticate(tx.conn(), caller).await?;
    let facts = command.load(tx.conn(), &principal).await?;
    let authorized = authorize(&principal, command, &facts, now)?;
    let decision = C::decide(authorized, facts, &DecideCtx::new(now, inner.mint.as_ref()))
        .map_err(ExecuteError::Rejected)?;
    let outcome = tx
        .append(decision, origin, now, inner.mint.as_ref())
        .await?;
    tx.commit().await?;
    Ok(outcome)
}

/// Resolves a [`Caller`] into a [`Principal`] on `conn` (inside the caller's transaction).
///
/// * `Tailnet(login)`: `tailnet_identity` -> person, plus every membership, gives `Person`.
///   An unknown login gives `Visitor`.
/// * `Worker(key)`: `worker WHERE key_digest = digest(key)`. Unrevoked gives `Worker`; revoked
///   gives `AuthError::WorkerRevoked`; missing gives `AuthError::UnknownWorkerKey`.
/// * `Anonymous` gives `Anonymous`; `Server` gives `Server`.
///
/// Lookups by SHA-256 digest of a 256-bit secret leak nothing useful through timing, so a unique
/// index is safe here.
pub(crate) async fn authenticate(
    conn: &mut PgConnection,
    caller: &Caller,
) -> Result<Principal, AuthError> {
    let _ = (conn, caller);
    unimplemented!()
}

/// An open write transaction that holds the log-head lock. Crate-private: nothing outside
/// abc-store can begin one, and `append` accepts only a `Decision`.
pub(crate) struct WriteTx {
    tx: Transaction<'static, Postgres>,
    head: Position,
}

impl WriteTx {
    /// `BEGIN ISOLATION LEVEL READ COMMITTED; SELECT position FROM event_log_head FOR UPDATE`.
    pub(crate) async fn begin(pool: &sqlx::PgPool) -> Result<Self, StoreError> {
        let _ = pool;
        unimplemented!()
    }

    /// The connection, for authentication and fact loading.
    pub(crate) fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// Records a decision: for each pending event in order,
    /// 1. `position = head + 1` (checked arithmetic; `StoreError::LogFull` at i64::MAX);
    /// 2. `stream_seq = 1 + max(stream_seq)` for its stream (index-only lookup; the head lock makes
    ///    it race-free, and the UNIQUE constraint is the backstop);
    /// 3. `id = mint.uuid(now)`; actor and workspace from `decision.permit()`; correlation from
    ///    `origin`; causation from `decision.cause()`, else `origin.causation`;
    /// 4. `INSERT INTO event`, then `projection::apply(&mut tx, &recorded)` with the same
    ///    `Recorded` value, so a view never sees an event the log does not hold;
    ///
    /// then `UPDATE event_log_head SET position = head` and `pg_notify('abc_log', head)`. A decision
    /// with zero events touches nothing and sends no notification. Returns the outcome.
    pub(crate) async fn append<O>(
        &mut self,
        decision: Decision<O>,
        origin: &Origin,
        now: Timestamp,
        mint: &dyn abc_core::decision::Mint,
    ) -> Result<O, StoreError> {
        let _ = (decision, origin, now, mint, &self.head);
        unimplemented!()
    }

    /// Commits. The notification goes out with the commit.
    pub(crate) async fn commit(self) -> Result<(), StoreError> {
        self.tx.commit().await.map_err(StoreError::Database)
    }
}

/// Production clock: `OffsetDateTime::now_utc()`, truncated to microseconds.
#[derive(Debug)]
pub(crate) struct SystemClock;

/// Production mint: UUIDv7 from the given instant plus OS entropy (`rand::rngs::OsRng`).
#[derive(Debug)]
pub(crate) struct OsMint;
