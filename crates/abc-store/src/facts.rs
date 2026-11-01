//! Fact loaders: one `Executable` impl per command. Each is read-only SQL that turns the open
//! write transaction's state into the plain records abc-core decides on. No rules live here: an
//! expiry comparison or a digest match belongs in the command's `prove` or `decide`.

use std::future::Future;

use abc_core::actor::Principal;
use abc_core::command::{
    ClaimServer, EndSession, EnrolFacts, EnrolWorker, InstallationFacts, IssueEnrolmentToken,
    IssueSetupCode, OpenSession, RevokeEnrolmentToken, RevokeWorker, SelfFacts, StartSession,
    TokenRecord, WorkerRecord,
};
use sqlx::PgConnection;

use crate::error::StoreError;
use crate::execute::{Executable, Sealed};

/// Shared by the two installation commands: `claimed = EXISTS (SELECT 1 FROM installation)`,
/// `setup_code = SELECT … FROM setup_code` (one row at most), and `login` from a `Visitor`
/// principal.
async fn installation_facts(
    conn: &mut PgConnection,
    principal: &Principal,
) -> Result<InstallationFacts, StoreError> {
    let _ = (conn, principal);
    unimplemented!()
}

impl Sealed for IssueSetupCode {}
impl Executable for IssueSetupCode {
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        principal: &'c Principal,
    ) -> impl Future<Output = Result<InstallationFacts, StoreError>> + Send + 'c {
        installation_facts(conn, principal)
    }
}

impl Sealed for ClaimServer {}
impl Executable for ClaimServer {
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        principal: &'c Principal,
    ) -> impl Future<Output = Result<InstallationFacts, StoreError>> + Send + 'c {
        installation_facts(conn, principal)
    }
}

impl Sealed for EnrolWorker {}
impl Executable for EnrolWorker {
    /// `SELECT … FROM enrolment_token WHERE digest = $1` with the digest of the presented token.
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        _principal: &'c Principal,
    ) -> impl Future<Output = Result<EnrolFacts, StoreError>> + Send + 'c {
        async move {
            let _ = (conn, TokenRecord::clone);
            unimplemented!()
        }
    }
}

impl Sealed for IssueEnrolmentToken {}
impl Executable for IssueEnrolmentToken {
    /// No facts: the policy's membership check (from the principal) is all it needs.
    fn load<'c>(
        &'c self,
        _conn: &'c mut PgConnection,
        _principal: &'c Principal,
    ) -> impl Future<Output = Result<(), StoreError>> + Send + 'c {
        async { Ok(()) }
    }
}

impl Sealed for RevokeEnrolmentToken {}
impl Executable for RevokeEnrolmentToken {
    /// `SELECT … FROM enrolment_token WHERE id = $1`.
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        _principal: &'c Principal,
    ) -> impl Future<Output = Result<Option<TokenRecord>, StoreError>> + Send + 'c {
        async move {
            let _ = conn;
            unimplemented!()
        }
    }
}

impl Sealed for RevokeWorker {}
impl Executable for RevokeWorker {
    /// `SELECT id, workspace_id, revoked_at IS NOT NULL FROM worker WHERE id = $1`.
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        _principal: &'c Principal,
    ) -> impl Future<Output = Result<Option<WorkerRecord>, StoreError>> + Send + 'c {
        async move {
            let _ = conn;
            unimplemented!()
        }
    }
}

/// `StartSession`'s facts: the authenticated worker, copied from the principal.
fn self_facts(principal: &Principal) -> SelfFacts {
    match principal {
        Principal::Worker { id, .. } => SelfFacts { worker: Some(*id) },
        Principal::Person { .. }
        | Principal::Visitor { .. }
        | Principal::Anonymous
        | Principal::Server => SelfFacts { worker: None },
    }
}

impl Sealed for StartSession {}
impl Executable for StartSession {
    fn load<'c>(
        &'c self,
        _conn: &'c mut PgConnection,
        principal: &'c Principal,
    ) -> impl Future<Output = Result<SelfFacts, StoreError>> + Send + 'c {
        async move { Ok(self_facts(principal)) }
    }
}

impl Sealed for EndSession {}
impl Executable for EndSession {
    /// `SELECT id, workspace_id, open_session FROM worker WHERE id = $1`.
    fn load<'c>(
        &'c self,
        conn: &'c mut PgConnection,
        _principal: &'c Principal,
    ) -> impl Future<Output = Result<Option<OpenSession>, StoreError>> + Send + 'c {
        async move {
            let _ = conn;
            unimplemented!()
        }
    }
}
