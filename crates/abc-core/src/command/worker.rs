//! Worker lifecycle after enrolment: sessions and revocation.
//!
//! * `StartSession` is recorded by the worker itself. It carries no worker id: its facts hold the
//!   worker the store authenticated ([`SelfFacts`]), so a worker cannot start a session for another
//!   worker (no confused deputy).
//! * `EndSession` is recorded by the server (`Principal::Server`), which observed the socket close,
//!   or found the session left open by a dead process (the boot sweep). So a revoked worker's
//!   last session still ends in the log, and "online" (the `worker` view's `open_session`) is
//!   always derived from the log alone.
//! * `RevokeWorker` is the owner's.

use std::convert::Infallible;

use semver::Version;
use thiserror::Error;

use crate::decision::{DecideCtx, Decision, DecisionBuilder};
use crate::event::{SessionEndReason, WorkerEvent};
use crate::id::{SessionId, WorkerId, WorkspaceId};
use crate::policy::{
    Authorized, Capability, DenialReason, NoProof, Proof, Request, Scope, WriteAction,
};
use crate::time::Timestamp;

use super::Command;

/// A worker completed the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartSession {
    /// The session id the server minted for this socket.
    pub session: SessionId,
    /// The release from the worker's `Hello`.
    pub build: Version,
}

/// The authenticated worker, copied from `Principal::Worker` by the store's loader (no SQL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfFacts {
    /// `Some` exactly when the principal is a worker.
    pub worker: Option<WorkerId>,
}

/// Proof that the caller is the worker it acts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfProof {
    worker: WorkerId,
}

impl Proof for SelfProof {
    fn capability(&self) -> Capability {
        Capability::None
    }
}

impl Request for StartSession {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ActAsWorker;
    type Facts = SelfFacts;
    type Proof = SelfProof;
    fn scope(&self) -> Scope {
        Scope::OwnWorkspace
    }
    fn prove(&self, facts: &SelfFacts, _now: Timestamp) -> Result<SelfProof, DenialReason> {
        facts
            .worker
            .map(|worker| SelfProof { worker })
            .ok_or(DenialReason::WorkerOnly)
    }
}

impl Command for StartSession {
    type Outcome = ();
    type Rejection = Infallible;

    fn decide(
        auth: Authorized<Self>,
        _facts: SelfFacts,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<()>, Infallible> {
        // A session that starts while another is open supersedes it: the projection replaces
        // `open_session`, and the superseded session's later `EndSession` is a no-op (below).
        let mut decision = DecisionBuilder::new(&auth);
        let SelfProof { worker } = *auth.proof();
        let StartSession { session, build } = auth.into_request();
        decision.emit(worker, WorkerEvent::SessionStarted { session, build });
        Ok(decision.finish(()))
    }
}

/// The server saw a session end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndSession {
    /// The worker.
    pub worker: WorkerId,
    /// The session.
    pub session: SessionId,
    /// Why.
    pub reason: SessionEndReason,
}

/// A worker's currently open session, from the `worker` view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenSession {
    /// The worker.
    pub worker: WorkerId,
    /// Its workspace: the scope of the session events.
    pub workspace: WorkspaceId,
    /// The open session, if any.
    pub session: Option<SessionId>,
}

/// Proof that the session's worker exists, and in which workspace: the permit's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionProof {
    open: OpenSession,
}

impl Proof for SessionProof {
    fn capability(&self) -> Capability {
        Capability::WorkerHome {
            workspace: self.open.workspace,
        }
    }
}

impl Request for EndSession {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::RecordAsServer;
    type Facts = Option<OpenSession>;
    type Proof = SessionProof;
    fn scope(&self) -> Scope {
        Scope::CapabilityWorkspace
    }
    fn prove(
        &self,
        facts: &Option<OpenSession>,
        _now: Timestamp,
    ) -> Result<SessionProof, DenialReason> {
        facts
            .map(|open| SessionProof { open })
            .ok_or(DenialReason::UnknownWorker)
    }
}

impl Command for EndSession {
    type Outcome = ();
    type Rejection = Infallible;

    fn decide(
        auth: Authorized<Self>,
        _facts: Option<OpenSession>,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<()>, Infallible> {
        // Idempotent and supersession-safe: emit SessionEnded only when
        // `proof.open.session == Some(command.session)`. A superseded session, a second call, or a
        // session the boot sweep already closed all yield zero events.
        //
        // Boot sweep (abc-server `run`, before the sidecar socket is bound, so no socket is live):
        //   for open in store.query(&Caller::Server, OpenSessions)? {
        //       store.execute(&Caller::Server, &origin, EndSession { worker, session, reason: ServerRestart })?
        //   }
        let _ = (auth, DecisionBuilder::finish::<()>);
        unimplemented!()
    }
}

/// The owner revokes a worker. Its key stops authenticating in the same transaction, and live
/// sessions close on their next `LogFeed` wake-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevokeWorker {
    /// The workspace (the scope of the check).
    pub workspace: WorkspaceId,
    /// The worker.
    pub worker: WorkerId,
}

/// The stored state of a worker that the revocation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerRecord {
    /// The worker.
    pub id: WorkerId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Whether it is already revoked.
    pub revoked: bool,
}

/// Why a worker cannot be revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RevokeWorkerRejection {
    /// No worker with this id in this workspace.
    #[error("no such worker in this workspace")]
    NotFound,
}

impl Request for RevokeWorker {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ManageWorkers;
    type Facts = Option<WorkerRecord>;
    type Proof = NoProof;
    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }
    fn prove(
        &self,
        _facts: &Option<WorkerRecord>,
        _now: Timestamp,
    ) -> Result<NoProof, DenialReason> {
        Ok(NoProof)
    }
}

impl Command for RevokeWorker {
    type Outcome = ();
    type Rejection = RevokeWorkerRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: Option<WorkerRecord>,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<()>, RevokeWorkerRejection> {
        // None, or a record from another workspace => NotFound (the scope check already proved the
        // caller owns `workspace`; this keeps a worker id from another workspace from being touched).
        // Already revoked => zero events (idempotent). Otherwise emit WorkerEvent::Revoked.
        let _ = (auth, facts);
        unimplemented!()
    }
}
