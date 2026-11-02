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
        let Self { session, build } = auth.into_request();
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
        // Idempotent and supersession-safe: a superseded session, a second call, or a session the
        // boot sweep already closed all find another session (or none) open and record nothing.
        let mut decision = DecisionBuilder::new(&auth);
        let SessionProof { open } = *auth.proof();
        let Self {
            worker,
            session,
            reason,
        } = auth.into_request();
        if open.worker == worker && open.session == Some(session) {
            decision.emit(worker, WorkerEvent::SessionEnded { session, reason });
        }
        Ok(decision.finish(()))
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
        // The check proved the caller owns `workspace`; a worker of another workspace is not found.
        let mut decision = DecisionBuilder::new(&auth);
        let command = auth.into_request();
        let record = facts
            .filter(|record| record.id == command.worker && record.workspace == command.workspace)
            .ok_or(RevokeWorkerRejection::NotFound)?;
        if !record.revoked {
            decision.emit(record.id, WorkerEvent::Revoked);
        }
        Ok(decision.finish(()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{HOME, Outcome, events, id, now, owner, run};
    use super::*;
    use crate::actor::{Actor, Principal};
    use crate::policy::{ActionName, Denied};

    fn worker() -> Principal {
        Principal::Worker {
            id: id(3),
            workspace: id(HOME),
        }
    }

    #[test]
    fn a_worker_starts_its_own_session() {
        let command = StartSession {
            session: id(5),
            build: Version::new(0, 2, 0),
        };
        let decision = run(
            &worker(),
            command,
            SelfFacts {
                worker: Some(id(3)),
            },
            now(),
        )
        .unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Worker(id(3)),
            "recorded as the worker"
        );
        assert_eq!(
            events(&decision),
            [(
                uuid::Uuid::from_u128(3),
                WorkerEvent::SessionStarted {
                    session: id(5),
                    build: Version::new(0, 2, 0),
                }
                .into()
            )],
            "on its own stream"
        );
        let start = || StartSession {
            session: id(5),
            build: Version::new(0, 2, 0),
        };
        assert_eq!(
            run(&owner(), start(), SelfFacts { worker: None }, now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::ActAsWorker),
                reason: DenialReason::WorkerOnly,
            }),
            "only a worker starts a session"
        );
    }

    fn open(session: Option<u128>) -> OpenSession {
        OpenSession {
            worker: id(3),
            workspace: id(HOME),
            session: session.map(id),
        }
    }

    fn end(session: u128) -> EndSession {
        EndSession {
            worker: id(3),
            session: id(session),
            reason: SessionEndReason::Closed,
        }
    }

    #[test]
    fn the_server_ends_the_open_session_and_nothing_else() {
        let decision = run(&Principal::Server, end(5), Some(open(Some(5))), now()).unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Server,
            "recorded as the server"
        );
        assert_eq!(
            decision.permit().workspace(),
            id(HOME),
            "in the worker's workspace"
        );
        assert_eq!(
            events(&decision),
            [(
                uuid::Uuid::from_u128(3),
                WorkerEvent::SessionEnded {
                    session: id(5),
                    reason: SessionEndReason::Closed,
                }
                .into()
            )],
            "the open session ends"
        );
        for (facts, why) in [
            (Some(open(Some(6))), "a superseded session's end"),
            (Some(open(None)), "a repeated end"),
        ] {
            assert!(
                run(&Principal::Server, end(5), facts, now())
                    .unwrap()
                    .events()
                    .is_empty(),
                "{why} records nothing"
            );
        }
        let foreign = Some(OpenSession {
            worker: id(4),
            workspace: id(HOME),
            session: Some(id(5)),
        });
        assert!(
            run(&Principal::Server, end(5), foreign, now())
                .unwrap()
                .events()
                .is_empty(),
            "another worker's session is not this one"
        );
        assert_eq!(
            run(&Principal::Server, end(5), None, now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::RecordAsServer),
                reason: DenialReason::UnknownWorker,
            }),
            "an unknown worker"
        );
    }

    fn revoke() -> RevokeWorker {
        RevokeWorker {
            workspace: id(HOME),
            worker: id(3),
        }
    }

    fn record(revoked: bool) -> WorkerRecord {
        WorkerRecord {
            id: id(3),
            workspace: id(HOME),
            revoked,
        }
    }

    #[test]
    fn an_owner_revokes_a_worker_once() {
        let decision = run(&owner(), revoke(), Some(record(false)), now()).unwrap();
        assert_eq!(
            events(&decision),
            [(uuid::Uuid::from_u128(3), WorkerEvent::Revoked.into())],
            "revoked"
        );
        assert!(
            run(&owner(), revoke(), Some(record(true)), now())
                .unwrap()
                .events()
                .is_empty(),
            "twice records nothing"
        );
    }

    #[test]
    fn only_a_worker_of_this_workspace_can_be_revoked() {
        for (facts, why) in [
            (None, "unknown"),
            (
                Some(WorkerRecord {
                    id: id(3),
                    workspace: id(999),
                    revoked: false,
                }),
                "another workspace's",
            ),
            (
                Some(WorkerRecord {
                    id: id(4),
                    workspace: id(HOME),
                    revoked: false,
                }),
                "a different worker",
            ),
        ] {
            assert_eq!(
                run(&owner(), revoke(), facts, now())
                    .map(|_| ())
                    .unwrap_err(),
                Outcome::Rejected(RevokeWorkerRejection::NotFound),
                "{why}"
            );
        }
    }

    #[test]
    fn session_proofs_carry_no_bearer_capability_but_the_workers_home() {
        assert_eq!(
            SelfProof { worker: id(3) }.capability(),
            Capability::None,
            "self"
        );
        assert_eq!(
            SessionProof {
                open: OpenSession {
                    worker: id(3),
                    workspace: id(HOME),
                    session: None,
                }
            }
            .capability(),
            Capability::WorkerHome {
                workspace: id(HOME)
            },
            "the worker's workspace"
        );
    }
}
