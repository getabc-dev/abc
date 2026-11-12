//! The read pipeline: authenticate, then the same `abc_core::policy::authorize`, then fetch, all in
//! one REPEATABLE READ, READ ONLY transaction (one consistent snapshot, no head lock).

use std::future::Future;

use abc_core::actor::{Principal, Role};
use abc_core::command::OpenSession;
use abc_core::command::RequestRecord;
use abc_core::event::{Position, Recorded, StreamKind, StreamRef};
use abc_core::id::{EnrolmentTokenId, FromUuid as _, PersonId, SessionId, WorkerId, WorkspaceId};
use abc_core::identity::{Arch, DisplayName, MachineLabel, Os, WorkspaceName};
use abc_core::policy::{Authorized, authorize};
use abc_core::query::{
    EnrolmentRequestStatus, InstallationState, InstallationStatus, ListEnrolmentTokens,
    ListMachines, MachineCard, MachinesSince, MachinesSnapshot, OpenSessions, Query, RequestCard,
    RequestState, RequestStatus, RunEvents, SessionView, TokenStatus, TokenSummary,
    ViewEnrolmentRequest, WhoAmI, WorkerIdentity, WorkerSelf, WorkspaceSummary,
};
use abc_core::time::Timestamp;
use sqlx::PgConnection;

use crate::error::{QueryError, StoreError, stored};
use crate::execute::authenticate;
use crate::facts::RequestRow;
use crate::{Caller, Inner};

mod sealed {
    pub trait Sealed {}
}

/// A query the store can answer. Sealed; one impl per abc-core query. `fetch` takes the
/// `Authorized<Self>`, so no implementation can run without the check (D10 for reads, I4).
pub trait Fetchable: Query<Answer: Send, Facts: Send> + Send + Sync + sealed::Sealed {
    /// Loads the facts the query's proof is checked against, in its snapshot. Most queries load
    /// none.
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
    ) -> impl Future<Output = Result<Self::Facts, StoreError>> + Send + 'c;

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
    let facts = query.load(&mut tx).await?;
    let auth = authorize(&principal, query, &facts, now)?;
    let answer = Q::fetch(&auth, &principal, &mut tx, now).await?;
    tx.commit().await.map_err(StoreError::from)?;
    Ok(answer)
}

/// `Fetchable::load` for a query that loads no facts.
macro_rules! no_facts {
    () => {
        fn load<'c>(
            &'c self,
            _conn: &'c mut PgConnection,
        ) -> impl Future<Output = Result<(), StoreError>> + Send + 'c {
            std::future::ready(Ok(()))
        }
    };
}

impl sealed::Sealed for WhoAmI {}
impl Fetchable for WhoAmI {
    no_facts!();

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
    now: Timestamp,
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
    // From the origin, the pending requests; after a position, every request changed since,
    // so a decided one leaves the screen. An expired one goes by its `expires_at`.
    let pending_only = after == Position::ORIGIN;
    let request_rows = sqlx::query_as!(
        RequestRow,
        "SELECT id, workspace_id, reserved_worker, key_digest, login, node, hostname, os, arch,
                build, code, requested_at, requested_event, expires_at, decision, changed_at
         FROM enrolment_request
         WHERE workspace_id = $1 AND changed_at > $2
           AND (NOT $3 OR (decision IS NULL AND expires_at > $4))
         ORDER BY requested_at, id",
        workspace.as_uuid(),
        after.get(),
        pending_only,
        now.as_offset()
    )
    .fetch_all(&mut *conn)
    .await?;
    let requests = request_rows
        .iter()
        .map(|row| request_card(row, now))
        .collect::<Result<_, StoreError>>()?;
    Ok(MachinesSnapshot {
        head: stored("event_log_head.position", Position::from_stored(head))?,
        machines,
        requests,
    })
}

impl sealed::Sealed for ListMachines {}
impl Fetchable for ListMachines {
    no_facts!();

    fn fetch<'c>(
        auth: &'c Authorized<Self>,
        _principal: &'c Principal,
        conn: &'c mut PgConnection,
        now: Timestamp,
    ) -> impl Future<Output = Result<MachinesSnapshot, StoreError>> + Send + 'c {
        machines(conn, auth.request().workspace, Position::ORIGIN, now)
    }
}

impl sealed::Sealed for MachinesSince {}
impl Fetchable for MachinesSince {
    no_facts!();

    fn fetch<'c>(
        auth: &'c Authorized<Self>,
        _principal: &'c Principal,
        conn: &'c mut PgConnection,
        now: Timestamp,
    ) -> impl Future<Output = Result<MachinesSnapshot, StoreError>> + Send + 'c {
        machines(conn, auth.request().workspace, auth.request().after, now)
    }
}

impl sealed::Sealed for ListEnrolmentTokens {}
impl Fetchable for ListEnrolmentTokens {
    no_facts!();

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
    no_facts!();

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
    no_facts!();

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
    no_facts!();

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

impl sealed::Sealed for EnrolmentRequestStatus {}
impl Fetchable for EnrolmentRequestStatus {
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
    ) -> impl Future<Output = Result<Option<RequestRecord>, StoreError>> + Send + 'c {
        crate::facts::status_facts(self, conn)
    }

    /// Where the proven request stands now.
    fn fetch<'c>(
        auth: &'c Authorized<Self>,
        _principal: &'c Principal,
        _conn: &'c mut PgConnection,
        now: Timestamp,
    ) -> impl Future<Output = Result<RequestState, StoreError>> + Send + 'c {
        std::future::ready(Ok(auth.proof().state(now)))
    }
}

/// A request's card from its row.
fn request_card(row: &RequestRow, now: Timestamp) -> Result<RequestCard, StoreError> {
    let record = RequestRecord::try_from(row)?;
    Ok(RequestCard {
        id: record.id,
        status: RequestStatus::of(&record, now),
        hostname: record.machine.hostname,
        os: record.machine.os,
        arch: record.machine.arch,
        build: record.build.to_string(),
        login: record.requester.login,
        node: record.requester.node,
        code: record.code,
        requested_at: Timestamp::from_utc(row.requested_at),
        expires_at: record.expires_at,
        changed_at: stored(
            "enrolment_request.changed_at",
            Position::from_stored(row.changed_at),
        )?,
    })
}

impl sealed::Sealed for ViewEnrolmentRequest {}
impl Fetchable for ViewEnrolmentRequest {
    no_facts!();

    async fn fetch(
        auth: &Authorized<Self>,
        _principal: &Principal,
        conn: &mut PgConnection,
        now: Timestamp,
    ) -> Result<Option<RequestCard>, StoreError> {
        let row = sqlx::query_as!(
            RequestRow,
            "SELECT id, workspace_id, reserved_worker, key_digest, login, node, hostname, os, arch,
                    build, code, requested_at, requested_event, expires_at, decision, changed_at
             FROM enrolment_request WHERE id = $1 AND workspace_id = $2",
            auth.request().request.as_uuid(),
            auth.request().workspace.as_uuid()
        )
        .fetch_optional(&mut *conn)
        .await?;
        row.as_ref().map(|row| request_card(row, now)).transpose()
    }
}

impl sealed::Sealed for RunEvents {}
impl Fetchable for RunEvents {
    no_facts!();

    async fn fetch(
        auth: &Authorized<Self>,
        _principal: &Principal,
        conn: &mut PgConnection,
        _now: Timestamp,
    ) -> Result<Vec<Recorded>, StoreError> {
        let run = auth.request().run;
        crate::log::stream(
            conn,
            StreamRef {
                kind: StreamKind::Run,
                id: run.as_uuid(),
            },
        )
        .await
    }
}
