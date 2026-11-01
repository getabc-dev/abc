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

impl PolicyAction for WriteAction {
    type Permit = WritePermit;

    fn permit(self, standing: &Standing<'_>) -> Result<WritePermit, DenialReason> {
        // The write matrix. Sketch of the arms (each `match` lists every variant, with no wildcards):
        //
        // IssueSetupCode:
        //   (Server, ReserveInstallation { workspace }) => Server in workspace
        //   (anyone else, _)                            => ServerOnly
        // RecordAsServer:
        //   (Server, WorkerHome { workspace })          => Server in workspace
        //   (anyone else, _)                            => ServerOnly
        // ClaimServer:
        //   (Visitor, OwnerSetup { owner, workspace })  => Person(owner) in workspace
        //   (Person, _)                                 => ServerAlreadyClaimed
        //   (Visitor, None | EnrolmentToken)            => SetupCodeRequired
        //   (Worker | Anonymous | Server, _)            => TailnetIdentityRequired
        // ManageEnrolment | ManageWorkers:
        //   scope Workspace(w), Person with role_in(w) == Some(Owner) => Person(id) in w
        //   Person without membership                   => NotAMember
        //   Worker | Visitor | Anonymous                => TailnetIdentityRequired / NotAMember
        // EnrolWorker:
        //   (_, EnrolmentToken { worker, workspace })   => Worker(worker) in workspace
        //   (_, None | OwnerSetup | ReserveInstallation | WorkerHome) => EnrolmentTokenRequired
        // ActAsWorker:
        //   (Worker { id, workspace }, scope OwnWorkspace) => Worker(id) in workspace
        //   otherwise                                   => WorkerOnly
        let _ = (standing, Role::Owner);
        unimplemented!()
    }
}

impl PolicyAction for ReadAction {
    type Permit = ReadPermit;

    fn permit(self, standing: &Standing<'_>) -> Result<ReadPermit, DenialReason> {
        // ViewSession:      any principal                         => Installation
        // ViewWorkspace:    Person with any role in Workspace(w)  => Workspace(w); else NotAMember
        // ViewSelfAsWorker: Worker { workspace, .. } + OwnWorkspace => Workspace(workspace); else WorkerOnly
        // ViewAsServer:     Server                                => Installation; else ServerOnly
        let _ = standing;
        unimplemented!()
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
    /// The action needs the owner role.
    #[error("only the workspace owner can do this")]
    OwnerOnly,
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
}
