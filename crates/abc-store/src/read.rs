//! The read pipeline: authenticate, then the same `abc_core::policy::authorize`, then fetch, all in
//! one REPEATABLE READ, READ ONLY transaction (one consistent snapshot, no head lock).

use std::future::Future;

use abc_core::actor::{Principal, Role};
use abc_core::command::OpenSession;
use abc_core::event::Position;
use abc_core::id::{EnrolmentTokenId, FromUuid as _, PersonId, SessionId, WorkerId, WorkspaceId};
use abc_core::identity::{Arch, DisplayName, MachineLabel, Os, WorkspaceName};
use abc_core::policy::{Authorized, authorize};
use abc_core::query::{
    InstallationState, InstallationStatus, ListEnrolmentTokens, ListMachines, MachineCard,
    MachinesSince, MachinesSnapshot, OpenSessions, Query, SessionView, TokenStatus, TokenSummary,
    WhoAmI, WorkerIdentity, WorkerSelf, WorkspaceSummary,
};
use abc_core::time::Timestamp;
use sqlx::PgConnection;

use crate::error::{QueryError, StoreError};
use crate::execute::authenticate;
use crate::{Caller, Inner};

mod sealed {
    pub trait Sealed {}
}

/// A query the store can answer. Sealed; one impl per abc-core query. `fetch` takes the
/// `Authorized<Self>`, so no implementation can run without the check (D10 for reads, I4).
pub trait Fetchable: Query<Answer: Send> + Send + Sync + sealed::Sealed {
    /// Reads the answer.
    fn fetch<'c>(
        auth: &'c Authorized<Self>,
        principal: &'c Principal,
        conn: &'c mut PgConnection,
        now: Timestamp,
    ) -> impl Future<Output = Result<Self::Answer, StoreError>> + Send + 'c;
}

/// The whole pipeline for one query.
pub(crate) async fn run<Q: Fetchable>(
    inner: &Inner,
    caller: &Caller,
    query: Q,
) -> Result<Q::Answer, QueryError> {
    let mut tx = inner
        .pool
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(StoreError::from)?;
    let now = inner.clock.now();
    let principal = authenticate(&mut tx, caller).await??;
    let auth = authorize(&principal, query, &(), now)?;
    let answer = Q::fetch(&auth, &principal, &mut tx, now).await?;
    tx.commit().await.map_err(StoreError::from)?;
    Ok(answer)
}

/// Reads a stored value with `parse`, or reports which column is corrupt.
fn stored<T, E>(column: &'static str, parsed: Result<T, E>) -> Result<T, StoreError> {
    parsed.ok().ok_or(StoreError::Corrupt(column))
}

impl sealed::Sealed for WhoAmI {}
impl Fetchable for WhoAmI {
    async fn fetch(
        _auth: &Authorized<Self>,
        principal: &Principal,
        conn: &mut PgConnection,
        _now: Timestamp,
    ) -> Result<SessionView, StoreError> {
        match principal {
            Principal::Anonymous | Principal::Worker { .. } | Principal::Server => {
                Ok(SessionView::NoIdentity)
            }
            Principal::Visitor { login } => {
                let claimed = sqlx::query_scalar!(
                    r#"SELECT EXISTS (SELECT 1 FROM installation) AS "claimed!""#
                )
                .fetch_one(&mut *conn)
                .await?;
                let login = login.clone();
                Ok(if claimed {
                    SessionView::NotAMember { login }
                } else {
                    SessionView::Unclaimed { login }
                })
            }
            Principal::Person { id, .. } => member(conn, *id).await,
        }
    }
}

/// A member's session view: their name and every workspace they belong to.
async fn member(conn: &mut PgConnection, person: PersonId) -> Result<SessionView, StoreError> {
    let name = sqlx::query_scalar!(
        "SELECT display_name FROM actor WHERE id = $1",
        person.as_uuid()
    )
    .fetch_one(&mut *conn)
    .await?;
    let rows = sqlx::query!(
        "SELECT w.id, w.name, m.role FROM membership m JOIN workspace w ON w.id = m.workspace_id
         WHERE m.person_id = $1 ORDER BY w.created_at, w.id",
        person.as_uuid()
    )
    .fetch_all(&mut *conn)
    .await?;
    let workspaces = rows
        .into_iter()
        .map(|row| {
            Ok(WorkspaceSummary {
                id: WorkspaceId::from_uuid(row.id),
                name: stored("workspace.name", WorkspaceName::parse(&row.name))?,
                role: stored("membership.role", Role::parse(&row.role))?,
            })
        })
        .collect::<Result<_, StoreError>>()?;
    Ok(SessionView::Member {
        person,
        display_name: stored("actor.display_name", DisplayName::parse(&name))?,
        workspaces,
    })
}

/// The workspace's machine cards changed after `after`, with the head they reflect. Run inside
/// the query's snapshot, so the head is exact.
async fn machines(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    after: Position,
) -> Result<MachinesSnapshot, StoreError> {
    let head = sqlx::query_scalar!("SELECT position FROM event_log_head")
        .fetch_one(&mut *conn)
        .await?;
    let rows = sqlx::query!(
        r#"SELECT w.id, a.display_name, w.hostname, w.os, w.arch, w.build,
                  w.open_session IS NOT NULL AS "online!", w.enrolled_at,
                  GREATEST(w.last_session_started_at, w.last_session_ended_at) AS last_seen_at,
                  w.revoked_at IS NOT NULL AS "revoked!", w.changed_at
           FROM worker w JOIN actor a ON a.id = w.id
           WHERE w.workspace_id = $1 AND w.changed_at > $2
           ORDER BY w.enrolled_at, w.id"#,
        workspace.as_uuid(),
        after.get()
    )
    .fetch_all(&mut *conn)
    .await?;
    let machines = rows
        .into_iter()
        .map(|row| {
            Ok(MachineCard {
                worker: WorkerId::from_uuid(row.id),
                name: stored("actor.display_name", DisplayName::parse(&row.display_name))?,
                hostname: stored("worker.hostname", DisplayName::parse(&row.hostname))?,
                os: stored("worker.os", Os::parse(&row.os))?,
                arch: stored("worker.arch", Arch::parse(&row.arch))?,
                build: row.build,
                online: row.online,
                enrolled_at: Timestamp::from_utc(row.enrolled_at),
                last_seen_at: row.last_seen_at.map(Timestamp::from_utc),
                revoked: row.revoked,
                changed_at: stored("worker.changed_at", Position::from_stored(row.changed_at))?,
            })
        })
        .collect::<Result<_, StoreError>>()?;
    Ok(MachinesSnapshot {
        head: stored("event_log_head.position", Position::from_stored(head))?,
        machines,
    })
}

impl sealed::Sealed for ListMachines {}
impl Fetchable for ListMachines {
    fn fetch<'c>(
        auth: &'c Authorized<Self>,
        _principal: &'c Principal,
        conn: &'c mut PgConnection,
        _now: Timestamp,
    ) -> impl Future<Output = Result<MachinesSnapshot, StoreError>> + Send + 'c {
        machines(conn, auth.request().workspace, Position::ORIGIN)
    }
}

impl sealed::Sealed for MachinesSince {}
impl Fetchable for MachinesSince {
    fn fetch<'c>(
        auth: &'c Authorized<Self>,
        _principal: &'c Principal,
        conn: &'c mut PgConnection,
        _now: Timestamp,
    ) -> impl Future<Output = Result<MachinesSnapshot, StoreError>> + Send + 'c {
        machines(conn, auth.request().workspace, auth.request().after)
    }
}

impl sealed::Sealed for ListEnrolmentTokens {}
impl Fetchable for ListEnrolmentTokens {
    async fn fetch(
        auth: &Authorized<Self>,
        _principal: &Principal,
        conn: &mut PgConnection,
        now: Timestamp,
    ) -> Result<Vec<TokenSummary>, StoreError> {
        let rows = sqlx::query!(
            r#"SELECT id, label, issued_at, expires_at, revoked_at IS NOT NULL AS "revoked!",
                      CASE WHEN redeemed_at IS NULL THEN NULL ELSE reserved_worker END
                          AS redeemed_by
               FROM enrolment_token WHERE workspace_id = $1
               ORDER BY issued_at DESC, id"#,
            auth.request().workspace.as_uuid()
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|row| {
                let expires_at = Timestamp::from_utc(row.expires_at);
                Ok(TokenSummary {
                    id: EnrolmentTokenId::from_uuid(row.id),
                    label: stored(
                        "enrolment_token.label",
                        row.label
                            .map(|label| MachineLabel::parse(&label))
                            .transpose(),
                    )?,
                    issued_at: Timestamp::from_utc(row.issued_at),
                    expires_at,
                    status: TokenStatus::at(
                        row.revoked,
                        row.redeemed_by.map(WorkerId::from_uuid),
                        expires_at,
                        now,
                    ),
                })
            })
            .collect()
    }
}

impl sealed::Sealed for WorkerSelf {}
impl Fetchable for WorkerSelf {
    /// The worker the store authenticated. Running the query is the point: authentication fails
    /// with `WorkerRevoked` once the worker is revoked.
    fn fetch<'c>(
        _auth: &'c Authorized<Self>,
        principal: &'c Principal,
        _conn: &'c mut PgConnection,
        _now: Timestamp,
    ) -> impl Future<Output = Result<WorkerIdentity, StoreError>> + Send + 'c {
        std::future::ready(match principal {
            Principal::Worker { id, workspace } => Ok(WorkerIdentity {
                worker: *id,
                workspace: *workspace,
            }),
            // The matrix grants this read to workers only.
            Principal::Person { .. }
            | Principal::Visitor { .. }
            | Principal::Anonymous
            | Principal::Server => Err(StoreError::Corrupt("a non-worker passed ViewSelfAsWorker")),
        })
    }
}

impl sealed::Sealed for OpenSessions {}
impl Fetchable for OpenSessions {
    async fn fetch(
        _auth: &Authorized<Self>,
        _principal: &Principal,
        conn: &mut PgConnection,
        _now: Timestamp,
    ) -> Result<Vec<OpenSession>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, workspace_id, open_session FROM worker
             WHERE open_session IS NOT NULL ORDER BY id"
        )
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| OpenSession {
                worker: WorkerId::from_uuid(row.id),
                workspace: WorkspaceId::from_uuid(row.workspace_id),
                session: row.open_session.map(SessionId::from_uuid),
            })
            .collect())
    }
}

impl sealed::Sealed for InstallationStatus {}
impl Fetchable for InstallationStatus {
    async fn fetch(
        _auth: &Authorized<Self>,
        _principal: &Principal,
        conn: &mut PgConnection,
        _now: Timestamp,
    ) -> Result<InstallationState, StoreError> {
        let claimed =
            sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM installation) AS "claimed!""#)
                .fetch_one(&mut *conn)
                .await?;
        Ok(if claimed {
            InstallationState::Claimed
        } else {
            InstallationState::Unclaimed
        })
    }
}
