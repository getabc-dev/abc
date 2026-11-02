//! The one permission check (I4) and the only constructor of [`Authorized`] (D10).
//!
//! [`authorize`] does two pure things, in order:
//!
//! 1. **Prove.** If the request carries a bearer capability (the owner setup code or an enrolment
//!    token), [`Request::prove`] checks it against the facts the store loaded in the same
//!    transaction. The result is a typed proof such as `TokenProof { worker, replay }`, or a
//!    [`DenialReason`] such as `EnrolmentTokenExpired`.
//! 2. **Permit.** The single policy matrix, [`PolicyAction::permit`], judges (principal, role in
//!    scope, capability) for the request's action. It has one exhaustive match for writes and one
//!    for reads, and this file is the whole of abc's access policy.
//!
//! `Authorized<R>` keeps the request, the permit and the proof. It is not `Clone`, `Serialize` or
//! `Deserialize`, and its fields are private, so it cannot be copied, smuggled through serde or
//! built by hand. `decide` consumes it.

use thiserror::Error;

use crate::actor::{Actor, Principal, Role};
use crate::id::{PersonId, WorkerId, WorkspaceId};
use crate::time::Timestamp;

/// Something a principal asks abc to do or show. Implemented by every command and query.
pub trait Request: Sized {
    /// Whether this is a write or a read. That picks which half of the matrix judges it.
    type Action: PolicyAction;
    /// The action, for the matrix and for error messages.
    const ACTION: Self::Action;
    /// Plain records the store loads in the request's transaction: the request's view of the
    /// world when it is checked and decided. Queries usually load nothing (`()`).
    type Facts;
    /// What a bearer capability in this request proves. Requests without one use [`NoProof`].
    type Proof: Proof;

    /// Where the request acts.
    fn scope(&self) -> Scope;

    /// Verifies the request's bearer capability against `facts` at `now`.
    ///
    /// # Errors
    ///
    /// Returns the specific reason a presented capability fails: unknown, expired, revoked, used,
    /// mismatched or already claimed.
    fn prove(&self, facts: &Self::Facts, now: Timestamp) -> Result<Self::Proof, DenialReason>;
}

/// Where a request acts. The matrix resolves it to a workspace or to the installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The server as a whole: the session view, the claim.
    Installation,
    /// A named workspace.
    Workspace(WorkspaceId),
    /// The principal's own workspace (a worker acting for itself).
    OwnWorkspace,
    /// The workspace the request's capability names (the enrolment token's workspace).
    CapabilityWorkspace,
}

/// What a proof grants, summarized for the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// The request carries no bearer capability.
    None,
    /// A valid owner setup code for an unclaimed server, with the reserved owner and workspace.
    OwnerSetup {
        /// The person the claim will create.
        owner: PersonId,
        /// The workspace the claim will create.
        workspace: WorkspaceId,
    },
    /// The installation's setup-code reservation (only `IssueSetupCode` proves this).
    ReserveInstallation {
        /// The workspace the claim will create, which installation events belong to.
        workspace: WorkspaceId,
    },
    /// The target worker exists, in this workspace (loaded in the same transaction).
    WorkerHome {
        /// The worker's workspace.
        workspace: WorkspaceId,
    },
    /// A redeemable enrolment token (or one already redeemed by the same key).
    EnrolmentToken {
        /// The worker the token reserved.
        worker: WorkerId,
        /// The token's workspace.
        workspace: WorkspaceId,
    },
}

/// A proof's summary for the matrix.
pub trait Proof {
    /// What this proof grants.
    fn capability(&self) -> Capability;
}

/// The proof of a request that carries no capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoProof;

impl Proof for NoProof {
    fn capability(&self) -> Capability {
        Capability::None
    }
}

/// Everything the matrix may consult.
#[derive(Debug)]
pub struct Standing<'a> {
    /// Who is asking.
    pub principal: &'a Principal,
    /// Where.
    pub scope: Scope,
    /// What their capability grants.
    pub capability: Capability,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::WriteAction {}
    impl Sealed for super::ReadAction {}
}

/// One half of the policy matrix. Sealed: the two impls below are the whole policy.
pub trait PolicyAction: sealed::Sealed + Copy + Into<ActionName> {
    /// What a successful check grants.
    type Permit: std::fmt::Debug;
    /// The matrix.
    ///
    /// # Errors
    ///
    /// Returns why the standing does not permit this action.
    fn permit(self, standing: &Standing<'_>) -> Result<Self::Permit, DenialReason>;
}

/// Everything that changes state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAction {
    /// Issue a setup code for an unclaimed server (the host operator, through `Server`).
    IssueSetupCode,
    /// Claim an unclaimed server.
    ClaimServer,
    /// Issue, revoke and list enrolment tokens.
    ManageEnrolment,
    /// Revoke workers.
    ManageWorkers,
    /// Redeem an enrolment token.
    EnrolWorker,
    /// A worker recording its own session start.
    ActAsWorker,
    /// The server recording what it observed (a session's end).
    RecordAsServer,
}

/// Everything that only reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAction {
    /// Who am I, and is the server claimed? Open to every principal.
    ViewSession,
    /// See a workspace's machines and tokens.
    ViewWorkspace,
    /// A worker reading its own identity, at connect and while connected.
    ViewSelfAsWorker,
    /// The server reading installation-wide state it acts on (the open sessions at boot).
    ViewAsServer,
}

/// An action's name, for errors and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionName {
    /// A write.
    Write(WriteAction),
    /// A read.
    Read(ReadAction),
}

impl From<WriteAction> for ActionName {
    fn from(action: WriteAction) -> Self {
        Self::Write(action)
    }
}

impl From<ReadAction> for ActionName {
    fn from(action: ReadAction) -> Self {
        Self::Read(action)
    }
}

/// What a write is allowed to do: who it is recorded as, and in which workspace. Every event of
/// the decision is stamped with both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePermit {
    actor: Actor,
    workspace: WorkspaceId,
}

impl WritePermit {
    /// The actor every event of this write records.
    #[must_use]
    pub const fn actor(&self) -> Actor {
        self.actor
    }

    /// The workspace every event of this write belongs to.
    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }
}

/// What a read is allowed to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPermit {
    /// The installation-level session view.
    Installation,
    /// One workspace.
    Workspace(WorkspaceId),
}

/// The write permit for `actor` in `workspace`.
const fn write(actor: Actor, workspace: WorkspaceId) -> WritePermit {
    WritePermit { actor, workspace }
}

/// The workspace a workspace-scoped request names, if it names one.
const fn named_workspace(scope: Scope) -> Option<WorkspaceId> {
    match scope {
        Scope::Workspace(workspace) => Some(workspace),
        Scope::Installation | Scope::OwnWorkspace | Scope::CapabilityWorkspace => None,
    }
}

/// Who may act on a workspace's tokens and workers: an owner of the workspace the request names.
fn owner_of(standing: &Standing<'_>) -> Result<WritePermit, DenialReason> {
    match standing.principal {
        Principal::Person { id, .. } => named_workspace(standing.scope)
            .filter(|&workspace| standing.principal.role_in(workspace) == Some(Role::Owner))
            .map(|workspace| write(Actor::Person(*id), workspace))
            .ok_or(DenialReason::NotAMember),
        Principal::Visitor { .. } => Err(DenialReason::NotAMember),
        Principal::Worker { .. } | Principal::Anonymous | Principal::Server => {
            Err(DenialReason::TailnetIdentityRequired)
        }
    }
}

impl PolicyAction for WriteAction {
    type Permit = WritePermit;

    fn permit(self, standing: &Standing<'_>) -> Result<WritePermit, DenialReason> {
        let principal = standing.principal;
        match self {
            Self::IssueSetupCode => match (principal, standing.capability) {
                (Principal::Server, Capability::ReserveInstallation { workspace }) => {
                    Ok(write(Actor::Server, workspace))
                }
                _ => Err(DenialReason::ServerOnly),
            },
            Self::RecordAsServer => match (principal, standing.capability) {
                (Principal::Server, Capability::WorkerHome { workspace }) => {
                    Ok(write(Actor::Server, workspace))
                }
                _ => Err(DenialReason::ServerOnly),
            },
            Self::ClaimServer => match principal {
                Principal::Visitor { .. } => match standing.capability {
                    Capability::OwnerSetup { owner, workspace } => {
                        Ok(write(Actor::Person(owner), workspace))
                    }
                    Capability::None
                    | Capability::ReserveInstallation { .. }
                    | Capability::WorkerHome { .. }
                    | Capability::EnrolmentToken { .. } => Err(DenialReason::SetupCodeRequired),
                },
                Principal::Person { .. } => Err(DenialReason::ServerAlreadyClaimed),
                Principal::Worker { .. } | Principal::Anonymous | Principal::Server => {
                    Err(DenialReason::TailnetIdentityRequired)
                }
            },
            Self::ManageEnrolment | Self::ManageWorkers => owner_of(standing),
            Self::EnrolWorker => match standing.capability {
                Capability::EnrolmentToken { worker, workspace } => {
                    Ok(write(Actor::Worker(worker), workspace))
                }
                Capability::None
                | Capability::OwnerSetup { .. }
                | Capability::ReserveInstallation { .. }
                | Capability::WorkerHome { .. } => Err(DenialReason::EnrolmentTokenRequired),
            },
            Self::ActAsWorker => match (principal, standing.scope) {
                (Principal::Worker { id, workspace }, Scope::OwnWorkspace) => {
                    Ok(write(Actor::Worker(*id), *workspace))
                }
                _ => Err(DenialReason::WorkerOnly),
            },
        }
    }
}

impl PolicyAction for ReadAction {
    type Permit = ReadPermit;

    fn permit(self, standing: &Standing<'_>) -> Result<ReadPermit, DenialReason> {
        let principal = standing.principal;
        match self {
            Self::ViewSession => Ok(ReadPermit::Installation),
            Self::ViewWorkspace => match principal {
                Principal::Person { .. } | Principal::Visitor { .. } => {
                    named_workspace(standing.scope)
                        .filter(|&workspace| principal.role_in(workspace).is_some())
                        .map(ReadPermit::Workspace)
                        .ok_or(DenialReason::NotAMember)
                }
                Principal::Worker { .. } | Principal::Anonymous | Principal::Server => {
                    Err(DenialReason::TailnetIdentityRequired)
                }
            },
            Self::ViewSelfAsWorker => match (principal, standing.scope) {
                (Principal::Worker { workspace, .. }, Scope::OwnWorkspace) => {
                    Ok(ReadPermit::Workspace(*workspace))
                }
                _ => Err(DenialReason::WorkerOnly),
            },
            Self::ViewAsServer => match principal {
                Principal::Server => Ok(ReadPermit::Installation),
                Principal::Person { .. }
                | Principal::Worker { .. }
                | Principal::Visitor { .. }
                | Principal::Anonymous => Err(DenialReason::ServerOnly),
            },
        }
    }
}

/// Proof that `request` passed the one permission check. Only [`authorize`] builds it.
#[derive(Debug)]
#[must_use = "an authorization that is not used is a check without an action"]
pub struct Authorized<R: Request> {
    request: R,
    permit: <R::Action as PolicyAction>::Permit,
    proof: R::Proof,
}

impl<R: Request> Authorized<R> {
    /// The authorized request.
    pub const fn request(&self) -> &R {
        &self.request
    }

    /// What the check granted.
    pub const fn permit(&self) -> &<R::Action as PolicyAction>::Permit {
        &self.permit
    }

    /// What the request's capability proved.
    pub const fn proof(&self) -> &R::Proof {
        &self.proof
    }

    /// Consumes the authorization and returns the request. Used by `decide` once its
    /// `DecisionBuilder` has copied the permit.
    pub fn into_request(self) -> R {
        self.request
    }
}

/// **The** permission check. Every request, read or write, passes through here, and nothing else
/// can produce an [`Authorized`].
///
/// The store calls it inside the request's transaction, after authenticating `principal` and
/// loading `facts` in that same transaction. That is what makes time of check equal time of use.
///
/// # Errors
///
/// Returns [`Denied`] with the action and the reason.
pub fn authorize<R: Request>(
    principal: &Principal,
    request: R,
    facts: &R::Facts,
    now: Timestamp,
) -> Result<Authorized<R>, Denied> {
    let action = R::ACTION;
    let denied = |reason| Denied {
        action: action.into(),
        reason,
    };
    let proof = request.prove(facts, now).map_err(denied)?;
    let standing = Standing {
        principal,
        scope: request.scope(),
        capability: proof.capability(),
    };
    let permit = action.permit(&standing).map_err(denied)?;
    Ok(Authorized {
        request,
        permit,
        proof,
    })
}

/// A request the policy refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{action:?} denied: {reason}")]
pub struct Denied {
    /// What was asked.
    pub action: ActionName,
    /// Why it was refused.
    pub reason: DenialReason,
}

/// Why the policy refused. abc-server maps each reason to one stable `ApiCode`, exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DenialReason {
    /// The request needs a tailnet person, and the request carried no identity (for example, it
    /// came from a tagged device).
    #[error("this needs a signed-in tailnet person")]
    TailnetIdentityRequired,
    /// The person is not a member of the workspace.
    #[error("you are not a member of this workspace")]
    NotAMember,
    /// Only a worker acting for itself may do this.
    #[error("only a worker can do this")]
    WorkerOnly,
    /// The server already has an owner.
    #[error("this server has already been claimed")]
    ServerAlreadyClaimed,
    /// No setup code was presented.
    #[error("a setup code is required")]
    SetupCodeRequired,
    /// The setup code does not match.
    #[error("the setup code does not match")]
    SetupCodeMismatch,
    /// The setup code is older than an hour. Run `abc server setup-code` again.
    #[error("the setup code expired; run `abc server setup-code` on the server for a new one")]
    SetupCodeExpired,
    /// Only the server itself may do this.
    #[error("only the server itself can do this")]
    ServerOnly,
    /// The worker named by the request does not exist.
    #[error("no such worker")]
    UnknownWorker,
    /// No enrolment token was presented.
    #[error("an enrolment token is required")]
    EnrolmentTokenRequired,
    /// No token has this digest.
    #[error("unknown enrolment token")]
    EnrolmentTokenUnknown,
    /// The token expired.
    #[error("the enrolment token expired")]
    EnrolmentTokenExpired,
    /// The owner revoked the token.
    #[error("the enrolment token was revoked")]
    EnrolmentTokenRevoked,
    /// Another key already redeemed the token.
    #[error("the enrolment token was already used")]
    EnrolmentTokenUsed,
    /// Another enrolled worker already holds the presented key.
    #[error("another worker already uses this key; generate a new one")]
    WorkerKeyInUse,
    /// The worker was revoked.
    #[error("this worker was revoked")]
    WorkerRevoked,
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::actor::Membership;
    use crate::id::FromUuid;
    use crate::identity::TailnetLogin;

    fn id<T: FromUuid>(n: u128) -> T {
        T::from_uuid(Uuid::from_u128(n))
    }

    const HOME: u128 = 100;
    const OTHER: u128 = 200;

    fn owner() -> Principal {
        Principal::Person {
            id: id(1),
            memberships: vec![Membership {
                workspace: id(HOME),
                role: Role::Owner,
            }],
        }
    }

    fn visitor() -> Principal {
        Principal::Visitor {
            login: TailnetLogin::parse("bob@example.com").unwrap(),
        }
    }

    fn worker() -> Principal {
        Principal::Worker {
            id: id(3),
            workspace: id(HOME),
        }
    }

    /// Every principal, with a name for messages.
    fn everyone() -> Vec<(&'static str, Principal)> {
        vec![
            ("owner", owner()),
            ("visitor", visitor()),
            ("worker", worker()),
            ("anonymous", Principal::Anonymous),
            ("server", Principal::Server),
        ]
    }

    /// Every capability.
    fn capabilities() -> Vec<Capability> {
        vec![
            Capability::None,
            Capability::OwnerSetup {
                owner: id(1),
                workspace: id(HOME),
            },
            Capability::ReserveInstallation {
                workspace: id(HOME),
            },
            Capability::WorkerHome {
                workspace: id(HOME),
            },
            Capability::EnrolmentToken {
                worker: id(3),
                workspace: id(HOME),
            },
        ]
    }

    /// Every scope.
    fn scopes() -> Vec<Scope> {
        vec![
            Scope::Installation,
            Scope::Workspace(id(HOME)),
            Scope::Workspace(id(OTHER)),
            Scope::OwnWorkspace,
            Scope::CapabilityWorkspace,
        ]
    }

    fn standing(principal: &Principal, scope: Scope, capability: Capability) -> Standing<'_> {
        Standing {
            principal,
            scope,
            capability,
        }
    }

    /// Runs `action` over every principal, scope and capability and checks each verdict against
    /// `expected`, an independent statement of the rule.
    fn check<A>(
        action: A,
        expected: impl Fn(&str, Scope, Capability) -> Result<A::Permit, DenialReason>,
    ) where
        A: PolicyAction + std::fmt::Debug,
        A::Permit: PartialEq,
    {
        for (name, principal) in everyone() {
            for scope in scopes() {
                for capability in capabilities() {
                    assert_eq!(
                        action.permit(&standing(&principal, scope, capability)),
                        expected(name, scope, capability),
                        "{action:?} for {name} in {scope:?} with {capability:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn only_the_server_issues_setup_codes() {
        check(WriteAction::IssueSetupCode, |who, _, capability| {
            match (who, capability) {
                ("server", Capability::ReserveInstallation { workspace }) => {
                    Ok(write(Actor::Server, workspace))
                }
                _ => Err(DenialReason::ServerOnly),
            }
        });
    }

    #[test]
    fn only_the_server_records_what_it_observed() {
        check(WriteAction::RecordAsServer, |who, _, capability| {
            match (who, capability) {
                ("server", Capability::WorkerHome { workspace }) => {
                    Ok(write(Actor::Server, workspace))
                }
                _ => Err(DenialReason::ServerOnly),
            }
        });
    }

    #[test]
    fn a_visitor_with_a_setup_code_claims_as_the_reserved_person() {
        check(WriteAction::ClaimServer, |who, _, capability| match who {
            "visitor" => {
                if let Capability::OwnerSetup { owner, workspace } = capability {
                    Ok(write(Actor::Person(owner), workspace))
                } else {
                    Err(DenialReason::SetupCodeRequired)
                }
            }
            "owner" => Err(DenialReason::ServerAlreadyClaimed),
            _ => Err(DenialReason::TailnetIdentityRequired),
        });
    }

    #[test]
    fn only_an_owner_of_the_named_workspace_manages_it() {
        for action in [WriteAction::ManageEnrolment, WriteAction::ManageWorkers] {
            check(action, |who, scope, _| match who {
                "owner" if scope == Scope::Workspace(id(HOME)) => {
                    Ok(write(Actor::Person(id(1)), id(HOME)))
                }
                "owner" | "visitor" => Err(DenialReason::NotAMember),
                _ => Err(DenialReason::TailnetIdentityRequired),
            });
        }
    }

    #[test]
    fn an_enrolment_token_enrols_as_its_reserved_worker_whoever_presents_it() {
        check(WriteAction::EnrolWorker, |_, _, capability| {
            if let Capability::EnrolmentToken { worker, workspace } = capability {
                Ok(write(Actor::Worker(worker), workspace))
            } else {
                Err(DenialReason::EnrolmentTokenRequired)
            }
        });
    }

    #[test]
    fn a_worker_acts_only_for_itself_in_its_own_workspace() {
        check(WriteAction::ActAsWorker, |who, scope, _| {
            match (who, scope) {
                ("worker", Scope::OwnWorkspace) => Ok(write(Actor::Worker(id(3)), id(HOME))),
                _ => Err(DenialReason::WorkerOnly),
            }
        });
    }

    #[test]
    fn anyone_may_view_the_session() {
        check(ReadAction::ViewSession, |_, _, _| {
            Ok(ReadPermit::Installation)
        });
    }

    #[test]
    fn only_members_view_a_workspace() {
        check(ReadAction::ViewWorkspace, |who, scope, _| match who {
            "owner" if scope == Scope::Workspace(id(HOME)) => Ok(ReadPermit::Workspace(id(HOME))),
            "owner" | "visitor" => Err(DenialReason::NotAMember),
            _ => Err(DenialReason::TailnetIdentityRequired),
        });
    }

    #[test]
    fn a_worker_views_only_itself() {
        check(ReadAction::ViewSelfAsWorker, |who, scope, _| {
            match (who, scope) {
                ("worker", Scope::OwnWorkspace) => Ok(ReadPermit::Workspace(id(HOME))),
                _ => Err(DenialReason::WorkerOnly),
            }
        });
    }

    #[test]
    fn only_the_server_views_installation_wide_state() {
        check(ReadAction::ViewAsServer, |who, _, _| match who {
            "server" => Ok(ReadPermit::Installation),
            _ => Err(DenialReason::ServerOnly),
        });
    }

    /// A request for exercising `authorize`: its proof is whatever the facts say.
    #[derive(Debug)]
    struct Probe(Scope);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ProbeProof(Capability);

    impl Proof for ProbeProof {
        fn capability(&self) -> Capability {
            self.0
        }
    }

    impl Request for Probe {
        type Action = WriteAction;
        const ACTION: WriteAction = WriteAction::EnrolWorker;
        type Facts = Result<Capability, DenialReason>;
        type Proof = ProbeProof;

        fn scope(&self) -> Scope {
            self.0
        }

        fn prove(
            &self,
            facts: &Result<Capability, DenialReason>,
            _now: Timestamp,
        ) -> Result<ProbeProof, DenialReason> {
            facts.map(ProbeProof)
        }
    }

    fn now() -> Timestamp {
        Timestamp::from_utc(time::OffsetDateTime::UNIX_EPOCH)
    }

    #[test]
    fn authorize_proves_then_permits_and_keeps_both() {
        let token = Capability::EnrolmentToken {
            worker: id(3),
            workspace: id(HOME),
        };
        let authorized = authorize(
            &Principal::Anonymous,
            Probe(Scope::CapabilityWorkspace),
            &Ok(token),
            now(),
        )
        .unwrap();
        assert_eq!(
            authorized.permit(),
            &write(Actor::Worker(id(3)), id(HOME)),
            "the matrix's permit"
        );
        assert_eq!(authorized.proof(), &ProbeProof(token), "the proof is kept");
        assert_eq!(
            authorized.request().0,
            Scope::CapabilityWorkspace,
            "and the request"
        );
        assert_eq!(
            authorized.into_request().0,
            Scope::CapabilityWorkspace,
            "which can be taken back"
        );
    }

    #[test]
    fn authorize_reports_a_failed_proof_before_the_matrix() {
        let denied = authorize(
            &Principal::Anonymous,
            Probe(Scope::CapabilityWorkspace),
            &Err(DenialReason::EnrolmentTokenExpired),
            now(),
        )
        .unwrap_err();
        assert_eq!(
            denied,
            Denied {
                action: ActionName::Write(WriteAction::EnrolWorker),
                reason: DenialReason::EnrolmentTokenExpired,
            },
            "the proof's reason"
        );
        assert_eq!(
            denied.to_string(),
            "Write(EnrolWorker) denied: the enrolment token expired",
            "the message names the action and the reason"
        );
    }

    #[test]
    fn authorize_reports_the_matrix_refusal() {
        let denied = authorize(
            &Principal::Anonymous,
            Probe(Scope::CapabilityWorkspace),
            &Ok(Capability::None),
            now(),
        )
        .unwrap_err();
        assert_eq!(
            denied.reason,
            DenialReason::EnrolmentTokenRequired,
            "no capability, no enrolment"
        );
        assert_eq!(
            ActionName::from(ReadAction::ViewSession),
            ActionName::Read(ReadAction::ViewSession),
            "reads are named too"
        );
    }

    #[test]
    fn a_write_permit_exposes_its_actor_and_workspace() {
        let permit = write(Actor::Server, id(HOME));
        assert_eq!(permit.actor(), Actor::Server, "actor");
        assert_eq!(permit.workspace(), id(HOME), "workspace");
        assert_eq!(
            NoProof.capability(),
            Capability::None,
            "no proof, no capability"
        );
    }
}
