//! Queries and the read models they return. Read models are the only shapes the web app sees
//! (generated to TypeScript): views, never raw events.
//!
//! A query passes the same [`crate::policy::authorize`] as a command (I4). abc-store implements
//! `Fetchable` for each query, and that trait's `fetch` takes an `&Authorized<Q>`, so an
//! implementation cannot be invoked without the check.

use schemars::JsonSchema;
use serde::Serialize;
use ts_rs::TS;

use crate::actor::Role;
use crate::command::{RequestRecord, RequestStanding};
use crate::credential::ApprovalCode;
use crate::event::Position;
use crate::id::{EnrolmentRequestId, EnrolmentTokenId, PersonId, WorkerId, WorkspaceId};
use crate::identity::{
    Arch, DisplayName, MachineLabel, Os, Requester, TailnetLogin, TailnetNode, WorkspaceName,
};
use crate::policy::{Capability, DenialReason, NoProof, Proof, ReadAction, Request, Scope};
use crate::time::Timestamp;

/// A request that only reads. Most load no facts and carry no proof; one that checks a
/// capability (the enrolment request's status) loads the facts it proves against, as a command
/// does.
pub trait Query: Request<Action = ReadAction> {
    /// What it returns.
    type Answer;
}

macro_rules! query {
    ($name:ty, $action:expr, $answer:ty, |$s:ident| $scope:expr) => {
        impl Request for $name {
            type Action = ReadAction;
            const ACTION: ReadAction = $action;
            type Facts = ();
            type Proof = NoProof;
            fn scope(&self) -> Scope {
                let $s = self;
                $scope
            }
            fn prove(&self, (): &(), _now: Timestamp) -> Result<NoProof, DenialReason> {
                Ok(NoProof)
            }
        }
        impl Query for $name {
            type Answer = $answer;
        }
    };
}

/// Who am I, and what can I do here? The web app's first call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WhoAmI;
query!(WhoAmI, ReadAction::ViewSession, SessionView, |_s| {
    Scope::Installation
});

/// The answer to [`WhoAmI`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, TS, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionView {
    /// The request carried no tailnet identity (a tagged device, or a non-browser client).
    NoIdentity,
    /// The server has no owner yet. The web app shows the setup-code form.
    Unclaimed {
        /// The login that will own the server.
        login: TailnetLogin,
    },
    /// The server is claimed and this login is not a member.
    NotAMember {
        /// The visitor's login.
        login: TailnetLogin,
    },
    /// A member.
    Member {
        /// The person.
        person: PersonId,
        /// Their name.
        display_name: DisplayName,
        /// Every workspace they belong to.
        workspaces: Vec<WorkspaceSummary>,
    },
}

/// One of a person's workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, TS, JsonSchema)]
pub struct WorkspaceSummary {
    /// The workspace.
    pub id: WorkspaceId,
    /// Its name.
    pub name: WorkspaceName,
    /// The person's role in it.
    pub role: Role,
}

/// Every machine in a workspace, with the log position the snapshot reflects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListMachines {
    /// The workspace.
    pub workspace: WorkspaceId,
}
query!(
    ListMachines,
    ReadAction::ViewWorkspace,
    MachinesSnapshot,
    |s| Scope::Workspace(s.workspace)
);

/// The machines changed after `after`. The SSE stream's delta read, and its resume point: the SSE
/// `id:` is the head, so a reconnect with `Last-Event-ID: N` asks for exactly this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachinesSince {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The position the subscriber has already seen.
    pub after: Position,
}
query!(
    MachinesSince,
    ReadAction::ViewWorkspace,
    MachinesSnapshot,
    |s| Scope::Workspace(s.workspace)
);

/// Machine cards and the head position they reflect. Both are read in one REPEATABLE READ
/// transaction, so `head` is exact: nothing at or below it is missing from `machines`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct MachinesSnapshot {
    /// The log head the cards reflect.
    pub head: Position,
    /// The cards: all of them for `ListMachines`, only those changed after `after` for `MachinesSince`.
    pub machines: Vec<MachineCard>,
    /// Enrolment requests: the pending ones for `ListMachines`, every one changed after `after`
    /// for `MachinesSince`, so a decided request leaves the screen.
    pub requests: Vec<RequestCard>,
}

/// One enrolment request as the Machines notice and the Approve page show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct RequestCard {
    /// The request.
    pub id: EnrolmentRequestId,
    /// The machine's hostname, which becomes the worker's name.
    pub hostname: DisplayName,
    /// Its operating system.
    pub os: Os,
    /// Its architecture.
    pub arch: Arch,
    /// The abc release it runs.
    pub build: String,
    /// The tailnet person who asked.
    pub login: TailnetLogin,
    /// The node they asked from.
    pub node: TailnetNode,
    /// The code the machine's terminal shows.
    pub code: ApprovalCode,
    /// When it was asked.
    pub requested_at: Timestamp,
    /// When it can no longer be approved.
    pub expires_at: Timestamp,
    /// Where it stands, derived when read.
    pub status: RequestStatus,
    /// The log position that last changed it.
    pub changed_at: Position,
}

/// Where a request stands, for the web app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequestStatus {
    /// Waiting for the owner.
    Pending,
    /// Approved: this worker exists.
    Approved {
        /// The worker.
        worker: WorkerId,
    },
    /// Denied.
    Denied,
    /// Not decided in time.
    Expired,
}

impl RequestStatus {
    /// The status of `record` at `now`, by the same rule the commands decide with.
    #[must_use]
    pub fn of(record: &RequestRecord, now: Timestamp) -> Self {
        match record.standing(now) {
            RequestStanding::Pending => Self::Pending,
            RequestStanding::Approved => Self::Approved {
                worker: record.worker,
            },
            RequestStanding::Denied => Self::Denied,
            RequestStanding::Expired => Self::Expired,
        }
    }
}

/// The Approve page: one request of a workspace. `None` when it has no such request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewEnrolmentRequest {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The request.
    pub request: EnrolmentRequestId,
}
query!(
    ViewEnrolmentRequest,
    ReadAction::ViewWorkspace,
    Option<RequestCard>,
    |s| Scope::Workspace(s.workspace)
);

/// A waiting machine asks where its request stands, from the login and node that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolmentRequestStatus {
    /// The request.
    pub request: EnrolmentRequestId,
    /// Who asks now, built like the request's own requester.
    pub requester: Requester,
}

/// The request was made by the caller's login from the caller's node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicantProof {
    record: RequestRecord,
}

impl ApplicantProof {
    /// Where the request stands at `now`, as the machine learns it.
    #[must_use]
    pub fn state(&self, now: Timestamp) -> RequestState {
        let record = &self.record;
        match record.standing(now) {
            RequestStanding::Pending => RequestState::Pending {
                code: record.code.clone(),
                expires_at: record.expires_at,
            },
            RequestStanding::Approved => RequestState::Approved {
                worker: record.worker,
                workspace: record.workspace,
                name: record.machine.hostname.clone(),
            },
            RequestStanding::Denied => RequestState::Denied,
            RequestStanding::Expired => RequestState::Expired,
        }
    }
}

impl Proof for ApplicantProof {
    fn capability(&self) -> Capability {
        Capability::EnrolmentApplicant {
            workspace: self.record.workspace,
        }
    }
}

impl Request for EnrolmentRequestStatus {
    type Action = ReadAction;
    const ACTION: ReadAction = ReadAction::ViewOwnEnrolmentRequest;
    type Facts = Option<RequestRecord>;
    type Proof = ApplicantProof;

    fn scope(&self) -> Scope {
        Scope::CapabilityWorkspace
    }

    /// Another login or node gets the same answer as a request that does not exist.
    fn prove(
        &self,
        facts: &Option<RequestRecord>,
        _now: Timestamp,
    ) -> Result<ApplicantProof, DenialReason> {
        facts
            .as_ref()
            .filter(|record| record.id == self.request && record.requester == self.requester)
            .map(|record| ApplicantProof {
                record: record.clone(),
            })
            .ok_or(DenialReason::EnrolmentRequestUnknown)
    }
}

impl Query for EnrolmentRequestStatus {
    type Answer = RequestState;
}

/// Where an enrolment request stands, as the waiting machine learns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, TS, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RequestState {
    /// Waiting for the owner.
    Pending {
        /// The code to compare with the Approve page.
        code: ApprovalCode,
        /// When it stops waiting.
        expires_at: Timestamp,
    },
    /// Approved: the machine's key is now this worker's.
    Approved {
        /// The worker.
        worker: WorkerId,
        /// Its workspace.
        workspace: WorkspaceId,
        /// Its display name.
        name: DisplayName,
    },
    /// The owner denied it.
    Denied,
    /// Nobody decided in time.
    Expired,
}

/// One worker as the Machines screen shows it. Everything here, online-ness included, is derived
/// from the log through the `worker` view, so one SSE mechanism (a cursor over positions) keeps the
/// screen current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct MachineCard {
    /// The worker.
    pub worker: WorkerId,
    /// Its display name.
    pub name: DisplayName,
    /// Its hostname at enrolment.
    pub hostname: DisplayName,
    /// Its OS.
    pub os: Os,
    /// Its architecture.
    pub arch: Arch,
    /// The release it last connected with.
    pub build: String,
    /// Whether it has an open session: its last `session_started` has no matching `session_ended`.
    /// After a server crash the boot sweep ends stale sessions before any socket is accepted, so
    /// this is never stale for longer than the server is down.
    pub online: bool,
    /// When it enrolled.
    pub enrolled_at: Timestamp,
    /// The later of its last session start and its last session end, if it ever connected.
    pub last_seen_at: Option<Timestamp>,
    /// Whether the owner revoked it. Revoked workers stay listed, greyed out.
    pub revoked: bool,
    /// The position of the last event that changed this card.
    pub changed_at: Position,
}

/// The workspace's enrolment tokens (never their secrets), newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListEnrolmentTokens {
    /// The workspace.
    pub workspace: WorkspaceId,
}
query!(
    ListEnrolmentTokens,
    ReadAction::ViewWorkspace,
    Vec<TokenSummary>,
    |s| Scope::Workspace(s.workspace)
);

/// A token as the owner sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct TokenSummary {
    /// The token.
    pub id: EnrolmentTokenId,
    /// The owner's note.
    pub label: Option<MachineLabel>,
    /// When it was issued.
    pub issued_at: Timestamp,
    /// When it stops working.
    pub expires_at: Timestamp,
    /// Its status at the store's clock when read.
    pub status: TokenStatus,
}

/// A token's status. Derived when read from `revoked_at`, `redeemed_at` and `expires_at`, never
/// stored, so it never goes stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TokenStatus {
    /// Redeemable.
    Open,
    /// Past `expires_at`, unredeemed.
    Expired,
    /// Revoked by the owner.
    Revoked,
    /// Redeemed by a worker.
    Redeemed {
        /// The worker it created.
        worker: WorkerId,
    },
}

impl TokenStatus {
    /// A token's status at `now`: revoked and redeemed are final; otherwise it is open until
    /// `expires_at`.
    #[must_use]
    pub fn at(
        revoked: bool,
        redeemed_by: Option<WorkerId>,
        expires_at: Timestamp,
        now: Timestamp,
    ) -> Self {
        match (revoked, redeemed_by) {
            (true, _) => Self::Revoked,
            (false, Some(worker)) => Self::Redeemed { worker },
            (false, None) if now >= expires_at => Self::Expired,
            (false, None) => Self::Open,
        }
    }
}

/// A connected worker reading its own identity: at the WebSocket upgrade, and again each time the
/// log head moves, to notice revocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSelf;
query!(
    WorkerSelf,
    ReadAction::ViewSelfAsWorker,
    WorkerIdentity,
    |_s| Scope::OwnWorkspace
);

/// The answer to [`WorkerSelf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerIdentity {
    /// The worker.
    pub worker: WorkerId,
    /// Its workspace.
    pub workspace: WorkspaceId,
}

/// Whether the server has an owner yet: what the server logs at start. Server only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallationStatus;
query!(
    InstallationStatus,
    ReadAction::ViewAsServer,
    InstallationState,
    |_s| Scope::Installation
);

/// The answer to [`InstallationStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallationState {
    /// No one has claimed it: `abc server setup-code` gives the operator a code.
    Unclaimed,
    /// It has an owner.
    Claimed,
}

/// Every worker with an open session, across workspaces: the boot sweep's input. Server only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenSessions;
query!(
    OpenSessions,
    ReadAction::ViewAsServer,
    Vec<crate::command::OpenSession>,
    |_s| Scope::Installation
);

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;
    use crate::id::FromUuid;

    #[test]
    fn a_tokens_status_is_derived_at_read_time() {
        let expires = Timestamp::from_utc(datetime!(2026-01-01 01:00:00 UTC));
        let before = Timestamp::from_utc(datetime!(2026-01-01 00:59:59 UTC));
        let worker = WorkerId::from_uuid(Uuid::from_u128(1));
        assert_eq!(
            TokenStatus::at(false, None, expires, before),
            TokenStatus::Open,
            "open"
        );
        assert_eq!(
            TokenStatus::at(false, None, expires, expires),
            TokenStatus::Expired,
            "expired at expires_at"
        );
        assert_eq!(
            TokenStatus::at(true, None, expires, before),
            TokenStatus::Revoked,
            "revoked"
        );
        assert_eq!(
            TokenStatus::at(false, Some(worker), expires, expires),
            TokenStatus::Redeemed { worker },
            "redeemed stays redeemed after expiry"
        );
        assert_eq!(
            serde_json::to_string(&TokenStatus::Redeemed { worker }).unwrap(),
            format!(r#"{{"status":"redeemed","worker":"{}"}}"#, worker.as_uuid()),
            "tagged by status on the wire"
        );
    }

    fn record() -> RequestRecord {
        use crate::command::REQUEST_TTL;
        use crate::credential::Digest;
        RequestRecord {
            id: EnrolmentRequestId::from_uuid(Uuid::from_u128(20)),
            workspace: WorkspaceId::from_uuid(Uuid::from_u128(100)),
            worker: WorkerId::from_uuid(Uuid::from_u128(21)),
            key: Digest::from_bytes([7; 32]),
            requester: requester("alice@example.com", "mbp.ts.net"),
            machine: crate::identity::MachineInfo {
                hostname: DisplayName::parse("mbp").unwrap(),
                os: Os::Macos,
                arch: Arch::Aarch64,
            },
            build: semver::Version::new(0, 1, 0),
            code: ApprovalCode::parse("ABCD-EFGH").unwrap(),
            expires_at: at().checked_add(REQUEST_TTL).unwrap(),
            decision: None,
            requested_event: crate::id::EventId::from_uuid(Uuid::from_u128(22)),
        }
    }

    fn requester(login: &str, node: &str) -> Requester {
        Requester {
            login: TailnetLogin::parse(login).unwrap(),
            node: TailnetNode::parse(node).unwrap(),
        }
    }

    fn at() -> Timestamp {
        Timestamp::from_utc(datetime!(2026-01-01 00:00:00 UTC))
    }

    #[test]
    fn only_the_requesting_login_and_node_learn_where_a_request_stands() {
        let ask = |login: &str, node: &str| EnrolmentRequestStatus {
            request: record().id,
            requester: requester(login, node),
        };
        let proof = ask("alice@example.com", "mbp.ts.net")
            .prove(&Some(record()), at())
            .unwrap();
        assert_eq!(
            proof.capability(),
            Capability::EnrolmentApplicant {
                workspace: record().workspace
            },
            "the request's workspace"
        );
        for (who, facts) in [
            (ask("alice@example.com", "other.ts.net"), Some(record())),
            (ask("bob@example.com", "mbp.ts.net"), Some(record())),
            (ask("alice@example.com", "mbp.ts.net"), None),
            (
                EnrolmentRequestStatus {
                    request: EnrolmentRequestId::from_uuid(Uuid::from_u128(99)),
                    requester: requester("alice@example.com", "mbp.ts.net"),
                },
                Some(record()),
            ),
        ] {
            assert_eq!(
                who.prove(&facts, at()),
                Err(DenialReason::EnrolmentRequestUnknown),
                "{who:?} sees nothing"
            );
        }
        assert_eq!(
            proof.state(at()),
            RequestState::Pending {
                code: record().code,
                expires_at: record().expires_at
            },
            "pending, with the code"
        );
        let later = record().expires_at;
        assert_eq!(proof.state(later), RequestState::Expired, "then expired");
        let approved = ApplicantProof {
            record: RequestRecord {
                decision: Some(crate::command::RequestDecision::Approved),
                ..record()
            },
        };
        assert_eq!(
            approved.state(later),
            RequestState::Approved {
                worker: record().worker,
                workspace: record().workspace,
                name: DisplayName::parse("mbp").unwrap(),
            },
            "approved, for good"
        );
        let denied = ApplicantProof {
            record: RequestRecord {
                decision: Some(crate::command::RequestDecision::Denied),
                ..record()
            },
        };
        assert_eq!(denied.state(at()), RequestState::Denied, "denied");
        assert_eq!(
            serde_json::to_value(RequestState::Denied).unwrap(),
            serde_json::json!({"state": "denied"}),
            "tagged by state on the wire"
        );
    }

    #[test]
    fn a_request_cards_status_follows_the_record() {
        assert_eq!(
            RequestStatus::of(&record(), at()),
            RequestStatus::Pending,
            "pending"
        );
        assert_eq!(
            RequestStatus::of(&record(), record().expires_at),
            RequestStatus::Expired,
            "expired"
        );
        let approved = RequestRecord {
            decision: Some(crate::command::RequestDecision::Approved),
            ..record()
        };
        assert_eq!(
            RequestStatus::of(&approved, at()),
            RequestStatus::Approved {
                worker: record().worker
            },
            "approved, naming the worker"
        );
        let denied = RequestRecord {
            decision: Some(crate::command::RequestDecision::Denied),
            ..record()
        };
        assert_eq!(
            RequestStatus::of(&denied, at()),
            RequestStatus::Denied,
            "denied"
        );
    }
}
