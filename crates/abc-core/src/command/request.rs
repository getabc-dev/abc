//! Enrolment requests: a machine asks, its owner approves or denies in the browser.
//!
//! Lifecycle (every step under the log-head lock):
//!
//! ```text
//!   requested ──approve──▶ approved (+ worker.enrolled) ──approve again──▶ zero events
//!     │                        └──deny──▶ refused: already approved
//!     ├──deny──▶ denied ──deny again──▶ zero events; approve──▶ refused: denied
//!     └──(now ≥ expires_at)──▶ expired (derived; approve refused, deny records nothing)
//! ```
//!
//! The request is made by a member of the workspace from their own tailnet login, so a stranger
//! on the tailnet cannot put a machine in front of the owner. The machine's credential is its own
//! key, of which the server only ever sees the digest; approving it hands out nothing.

use semver::Version;
use thiserror::Error;

use super::Command;
use super::claim::expiry;
use super::enrolment::Enrolled;
use crate::credential::{ApprovalCode, WorkerKeyDigest};
use crate::decision::{DecideCtx, Decision, DecisionBuilder};
use crate::event::{EnrolledVia, EnrolmentRequestEvent, RequestedEnrolment, WorkerEvent};
use crate::id::{EnrolmentRequestId, EventId, PersonId, WorkerId, WorkspaceId};
use crate::identity::{MachineInfo, Requester};
use crate::policy::{
    Authorized, Capability, DenialReason, NoProof, Proof, Request, Scope, WriteAction,
};
use crate::time::Timestamp;

/// How long a request waits for its owner: ten minutes.
pub const REQUEST_TTL: time::Duration = time::Duration::minutes(10);

/// A member asks, from a machine, for it to enrol into `workspace` with the key it generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestEnrolment {
    /// Where the machine will enrol.
    pub workspace: WorkspaceId,
    /// Who asks, and from which node: built by abc-server from Serve's identity and its own
    /// tailscaled.
    pub requester: Requester,
    /// The digest of the key the machine generated and saved before sending this.
    pub key: WorkerKeyDigest,
    /// What it reports about itself.
    pub machine: MachineInfo,
    /// The abc release it runs.
    pub build: Version,
}

/// How a request was decided, if it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDecision {
    /// The owner approved it.
    Approved,
    /// The owner denied it.
    Denied,
}

/// The stored state of a request, as the `enrolment_request` view holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// The request.
    pub id: EnrolmentRequestId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// The worker id it reserved.
    pub worker: WorkerId,
    /// The digest of the machine's key.
    pub key: WorkerKeyDigest,
    /// Who asked.
    pub requester: Requester,
    /// What the machine reported.
    pub machine: MachineInfo,
    /// Its release.
    pub build: Version,
    /// The code both sides show.
    pub code: ApprovalCode,
    /// When it can no longer be approved.
    pub expires_at: Timestamp,
    /// The owner's decision, if any.
    pub decision: Option<RequestDecision>,
    /// Its `requested` event, which becomes the enrolment's causation.
    pub requested_event: EventId,
}

/// Where a request stands at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStanding {
    /// Waiting for the owner.
    Pending,
    /// Approved: the worker exists.
    Approved,
    /// Denied.
    Denied,
    /// Not decided in time.
    Expired,
}

impl RequestRecord {
    /// Where it stands at `now`: a decision stands for good; otherwise it is pending until
    /// `expires_at`.
    #[must_use]
    pub fn standing(&self, now: Timestamp) -> RequestStanding {
        match self.decision {
            Some(RequestDecision::Approved) => RequestStanding::Approved,
            Some(RequestDecision::Denied) => RequestStanding::Denied,
            None if now >= self.expires_at => RequestStanding::Expired,
            None => RequestStanding::Pending,
        }
    }
}

/// Loaded in the request's transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestFacts {
    /// The person the requester's login belongs to, if any.
    pub requester_person: Option<PersonId>,
    /// The request already made with this key, if any: a key names one request.
    pub existing: Option<RequestRecord>,
    /// Whether an enrolled worker already holds this key.
    pub key_held: bool,
}

/// The requester's login belongs to this person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequesterProof {
    person: PersonId,
}

impl Proof for RequesterProof {
    fn capability(&self) -> Capability {
        Capability::Requester {
            person: self.person,
        }
    }
}

/// What the machine shows while it waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolmentRequested {
    /// The request, which the machine polls.
    pub request: EnrolmentRequestId,
    /// The code to compare with the Approve page.
    pub code: ApprovalCode,
    /// When it stops waiting.
    pub expires_at: Timestamp,
}

/// Why a request cannot be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RequestRejection {
    /// The key is a worker's already, or another request's.
    #[error("this key is already in use; generate a new one")]
    KeyInUse,
}

impl Request for RequestEnrolment {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::RequestEnrolment;
    type Facts = RequestFacts;
    type Proof = RequesterProof;

    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }

    fn prove(&self, facts: &RequestFacts, _now: Timestamp) -> Result<RequesterProof, DenialReason> {
        facts
            .requester_person
            .map(|person| RequesterProof { person })
            .ok_or(DenialReason::NotAMember)
    }
}

impl Command for RequestEnrolment {
    type Outcome = EnrolmentRequested;
    type Rejection = RequestRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: RequestFacts,
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<EnrolmentRequested>, RequestRejection> {
        let mut decision = DecisionBuilder::new(&auth);
        let command = auth.into_request();
        if facts.key_held {
            return Err(RequestRejection::KeyInUse);
        }
        if let Some(existing) = facts.existing {
            // The same machine asking again, its answer lost: the same request, nothing new.
            let replay =
                existing.requester == command.requester && existing.workspace == command.workspace;
            return if replay {
                Ok(decision.finish(EnrolmentRequested {
                    request: existing.id,
                    code: existing.code,
                    expires_at: existing.expires_at,
                }))
            } else {
                Err(RequestRejection::KeyInUse)
            };
        }
        let request: EnrolmentRequestId = ctx.id();
        let worker: WorkerId = ctx.id();
        let code = ApprovalCode::generate(ctx.mint());
        let expires_at = expiry(ctx.now(), REQUEST_TTL);
        decision.emit(
            request,
            EnrolmentRequestEvent::Requested(Box::new(RequestedEnrolment {
                worker,
                key: command.key,
                requester: command.requester,
                machine: command.machine,
                build: command.build,
                code: code.clone(),
                expires_at,
            })),
        );
        Ok(decision.finish(EnrolmentRequested {
            request,
            code,
            expires_at,
        }))
    }
}

/// The owner approves a pending request, which enrols the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApproveEnrolment {
    /// Its workspace (the scope of the check).
    pub workspace: WorkspaceId,
    /// The request.
    pub request: EnrolmentRequestId,
}

/// Loaded for approving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveFacts {
    /// The request with this id, if any.
    pub request: Option<RequestRecord>,
    /// Whether a worker other than the reserved one holds its key.
    pub key_held: bool,
}

/// Why a request cannot be approved or denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DecideRequestRejection {
    /// No request with this id in this workspace.
    #[error("no such enrolment request in this workspace")]
    NotFound,
    /// It expired before anyone decided.
    #[error("the enrolment request expired; run abc worker setup on the machine again")]
    Expired,
    /// It was denied.
    #[error("the enrolment request was denied")]
    AlreadyDenied,
    /// It was approved: revoke the worker instead.
    #[error("the enrolment request was already approved; revoke the worker instead")]
    AlreadyApproved,
    /// Another worker holds its key.
    #[error("another worker already holds this machine's key")]
    KeyInUse,
}

impl Request for ApproveEnrolment {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ManageEnrolment;
    type Facts = ApproveFacts;
    type Proof = NoProof;
    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }
    fn prove(&self, _facts: &ApproveFacts, _now: Timestamp) -> Result<NoProof, DenialReason> {
        Ok(NoProof)
    }
}

impl Command for ApproveEnrolment {
    type Outcome = Enrolled;
    type Rejection = DecideRequestRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: ApproveFacts,
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<Enrolled>, DecideRequestRejection> {
        let mut decision = DecisionBuilder::new(&auth);
        let command = auth.into_request();
        let record = facts
            .request
            .filter(|record| record.workspace == command.workspace)
            .ok_or(DecideRequestRejection::NotFound)?;
        let enrolled = Enrolled {
            worker: record.worker,
            workspace: record.workspace,
            name: record.machine.hostname.clone(),
        };
        match record.standing(ctx.now()) {
            RequestStanding::Approved => return Ok(decision.finish(enrolled)),
            RequestStanding::Denied => return Err(DecideRequestRejection::AlreadyDenied),
            RequestStanding::Expired => return Err(DecideRequestRejection::Expired),
            RequestStanding::Pending => {}
        }
        if facts.key_held {
            return Err(DecideRequestRejection::KeyInUse);
        }
        decision.caused_by(record.requested_event);
        decision.emit(record.id, EnrolmentRequestEvent::Approved);
        decision.emit(
            record.worker,
            WorkerEvent::Enrolled {
                via: EnrolledVia::Request { request: record.id },
                key: record.key,
                name: enrolled.name.clone(),
                machine: record.machine,
                build: record.build,
            },
        );
        Ok(decision.finish(enrolled))
    }
}

/// The owner denies a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenyEnrolment {
    /// Its workspace (the scope of the check).
    pub workspace: WorkspaceId,
    /// The request.
    pub request: EnrolmentRequestId,
}

impl Request for DenyEnrolment {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ManageEnrolment;
    type Facts = Option<RequestRecord>;
    type Proof = NoProof;
    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }
    fn prove(
        &self,
        _facts: &Option<RequestRecord>,
        _now: Timestamp,
    ) -> Result<NoProof, DenialReason> {
        Ok(NoProof)
    }
}

impl Command for DenyEnrolment {
    type Outcome = ();
    type Rejection = DecideRequestRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: Option<RequestRecord>,
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<()>, DecideRequestRejection> {
        let mut decision = DecisionBuilder::new(&auth);
        let command = auth.into_request();
        let record = facts
            .filter(|record| record.workspace == command.workspace)
            .ok_or(DecideRequestRejection::NotFound)?;
        match record.standing(ctx.now()) {
            RequestStanding::Approved => return Err(DecideRequestRejection::AlreadyApproved),
            // Already over: nothing to record.
            RequestStanding::Denied | RequestStanding::Expired => {}
            RequestStanding::Pending => {
                decision.emit(record.id, EnrolmentRequestEvent::Denied);
            }
        }
        Ok(decision.finish(()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{HOME, Outcome, events, id, now, owner, run, visitor};
    use super::*;
    use crate::actor::{Actor, Membership, Principal, Role};
    use crate::credential::Digest;
    use crate::identity::{Arch, DisplayName, Os, TailnetLogin, TailnetNode};
    use crate::policy::{ActionName, Denied};

    fn requester(node: &str) -> Requester {
        Requester {
            login: TailnetLogin::parse("alice@example.com").unwrap(),
            node: TailnetNode::parse(node).unwrap(),
        }
    }

    fn machine() -> MachineInfo {
        MachineInfo {
            hostname: DisplayName::parse("mbp").unwrap(),
            os: Os::Macos,
            arch: Arch::Aarch64,
        }
    }

    fn key(byte: u8) -> WorkerKeyDigest {
        Digest::from_bytes([byte; 32])
    }

    fn ask() -> RequestEnrolment {
        RequestEnrolment {
            workspace: id(HOME),
            requester: requester("mbp.tail1234.ts.net"),
            key: key(7),
            machine: machine(),
            build: Version::new(0, 1, 0),
        }
    }

    fn facts() -> RequestFacts {
        RequestFacts {
            requester_person: Some(id(1)),
            existing: None,
            key_held: false,
        }
    }

    fn record() -> RequestRecord {
        RequestRecord {
            id: id(20),
            workspace: id(HOME),
            worker: id(21),
            key: key(7),
            requester: requester("mbp.tail1234.ts.net"),
            machine: machine(),
            build: Version::new(0, 1, 0),
            code: ApprovalCode::parse("ABCD-EFGH").unwrap(),
            expires_at: now().checked_add(REQUEST_TTL).unwrap(),
            decision: None,
            requested_event: id(22),
        }
    }

    #[test]
    fn a_member_asks_and_the_request_waits_ten_minutes_with_a_code() {
        let decision = run(&owner(), ask(), facts(), now()).unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Person(id(1)),
            "recorded as the person who asked"
        );
        assert_eq!(
            decision.permit().workspace(),
            id(HOME),
            "in their workspace"
        );
        let recorded = events(&decision);
        let requested = decision.into_outcome();
        let expires_at = now().checked_add(time::Duration::minutes(10)).unwrap();
        assert_eq!(requested.expires_at, expires_at, "ten minutes");
        assert_eq!(requested.request, id(1), "the first minted id");
        assert_eq!(
            recorded,
            [(
                requested.request.as_uuid(),
                EnrolmentRequestEvent::Requested(Box::new(RequestedEnrolment {
                    worker: id(2),
                    key: key(7),
                    requester: requester("mbp.tail1234.ts.net"),
                    machine: machine(),
                    build: Version::new(0, 1, 0),
                    code: requested.code,
                    expires_at,
                }))
                .into()
            )],
            "who asked, from where, with which key, and the reserved worker"
        );
    }

    #[test]
    fn only_a_member_whose_login_it_is_may_ask() {
        let denied = |principal: &Principal, facts: RequestFacts| match run(
            principal,
            ask(),
            facts,
            now(),
        ) {
            Err(Outcome::Denied(denied)) => denied.reason,
            other => panic!("expected a denial, got {other:?}"),
        };
        assert_eq!(
            denied(
                &visitor(),
                RequestFacts {
                    requester_person: None,
                    ..facts()
                }
            ),
            DenialReason::NotAMember,
            "a tailnet stranger"
        );
        assert_eq!(
            denied(
                &owner(),
                RequestFacts {
                    requester_person: Some(id(9)),
                    ..facts()
                }
            ),
            DenialReason::NotAMember,
            "a login that is someone else's"
        );
        let elsewhere = Principal::Person {
            id: id(1),
            memberships: vec![Membership {
                workspace: id(300),
                role: Role::Owner,
            }],
        };
        assert_eq!(
            denied(&elsewhere, facts()),
            DenialReason::NotAMember,
            "a person of another workspace"
        );
        for principal in [
            Principal::Anonymous,
            Principal::Server,
            Principal::Worker {
                id: id(3),
                workspace: id(HOME),
            },
        ] {
            assert_eq!(
                denied(&principal, facts()),
                DenialReason::TailnetIdentityRequired,
                "{principal:?}"
            );
        }
        assert_eq!(
            run(
                &visitor(),
                ask(),
                RequestFacts {
                    requester_person: None,
                    ..facts()
                },
                now()
            )
            .unwrap_err(),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::RequestEnrolment),
                reason: DenialReason::NotAMember,
            }),
            "the denial names the action"
        );
    }

    #[test]
    fn asking_again_with_the_same_key_gets_the_same_request_and_records_nothing() {
        let again = run(
            &owner(),
            ask(),
            RequestFacts {
                existing: Some(record()),
                ..facts()
            },
            now(),
        )
        .unwrap();
        assert!(again.events().is_empty(), "nothing new");
        assert_eq!(
            again.into_outcome(),
            EnrolmentRequested {
                request: id(20),
                code: ApprovalCode::parse("ABCD-EFGH").unwrap(),
                expires_at: record().expires_at,
            },
            "the same request"
        );
    }

    #[test]
    fn a_key_in_use_cannot_be_asked_with() {
        let refused = |facts| match run(&owner(), ask(), facts, now()) {
            Err(Outcome::Rejected(rejection)) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        };
        assert_eq!(
            refused(RequestFacts {
                key_held: true,
                ..facts()
            }),
            RequestRejection::KeyInUse,
            "a worker's key"
        );
        assert_eq!(
            refused(RequestFacts {
                existing: Some(RequestRecord {
                    requester: requester("other.tail1234.ts.net"),
                    ..record()
                }),
                ..facts()
            }),
            RequestRejection::KeyInUse,
            "the same key from another node is not a replay"
        );
        assert_eq!(
            refused(RequestFacts {
                existing: Some(RequestRecord {
                    workspace: id(300),
                    ..record()
                }),
                ..facts()
            }),
            RequestRejection::KeyInUse,
            "nor for another workspace"
        );
    }

    fn approve() -> ApproveEnrolment {
        ApproveEnrolment {
            workspace: id(HOME),
            request: id(20),
        }
    }

    fn approving(record: RequestRecord) -> ApproveFacts {
        ApproveFacts {
            request: Some(record),
            key_held: false,
        }
    }

    fn enrolled() -> Enrolled {
        Enrolled {
            worker: id(21),
            workspace: id(HOME),
            name: DisplayName::parse("mbp").unwrap(),
        }
    }

    #[test]
    fn approving_enrols_the_reserved_worker_as_the_owner() {
        let decision = run(&owner(), approve(), approving(record()), now()).unwrap();
        assert_eq!(decision.permit().actor(), Actor::Person(id(1)), "the owner");
        assert_eq!(decision.cause(), Some(id(22)), "caused by the request");
        assert_eq!(
            events(&decision),
            [
                (
                    id::<EnrolmentRequestId>(20).as_uuid(),
                    EnrolmentRequestEvent::Approved.into()
                ),
                (
                    id::<WorkerId>(21).as_uuid(),
                    WorkerEvent::Enrolled {
                        via: EnrolledVia::Request { request: id(20) },
                        key: key(7),
                        name: DisplayName::parse("mbp").unwrap(),
                        machine: machine(),
                        build: Version::new(0, 1, 0),
                    }
                    .into()
                ),
            ],
            "the request is approved and the worker enrolled with its key"
        );
        assert_eq!(decision.into_outcome(), enrolled(), "who it became");
    }

    #[test]
    fn approving_again_records_nothing_and_the_end_states_refuse() {
        let approved = RequestRecord {
            decision: Some(RequestDecision::Approved),
            ..record()
        };
        let again = run(&owner(), approve(), approving(approved), now()).unwrap();
        assert!(again.events().is_empty(), "idempotent");
        assert_eq!(again.into_outcome(), enrolled(), "the same answer");
        let refused = |facts| match run(&owner(), approve(), facts, now()) {
            Err(Outcome::Rejected(rejection)) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        };
        assert_eq!(
            refused(approving(RequestRecord {
                decision: Some(RequestDecision::Denied),
                ..record()
            })),
            DecideRequestRejection::AlreadyDenied,
            "denied"
        );
        assert_eq!(
            refused(approving(RequestRecord {
                expires_at: now(),
                ..record()
            })),
            DecideRequestRejection::Expired,
            "expired at expires_at"
        );
        assert_eq!(
            refused(ApproveFacts {
                request: None,
                key_held: false
            }),
            DecideRequestRejection::NotFound,
            "unknown"
        );
        assert_eq!(
            refused(approving(RequestRecord {
                workspace: id(300),
                ..record()
            })),
            DecideRequestRejection::NotFound,
            "another workspace's"
        );
        assert_eq!(
            refused(ApproveFacts {
                request: Some(record()),
                key_held: true
            }),
            DecideRequestRejection::KeyInUse,
            "a key another worker took meanwhile"
        );
    }

    #[test]
    fn only_the_owner_decides() {
        for (principal, reason) in [
            (visitor(), DenialReason::NotAMember),
            (Principal::Anonymous, DenialReason::TailnetIdentityRequired),
            (
                Principal::Worker {
                    id: id(3),
                    workspace: id(HOME),
                },
                DenialReason::TailnetIdentityRequired,
            ),
        ] {
            assert_eq!(
                run(&principal, approve(), approving(record()), now()).unwrap_err(),
                Outcome::Denied(Denied {
                    action: ActionName::Write(WriteAction::ManageEnrolment),
                    reason,
                }),
                "approve: {principal:?}"
            );
            let deny = DenyEnrolment {
                workspace: id(HOME),
                request: id(20),
            };
            assert_eq!(
                run(&principal, deny, Some(record()), now()).unwrap_err(),
                Outcome::Denied(Denied {
                    action: ActionName::Write(WriteAction::ManageEnrolment),
                    reason,
                }),
                "deny: {principal:?}"
            );
        }
    }

    #[test]
    fn denying_records_once_and_cannot_undo_an_approval() {
        let deny = || DenyEnrolment {
            workspace: id(HOME),
            request: id(20),
        };
        let decision = run(&owner(), deny(), Some(record()), now()).unwrap();
        assert_eq!(
            events(&decision),
            [(
                id::<EnrolmentRequestId>(20).as_uuid(),
                EnrolmentRequestEvent::Denied.into()
            )],
            "denied"
        );
        for ended in [
            RequestRecord {
                decision: Some(RequestDecision::Denied),
                ..record()
            },
            RequestRecord {
                expires_at: now(),
                ..record()
            },
        ] {
            let again = run(&owner(), deny(), Some(ended.clone()), now()).unwrap();
            assert!(again.events().is_empty(), "already over: {ended:?}");
        }
        let refused = |facts| match run(&owner(), deny(), facts, now()) {
            Err(Outcome::Rejected(rejection)) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        };
        assert_eq!(
            refused(Some(RequestRecord {
                decision: Some(RequestDecision::Approved),
                ..record()
            })),
            DecideRequestRejection::AlreadyApproved,
            "approved stays approved"
        );
        assert_eq!(refused(None), DecideRequestRejection::NotFound, "unknown");
        assert_eq!(
            refused(Some(RequestRecord {
                workspace: id(300),
                ..record()
            })),
            DecideRequestRejection::NotFound,
            "another workspace's"
        );
    }

    #[test]
    fn a_request_stands_pending_until_decided_or_expired() {
        let at = |record: &RequestRecord, seconds| {
            record.standing(now().checked_add(time::Duration::seconds(seconds)).unwrap())
        };
        assert_eq!(at(&record(), 599), RequestStanding::Pending, "pending");
        assert_eq!(at(&record(), 600), RequestStanding::Expired, "expired");
        let approved = RequestRecord {
            decision: Some(RequestDecision::Approved),
            ..record()
        };
        assert_eq!(at(&approved, 3600), RequestStanding::Approved, "for good");
        let denied = RequestRecord {
            decision: Some(RequestDecision::Denied),
            ..record()
        };
        assert_eq!(at(&denied, 0), RequestStanding::Denied, "denied");
    }
}
