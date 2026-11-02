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
pub struct ApiErrorBody {
    /// The error.
    pub error: ApiErrorDetail,
}

/// An error's code and human message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ApiErrorDetail {
    /// Branch on this.
    pub code: ApiCode,
    /// Show this. It never contains a secret or echoes request input.
    pub message: String,
}

/// Every error code. Grouped by who has to act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiCode {
    // The request itself.
    /// The body or path did not parse. The message names the field, never its value.
    MalformedRequest,
    /// A state-changing request came from another site, or named a host other than the server's
    /// public URL (CSRF and DNS-rebinding guard).
    CrossSiteRequest,
    /// Nothing at this path.
    NotFound,
    /// The path exists, but not for this method.
    MethodNotAllowed,

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
    /// Only the server itself can do this. A client never causes it; seeing it is a bug.
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
    /// Another worker already enrolled with this key. Generate a new key and enrol again.
    WorkerKeyInUse,

    // Domain rules.
    /// The token was already redeemed: revoke the worker instead.
    EnrolmentTokenAlreadyRedeemed,

    // Load.
    /// Too many requests from this source or for this worker. Retry later.
    RateLimited,

    // The server.
    /// The server is starting, draining, or cannot reach its database. Retry.
    Unavailable,
    /// A bug. The response carries a request id to quote.
    Internal,
}

impl ApiCode {
    /// Every code, for exhaustive tests on both sides of the wire.
    pub const ALL: [Self; 25] = [
        Self::MalformedRequest,
        Self::CrossSiteRequest,
        Self::NotFound,
        Self::MethodNotAllowed,
        Self::TailnetIdentityRequired,
        Self::InvalidWorkerKey,
        Self::WorkerRevoked,
        Self::NotAMember,
        Self::WorkerOnly,
        Self::ServerAlreadyClaimed,
        Self::SetupCodeRequired,
        Self::SetupCodeMismatch,
        Self::SetupCodeExpired,
        Self::ServerOnly,
        Self::UnknownWorker,
        Self::EnrolmentTokenRequired,
        Self::EnrolmentTokenUnknown,
        Self::EnrolmentTokenExpired,
        Self::EnrolmentTokenRevoked,
        Self::EnrolmentTokenUsed,
        Self::WorkerKeyInUse,
        Self::EnrolmentTokenAlreadyRedeemed,
        Self::RateLimited,
        Self::Unavailable,
        Self::Internal,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_snake_case_on_the_wire() {
        let body = ApiErrorBody {
            error: ApiErrorDetail {
                code: ApiCode::EnrolmentTokenAlreadyRedeemed,
                message: "revoke the worker instead".to_owned(),
            },
        };
        let json = serde_json::to_string(&body).unwrap();
        insta::assert_snapshot!(
            json,
            @r#"{"error":{"code":"enrolment_token_already_redeemed","message":"revoke the worker instead"}}"#
        );
        assert_eq!(
            serde_json::from_str::<ApiErrorBody>(&json).unwrap(),
            body,
            "and read back"
        );
    }

    #[test]
    fn every_code_has_a_distinct_name() {
        let names: std::collections::BTreeSet<String> = ApiCode::ALL
            .iter()
            .map(|code| serde_json::to_string(code).unwrap())
            .collect();
        assert_eq!(names.len(), ApiCode::ALL.len(), "no two codes share a name");
        assert!(
            serde_json::from_str::<ApiCode>(r#""owner_only""#).is_err(),
            "an unknown code is an error"
        );
    }
}
