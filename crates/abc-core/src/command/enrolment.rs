//! Enrolment tokens: issue, revoke, redeem.
//!
//! Lifecycle (every step under the log-head lock):
//!
//! ```text
//!   issued ──redeem(key K)──▶ redeemed(K) ──redeem(K) again──▶ same Enrolled, zero events
//!     │                          └──────────redeem(K' ≠ K)────▶ denied: enrolment_token_used
//!     ├──revoke──▶ revoked ──redeem──▶ denied: enrolment_token_revoked
//!     └──(now ≥ expires_at)──▶ expired (derived; no event, no sweeper)
//! ```

use std::convert::Infallible;

use semver::Version;
use thiserror::Error;

use crate::credential::{EnrolmentToken, WorkerKeyDigest};
use crate::decision::{DecideCtx, Decision, DecisionBuilder};
use crate::event::{EnrolmentEvent, WorkerEvent};
use crate::id::{EnrolmentTokenId, EventId, WorkerId, WorkspaceId};
use crate::identity::{DisplayName, MachineInfo, MachineLabel};
use crate::policy::{
    Authorized, Capability, DenialReason, NoProof, Proof, Request, Scope, WriteAction,
};
use crate::secret::Secret;
use crate::time::Timestamp;

use super::Command;

/// How long a new token stays redeemable: 5 minutes to 24 hours, 30 minutes by default. On the
/// wire, whole seconds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, ts_rs::TS, schemars::JsonSchema,
)]
#[serde(try_from = "u32")]
#[ts(export, type = "number")]
pub struct TokenTtl(#[schemars(with = "u32")] time::Duration);

impl Default for TokenTtl {
    fn default() -> Self {
        Self(time::Duration::minutes(30))
    }
}

impl TryFrom<u32> for TokenTtl {
    type Error = crate::identity::InvalidValue;
    /// Seconds, 300 to 86 400.
    fn try_from(seconds: u32) -> Result<Self, Self::Error> {
        let _ = seconds;
        unimplemented!()
    }
}

/// The owner issues a token for one machine to enrol into `workspace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEnrolmentToken {
    /// Where the machine will enrol.
    pub workspace: WorkspaceId,
    /// An optional note, which also becomes the worker's name.
    pub label: Option<MachineLabel>,
    /// How long it stays redeemable.
    pub ttl: TokenTtl,
}

/// The issued token. The plaintext lives only here: the log holds its digest.
#[derive(Debug)]
pub struct TokenIssued {
    /// The token's id, for listing and revoking.
    pub id: EnrolmentTokenId,
    /// The token, to show once.
    pub token: Secret<EnrolmentToken>,
    /// When it stops working.
    pub expires_at: Timestamp,
}

impl Request for IssueEnrolmentToken {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ManageEnrolment;
    type Facts = ();
    type Proof = NoProof;
    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }
    fn prove(&self, (): &(), _now: Timestamp) -> Result<NoProof, DenialReason> {
        Ok(NoProof)
    }
}

impl Command for IssueEnrolmentToken {
    type Outcome = TokenIssued;
    type Rejection = Infallible;

    fn decide(
        auth: Authorized<Self>,
        (): (),
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<TokenIssued>, Infallible> {
        // let mut decision = DecisionBuilder::new(&auth);
        // let command = auth.into_request();
        // let id: EnrolmentTokenId = ctx.id(); let worker: WorkerId = ctx.id();
        // let (token, digest) = EnrolmentToken::generate(ctx.mint());
        // let expires_at = ctx.now().checked_add(command.ttl.0)  (overflow is impossible within 24 h;
        //   handled by saturating at the TokenTtl boundary, not by a panic)
        // decision.emit(id, EnrolmentEvent::Issued { digest, worker, label: command.label, expires_at });
        // Ok(decision.finish(TokenIssued { id, token, expires_at }))
        let _ = (auth, ctx, DecisionBuilder::finish::<()>);
        unimplemented!()
    }
}

/// A machine redeems a token, registering the key it generated.
#[derive(Debug)]
pub struct EnrolWorker {
    /// The token the owner gave it.
    pub token: Secret<EnrolmentToken>,
    /// The digest of the key the worker generated and saved before sending this request.
    pub key: WorkerKeyDigest,
    /// What it reports about its machine.
    pub machine: MachineInfo,
    /// The abc release it runs.
    pub build: Version,
}

/// The stored state of a token, as the `enrolment_token` view holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    /// The token.
    pub id: EnrolmentTokenId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// The worker id it reserved.
    pub worker: WorkerId,
    /// The owner's note.
    pub label: Option<MachineLabel>,
    /// When it stops working.
    pub expires_at: Timestamp,
    /// Whether the owner revoked it.
    pub revoked: bool,
    /// The key that redeemed it, if one did.
    pub redeemed_by: Option<WorkerKeyDigest>,
    /// Its `issued` event, which becomes the enrolment's causation.
    pub issued_event: EventId,
}

/// Loaded by digest lookup on the presented token, in the enrolment's transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolFacts {
    /// The token with that digest, if any.
    pub token: Option<TokenRecord>,
}

/// A redeemable token, or one already redeemed by this same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenProof {
    record: TokenRecord,
    /// True when this key already redeemed the token: the earlier response was lost.
    replay: bool,
}

impl Proof for TokenProof {
    fn capability(&self) -> Capability {
        Capability::EnrolmentToken {
            worker: self.record.worker,
            workspace: self.record.workspace,
        }
    }
}

/// The enrolment's outcome. The same for the first call and for every retry with the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// The worker's id. The worker stores it next to its key.
    pub worker: WorkerId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Its display name.
    pub name: DisplayName,
}

impl Request for EnrolWorker {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::EnrolWorker;
    type Facts = EnrolFacts;
    type Proof = TokenProof;

    fn scope(&self) -> Scope {
        Scope::CapabilityWorkspace
    }

    fn prove(&self, facts: &EnrolFacts, now: Timestamp) -> Result<TokenProof, DenialReason> {
        // None                                         => EnrolmentTokenUnknown
        // Some(t) with t.redeemed_by == Some(self.key) => replay (checked first: a retry after
        //                                                 expiry must still get its answer)
        // Some(t) with t.redeemed_by == Some(other)    => EnrolmentTokenUsed
        // Some(t) with t.revoked                       => EnrolmentTokenRevoked
        // Some(t) with now >= t.expires_at             => EnrolmentTokenExpired
        // Some(t)                                      => TokenProof { record: t, replay: false }
        let _ = (facts, now);
        unimplemented!()
    }
}

impl Command for EnrolWorker {
    type Outcome = Enrolled;
    type Rejection = Infallible;

    fn decide(
        auth: Authorized<Self>,
        _facts: EnrolFacts,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<Enrolled>, Infallible> {
        // let mut decision = DecisionBuilder::new(&auth);
        // let TokenProof { record, replay } = auth.proof().clone();
        // let command = auth.into_request();
        // let name = record.label → DisplayName, else command.machine.hostname.clone()
        // if replay { return Ok(decision.finish(Enrolled { worker, workspace, name })) }   // zero events
        // decision.caused_by(record.issued_event);
        // decision.emit(record.id, EnrolmentEvent::Redeemed { key: command.key });
        // decision.emit(record.worker, WorkerEvent::Enrolled { token: record.id, key, name, machine, build });
        // Ok(decision.finish(Enrolled { .. }))
        //
        // A replay's name comes from the label or hostname rule, not from the stored name. Once
        // workers can be renamed (later), the replay path loads the name from facts instead.
        let _ = (auth, EnrolmentEvent::Revoked, WorkerEvent::Revoked);
        unimplemented!()
    }
}

/// The owner revokes an unredeemed token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeEnrolmentToken {
    /// Its workspace (the scope of the check).
    pub workspace: WorkspaceId,
    /// The token.
    pub token: EnrolmentTokenId,
}

/// Why a token cannot be revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RevokeTokenRejection {
    /// No token with this id in this workspace.
    #[error("no such enrolment token in this workspace")]
    NotFound,
    /// It was redeemed: revoke the worker instead.
    #[error("the token was already redeemed; revoke the worker instead")]
    AlreadyRedeemed,
}

impl Request for RevokeEnrolmentToken {
    type Action = WriteAction;
    const ACTION: WriteAction = WriteAction::ManageEnrolment;
    type Facts = Option<TokenRecord>;
    type Proof = NoProof;
    fn scope(&self) -> Scope {
        Scope::Workspace(self.workspace)
    }
    fn prove(
        &self,
        _facts: &Option<TokenRecord>,
        _now: Timestamp,
    ) -> Result<NoProof, DenialReason> {
        Ok(NoProof)
    }
}

impl Command for RevokeEnrolmentToken {
    type Outcome = ();
    type Rejection = RevokeTokenRejection;

    fn decide(
        auth: Authorized<Self>,
        facts: Option<TokenRecord>,
        _ctx: &DecideCtx<'_>,
    ) -> Result<Decision<()>, RevokeTokenRejection> {
        // None or other workspace => NotFound; redeemed => AlreadyRedeemed;
        // already revoked => zero events (idempotent); else emit Revoked on the token stream.
        let _ = (auth, facts);
        unimplemented!()
    }
}
