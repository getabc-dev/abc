//! Web-facing request, response and live-update types, generated to TypeScript (ts-rs) and JSON
//! Schema (schemars). They are never hand-written on the web side.
//!
//! [`WebApi`] is the generation root: it references every type the web app exchanges with the
//! server, and ts-rs and schemars follow references transitively. The test
//! `generated_bindings_are_current` renders both from this root and compares them with the
//! committed files under `web/src/generated/`; run it with `ABC_UPDATE_BINDINGS=1` to rewrite
//! them. A type left out of the root has no generated TypeScript, so the web build fails when it
//! is imported.

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
pub(crate) struct ClaimRequest {
    /// The setup code as typed: case, spaces and dashes are forgiven.
    #[ts(type = "string")]
    pub(crate) code: Secret<SetupCode>,
    /// How the owner wants to be shown.
    pub(crate) display_name: DisplayName,
    /// The first workspace's name.
    pub(crate) workspace_name: WorkspaceName,
}

/// The claim's answer.
#[derive(Debug, Serialize, TS, JsonSchema)]
pub(crate) struct ClaimResponse {
    /// The new owner.
    pub(crate) owner: PersonId,
    /// The first workspace.
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
pub(crate) struct IssueTokenRequest {
    /// An optional note, which also becomes the worker's name.
    pub(crate) label: Option<MachineLabel>,
    /// Seconds, 300 to 86400. Defaults to 1800.
    #[serde(default)]
    #[ts(as = "Option<TokenTtl>", optional)]
    pub(crate) ttl: TokenTtl,
}

/// The only response that carries a secret: the token, shown once. The web app shows it beside the
/// `abc worker enrol <url>` command, which asks for it on standard input so it never reaches a
/// command line or a shell's history, and never stores it.
#[derive(Debug, Serialize, TS, JsonSchema)]
pub(crate) struct TokenIssuedResponse {
    /// The token's id, for listing and revoking.
    pub(crate) id: EnrolmentTokenId,
    /// The token.
    #[serde(serialize_with = "serialize_exposed")]
    #[ts(type = "string")]
    pub(crate) token: Secret<EnrolmentToken>,
    /// When it stops working.
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
pub(crate) enum LiveUpdate {
    /// The first message on a connection without `Last-Event-ID`: the full state. Replace, don't
    /// merge.
    Snapshot {
        /// The log head the cards reflect.
        head: Position,
        /// Every machine.
        machines: Vec<MachineCard>,
    },
    /// Cards that changed in the log after the previous `head` (or after `Last-Event-ID` on a
    /// resumed connection). Upsert by `worker`.
    Machines {
        /// The log head the cards reflect.
        head: Position,
        /// The changed cards.
        changed: Vec<MachineCard>,
    },
    /// The viewer lost access. The stream ends.
    AccessEnded {
        /// The last head the viewer saw.
        head: Position,
    },
}

impl LiveUpdate {
    /// The SSE `event:` name.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Snapshot { .. } => "snapshot",
            Self::Machines { .. } => "machines",
            Self::AccessEnded { .. } => "access_ended",
        }
    }

    /// The SSE `id:`: the head the message reflects.
    pub(crate) const fn head(&self) -> Position {
        match self {
            Self::Snapshot { head, .. }
            | Self::Machines { head, .. }
            | Self::AccessEnded { head } => *head,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

    /// Where the generated files are committed.
    fn committed() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/generated")
    }

    /// The schema's file name, beside the TypeScript.
    const SCHEMA: &str = "web-api.schema.json";

    /// Every generated file, by name.
    fn render() -> BTreeMap<String, String> {
        let dir = tempfile::tempdir().unwrap();
        let config = ts_rs::Config::new().with_out_dir(dir.path());
        WebApi::export_all(&config).unwrap();
        let mut files: BTreeMap<String, String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                (name, fs::read_to_string(entry.path()).unwrap())
            })
            .collect();
        let schema = schemars::schema_for!(WebApi);
        let mut text = serde_json::to_string_pretty(&schema).unwrap();
        text.push('\n');
        files.insert(SCHEMA.to_owned(), text);
        files
    }

    fn read_committed() -> BTreeMap<String, String> {
        fs::read_dir(committed())
            .map(|entries| {
                entries
                    .map(|entry| {
                        let entry = entry.unwrap();
                        let name = entry.file_name().into_string().unwrap();
                        (name, fs::read_to_string(entry.path()).unwrap())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn generated_bindings_are_current() {
        let rendered = render();
        if std::env::var_os("ABC_UPDATE_BINDINGS").is_some_and(|value| value == "1") {
            let dir = committed();
            let _absent = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            for (name, text) in &rendered {
                fs::write(dir.join(name), text).unwrap();
            }
        }
        let on_disk = read_committed();
        assert_eq!(
            on_disk.keys().collect::<Vec<_>>(),
            rendered.keys().collect::<Vec<_>>(),
            "web/src/generated/ has exactly the generated files; run the test with \
             ABC_UPDATE_BINDINGS=1 to rewrite them"
        );
        for (name, text) in &rendered {
            assert_eq!(
                on_disk.get(name),
                Some(text),
                "web/src/generated/{name} is stale; run the test with ABC_UPDATE_BINDINGS=1"
            );
        }
    }

    #[test]
    fn live_updates_name_their_event_and_head() {
        let head = Position::from_stored(7).unwrap();
        let updates = [
            LiveUpdate::Snapshot {
                head,
                machines: Vec::new(),
            },
            LiveUpdate::Machines {
                head,
                changed: Vec::new(),
            },
            LiveUpdate::AccessEnded { head },
        ];
        for update in &updates {
            let json = serde_json::to_value(update).unwrap();
            assert_eq!(json["type"], update.name(), "the SSE event is the tag");
            assert_eq!(update.head(), head, "the SSE id is the head");
        }
    }
}
