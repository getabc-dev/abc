//! Enrolment: how a machine becomes a worker, once, over HTTP.
//!
//! ```text
//! worker                                         server (POST /api/enrol, sidecar listener)
//!   key = WorkerKey::generate(OsMint)
//!   save { server, key } to worker.toml (0600)   ── BEFORE sending, so a crash can retry
//!   EnrolRequest { token, key: digest(key), .. } ──▶ EnrolWorker under the log-head lock
//!                                                ◀── 200 EnrolResponse { worker, workspace, name }
//!   save worker id; done                              or 4xx ApiErrorBody (token expired/used/…)
//!
//!   network error or timeout: send the SAME request again. The same token with the same key
//!   digest returns the same EnrolResponse and records nothing new.
//! ```
//!
//! The response carries no secret. Losing it costs a retry, not a token.

use abc_core::credential::{ApprovalCode, EnrolmentToken, WorkerKeyDigest};
use abc_core::id::{EnrolmentRequestId, WorkerId, WorkspaceId};
use abc_core::identity::{DisplayName, MachineInfo};
use abc_core::secret::{Secret, serialize_exposed};
use abc_core::time::Timestamp;
use semver::Version;
use serde::{Deserialize, Serialize};

/// The enrolment endpoint, relative to the server's tailnet URL. Requests carry
/// `Content-Type: application/json`.
pub const ENROL_PATH: &str = "/api/enrol";

/// The path of a workspace's enrolment requests, with `{workspace}` as the workspace's id:
/// `POST` asks to enrol (an [`ApprovalRequest`], answered with an [`ApprovalRequested`]).
pub const REQUESTS_PATH: &str = "/api/workspaces/{workspace}/enrolment-requests";

/// The path a waiting machine polls, with `{request}` as the request's id. It answers an
/// `abc_core::query::RequestState`, and only to the login and node that made the request.
pub const REQUEST_STATUS_PATH: &str = "/api/enrolment-requests/{request}";

/// The page where the owner approves a request, relative to the server's URL: the machine prints
/// it with the code. The web app serves it.
#[must_use]
pub fn approve_path(workspace: WorkspaceId, request: EnrolmentRequestId) -> String {
    format!("/machines/requests/{workspace}/{request}")
}

/// The worker's enrolment request.
#[derive(Debug, Serialize, Deserialize)]
pub struct EnrolRequest {
    /// The token the owner gave the user. Serialized deliberately: this request exists to carry it.
    #[serde(serialize_with = "serialize_exposed")]
    pub token: Secret<EnrolmentToken>,
    /// The digest of the key the worker generated and already saved.
    pub key: WorkerKeyDigest,
    /// The machine.
    pub machine: MachineInfo,
    /// The worker's abc release.
    pub build: Version,
}

/// A machine asking to be enrolled, to be approved in the browser. The key it generated stays on
/// the machine: only its digest travels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The digest of the key the worker generated and already saved.
    pub key: WorkerKeyDigest,
    /// The machine.
    pub machine: MachineInfo,
    /// The worker's abc release.
    pub build: Version,
}

/// What the machine shows while it waits for approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequested {
    /// The request, which the machine polls.
    pub request: EnrolmentRequestId,
    /// The code the Approve page shows too.
    pub code: ApprovalCode,
    /// When the request stops waiting.
    pub expires_at: Timestamp,
}

/// A successful enrolment. Identical for the first call and for every retry with the same key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrolResponse {
    /// The worker's id.
    pub worker: WorkerId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Its display name.
    pub name: DisplayName,
}

#[cfg(test)]
mod tests {
    use abc_core::credential::WorkerKey;
    use abc_core::decision::Mint;
    use abc_core::identity::{Arch, Os};
    use abc_core::time::Timestamp;
    use zeroize::Zeroizing;

    use super::*;

    #[derive(Debug)]
    struct Fixed;

    impl Mint for Fixed {
        fn uuid(&self, _now: Timestamp) -> uuid::Uuid {
            uuid::Uuid::nil()
        }

        fn secret(&self) -> Zeroizing<[u8; 32]> {
            Zeroizing::new([0; 32])
        }
    }

    #[test]
    fn the_request_carries_the_token_and_only_the_keys_digest() {
        let key = WorkerKey::generate(&Fixed);
        let token = format!("abc_enr_{}", "0".repeat(52));
        let request = EnrolRequest {
            token: EnrolmentToken::parse(&token).unwrap(),
            key: key.expose_secret().digest(),
            machine: MachineInfo {
                hostname: DisplayName::parse("mbp").unwrap(),
                os: Os::Macos,
                arch: Arch::Aarch64,
            },
            build: Version::new(0, 1, 0),
        };
        let json = serde_json::to_string(&request).unwrap();
        let digest = serde_json::to_string(&key.expose_secret().digest()).unwrap();
        assert_eq!(
            digest.len(),
            66,
            "the key travels as 64 hex characters of its digest"
        );
        insta::assert_snapshot!(
            json.replace(&digest, "\"<digest of the key>\""),
            @r#"{"token":"abc_enr_0000000000000000000000000000000000000000000000000000","key":"<digest of the key>","machine":{"hostname":"mbp","os":"macos","arch":"aarch64"},"build":"0.1.0"}"#
        );
        assert!(
            !json.contains(key.expose_secret().as_str()),
            "the key never leaves the worker"
        );
        let decoded: EnrolRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.key, request.key, "the digest reads back");
        assert_eq!(
            decoded.token.expose_secret().digest(),
            request.token.expose_secret().digest(),
            "and the token"
        );
    }

    #[test]
    fn the_response_names_the_worker() {
        let response = EnrolResponse {
            worker: abc_core::id::FromUuid::from_uuid(uuid::Uuid::from_u128(1)),
            workspace: abc_core::id::FromUuid::from_uuid(uuid::Uuid::from_u128(2)),
            name: DisplayName::parse("mbp").unwrap(),
        };
        let json = serde_json::to_string(&response).unwrap();
        insta::assert_snapshot!(
            json,
            @r#"{"worker":"00000000-0000-0000-0000-000000000001","workspace":"00000000-0000-0000-0000-000000000002","name":"mbp"}"#
        );
        assert_eq!(
            serde_json::from_str::<EnrolResponse>(&json).unwrap(),
            response,
            "and reads back"
        );
    }

    #[test]
    fn the_approve_path_names_the_workspace_and_the_request() {
        use abc_core::id::FromUuid as _;
        let workspace = WorkspaceId::from_uuid(uuid::Uuid::from_u128(1));
        let request = EnrolmentRequestId::from_uuid(uuid::Uuid::from_u128(2));
        assert_eq!(
            approve_path(workspace, request),
            "/machines/requests/00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002"
        );
    }
}
