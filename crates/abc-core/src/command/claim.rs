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

/// `now + ttl`. A request's instant is a real clock reading, far from the end of time, so the sum
/// always exists; were it ever not to, the credential would simply be expired at once.
pub(crate) fn expiry(now: Timestamp, ttl: time::Duration) -> Timestamp {
    now.checked_add(ttl).unwrap_or(now)
}

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
        _facts: InstallationFacts,
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<SetupCodeIssued>, Infallible> {
        let mut decision = DecisionBuilder::new(&auth);
        let Reservation { owner, workspace } = auth.proof().reservation;
        let code = SetupCode::generate(ctx.mint());
        let expires_at = expiry(ctx.now(), SETUP_CODE_TTL);
        decision.emit(
            Installation,
            InstallationEvent::SetupCodeIssued {
                digest: code.expose_secret().digest(),
                owner,
                workspace,
                expires_at,
            },
        );
        Ok(decision.finish(SetupCodeIssued { code, expires_at }))
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
        if facts.claimed {
            return Err(DenialReason::ServerAlreadyClaimed);
        }
        let Some(record) = facts.setup_code else {
            return Err(DenialReason::SetupCodeRequired);
        };
        let Some(login) = facts.login.clone() else {
            return Err(DenialReason::TailnetIdentityRequired);
        };
        // The digest is checked before expiry, so only the code's holder learns that it expired.
        if !self.code.expose_secret().digest().matches(&record.digest) {
            return Err(DenialReason::SetupCodeMismatch);
        }
        if now >= record.expires_at {
            return Err(DenialReason::SetupCodeExpired);
        }
        Ok(ClaimProof {
            owner: record.owner,
            workspace: record.workspace,
            login,
        })
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

#[cfg(test)]
mod tests {
    use super::super::fixtures::{Outcome, events, id, now, owner, run, visitor};
    use super::*;
    use crate::actor::{Actor, Principal};
    use crate::policy::{ActionName, Denied};

    fn reservation() -> Reservation {
        Reservation {
            owner: id(1),
            workspace: id(2),
        }
    }

    fn unclaimed() -> InstallationFacts {
        InstallationFacts {
            claimed: false,
            setup_code: None,
            login: None,
        }
    }

    fn denied(action: WriteAction, reason: DenialReason) -> Denied {
        Denied {
            action: ActionName::Write(action),
            reason,
        }
    }

    #[test]
    fn the_first_setup_code_reserves_the_candidate_ids() {
        let decision = run(
            &Principal::Server,
            IssueSetupCode {
                candidate: reservation(),
            },
            unclaimed(),
            now(),
        )
        .unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Server,
            "recorded as the server"
        );
        assert_eq!(
            decision.permit().workspace(),
            id(2),
            "in the reserved workspace"
        );
        let expires_at = now().checked_add(SETUP_CODE_TTL).unwrap();
        let recorded = events(&decision);
        let issued = decision.into_outcome();
        assert_eq!(issued.expires_at, expires_at, "an hour from now");
        assert_eq!(
            recorded,
            [(
                uuid::Uuid::nil(),
                InstallationEvent::SetupCodeIssued {
                    digest: issued.code.expose_secret().digest(),
                    owner: id(1),
                    workspace: id(2),
                    expires_at,
                }
                .into()
            )],
            "only the digest is recorded, on the installation stream"
        );
    }

    #[test]
    fn a_later_setup_code_keeps_the_first_reservation() {
        let facts = InstallationFacts {
            setup_code: Some(SetupCodeRecord {
                digest: crate::credential::Digest::from_bytes([0; 32]),
                owner: id(7),
                workspace: id(8),
                expires_at: now(),
            }),
            ..unclaimed()
        };
        let decision = run(
            &Principal::Server,
            IssueSetupCode {
                candidate: reservation(),
            },
            facts,
            now(),
        )
        .unwrap();
        assert_eq!(
            decision.permit().workspace(),
            id(8),
            "the earlier workspace"
        );
        assert!(
            matches!(
                events(&decision).as_slice(),
                [(_, crate::event::Event::Installation(InstallationEvent::SetupCodeIssued {
                    owner,
                    workspace,
                    ..
                }))] if *owner == id(7) && *workspace == id(8)
            ),
            "the earlier reservation is kept"
        );
    }

    #[test]
    fn no_setup_code_once_claimed_and_none_but_for_the_server() {
        let claimed = InstallationFacts {
            claimed: true,
            ..unclaimed()
        };
        let issue = || IssueSetupCode {
            candidate: reservation(),
        };
        assert_eq!(
            run(&Principal::Server, issue(), claimed, now()).unwrap_err(),
            Outcome::Denied(denied(
                WriteAction::IssueSetupCode,
                DenialReason::ServerAlreadyClaimed
            )),
            "once claimed, never again"
        );
        assert_eq!(
            run(&owner(), issue(), unclaimed(), now()).unwrap_err(),
            Outcome::Denied(denied(
                WriteAction::IssueSetupCode,
                DenialReason::ServerOnly
            )),
            "a person cannot mint one"
        );
    }

    fn code() -> Secret<SetupCode> {
        SetupCode::parse("10ABC-DEFGH-JKMNP-QRSTV").unwrap()
    }

    fn claim() -> ClaimServer {
        ClaimServer {
            code: code(),
            display_name: DisplayName::parse("Alice").unwrap(),
            workspace_name: WorkspaceName::parse("Home").unwrap(),
        }
    }

    fn issued(expires_at: Timestamp) -> InstallationFacts {
        InstallationFacts {
            claimed: false,
            setup_code: Some(SetupCodeRecord {
                digest: code().expose_secret().digest(),
                owner: id(1),
                workspace: id(2),
                expires_at,
            }),
            login: Some(TailnetLogin::parse("alice@example.com").unwrap()),
        }
    }

    fn later() -> Timestamp {
        now().checked_add(time::Duration::minutes(10)).unwrap()
    }

    #[test]
    fn a_valid_claim_registers_the_owner_and_the_first_workspace() {
        let decision = run(&visitor(), claim(), issued(later()), now()).unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Person(id(1)),
            "the reserved person"
        );
        assert_eq!(
            decision.permit().workspace(),
            id(2),
            "the reserved workspace"
        );
        assert_eq!(
            events(&decision),
            [
                (
                    uuid::Uuid::from_u128(1),
                    PersonEvent::Registered {
                        display_name: DisplayName::parse("Alice").unwrap(),
                        tailnet_login: TailnetLogin::parse("alice@example.com").unwrap(),
                    }
                    .into()
                ),
                (
                    uuid::Uuid::from_u128(2),
                    WorkspaceEvent::Created {
                        name: WorkspaceName::parse("Home").unwrap(),
                    }
                    .into()
                ),
                (
                    uuid::Uuid::from_u128(2),
                    WorkspaceEvent::MemberAdded {
                        person: id(1),
                        role: Role::Owner,
                    }
                    .into()
                ),
                (
                    uuid::Uuid::nil(),
                    InstallationEvent::Claimed {
                        owner: id(1),
                        workspace: id(2),
                    }
                    .into()
                ),
            ],
            "person, workspace, membership, claim"
        );
        assert_eq!(
            decision.into_outcome(),
            Claimed {
                owner: id(1),
                workspace: id(2),
            },
            "the outcome names both"
        );
    }

    #[test]
    fn a_claim_needs_an_unclaimed_server_a_login_and_the_current_code_in_time() {
        let refuse = |facts: InstallationFacts, at: Timestamp| {
            run(&visitor(), claim(), facts, at).map(|_| ()).unwrap_err()
        };
        let reason = |reason| Outcome::Denied(denied(WriteAction::ClaimServer, reason));
        assert_eq!(
            refuse(
                InstallationFacts {
                    claimed: true,
                    ..issued(later())
                },
                now()
            ),
            reason(DenialReason::ServerAlreadyClaimed),
            "claimed"
        );
        assert_eq!(
            refuse(
                InstallationFacts {
                    setup_code: None,
                    ..issued(later())
                },
                now()
            ),
            reason(DenialReason::SetupCodeRequired),
            "no code issued"
        );
        assert_eq!(
            refuse(
                InstallationFacts {
                    login: None,
                    ..issued(later())
                },
                now()
            ),
            reason(DenialReason::TailnetIdentityRequired),
            "no login"
        );
        let mut other = issued(later());
        if let Some(record) = other.setup_code.as_mut() {
            record.digest = crate::credential::Digest::from_bytes([9; 32]);
        }
        assert_eq!(
            refuse(other.clone(), now()),
            reason(DenialReason::SetupCodeMismatch),
            "wrong"
        );
        assert_eq!(
            refuse(other, later()),
            reason(DenialReason::SetupCodeMismatch),
            "a wrong code does not learn that the current one expired"
        );
        assert_eq!(
            refuse(issued(later()), later()),
            reason(DenialReason::SetupCodeExpired),
            "expired at exactly expires_at"
        );
        assert!(
            run(
                &visitor(),
                claim(),
                issued(later()),
                later()
                    .checked_add(time::Duration::microseconds(-1))
                    .unwrap()
            )
            .is_ok(),
            "valid until the last microsecond"
        );
    }

    #[test]
    fn expiry_saturates_at_the_end_of_time() {
        let end = Timestamp::from_utc(time::OffsetDateTime::new_utc(
            time::Date::MAX,
            time::Time::from_hms(23, 30, 0).unwrap(),
        ));
        assert_eq!(
            expiry(end, SETUP_CODE_TTL),
            end,
            "no overflow, just already expired"
        );
    }
}
