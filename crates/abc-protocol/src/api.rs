//! The stable, machine-readable error codes of abc's HTTP API: the single list for every client
//! (the worker and the web app, through ts-rs). A code never changes meaning. While abc is
//! unreleased (S2), codes are renamed freely; after release, a code is only ever added.
//!
//! abc-server maps every error to exactly one code through exhaustive matches
//! (`abc_server::api::error`), so a new `DenialReason` or rejection fails the build until mapped.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The JSON body of every non-2xx response: `{"error":{"code":"…","message":"…"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export)]
pub struct ApiErrorBody {
    /// The error.
    pub error: ApiErrorDetail,
}

/// An error's code and human message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export)]
pub struct ApiErrorDetail {
    /// Branch on this.
    pub code: ApiCode,
    /// Show this. It never contains a secret or echoes request input.
    pub message: String,
}

/// Every error code. Grouped by who has to act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ApiCode {
    // The request itself.
    /// The body or path did not parse. The message names the field, never its value.
    MalformedRequest,
    /// A state-changing request came from another site, or named a host other than the server's
    /// public URL (CSRF and DNS-rebinding guard).
    CrossSiteRequest,
    /// Nothing at this path.
    NotFound,

    // Authentication.
    /// The request needs a signed-in tailnet person and carried no identity (for example, a
    /// tagged device).
    TailnetIdentityRequired,
    /// The worker key is not known to this server.
    InvalidWorkerKey,
    /// The worker was revoked. Stop retrying and re-enrol.
    WorkerRevoked,

    // Authorization.
    /// Not a member of this workspace.
    NotAMember,
    /// Only the workspace owner can do this.
    OwnerOnly,
    /// Only a worker can do this.
    WorkerOnly,
    /// The server already has an owner.
    ServerAlreadyClaimed,
    /// A setup code is required.
    SetupCodeRequired,
    /// The setup code does not match.
    SetupCodeMismatch,
    /// The setup code expired. Run `abc server setup-code` on the server again.
    SetupCodeExpired,
    /// Only the server itself can do this (never expected from a client; a bug if seen).
    ServerOnly,
    /// No such worker.
    UnknownWorker,
    /// An enrolment token is required.
    EnrolmentTokenRequired,
    /// No such enrolment token.
    EnrolmentTokenUnknown,
    /// The enrolment token expired. Ask the owner for a new one.
    EnrolmentTokenExpired,
    /// The enrolment token was revoked.
    EnrolmentTokenRevoked,
    /// The enrolment token was already used by another machine.
    EnrolmentTokenUsed,

    // Domain rules.
    /// The token was already redeemed: revoke the worker instead.
    EnrolmentTokenAlreadyRedeemed,

    // The server.
    /// The server is starting, draining, or cannot reach its database. Retry.
    Unavailable,
    /// A bug. The response carries a request id to quote.
    Internal,
}
