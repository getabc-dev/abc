//! Commands: requests that change state. Each is a struct with its own [`Command`] impl, so
//! `Store::execute` returns that command's exact outcome type and callers never match on an
//! outcome they cannot get.
//!
//! Adding a command takes: the struct and its `Request` + `Command` impls here; the event variants
//! in `crate::event`; an `Executable` fact loader in abc-store (the bound on `execute` fails until
//! it exists); the projection arms (an exhaustive match fails until they exist); a handler.

mod claim;
mod enrolment;
mod request;
mod worker;

pub use claim::{
    ClaimProof, ClaimServer, Claimed, InstallationFacts, IssueSetupCode, Reservation,
    ReservationProof, SETUP_CODE_TTL, SetupCodeIssued, SetupCodeRecord,
};
pub use enrolment::{
    EnrolFacts, EnrolWorker, Enrolled, IssueEnrolmentToken, RevokeEnrolmentToken,
    RevokeTokenRejection, TokenIssued, TokenProof, TokenRecord, TokenTtl,
};
pub use request::{
    ApproveEnrolment, ApproveFacts, DecideRequestRejection, DenyEnrolment, EnrolmentRequested,
    REQUEST_TTL, RequestDecision, RequestEnrolment, RequestFacts, RequestRecord, RequestRejection,
    RequestStanding, RequesterProof,
};
pub use worker::{
    EndSession, OpenSession, RevokeWorker, RevokeWorkerRejection, SelfFacts, SelfProof,
    SessionProof, StartSession, WorkerRecord,
};

use crate::decision::{DecideCtx, Decision};
use crate::policy::{Authorized, Request, WriteAction};

/// A request that changes state.
pub trait Command: Request<Action = WriteAction> {
    /// What the caller gets back once the events are committed.
    type Outcome;
    /// Domain refusals that are not permission questions (such as revoking a token that was
    /// already redeemed). Use `std::convert::Infallible` when there are none.
    type Rejection: std::error::Error + Send + Sync + 'static;

    /// Decides. Pure: everything comes from `auth` (the request, permit and proof), `facts` (loaded
    /// in the same transaction) and `ctx` (time and fresh ids). Must be idempotent with respect to
    /// facts: running it again after its own events were applied yields zero events and the same
    /// outcome, where the command is naturally idempotent (revocations, enrolment retries).
    ///
    /// # Errors
    ///
    /// Returns [`Command::Rejection`].
    fn decide(
        auth: Authorized<Self>,
        facts: Self::Facts,
        ctx: &DecideCtx<'_>,
    ) -> Result<Decision<Self::Outcome>, Self::Rejection>;
}

/// Shared fixtures for the command tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::sync::atomic::{AtomicU64, Ordering};

    use time::macros::datetime;
    use uuid::Uuid;
    use zeroize::Zeroizing;

    use crate::actor::{Membership, Principal, Role};
    use crate::decision::{DecideCtx, Decision, Mint, Pending};
    use crate::id::FromUuid;
    use crate::identity::TailnetLogin;
    use crate::policy::authorize;
    use crate::time::Timestamp;

    use super::Command;

    /// A mint that counts: ids 1, 2, 3… and secret bytes that change with each call.
    #[derive(Debug, Default)]
    pub(crate) struct CountingMint(AtomicU64);

    impl CountingMint {
        fn next(&self) -> u64 {
            self.0.fetch_add(1, Ordering::SeqCst).saturating_add(1)
        }
    }

    impl Mint for CountingMint {
        fn uuid(&self, _now: Timestamp) -> Uuid {
            Uuid::from_u128(u128::from(self.next()))
        }

        fn secret(&self) -> Zeroizing<[u8; 32]> {
            let mut bytes = [0; 32];
            bytes[..8].copy_from_slice(&self.next().to_be_bytes());
            Zeroizing::new(bytes)
        }
    }

    pub(crate) fn id<T: FromUuid>(n: u128) -> T {
        T::from_uuid(Uuid::from_u128(n))
    }

    pub(crate) fn now() -> Timestamp {
        Timestamp::from_utc(datetime!(2026-01-01 00:00:00 UTC))
    }

    pub(crate) const HOME: u128 = 100;

    pub(crate) fn owner() -> Principal {
        Principal::Person {
            id: id(1),
            memberships: vec![Membership {
                workspace: id(HOME),
                role: Role::Owner,
            }],
        }
    }

    pub(crate) fn visitor() -> Principal {
        Principal::Visitor {
            login: TailnetLogin::parse("alice@example.com").unwrap(),
        }
    }

    /// Authorizes and decides `command` for `principal` at `at`, as the store would.
    pub(crate) fn run<C: Command>(
        principal: &Principal,
        command: C,
        facts: C::Facts,
        at: Timestamp,
    ) -> Result<Decision<C::Outcome>, Outcome<C::Rejection>>
    where
        C::Facts: Clone,
    {
        let authorized = authorize(principal, command, &facts, at).map_err(Outcome::Denied)?;
        C::decide(
            authorized,
            facts,
            &DecideCtx::new(at, &CountingMint::default()),
        )
        .map_err(Outcome::Rejected)
    }

    /// Why `run` produced no decision.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Outcome<R> {
        Denied(crate::policy::Denied),
        Rejected(R),
    }

    /// The (stream id, payload) pairs of a decision, for comparing.
    pub(crate) fn events<O>(decision: &Decision<O>) -> Vec<(Uuid, crate::event::Event)> {
        decision
            .events()
            .iter()
            .map(|pending: &Pending| (pending.stream().id, pending.event().clone()))
            .collect()
    }
}
