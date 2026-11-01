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

use abc_core::credential::{EnrolmentToken, WorkerKeyDigest};
use abc_core::id::{WorkerId, WorkspaceId};
use abc_core::identity::{DisplayName, MachineInfo};
use abc_core::secret::{Secret, serialize_exposed};
use semver::Version;
use serde::{Deserialize, Serialize};

/// The enrolment endpoint, relative to the server's tailnet URL.
pub const ENROL_PATH: &str = "/api/enrol";

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
