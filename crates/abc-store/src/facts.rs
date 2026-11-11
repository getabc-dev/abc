//! Fact loaders: one `Executable` impl per command. Each is read-only SQL that turns the open
//! write transaction's state into the plain records abc-core decides on. No rules live here: an
//! expiry comparison or a digest match belongs in the command's `prove` or `decide`.

use std::future::Future;

use abc_core::actor::Principal;
use abc_core::command::{
    ApproveEnrolment, ApproveFacts, ClaimServer, DenyEnrolment, EndSession, EnrolFacts,
    EnrolWorker, InstallationFacts, IssueEnrolmentToken, IssueSetupCode, OpenSession,
    RequestDecision, RequestEnrolment, RequestFacts, RequestRecord, RevokeEnrolmentToken,
    RevokeWorker, SelfFacts, SetupCodeRecord, StartSession, TokenRecord, WorkerRecord,
};
use abc_core::credential::{ApprovalCode, Digest, WorkerKeyDigest};
use abc_core::id::{
    EnrolmentRequestId, EnrolmentTokenId, EventId, FromUuid as _, PersonId, SessionId, WorkerId,
    WorkspaceId,
};
use abc_core::identity::{
    Arch, DisplayName, MachineInfo, MachineLabel, Os, Requester, TailnetLogin, TailnetNode,
};
use abc_core::query::EnrolmentRequestStatus;
use abc_core::time::Timestamp;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::StoreError;
use crate::execute::{Executable, Load};

/// Shared by the two installation commands: whether the server is claimed, the newest setup code,
/// and the claimant's login from a `Visitor` principal.
async fn installation_facts(
    conn: &mut PgConnection,
    principal: &Principal,
) -> Result<InstallationFacts, StoreError> {
    let claimed =
        sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM installation) AS "claimed!""#)
            .fetch_one(&mut *conn)
            .await?;
    let row = sqlx::query!("SELECT digest, owner_id, workspace_id, expires_at FROM setup_code")
        .fetch_optional(&mut *conn)
        .await?;
    let setup_code = row
        .map(|row| {
            Ok::<_, StoreError>(SetupCodeRecord {
                digest: Digest::from_slice(&row.digest)
                    .ok()
                    .ok_or(StoreError::Corrupt("setup_code.digest"))?,
                owner: PersonId::from_uuid(row.owner_id),
                workspace: WorkspaceId::from_uuid(row.workspace_id),
                expires_at: Timestamp::from_utc(row.expires_at),
            })
        })
        .transpose()?;
    let login = match principal {
        Principal::Visitor { login } => Some(login.clone()),
        Principal::Person { .. }
        | Principal::Worker { .. }
        | Principal::Anonymous
        | Principal::Server => None,
    };
    Ok(InstallationFacts {
        claimed,
        setup_code,
        login,
    })
}

impl Load for IssueSetupCode {
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        principal: &'c Principal,
    ) -> impl Future<Output = Result<InstallationFacts, StoreError>> + Send + 'c {
        installation_facts(conn, principal)
    }
}

impl Executable for ClaimServer {}
impl Load for ClaimServer {
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        principal: &'c Principal,
    ) -> impl Future<Output = Result<InstallationFacts, StoreError>> + Send + 'c {
        installation_facts(conn, principal)
    }
}

/// One `enrolment_token` row as abc-core sees it.
struct TokenRow {
    id: Uuid,
    workspace_id: Uuid,
    reserved_worker: Uuid,
    label: Option<String>,
    expires_at: time::OffsetDateTime,
    revoked: bool,
    redeemed_key: Option<Vec<u8>>,
    issued_event: Uuid,
}

impl TryFrom<TokenRow> for TokenRecord {
    type Error = StoreError;

    fn try_from(row: TokenRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: EnrolmentTokenId::from_uuid(row.id),
            workspace: WorkspaceId::from_uuid(row.workspace_id),
            worker: WorkerId::from_uuid(row.reserved_worker),
            label: row
                .label
                .map(|label| MachineLabel::parse(&label))
                .transpose()
                .ok()
                .ok_or(StoreError::Corrupt("enrolment_token.label"))?,
            expires_at: Timestamp::from_utc(row.expires_at),
            revoked: row.revoked,
            redeemed_by: row
                .redeemed_key
                .map(|key| Digest::from_slice(&key))
                .transpose()
                .ok()
                .ok_or(StoreError::Corrupt("enrolment_token.redeemed_key"))?,
            issued_event: EventId::from_uuid(row.issued_event),
        })
    }
}

impl Executable for EnrolWorker {}
impl Load for EnrolWorker {
    /// The token whose digest is the presented token's.
    async fn load(
        &self,
        conn: &mut PgConnection,
        _principal: &Principal,
    ) -> Result<EnrolFacts, StoreError> {
        let digest = self.token.expose_secret().digest();
        let row = sqlx::query_as!(
            TokenRow,
            r#"SELECT id, workspace_id, reserved_worker, label, expires_at,
                      revoked_at IS NOT NULL AS "revoked!", redeemed_key, issued_event
               FROM enrolment_token WHERE digest = $1"#,
            digest.as_bytes().as_slice()
        )
        .fetch_optional(&mut *conn)
        .await?;
        let token = row.map(TokenRecord::try_from).transpose()?;
        let reserved = token.as_ref().map(|token| token.worker.as_uuid());
        let key_in_use = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM worker WHERE key_digest = $1
                              AND id IS DISTINCT FROM $2) AS "in_use!""#,
            self.key.as_bytes().as_slice(),
            reserved
        )
        .fetch_one(&mut *conn)
        .await?;
        let reserved_worker_revoked = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM worker WHERE id = $1 AND revoked_at IS NOT NULL)
               AS "revoked!""#,
            reserved
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(EnrolFacts {
            token,
            key_in_use,
            reserved_worker_revoked,
        })
    }
}

impl Executable for IssueEnrolmentToken {}
impl Load for IssueEnrolmentToken {
    /// No facts: the policy's membership check, from the principal, is all it needs.
    fn load<'c>(
        &'c self,
        _conn: &'c mut PgConnection,
        _principal: &'c Principal,
    ) -> impl Future<Output = Result<(), StoreError>> + Send + 'c {
        std::future::ready(Ok(()))
    }
}

impl Executable for RevokeEnrolmentToken {}
impl Load for RevokeEnrolmentToken {
    /// The token with this id.
    async fn load(
        &self,
        conn: &mut PgConnection,
        _principal: &Principal,
    ) -> Result<Option<TokenRecord>, StoreError> {
        let row = sqlx::query_as!(
            TokenRow,
            r#"SELECT id, workspace_id, reserved_worker, label, expires_at,
                      revoked_at IS NOT NULL AS "revoked!", redeemed_key, issued_event
               FROM enrolment_token WHERE id = $1"#,
            self.token.as_uuid()
        )
        .fetch_optional(&mut *conn)
        .await?;
        row.map(TokenRecord::try_from).transpose()
    }
}

impl Executable for RevokeWorker {}
impl Load for RevokeWorker {
    /// The worker with this id.
    async fn load(
        &self,
        conn: &mut PgConnection,
        _principal: &Principal,
    ) -> Result<Option<WorkerRecord>, StoreError> {
        let row = sqlx::query!(
            r#"SELECT id, workspace_id, revoked_at IS NOT NULL AS "revoked!"
               FROM worker WHERE id = $1"#,
            self.worker.as_uuid()
        )
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(|row| WorkerRecord {
            id: WorkerId::from_uuid(row.id),
            workspace: WorkspaceId::from_uuid(row.workspace_id),
            revoked: row.revoked,
        }))
    }
}

impl Executable for StartSession {}
impl Load for StartSession {
    /// The authenticated worker, copied from the principal.
    fn load<'c>(
        &'c self,
        _conn: &'c mut PgConnection,
        principal: &'c Principal,
    ) -> impl Future<Output = Result<SelfFacts, StoreError>> + Send + 'c {
        let worker = match principal {
            Principal::Worker { id, .. } => Some(*id),
            Principal::Person { .. }
            | Principal::Visitor { .. }
            | Principal::Anonymous
            | Principal::Server => None,
        };
        std::future::ready(Ok(SelfFacts { worker }))
    }
}

impl Executable for EndSession {}
impl Load for EndSession {
    /// The worker and its open session, if any.
    async fn load(
        &self,
        conn: &mut PgConnection,
        _principal: &Principal,
    ) -> Result<Option<OpenSession>, StoreError> {
        let row = sqlx::query!(
            "SELECT id, workspace_id, open_session FROM worker WHERE id = $1",
            self.worker.as_uuid()
        )
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(|row| OpenSession {
            worker: WorkerId::from_uuid(row.id),
            workspace: WorkspaceId::from_uuid(row.workspace_id),
            session: row.open_session.map(SessionId::from_uuid),
        }))
    }
}

/// One `enrolment_request` row as abc-core sees it.
pub(crate) struct RequestRow {
    pub(crate) id: Uuid,
    pub(crate) workspace_id: Uuid,
    pub(crate) reserved_worker: Uuid,
    pub(crate) key_digest: Vec<u8>,
    pub(crate) login: String,
    pub(crate) node: String,
    pub(crate) hostname: String,
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) build: String,
    pub(crate) code: String,
    pub(crate) requested_at: time::OffsetDateTime,
    pub(crate) requested_event: Uuid,
    pub(crate) expires_at: time::OffsetDateTime,
    pub(crate) decision: Option<String>,
    pub(crate) changed_at: i64,
}

/// Reads a stored value with `parse`, or reports which column is corrupt.
fn stored<T, E>(column: &'static str, parsed: Result<T, E>) -> Result<T, StoreError> {
    parsed.ok().ok_or(StoreError::Corrupt(column))
}

impl TryFrom<&RequestRow> for RequestRecord {
    type Error = StoreError;

    fn try_from(row: &RequestRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: EnrolmentRequestId::from_uuid(row.id),
            workspace: WorkspaceId::from_uuid(row.workspace_id),
            worker: WorkerId::from_uuid(row.reserved_worker),
            key: stored(
                "enrolment_request.key_digest",
                Digest::from_slice(&row.key_digest),
            )?,
            requester: Requester {
                login: stored("enrolment_request.login", TailnetLogin::parse(&row.login))?,
                node: stored("enrolment_request.node", TailnetNode::parse(&row.node))?,
            },
            machine: MachineInfo {
                hostname: stored(
                    "enrolment_request.hostname",
                    DisplayName::parse(&row.hostname),
                )?,
                os: stored("enrolment_request.os", Os::parse(&row.os))?,
                arch: stored("enrolment_request.arch", Arch::parse(&row.arch))?,
            },
            build: stored(
                "enrolment_request.build",
                semver::Version::parse(&row.build),
            )?,
            code: stored("enrolment_request.code", ApprovalCode::parse(&row.code))?,
            expires_at: Timestamp::from_utc(row.expires_at),
            decision: match row.decision.as_deref() {
                None => None,
                Some("approved") => Some(RequestDecision::Approved),
                Some("denied") => Some(RequestDecision::Denied),
                Some(_) => return Err(StoreError::Corrupt("enrolment_request.decision")),
            },
            requested_event: EventId::from_uuid(row.requested_event),
        })
    }
}

/// The request with this id.
pub(crate) async fn request_by_id(
    conn: &mut PgConnection,
    id: EnrolmentRequestId,
) -> Result<Option<RequestRecord>, StoreError> {
    let row = sqlx::query_as!(
        RequestRow,
        "SELECT id, workspace_id, reserved_worker, key_digest, login, node, hostname, os, arch,
                build, code, requested_at, requested_event, expires_at, decision, changed_at
         FROM enrolment_request WHERE id = $1",
        id.as_uuid()
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(RequestRecord::try_from).transpose()
}

/// Whether a worker other than `except` holds the key with this digest.
async fn key_held(
    conn: &mut PgConnection,
    key: &WorkerKeyDigest,
    except: Option<WorkerId>,
) -> Result<bool, StoreError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM worker WHERE key_digest = $1
                          AND id IS DISTINCT FROM $2) AS "held!""#,
        key.as_bytes().as_slice(),
        except.map(WorkerId::as_uuid)
    )
    .fetch_one(&mut *conn)
    .await?)
}

impl Executable for RequestEnrolment {}
impl Load for RequestEnrolment {
    /// The person the requester's login belongs to, the request already holding this key, and
    /// whether a worker holds it.
    async fn load(
        &self,
        conn: &mut PgConnection,
        _principal: &Principal,
    ) -> Result<RequestFacts, StoreError> {
        let requester_person = sqlx::query_scalar!(
            "SELECT person_id FROM tailnet_identity WHERE login = $1",
            self.requester.login.as_str()
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(PersonId::from_uuid);
        let row = sqlx::query_as!(
            RequestRow,
            "SELECT id, workspace_id, reserved_worker, key_digest, login, node, hostname, os, arch,
                    build, code, requested_at, requested_event, expires_at, decision, changed_at
             FROM enrolment_request WHERE key_digest = $1",
            self.key.as_bytes().as_slice()
        )
        .fetch_optional(&mut *conn)
        .await?;
        let existing = row.as_ref().map(RequestRecord::try_from).transpose()?;
        // The request's own reserved worker holds the key once it is approved; that is the
        // replay's business, not a conflict.
        let reserved = existing.as_ref().map(|record| record.worker);
        Ok(RequestFacts {
            requester_person,
            existing,
            key_held: key_held(conn, &self.key, reserved).await?,
        })
    }
}

impl Executable for ApproveEnrolment {}
impl Load for ApproveEnrolment {
    /// The request with this id, and whether a worker other than its reserved one holds its key.
    async fn load(
        &self,
        conn: &mut PgConnection,
        _principal: &Principal,
    ) -> Result<ApproveFacts, StoreError> {
        let request = request_by_id(conn, self.request).await?;
        let key_held = match &request {
            Some(record) => key_held(conn, &record.key, Some(record.worker)).await?,
            None => false,
        };
        Ok(ApproveFacts { request, key_held })
    }
}

impl Executable for DenyEnrolment {}
impl Load for DenyEnrolment {
    /// The request with this id.
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        _principal: &'c Principal,
    ) -> impl Future<Output = Result<Option<RequestRecord>, StoreError>> + Send + 'c {
        request_by_id(conn, self.request)
    }
}

/// The status query's facts: the request with this id, which its `prove` compares with the
/// caller's login and node.
pub(crate) fn status_facts<'c>(
    query: &'c EnrolmentRequestStatus,
    conn: &'c mut PgConnection,
) -> impl Future<Output = Result<Option<RequestRecord>, StoreError>> + Send + 'c {
    request_by_id(conn, query.request)
}
