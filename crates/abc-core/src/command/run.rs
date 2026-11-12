//! Commands that record a graph run's life, one per runner event (`RunEvent`).
//!
//! A person starts a run and answers its escalations. The server, as the runner, grants leases,
//! expires them and applies the results workers report. These commands are the **recorders**:
//! they check what the stored facts can show (the run and lease exist, in this workspace; a lease
//! is live; a worker is online and idle) and write the event with its effect, in one transaction.
//! What needs the graph, such as which step is next or whether an answer is due, is the runner's
//! decision (S1 B2), which it makes before it issues a command.

use std::convert::Infallible;

use thiserror::Error;

use crate::decision::{DecideCtx, Decision, DecisionBuilder};
use crate::event::RunEvent;
use crate::graph::GraphVersion;
use crate::id::{LeaseId, RunId, WorkerId, WorkspaceId};
use crate::policy::{
    Authorized, Capability, DenialReason, NoProof, Proof, Request, Scope, WriteAction,
};
use crate::run::{Answer, StepResult};
use crate::time::Timestamp;

use super::Command;

/// A person starts a run of a graph, which the run pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartRun {
    /// The workspace the run belongs to.
    pub workspace: WorkspaceId,
    /// The graph version it runs.
    pub graph: GraphVersion,
}

impl Request for StartRun {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ManageRuns;
    type Facts = ();
    type Proof = NoProof;
    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }
    fn prove(&self, _facts: &(), _now: Timestamp) -> Result<NoProof, DenialReason> {
        Ok(NoProof)
    }
}

impl Command for StartRun {
    type Outcome = RunId;
    type Rejection = Infallible;

    fn decide(
        auth: Authorized<Self>,
        _facts: (),
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<RunId>, Infallible> {
        let mut decision = DecisionBuilder::new(&auth);
        let run = ctx.id::<RunId>();
        decision.emit(
            run,
            RunEvent::Started {
                graph: auth.into_request().graph,
            },
        );
        Ok(decision.finish(run))
    }
}

/// A run as the recorders see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRecord {
    /// The run.
    pub id: RunId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Its live lease, if one is out.
    pub live_lease: Option<LeaseId>,
}

/// A lease as the recorders see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseRecord {
    /// The lease.
    pub id: LeaseId,
    /// The run it is on.
    pub run: RunId,
    /// Whether it is live: not expired and its result not applied.
    pub live: bool,
}

/// A worker as a grant sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseWorker {
    /// The worker.
    pub id: WorkerId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Whether it has a session open.
    pub online: bool,
    /// Whether it holds a live lease.
    pub busy: bool,
}

/// What every server-side recorder loads: the run, and the lease or worker the command names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunFacts {
    /// The run the command names, if it exists.
    pub run: Option<RunRecord>,
    /// The lease it names, if it names one and it exists.
    pub lease: Option<LeaseRecord>,
    /// The worker it names, if it names one and it exists.
    pub worker: Option<LeaseWorker>,
}

/// Proof that the run exists, and in which workspace: the permit's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunProof {
    run: RunRecord,
}

impl Proof for RunProof {
    fn capability(&self) -> Capability {
        Capability::RunHome {
            workspace: self.run.workspace,
        }
    }
}

fn prove_run(facts: &RunFacts, run: RunId) -> Result<RunProof, DenialReason> {
    facts
        .run
        .filter(|record| record.id == run)
        .map(|run| RunProof { run })
        .ok_or(DenialReason::UnknownRun)
}

/// Why a recorder refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RunRejection {
    /// No such run in this workspace.
    #[error("no such run in this workspace")]
    UnknownRun,
    /// No such lease on this run.
    #[error("no such lease on this run")]
    UnknownLease,
    /// No such worker in the run's workspace.
    #[error("no such worker in this workspace")]
    UnknownWorker,
    /// The run already has a live lease: one live lease per step.
    #[error("the run already has a live lease")]
    RunLeased,
    /// The worker has no session open.
    #[error("the worker is offline")]
    WorkerOffline,
    /// The worker holds a live lease: a worker works on one step at a time.
    #[error("the worker already holds a live lease")]
    WorkerBusy,
}

/// The server leases the run's current step and phase to an idle, online worker (spec `grant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantLease {
    /// The run.
    pub run: RunId,
    /// The worker.
    pub worker: WorkerId,
}

impl Request for GrantLease {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::RunAsServer;
    type Facts = RunFacts;
    type Proof = RunProof;
    fn scope(&self) -> Scope {
        Scope::CapabilityWorkspace
    }
    fn prove(&self, facts: &RunFacts, _now: Timestamp) -> Result<RunProof, DenialReason> {
        prove_run(facts, self.run)
    }
}

impl Command for GrantLease {
    type Outcome = LeaseId;
    type Rejection = RunRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: RunFacts,
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<LeaseId>, RunRejection> {
        let mut decision = DecisionBuilder::new(&auth);
        let RunProof { run } = *auth.proof();
        let Self { worker, .. } = auth.into_request();
        let record = facts
            .worker
            .filter(|record| record.id == worker && record.workspace == run.workspace)
            .ok_or(RunRejection::UnknownWorker)?;
        if run.live_lease.is_some() {
            return Err(RunRejection::RunLeased);
        }
        if !record.online {
            return Err(RunRejection::WorkerOffline);
        }
        if record.busy {
            return Err(RunRejection::WorkerBusy);
        }
        let lease = ctx.id::<LeaseId>();
        decision.emit(run.id, RunEvent::LeaseGranted { lease, worker });
        Ok(decision.finish(lease))
    }
}

/// The server expires a lease whose worker stopped heartbeating it (spec `expire`). Expiring a
/// lease that is not live records nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpireLease {
    /// The run.
    pub run: RunId,
    /// The lease.
    pub lease: LeaseId,
}

impl Request for ExpireLease {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::RunAsServer;
    type Facts = RunFacts;
    type Proof = RunProof;
    fn scope(&self) -> Scope {
        Scope::CapabilityWorkspace
    }
    fn prove(&self, facts: &RunFacts, _now: Timestamp) -> Result<RunProof, DenialReason> {
        prove_run(facts, self.run)
    }
}

impl Command for ExpireLease {
    type Outcome = ();
    type Rejection = RunRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: RunFacts,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<()>, RunRejection> {
        let mut decision = DecisionBuilder::new(&auth);
        let RunProof { run } = *auth.proof();
        let Self { lease, .. } = auth.into_request();
        let record = known_lease(&facts, run.id, lease)?;
        if record.live {
            decision.emit(run.id, RunEvent::LeaseExpired { lease });
        }
        Ok(decision.finish(()))
    }
}

fn known_lease(facts: &RunFacts, run: RunId, lease: LeaseId) -> Result<LeaseRecord, RunRejection> {
    facts
        .lease
        .filter(|record| record.id == lease && record.run == run)
        .ok_or(RunRejection::UnknownLease)
}

/// What became of a worker's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultFate {
    /// Its lease was live: the run applied it.
    Applied,
    /// Its lease was not: expired, or already applied. The server acknowledges it all the same.
    Discarded,
}

/// A worker's result reaches the server (spec `deliver`). It is applied only if its lease is
/// live, so a result is applied at most once however late or often it arrives; any other result
/// is discarded and records nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyResult {
    /// The run.
    pub run: RunId,
    /// The lease the result is for.
    pub lease: LeaseId,
    /// What the worker reports.
    pub result: StepResult,
}

impl Request for ApplyResult {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::RunAsServer;
    type Facts = RunFacts;
    type Proof = RunProof;
    fn scope(&self) -> Scope {
        Scope::CapabilityWorkspace
    }
    fn prove(&self, facts: &RunFacts, _now: Timestamp) -> Result<RunProof, DenialReason> {
        prove_run(facts, self.run)
    }
}

impl Command for ApplyResult {
    type Outcome = ResultFate;
    type Rejection = RunRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: RunFacts,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<ResultFate>, RunRejection> {
        let mut decision = DecisionBuilder::new(&auth);
        let RunProof { run } = *auth.proof();
        let Self { lease, result, .. } = auth.into_request();
        if known_lease(&facts, run.id, lease)?.live {
            decision.emit(run.id, RunEvent::ResultApplied { lease, result });
            Ok(decision.finish(ResultFate::Applied))
        } else {
            Ok(decision.finish(ResultFate::Discarded))
        }
    }
}

/// A person answers the escalation a run is parked on (spec `answer`). That the run is parked is
/// the runner's to know, since it depends on the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnswerEscalation {
    /// The workspace (the scope of the check).
    pub workspace: WorkspaceId,
    /// The run.
    pub run: RunId,
    /// The answer.
    pub answer: Answer,
}

impl Request for AnswerEscalation {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ManageRuns;
    type Facts = Option<RunRecord>;
    type Proof = NoProof;
    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }
    fn prove(&self, _facts: &Option<RunRecord>, _now: Timestamp) -> Result<NoProof, DenialReason> {
        Ok(NoProof)
    }
}

impl Command for AnswerEscalation {
    type Outcome = ();
    type Rejection = RunRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: Option<RunRecord>,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<()>, RunRejection> {
        let mut decision = DecisionBuilder::new(&auth);
        let command = auth.into_request();
        // A run of another workspace is not found, as for a worker.
        let run = facts
            .filter(|record| record.id == command.run && record.workspace == command.workspace)
            .ok_or(RunRejection::UnknownRun)?;
        decision.emit(
            run.id,
            RunEvent::Answered {
                answer: command.answer,
            },
        );
        Ok(decision.finish(()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{HOME, Outcome, events, id, now, owner, run};
    use super::*;
    use crate::actor::{Actor, Principal};
    use crate::policy::{ActionName, Denied};
    use crate::run::{Confidence, Decision as Judged, Verdict};

    fn record(lease: Option<u128>) -> RunRecord {
        RunRecord {
            id: id(20),
            workspace: id(HOME),
            live_lease: lease.map(id),
        }
    }

    fn worker(online: bool, busy: bool) -> LeaseWorker {
        LeaseWorker {
            id: id(3),
            workspace: id(HOME),
            online,
            busy,
        }
    }

    fn lease(live: bool) -> LeaseRecord {
        LeaseRecord {
            id: id(30),
            run: id(20),
            live,
        }
    }

    fn facts(
        run: Option<RunRecord>,
        lease: Option<LeaseRecord>,
        worker: Option<LeaseWorker>,
    ) -> RunFacts {
        RunFacts { run, lease, worker }
    }

    fn graph() -> GraphVersion {
        GraphVersion::from_bytes([9; 32])
    }

    #[test]
    fn an_owner_starts_a_run_that_pins_its_graph() {
        let decision = run(
            &owner(),
            StartRun {
                workspace: id(HOME),
                graph: graph(),
            },
            (),
            now(),
        )
        .unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Person(id(1)),
            "recorded as the owner"
        );
        assert_eq!(decision.permit().workspace(), id(HOME), "in the workspace");
        let started = events(&decision);
        let started_run = decision.into_outcome();
        assert_eq!(started.len(), 1, "one event");
        assert_eq!(
            started[0].1,
            RunEvent::Started { graph: graph() }.into(),
            "pinning the version"
        );
        assert_eq!(
            started_run.as_uuid(),
            started[0].0,
            "the outcome is the run, which is the stream"
        );
        let elsewhere = StartRun {
            workspace: id(HOME + 1),
            graph: graph(),
        };
        assert_eq!(
            run(&owner(), elsewhere, (), now()).map(|_| ()).unwrap_err(),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::ManageRuns),
                reason: DenialReason::NotAMember,
            }),
            "only an owner of the workspace"
        );
    }

    #[test]
    fn the_server_grants_a_lease_to_an_idle_online_worker() {
        let grant = GrantLease {
            run: id(20),
            worker: id(3),
        };
        let decision = run(
            &Principal::Server,
            grant,
            facts(Some(record(None)), None, Some(worker(true, false))),
            now(),
        )
        .unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Server,
            "the server decides"
        );
        let granted = events(&decision);
        let granted_lease = decision.into_outcome();
        assert_eq!(granted.len(), 1, "one event");
        assert_eq!(
            granted[0].0,
            uuid::Uuid::from_u128(20),
            "on the run's stream"
        );
        assert_eq!(
            granted[0].1,
            RunEvent::LeaseGranted {
                lease: granted_lease,
                worker: id(3),
            }
            .into(),
            "naming the lease and the worker"
        );
        let refused = |facts| {
            run(&Principal::Server, grant, facts, now())
                .map(|_| ())
                .unwrap_err()
        };
        let rejected = |why| Outcome::Rejected(why);
        assert_eq!(
            refused(facts(
                Some(record(Some(31))),
                None,
                Some(worker(true, false))
            )),
            rejected(RunRejection::RunLeased),
            "one live lease per run"
        );
        assert_eq!(
            refused(facts(Some(record(None)), None, Some(worker(false, false)))),
            rejected(RunRejection::WorkerOffline),
            "offline"
        );
        assert_eq!(
            refused(facts(Some(record(None)), None, Some(worker(true, true)))),
            rejected(RunRejection::WorkerBusy),
            "one step at a time"
        );
        assert_eq!(
            refused(facts(Some(record(None)), None, None)),
            rejected(RunRejection::UnknownWorker),
            "no such worker"
        );
        let mut stranger = worker(true, false);
        stranger.workspace = id(HOME + 1);
        assert_eq!(
            refused(facts(Some(record(None)), None, Some(stranger))),
            rejected(RunRejection::UnknownWorker),
            "a worker of another workspace is not found"
        );
        assert_eq!(
            refused(facts(None, None, Some(worker(true, false)))),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::RunAsServer),
                reason: DenialReason::UnknownRun,
            }),
            "no such run"
        );
    }

    #[test]
    fn only_the_server_runs_the_runner() {
        let grant = GrantLease {
            run: id(20),
            worker: id(3),
        };
        assert_eq!(
            run(
                &owner(),
                grant,
                facts(Some(record(None)), None, Some(worker(true, false))),
                now()
            )
            .map(|_| ())
            .unwrap_err(),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::RunAsServer),
                reason: DenialReason::ServerOnly,
            }),
            "a person cannot grant leases"
        );
    }

    #[test]
    fn a_live_lease_expires_once() {
        let expire = ExpireLease {
            run: id(20),
            lease: id(30),
        };
        let at = |lease| facts(Some(record(None)), lease, None);
        let decision = run(&Principal::Server, expire, at(Some(lease(true))), now()).unwrap();
        assert_eq!(
            events(&decision),
            [(
                uuid::Uuid::from_u128(20),
                RunEvent::LeaseExpired { lease: id(30) }.into()
            )],
            "expired"
        );
        let again = run(&Principal::Server, expire, at(Some(lease(false))), now()).unwrap();
        assert!(
            events(&again).is_empty(),
            "expiring a dead lease records nothing"
        );
        assert_eq!(
            run(&Principal::Server, expire, at(None), now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Rejected(RunRejection::UnknownLease),
            "no such lease"
        );
        let mut other = lease(true);
        other.run = id(21);
        assert_eq!(
            run(&Principal::Server, expire, at(Some(other)), now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Rejected(RunRejection::UnknownLease),
            "a lease of another run is not this run's"
        );
    }

    #[test]
    fn a_result_is_applied_only_while_its_lease_is_live() {
        let result = StepResult::Judged(Judged {
            verdict: Verdict::Pass,
            confidence: Confidence::new(90).unwrap(),
        });
        let apply = ApplyResult {
            run: id(20),
            lease: id(30),
            result,
        };
        let at = |lease| facts(Some(record(Some(30))), lease, None);
        let live = run(&Principal::Server, apply, at(Some(lease(true))), now()).unwrap();
        let live_events = events(&live);
        assert_eq!(live.into_outcome(), ResultFate::Applied, "applied");
        assert_eq!(
            live_events,
            [(
                uuid::Uuid::from_u128(20),
                RunEvent::ResultApplied {
                    lease: id(30),
                    result
                }
                .into()
            )],
            "recorded"
        );
        let late = run(&Principal::Server, apply, at(Some(lease(false))), now()).unwrap();
        assert!(events(&late).is_empty(), "and recorded nowhere");
        assert_eq!(late.into_outcome(), ResultFate::Discarded, "discarded");
        assert_eq!(
            run(&Principal::Server, apply, at(None), now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Rejected(RunRejection::UnknownLease),
            "no such lease"
        );
    }

    #[test]
    fn an_owner_answers_a_run_of_their_workspace() {
        let answer = AnswerEscalation {
            workspace: id(HOME),
            run: id(20),
            answer: Answer::Accept,
        };
        let decision = run(&owner(), answer, Some(record(None)), now()).unwrap();
        assert_eq!(
            events(&decision),
            [(
                uuid::Uuid::from_u128(20),
                RunEvent::Answered {
                    answer: Answer::Accept
                }
                .into()
            )],
            "recorded"
        );
        let mut elsewhere = record(None);
        elsewhere.workspace = id(HOME + 1);
        for facts in [None, Some(elsewhere)] {
            assert_eq!(
                run(&owner(), answer, facts, now()).map(|_| ()).unwrap_err(),
                Outcome::Rejected(RunRejection::UnknownRun),
                "not found"
            );
        }
    }
}
