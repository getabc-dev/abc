//! Commands: requests that change state. Each is a struct with its own [`Command`] impl, so
//! `Store::execute` returns that command's exact outcome type and callers never match on an
//! outcome they cannot get.
//!
//! Adding a command takes: the struct and its `Request` + `Command` impls here; the event variants
//! in `crate::event`; an `Executable` fact loader in abc-store (the bound on `execute` fails until
//! it exists); the projection arms (an exhaustive match fails until they exist); a handler.

mod claim;
mod enrolment;
mod worker;

pub use claim::{
    ClaimProof, ClaimServer, Claimed, InstallationFacts, IssueSetupCode, Reservation,
    ReservationProof, SETUP_CODE_TTL, SetupCodeIssued, SetupCodeRecord,
};
pub use enrolment::{
    EnrolFacts, EnrolWorker, Enrolled, IssueEnrolmentToken, RevokeEnrolmentToken,
    RevokeTokenRejection, TokenIssued, TokenProof, TokenRecord, TokenTtl,
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
