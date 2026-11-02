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

use super::claim::expiry;
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
#[ts(type = "number")]
pub struct TokenTtl(#[schemars(with = "u32")] time::Duration);

impl TokenTtl {
    /// The shortest lifetime: five minutes.
    const MIN_SECONDS: u32 = 300;
    /// The longest lifetime: a day.
    const MAX_SECONDS: u32 = 86_400;
}

impl Default for TokenTtl {
    fn default() -> Self {
        Self(time::Duration::minutes(30))
    }
}

impl TryFrom<u32> for TokenTtl {
    type Error = crate::identity::InvalidValue;
    /// Seconds, 300 to 86 400.
    fn try_from(seconds: u32) -> Result<Self, Self::Error> {
        if (Self::MIN_SECONDS..=Self::MAX_SECONDS).contains(&seconds) {
            Ok(Self(time::Duration::seconds(i64::from(seconds))))
        } else {
            Err(crate::identity::InvalidValue {
                field: "token ttl",
                rule: "300 to 86400 seconds",
            })
        }
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
        let mut decision = DecisionBuilder::new(&auth);
        let command = auth.into_request();
        let id: EnrolmentTokenId = ctx.id();
        let worker: WorkerId = ctx.id();
        let (token, digest) = EnrolmentToken::generate(ctx.mint());
        let expires_at = expiry(ctx.now(), command.ttl.0);
        decision.emit(
            id,
            EnrolmentEvent::Issued {
                digest,
                worker,
                label: command.label,
                expires_at,
            },
        );
        Ok(decision.finish(TokenIssued {
            id,
            token,
            expires_at,
        }))
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
    /// Whether an enrolled worker already holds the presented key: a key names one worker.
    pub key_in_use: bool,
    /// Whether the worker the token reserved exists and was revoked.
    pub reserved_worker_revoked: bool,
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
        let Some(record) = &facts.token else {
            return Err(DenialReason::EnrolmentTokenUnknown);
        };
        // A replay is checked first: a retry after expiry must still get its answer, unless the
        // worker it created has since been revoked.
        let replay = match record.redeemed_by {
            Some(key) if key.matches(&self.key) => {
                if facts.reserved_worker_revoked {
                    return Err(DenialReason::WorkerRevoked);
                }
                true
            }
            Some(_) => return Err(DenialReason::EnrolmentTokenUsed),
            None if record.revoked => return Err(DenialReason::EnrolmentTokenRevoked),
            None if now >= record.expires_at => return Err(DenialReason::EnrolmentTokenExpired),
            None if facts.key_in_use => return Err(DenialReason::WorkerKeyInUse),
            None => false,
        };
        Ok(TokenProof {
            record: record.clone(),
            replay,
        })
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
        let mut decision = DecisionBuilder::new(&auth);
        let TokenProof { record, replay } = auth.proof().clone();
        let command = auth.into_request();
        // A replay's name follows the same label-or-hostname rule as the first enrolment. Once
        // workers can be renamed, the replay path will load the name from the facts instead.
        let name = record
            .label
            .map_or_else(|| command.machine.hostname.clone(), DisplayName::from);
        let enrolled = Enrolled {
            worker: record.worker,
            workspace: record.workspace,
            name: name.clone(),
        };
        if replay {
            return Ok(decision.finish(enrolled));
        }
        decision.caused_by(record.issued_event);
        decision.emit(record.id, EnrolmentEvent::Redeemed { key: command.key });
        decision.emit(
            record.worker,
            WorkerEvent::Enrolled {
                token: record.id,
                key: command.key,
                name,
                machine: command.machine,
                build: command.build,
            },
        );
        Ok(decision.finish(enrolled))
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
        let mut decision = DecisionBuilder::new(&auth);
        let command = auth.into_request();
        let record = facts
            .filter(|record| record.workspace == command.workspace)
            .ok_or(RevokeTokenRejection::NotFound)?;
        if record.redeemed_by.is_some() {
            return Err(RevokeTokenRejection::AlreadyRedeemed);
        }
        if !record.revoked {
            decision.emit(record.id, EnrolmentEvent::Revoked);
        }
        Ok(decision.finish(()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{HOME, Outcome, events, id, now, owner, run};
    use super::*;
    use crate::actor::{Actor, Principal};
    use crate::credential::Digest;
    use crate::identity::{Arch, Os};
    use crate::policy::{ActionName, Denied};

    #[test]
    fn token_lifetimes_are_five_minutes_to_a_day() {
        assert_eq!(
            TokenTtl::default().0,
            time::Duration::minutes(30),
            "30 minutes by default"
        );
        assert_eq!(
            TokenTtl::try_from(300).map(|ttl| ttl.0),
            Ok(time::Duration::minutes(5)),
            "min"
        );
        assert_eq!(
            TokenTtl::try_from(86_400).map(|ttl| ttl.0),
            Ok(time::Duration::days(1)),
            "max"
        );
        for seconds in [0, 299, 86_401] {
            assert_eq!(
                TokenTtl::try_from(seconds).unwrap_err().to_string(),
                "invalid token ttl: 300 to 86400 seconds",
                "{seconds} s"
            );
        }
        assert_eq!(
            serde_json::from_str::<TokenTtl>("600")
                .map(|ttl| ttl.0)
                .unwrap(),
            time::Duration::minutes(10),
            "whole seconds on the wire"
        );
    }

    #[test]
    fn an_owner_issues_a_token_that_reserves_a_worker() {
        let command = IssueEnrolmentToken {
            workspace: id(HOME),
            label: Some(MachineLabel::parse("laptop").unwrap()),
            ttl: TokenTtl::try_from(600).unwrap(),
        };
        let decision = run(&owner(), command, (), now()).unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Person(id(1)),
            "the owner issued it"
        );
        let recorded = events(&decision);
        let issued = decision.into_outcome();
        let expires_at = now().checked_add(time::Duration::minutes(10)).unwrap();
        assert_eq!(issued.expires_at, expires_at, "ten minutes");
        assert_eq!(
            recorded,
            [(
                issued.id.as_uuid(),
                EnrolmentEvent::Issued {
                    digest: issued.token.expose_secret().digest(),
                    worker: id(2),
                    label: Some(MachineLabel::parse("laptop").unwrap()),
                    expires_at,
                }
                .into()
            )],
            "the digest and the reserved worker are recorded on the token's stream"
        );
        assert_eq!(issued.id, id(1), "the first minted id is the token's");
    }

    fn record() -> TokenRecord {
        TokenRecord {
            id: id(10),
            workspace: id(HOME),
            worker: id(11),
            label: None,
            expires_at: now().checked_add(time::Duration::minutes(30)).unwrap(),
            revoked: false,
            redeemed_by: None,
            issued_event: id(12),
        }
    }

    fn key(byte: u8) -> WorkerKeyDigest {
        Digest::from_bytes([byte; 32])
    }

    fn enrol(with: WorkerKeyDigest) -> EnrolWorker {
        EnrolWorker {
            token: EnrolmentToken::parse(&format!("abc_enr_{}", "0".repeat(52))).unwrap(),
            key: with,
            machine: MachineInfo {
                hostname: DisplayName::parse("mbp").unwrap(),
                os: Os::Macos,
                arch: Arch::Aarch64,
            },
            build: Version::new(0, 1, 0),
        }
    }

    fn facts(record: TokenRecord) -> EnrolFacts {
        EnrolFacts {
            token: Some(record),
            key_in_use: false,
            reserved_worker_revoked: false,
        }
    }

    fn expected() -> Enrolled {
        Enrolled {
            worker: id(11),
            workspace: id(HOME),
            name: DisplayName::parse("mbp").unwrap(),
        }
    }

    #[test]
    fn redeeming_an_open_token_enrols_the_reserved_worker() {
        let decision = run(&Principal::Anonymous, enrol(key(1)), facts(record()), now()).unwrap();
        assert_eq!(
            decision.permit().actor(),
            Actor::Worker(id(11)),
            "recorded as the worker"
        );
        assert_eq!(
            decision.permit().workspace(),
            id(HOME),
            "in the token's workspace"
        );
        assert_eq!(
            decision.cause(),
            Some(id(12)),
            "caused by the token's issue"
        );
        assert_eq!(
            events(&decision),
            [
                (
                    uuid::Uuid::from_u128(10),
                    EnrolmentEvent::Redeemed { key: key(1) }.into()
                ),
                (
                    uuid::Uuid::from_u128(11),
                    WorkerEvent::Enrolled {
                        token: id(10),
                        key: key(1),
                        name: DisplayName::parse("mbp").unwrap(),
                        machine: enrol(key(1)).machine,
                        build: Version::new(0, 1, 0),
                    }
                    .into()
                ),
            ],
            "redeemed, then enrolled"
        );
        assert_eq!(decision.into_outcome(), expected(), "hostname is the name");
    }

    #[test]
    fn a_label_names_the_worker() {
        let labelled = TokenRecord {
            label: Some(MachineLabel::parse("work laptop").unwrap()),
            ..record()
        };
        let enrolled = run(&Principal::Anonymous, enrol(key(1)), facts(labelled), now())
            .unwrap()
            .into_outcome();
        assert_eq!(
            enrolled.name.as_str(),
            "work laptop",
            "the label wins over the hostname"
        );
    }

    #[test]
    fn the_same_key_again_gets_the_same_answer_and_records_nothing_even_after_expiry() {
        let redeemed = TokenRecord {
            redeemed_by: Some(key(1)),
            ..record()
        };
        let late = now().checked_add(time::Duration::hours(2)).unwrap();
        let decision = run(&Principal::Anonymous, enrol(key(1)), facts(redeemed), late).unwrap();
        assert!(decision.events().is_empty(), "a replay records nothing");
        assert_eq!(decision.cause(), None, "and claims no cause");
        assert_eq!(decision.into_outcome(), expected(), "the same worker");
    }

    #[test]
    fn a_key_another_worker_holds_is_refused() {
        let held = EnrolFacts {
            key_in_use: true,
            ..facts(record())
        };
        assert_eq!(
            run(&Principal::Anonymous, enrol(key(1)), held, now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::EnrolWorker),
                reason: DenialReason::WorkerKeyInUse,
            }),
            "one key, one worker"
        );
    }

    #[test]
    fn a_replay_for_a_revoked_worker_is_refused() {
        let gone = EnrolFacts {
            reserved_worker_revoked: true,
            ..facts(TokenRecord {
                redeemed_by: Some(key(1)),
                ..record()
            })
        };
        assert_eq!(
            run(&Principal::Anonymous, enrol(key(1)), gone, now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::EnrolWorker),
                reason: DenialReason::WorkerRevoked,
            }),
            "a revoked worker's token no longer answers"
        );
    }

    #[test]
    fn unknown_used_revoked_and_expired_tokens_are_refused() {
        let refuse = |facts: EnrolFacts, at: Timestamp| {
            run(&Principal::Anonymous, enrol(key(1)), facts, at)
                .map(|_| ())
                .unwrap_err()
        };
        let reason = |reason| {
            Outcome::Denied(Denied {
                action: ActionName::Write(WriteAction::EnrolWorker),
                reason,
            })
        };
        assert_eq!(
            refuse(
                EnrolFacts {
                    token: None,
                    key_in_use: false,
                    reserved_worker_revoked: false,
                },
                now()
            ),
            reason(DenialReason::EnrolmentTokenUnknown),
            "unknown"
        );
        assert_eq!(
            refuse(
                facts(TokenRecord {
                    redeemed_by: Some(key(2)),
                    ..record()
                }),
                now()
            ),
            reason(DenialReason::EnrolmentTokenUsed),
            "another key redeemed it"
        );
        assert_eq!(
            refuse(
                facts(TokenRecord {
                    revoked: true,
                    ..record()
                }),
                now()
            ),
            reason(DenialReason::EnrolmentTokenRevoked),
            "revoked"
        );
        assert_eq!(
            refuse(facts(record()), record().expires_at),
            reason(DenialReason::EnrolmentTokenExpired),
            "expired at exactly expires_at"
        );
        assert_eq!(
            refuse(
                facts(TokenRecord {
                    revoked: true,
                    ..record()
                }),
                record().expires_at
            ),
            reason(DenialReason::EnrolmentTokenRevoked),
            "revocation is reported before expiry"
        );
    }

    fn revoke() -> RevokeEnrolmentToken {
        RevokeEnrolmentToken {
            workspace: id(HOME),
            token: id(10),
        }
    }

    #[test]
    fn revoking_an_open_token_records_it_once() {
        let decision = run(&owner(), revoke(), Some(record()), now()).unwrap();
        assert_eq!(
            events(&decision),
            [(uuid::Uuid::from_u128(10), EnrolmentEvent::Revoked.into())],
            "revoked on the token's stream"
        );
        let again = run(
            &owner(),
            revoke(),
            Some(TokenRecord {
                revoked: true,
                ..record()
            }),
            now(),
        )
        .unwrap();
        assert!(again.events().is_empty(), "revoking twice records nothing");
    }

    #[test]
    fn a_redeemed_or_foreign_token_cannot_be_revoked() {
        assert_eq!(
            run(&owner(), revoke(), None, now())
                .map(|_| ())
                .unwrap_err(),
            Outcome::Rejected(RevokeTokenRejection::NotFound),
            "unknown"
        );
        assert_eq!(
            run(
                &owner(),
                revoke(),
                Some(TokenRecord {
                    workspace: id(999),
                    ..record()
                }),
                now()
            )
            .map(|_| ())
            .unwrap_err(),
            Outcome::Rejected(RevokeTokenRejection::NotFound),
            "another workspace's token is not found here"
        );
        assert_eq!(
            run(
                &owner(),
                revoke(),
                Some(TokenRecord {
                    redeemed_by: Some(key(1)),
                    ..record()
                }),
                now()
            )
            .map(|_| ())
            .unwrap_err(),
            Outcome::Rejected(RevokeTokenRejection::AlreadyRedeemed),
            "redeemed: revoke the worker instead"
        );
        assert_eq!(
            RevokeTokenRejection::AlreadyRedeemed.to_string(),
            "the token was already redeemed; revoke the worker instead",
            "the message says what to do"
        );
    }
}
