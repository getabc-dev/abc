//! The write pipeline: one transaction per command, from credential to commit.
//!
//! ```text
//! BEGIN ISOLATION LEVEL READ COMMITTED
//!   SELECT position FROM event_log_head FOR UPDATE   -- first statement; all writers queue here
//!   authenticate(caller)      -> Principal            -- in-txn: revocation is seen immediately
//!   C::load(principal)        -> C::Facts             -- in-txn: the facts the check rests on
//!   abc_core::authorize(..)   -> Authorized<C>        -- the one permission check
//!   C::decide(..)             -> Decision<Outcome>    -- pure
//!   for each event: INSERT event (position = head+i, stream_seq = next) ; projection::apply
//!   UPDATE event_log_head ; SELECT pg_notify('abc_log', {head, workspace, streams})
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

use abc_core::actor::{Membership, Principal, Role};
use abc_core::decision::{DecideCtx, Decision, Mint, Origin};
use abc_core::event::{Position, Recorded};
use abc_core::id::{EventId, FromUuid as _, PersonId, WorkerId, WorkspaceId};
use abc_core::policy::Denied;
use abc_core::policy::authorize;
use abc_core::time::{Clock, Timestamp};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::entropy::ChaChaMint;
use crate::error::{AuthError, ExecuteError, StoreError};
use crate::{Caller, Inner, projection};

mod sealed {
    use std::future::Future;

    use abc_core::actor::Principal;
    use abc_core::command::Command;
    use sqlx::PgConnection;

    use crate::error::StoreError;

    /// A command the store knows how to load facts for, implemented in `facts.rs` for every
    /// abc-core command. Public only inside this private module: no other crate can name it.
    pub trait Load: Command<Facts: Send, Outcome: Send> + Send + Sync {
        /// Loads the facts `authorize` and `decide` need, from the open write transaction.
        /// Read-only SQL: a write here would make views diverge from the log, which
        /// `assert_views_replay` catches.
        fn load<'c>(
            &'c self,
            conn: &'c mut PgConnection,
            principal: &'c Principal,
        ) -> impl Future<Output = Result<Self::Facts, StoreError>> + Send + 'c;
    }
}

/// A command any caller of `Store::execute` may run: every abc-core command except
/// `IssueSetupCode`, which only `Store::issue_setup_code` runs. A command without a loader cannot
/// be passed to `execute`, so adding a command fails the build until its loader exists.
pub trait Executable: Load {}

pub(crate) use sealed::Load;

/// The whole pipeline for one command.
pub(crate) async fn run<C: Load>(
    inner: &Inner,
    caller: &Caller,
    origin: &Origin,
    command: C,
) -> Result<C::Outcome, ExecuteError<C::Rejection>> {
    let mint = ChaChaMint::from_seed(inner.entropy.seed().map_err(StoreError::from)?);
    if matches!(caller, Caller::Anonymous) {
        admit(inner, &command).await?;
    }
    let mut tx = WriteTx::begin(&inner.pool).await?;
    let now = inner.clock.now();
    let principal = authenticate(tx.conn(), caller).await??;
    let facts = command.load(tx.conn(), &principal).await?;
    let authorized = authorize(&principal, command, &facts, now)?;
    let decision = C::decide(authorized, facts, &DecideCtx::new(now, &mint))
        .map_err(ExecuteError::Rejected)?;
    let events = decision.events().len();
    let outcome = tx.append(decision, origin, now, &mint).await?;
    tx.commit().await?;
    tracing::debug!(action = ?C::ACTION, events, "command recorded");
    Ok(outcome)
}

/// Refuses an anonymous command whose own capability already fails, before it can queue for the
/// log head: an unknown enrolment token costs a read, never the global write lock. The command is
/// checked again, in full, under the lock, so this only ever refuses early.
async fn admit<C: Load>(inner: &Inner, command: &C) -> Result<(), ExecuteError<C::Rejection>> {
    let mut conn = inner.pool.acquire().await.map_err(StoreError::from)?;
    let facts = command.load(&mut conn, &Principal::Anonymous).await?;
    command
        .prove(&facts, inner.clock.now())
        .map(drop)
        .map_err(|reason| {
            ExecuteError::Denied(Denied {
                action: C::ACTION.into(),
                reason,
            })
        })
}

/// Resolves a [`Caller`] into a [`Principal`] on `conn`, inside the caller's transaction.
///
/// * `Tailnet(login)`: `tailnet_identity` gives the person, plus every membership. An unknown
///   login gives `Visitor`.
/// * `Worker(key)`: the worker with that key's digest. Unrevoked gives `Worker`; revoked gives
///   `AuthError::WorkerRevoked`; missing gives `AuthError::UnknownWorkerKey`.
/// * `Anonymous` gives `Anonymous`; `Server` gives `Server`.
///
/// Lookups by the SHA-256 digest of a 256-bit secret leak nothing useful through timing, so a
/// unique index is safe here.
pub(crate) async fn authenticate(
    conn: &mut PgConnection,
    caller: &Caller,
) -> Result<Result<Principal, AuthError>, StoreError> {
    match caller {
        Caller::Tailnet(login) => {
            let person = sqlx::query_scalar!(
                "SELECT person_id FROM tailnet_identity WHERE login = $1",
                login.as_str()
            )
            .fetch_optional(&mut *conn)
            .await?;
            let Some(person) = person else {
                return Ok(Ok(Principal::Visitor {
                    login: login.clone(),
                }));
            };
            let rows = sqlx::query!(
                "SELECT workspace_id, role FROM membership WHERE person_id = $1",
                person
            )
            .fetch_all(&mut *conn)
            .await?;
            let mut memberships = rows
                .into_iter()
                .map(|row| {
                    Ok(Membership {
                        workspace: WorkspaceId::from_uuid(row.workspace_id),
                        role: Role::parse(&row.role)
                            .ok()
                            .ok_or(StoreError::Corrupt("membership.role"))?,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            memberships.sort_by_key(|membership| membership.workspace);
            Ok(Ok(Principal::Person {
                id: PersonId::from_uuid(person),
                memberships,
            }))
        }
        Caller::Worker(key) => {
            let digest = key.expose_secret().digest();
            let row = sqlx::query!(
                r#"SELECT id, workspace_id, revoked_at IS NOT NULL AS "revoked!"
                   FROM worker WHERE key_digest = $1"#,
                digest.as_bytes().as_slice()
            )
            .fetch_optional(&mut *conn)
            .await?;
            Ok(match row {
                None => Err(AuthError::UnknownWorkerKey),
                Some(row) if row.revoked => Err(AuthError::WorkerRevoked),
                Some(row) => Ok(Principal::Worker {
                    id: WorkerId::from_uuid(row.id),
                    workspace: WorkspaceId::from_uuid(row.workspace_id),
                }),
            })
        }
        Caller::Anonymous => Ok(Ok(Principal::Anonymous)),
        Caller::Server => Ok(Ok(Principal::Server)),
    }
}

/// An open write transaction that holds the log-head lock. Crate-private: nothing outside
/// abc-store can begin one, and `append` accepts only a `Decision`.
pub(crate) struct WriteTx {
    tx: Transaction<'static, Postgres>,
    head: Position,
}

impl WriteTx {
    /// Begins a READ COMMITTED transaction and locks the log head as its first statement.
    pub(crate) async fn begin(pool: &PgPool) -> Result<Self, StoreError> {
        let mut tx = pool
            .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await?;
        let head = sqlx::query_scalar!("SELECT position FROM event_log_head FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
        let head = Position::from_stored(head)
            .ok()
            .ok_or(StoreError::Corrupt("event_log_head.position"))?;
        Ok(Self { tx, head })
    }

    /// The connection, for authentication and fact loading.
    pub(crate) fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// Records a decision. For each event in order: the next position, the next sequence number
    /// in its stream, a fresh id, the permit's actor and workspace, the origin's correlation and
    /// the decision's cause (else the origin's); then the insert, and the projection of that very
    /// `Recorded` value, so a view never sees an event the log does not hold. Finally the head
    /// moves and the notification is queued for commit. A decision with no events touches nothing
    /// and notifies no one.
    pub(crate) async fn append<O>(
        &mut self,
        decision: Decision<O>,
        origin: &Origin,
        now: Timestamp,
        mint: &dyn Mint,
    ) -> Result<O, StoreError> {
        let permit = decision.permit();
        let causation = decision.cause().or(origin.causation);
        for pending in decision.events() {
            let position = self.head.next().ok_or(StoreError::LogFull)?;
            let stream = pending.stream();
            let stream_seq = sqlx::query_scalar!(
                r#"SELECT COALESCE(MAX(stream_seq), 0) + 1 AS "next!"
                   FROM event WHERE stream_kind = $1 AND stream_id = $2"#,
                stream.kind.as_str(),
                stream.id
            )
            .fetch_one(&mut *self.tx)
            .await?;
            let recorded = Recorded {
                position,
                id: EventId::from_uuid(mint.uuid(now)),
                recorded_at: now,
                workspace: permit.workspace(),
                stream,
                stream_seq,
                actor: permit.actor(),
                correlation: origin.correlation,
                causation,
                event: pending.event().clone(),
            };
            let payload = recorded.event.encode().map_err(StoreError::Encode)?;
            sqlx::query!(
                "INSERT INTO event (position, id, recorded_at, workspace_id, stream_kind, stream_id,
                    stream_seq, schema_version, actor_kind, actor_id, correlation_id, causation_id,
                    payload)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                position.get(),
                recorded.id.as_uuid(),
                now.as_offset(),
                recorded.workspace.as_uuid(),
                stream.kind.as_str(),
                stream.id,
                stream_seq,
                recorded.event.schema_version().0,
                recorded.actor.kind(),
                recorded.actor.id(),
                recorded.correlation.as_uuid(),
                recorded.causation.map(EventId::as_uuid),
                payload,
            )
            .execute(&mut *self.tx)
            .await?;
            projection::apply(&mut self.tx, &recorded).await?;
            self.head = position;
        }
        if !decision.events().is_empty() {
            sqlx::query!("UPDATE event_log_head SET position = $1", self.head.get())
                .execute(&mut *self.tx)
                .await?;
            let streams: Vec<_> = decision
                .events()
                .iter()
                .map(abc_core::decision::Pending::stream)
                .collect();
            let payload = crate::feed::payload(self.head, permit.workspace().as_uuid(), &streams)?;
            sqlx::query!("SELECT pg_notify('abc_log', $1)", payload)
                .execute(&mut *self.tx)
                .await?;
        }
        Ok(decision.into_outcome())
    }

    /// Commits. The notification goes out with the commit.
    pub(crate) async fn commit(self) -> Result<(), StoreError> {
        Ok(self.tx.commit().await?)
    }
}

/// Production clock: the system's UTC time, truncated to microseconds.
#[derive(Debug)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_utc(time::OffsetDateTime::now_utc())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_clock_reads_the_present() {
        let before = time::OffsetDateTime::now_utc();
        let now = SystemClock.now().as_offset();
        assert!(
            now >= before.replace_nanosecond(0).unwrap(),
            "not in the past"
        );
        assert!(now <= time::OffsetDateTime::now_utc(), "not in the future");
    }
}
