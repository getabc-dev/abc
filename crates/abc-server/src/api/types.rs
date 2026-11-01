//! Web-facing request, response and live-update types, generated to TypeScript (ts-rs) and JSON
//! Schema (schemars). They are never hand-written on the web side.
//!
//! [`WebApi`] is the generation root: it references every type the web app exchanges with the
//! server, and ts-rs and schemars follow references transitively. The test
//! `generated_bindings_are_current` renders both from this root and compares them with the
//! committed files under `web/src/generated/`. Run it with `ABC_UPDATE_BINDINGS=1` to rewrite them.
//! A type left out of the root has no generated TypeScript, so the web build fails when it is
//! imported.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use abc_core::command::{Claimed, TokenIssued, TokenTtl};
use abc_core::credential::{EnrolmentToken, SetupCode};
use abc_core::event::Position;
use abc_core::id::{EnrolmentTokenId, PersonId, WorkspaceId};
use abc_core::identity::{DisplayName, MachineLabel, WorkspaceName};
use abc_core::query::{MachineCard, SessionView, TokenSummary};
use abc_core::secret::{Secret, serialize_exposed};
use abc_core::time::Timestamp;
use abc_protocol::api::ApiErrorBody;

/// The generation root. Never constructed.
#[derive(TS, JsonSchema)]
#[ts(export)]
#[expect(
    dead_code,
    reason = "a type-level index of the web API for ts-rs and schemars, never built"
)]
pub(crate) struct WebApi {
    session: SessionView,
    claim_request: ClaimRequest,
    claim_response: ClaimResponse,
    issue_token_request: IssueTokenRequest,
    token_issued: TokenIssuedResponse,
    tokens: Vec<TokenSummary>,
    live: LiveUpdate,
    error: ApiErrorBody,
}

/// `POST /api/owner/claim`.
#[derive(Debug, Deserialize, TS, JsonSchema)]
#[ts(export)]
pub(crate) struct ClaimRequest {
    /// As typed: case, spaces and dashes are forgiven.
    pub(crate) code: Secret<SetupCode>,
    pub(crate) display_name: DisplayName,
    pub(crate) workspace_name: WorkspaceName,
}

#[derive(Debug, Serialize, TS, JsonSchema)]
#[ts(export)]
pub(crate) struct ClaimResponse {
    pub(crate) owner: PersonId,
    pub(crate) workspace: WorkspaceId,
}

impl From<Claimed> for ClaimResponse {
    fn from(claimed: Claimed) -> Self {
        Self {
            owner: claimed.owner,
            workspace: claimed.workspace,
        }
    }
}

/// `POST /api/workspaces/{ws}/enrolment-tokens`.
#[derive(Debug, Deserialize, TS, JsonSchema)]
#[ts(export)]
pub(crate) struct IssueTokenRequest {
    pub(crate) label: Option<MachineLabel>,
    /// Seconds, 300 to 86400. Defaults to 1800.
    #[serde(default)]
    pub(crate) ttl: TokenTtl,
}

/// The only response that carries a secret: the token, shown once. The web app renders it as a
/// ready-to-paste `abc worker enrol <url> --token …` command and never stores it.
#[derive(Debug, Serialize, TS, JsonSchema)]
#[ts(export)]
pub(crate) struct TokenIssuedResponse {
    pub(crate) id: EnrolmentTokenId,
    #[serde(serialize_with = "serialize_exposed")]
    pub(crate) token: Secret<EnrolmentToken>,
    pub(crate) expires_at: Timestamp,
}

impl From<TokenIssued> for TokenIssuedResponse {
    fn from(issued: TokenIssued) -> Self {
        Self {
            id: issued.id,
            token: issued.token,
            expires_at: issued.expires_at,
        }
    }
}

/// One message on `GET /api/workspaces/{ws}/live` (SSE `data:`, with `event:` set to the tag and
/// `id:` set to `head`). Everything, online-ness included, comes from the log through views, so
/// one cursor (`head`) covers it all.
#[derive(Debug, Serialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub(crate) enum LiveUpdate {
    /// First message on a connection without `Last-Event-ID`: the full state. Replace, don't merge.
    Snapshot {
        head: Position,
        machines: Vec<MachineCard>,
    },
    /// Cards that changed in the log after the previous `head` (or after `Last-Event-ID` on a
    /// resumed connection). Upsert by `worker`.
    Machines {
        head: Position,
        changed: Vec<MachineCard>,
    },
    /// The viewer lost access (removed, or the workspace is gone). The stream ends.
    AccessEnded,
}
