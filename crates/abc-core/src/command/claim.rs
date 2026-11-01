//! Claiming an unclaimed server with a one-time setup code (D4).
//!
//! ```text
//! host shell:  abc server setup-code ── IssueSetupCode (actor Server) ──▶ installation.setup_code_issued
//!              prints the code to the operator's terminal only            { digest, owner, workspace, expires_at }
//! browser:     POST /api/owner/claim { code } over the sidecar ── ClaimServer (actor: the reserved person)
//!              ──▶ person.registered, workspace.created, workspace.member_added, installation.claimed
//! ```
//!
//! Two factors: the sidecar-vouched tailnet login says **who** becomes owner, and the code proves
//! the claimant was given it by someone with a shell on the server host (the root of trust). The
//! plaintext exists only in the CLI process and on the operator's terminal: never in the running
//! server, the database, a log or a response.
//!
//! Single use comes from the `installation` view, not from the code: both commands run under the
//! log-head lock, so a second claim finds `claimed == true` whatever code it presents, and no code
//! can be issued once claimed.

use std::convert::Infallible;

use crate::actor::Role;
use crate::credential::{SetupCode, SetupCodeDigest};
use crate::decision::{DecideCtx, Decision, DecisionBuilder};
use crate::event::{Installation, InstallationEvent, PersonEvent, WorkspaceEvent};
use crate::id::{PersonId, WorkspaceId};
use crate::identity::{DisplayName, TailnetLogin, WorkspaceName};
use crate::policy::{Authorized, Capability, DenialReason, Proof, Request, Scope, WriteAction};
use crate::secret::Secret;
use crate::time::Timestamp;

use super::Command;

/// How long a setup code stays valid.
pub const SETUP_CODE_TTL: time::Duration = time::Duration::hours(1);

/// The host operator asks for a setup code. Only `Principal::Server` may do this, and only while
/// the server is unclaimed. Each issue supersedes the previous code. The reserved ids carry over
/// from the first code, so every code claims the same owner and workspace ids; `candidate` (minted
/// by `Store::issue_setup_code`) is used only when no code was issued before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssueSetupCode {
    /// Fresh ids, used only for the first code.
    pub candidate: Reservation,
}

/// The ids a setup code reserves for the claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// The owner-to-be.
    pub owner: PersonId,
    /// The first workspace-to-be: also the workspace every installation event belongs to.
    pub workspace: WorkspaceId,
}

/// The reservation [`IssueSetupCode`] will record: the earlier one, else the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationProof {
    reservation: Reservation,
}

impl Proof for ReservationProof {
    fn capability(&self) -> Capability {
        Capability::ReserveInstallation {
            workspace: self.reservation.workspace,
        }
    }
}

/// What the store loads for [`IssueSetupCode`] and [`ClaimServer`]: the installation's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationFacts {
    /// Whether the `installation` view has a row.
    pub claimed: bool,
    /// The newest setup code, if one was ever issued.
    pub setup_code: Option<SetupCodeRecord>,
    /// The claimant's login, taken from a `Visitor` principal (claim only).
    pub login: Option<TailnetLogin>,
}

/// The `setup_code` view: the newest code and the ids it reserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupCodeRecord {
    /// The code's digest.
    pub digest: SetupCodeDigest,
    /// The owner-to-be.
    pub owner: PersonId,
    /// The first workspace-to-be.
    pub workspace: WorkspaceId,
    /// When it stops working.
    pub expires_at: Timestamp,
}

/// The issued code: the only value that holds the plaintext. The CLI prints it and drops it.
#[derive(Debug)]
pub struct SetupCodeIssued {
    /// The code, to print once.
    pub code: Secret<SetupCode>,
    /// When it stops working.
    pub expires_at: Timestamp,
}

impl Request for IssueSetupCode {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::IssueSetupCode;
    type Facts = InstallationFacts;
    type Proof = ReservationProof;
    fn scope(&self) -> Scope {
        Scope::Installation
    }
    fn prove(
        &self,
        facts: &InstallationFacts,
        _now: Timestamp,
    ) -> Result<ReservationProof, DenialReason> {
        if facts.claimed {
            return Err(DenialReason::ServerAlreadyClaimed);
        }
        let reservation = facts.setup_code.map_or(self.candidate, |r| Reservation {
            owner: r.owner,
            workspace: r.workspace,
        });
        Ok(ReservationProof { reservation })
    }
}

impl Command for IssueSetupCode {
    type Outcome = SetupCodeIssued;
    type Rejection = Infallible;

    fn decide(
        auth: Authorized<Self>,
        facts: InstallationFacts,
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<SetupCodeIssued>, Infallible> {
        // The permit (actor Server, workspace = the reservation's) came from the matrix.
        // let mut decision = DecisionBuilder::new(&auth);
        // let Reservation { owner, workspace } = auth.proof().reservation;
        // let code = SetupCode::generate(ctx.mint()); let digest = code.expose_secret().digest();
        // let expires_at = ctx.now().checked_add(SETUP_CODE_TTL)   (saturates; never panics)
        // decision.emit(Installation, InstallationEvent::SetupCodeIssued { digest, owner, workspace, expires_at });
        // Ok(decision.finish(SetupCodeIssued { code, expires_at }))
        let _ = (
            auth,
            facts,
            ctx,
            DecisionBuilder::finish::<()>,
            SETUP_CODE_TTL,
        );
        unimplemented!()
    }
}

/// Claim the server: become its owner and create the first workspace.
#[derive(Debug)]
pub struct ClaimServer {
    /// The code the operator was shown by `abc server setup-code`.
    pub code: Secret<SetupCode>,
    /// How the owner wants to be shown. The web app defaults it from `Tailscale-User-Name`.
    pub display_name: DisplayName,
    /// The first workspace's name.
    pub workspace_name: WorkspaceName,
}

/// A valid claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimProof {
    owner: PersonId,
    workspace: WorkspaceId,
    login: TailnetLogin,
}

impl Proof for ClaimProof {
    fn capability(&self) -> Capability {
        Capability::OwnerSetup {
            owner: self.owner,
            workspace: self.workspace,
        }
    }
}

/// The claim's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claimed {
    /// The new owner.
    pub owner: PersonId,
    /// The first workspace.
    pub workspace: WorkspaceId,
}

impl Request for ClaimServer {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ClaimServer;
    type Facts = InstallationFacts;
    type Proof = ClaimProof;

    fn scope(&self) -> Scope {
        Scope::Installation
    }

    fn prove(&self, facts: &InstallationFacts, now: Timestamp) -> Result<ClaimProof, DenialReason> {
        // if facts.claimed                                          => ServerAlreadyClaimed
        // let Some(r) = facts.setup_code else                       => SetupCodeRequired (none issued)
        // let Some(login) = facts.login.clone() else                => TailnetIdentityRequired
        // if !self.code.expose_secret().digest().matches(&r.digest) => SetupCodeMismatch
        //    (checked before expiry, so an expired code's state is revealed only to its holder)
        // if now >= r.expires_at                                    => SetupCodeExpired
        // Ok(ClaimProof { owner: r.owner, workspace: r.workspace, login })
        let _ = (facts, now);
        unimplemented!()
    }
}

impl Command for ClaimServer {
    type Outcome = Claimed;
    type Rejection = Infallible;

    fn decide(
        auth: Authorized<Self>,
        _facts: InstallationFacts,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<Claimed>, Infallible> {
        let mut decision = DecisionBuilder::new(&auth);
        let ClaimProof {
            owner,
            workspace,
            login,
        } = auth.proof().clone();
        let command = auth.into_request();
        decision.emit(
            owner,
            PersonEvent::Registered {
                display_name: command.display_name,
                tailnet_login: login,
            },
        );
        decision.emit(
            workspace,
            WorkspaceEvent::Created {
                name: command.workspace_name,
            },
        );
        decision.emit(
            workspace,
            WorkspaceEvent::MemberAdded {
                person: owner,
                role: Role::Owner,
            },
        );
        decision.emit(
            Installation,
            InstallationEvent::Claimed { owner, workspace },
        );
        Ok(decision.finish(Claimed { owner, workspace }))
    }
}
