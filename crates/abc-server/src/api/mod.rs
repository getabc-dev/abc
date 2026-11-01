//! The sidecar API: every route a person or a worker reaches over the tailnet.
//!
//! Every mutating handler has one shape: extract, build an abc-core command, `store.execute`,
//! convert. Every reading handler: extract, build a query, `store.query`, convert. Handlers hold
//! no rules, so the whole policy stays in `abc_core::policy`.
//!
//! | Method and path | Caller | Store call |
//! | --- | --- | --- |
//! | `GET /api/session` | tailnet or none | `query(WhoAmI)` |
//! | `POST /api/owner/claim` | tailnet | `execute(ClaimServer)` |
//! | `GET /api/workspaces/{ws}/enrolment-tokens` | tailnet | `query(ListEnrolmentTokens)` |
//! | `POST /api/workspaces/{ws}/enrolment-tokens` | tailnet | `execute(IssueEnrolmentToken)` |
//! | `DELETE /api/workspaces/{ws}/enrolment-tokens/{id}` | tailnet | `execute(RevokeEnrolmentToken)` |
//! | `DELETE /api/workspaces/{ws}/workers/{id}` | tailnet | `execute(RevokeWorker)` |
//! | `GET /api/workspaces/{ws}/live` (SSE) | tailnet | `query(ListMachines)`, then `MachinesSince` |
//! | `POST /api/enrol` | anonymous (the token is the authority) | `execute(EnrolWorker)` |
//! | `GET /api/worker/connect` (WebSocket) | worker key | `query(WorkerSelf)`, `execute(Start/EndSession)` |

pub(crate) mod error;
pub(crate) mod types;

use axum::Json;
use axum::extract::{FromRequest, FromRequestParts, Path, State};
use axum::http::request::Parts;

use abc_core::command::{
    ClaimServer, EnrolWorker, IssueEnrolmentToken, RevokeEnrolmentToken, RevokeWorker,
};
use abc_core::decision::Origin;
use abc_core::id::{CorrelationId, EnrolmentTokenId, WorkerId, WorkspaceId};
use abc_core::query::{ListEnrolmentTokens, SessionView, TokenSummary, WhoAmI};
use abc_protocol::enrol::{EnrolRequest, EnrolResponse};
use abc_store::{Caller, Store};

use crate::listen::SidecarApp;
use crate::sessions::Sessions;
use crate::sidecar::{MaybeTailnetCaller, TailnetCaller};
use error::ApiError;
use types::{ClaimRequest, ClaimResponse, IssueTokenRequest, TokenIssuedResponse};

/// Shared by every sidecar handler.
#[derive(Debug, Clone)]
pub(crate) struct SidecarState {
    pub(crate) store: Store,
    pub(crate) sessions: Sessions,
    /// The server's public tailnet URL (`ABC_PUBLIC_URL`). The identity extractor accepts only
    /// requests whose `Host` names it (DNS rebinding) and whose `Origin`, if any, equals it (CSRF).
    pub(crate) public_url: PublicUrl,
}

/// `https://<name>.<tailnet>.ts.net`, validated at start: https, a host, no path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicUrl {
    origin: String,
    host: String,
}

impl PublicUrl {
    /// Parses `ABC_PUBLIC_URL`.
    ///
    /// # Errors
    ///
    /// [`crate::ServeError::PublicUrl`] naming what is wrong (never a secret: it is a URL).
    pub fn parse(raw: &str) -> Result<Self, crate::ServeError> {
        let _ = raw;
        unimplemented!()
    }
}

/// Builds the sidecar app: routes, the request-id layer (a UUIDv7 per request, echoed as
/// `x-request-id`, used as the events' correlation id), the trace layer (method, `MatchedPath`,
/// status, latency, request id; never headers, raw URIs or bodies), a 64 KiB body limit and
/// timeouts on non-streaming routes.
pub(crate) fn sidecar_app(state: SidecarState) -> SidecarApp {
    // Router::new()
    //   .route("/api/session", get(session))
    //   .route("/api/owner/claim", post(claim))
    //   .route("/api/workspaces/{workspace}/enrolment-tokens", get(list_tokens).post(issue_token))
    //   .route("/api/workspaces/{workspace}/enrolment-tokens/{token}", delete(revoke_token))
    //   .route("/api/workspaces/{workspace}/workers/{worker}", delete(revoke_worker))
    //   .route("/api/workspaces/{workspace}/live", get(crate::live::stream))
    //   .route(abc_protocol::enrol::ENROL_PATH, post(enrol))
    //   .route(abc_protocol::link::CONNECT_PATH, get(crate::worker_link::upgrade))
    //   .fallback(|| async { ApiError::not_found() })
    //   .layer(...).with_state(state)
    let _ = state;
    unimplemented!()
}

/// The request's correlation id, minted by the request-id layer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestOrigin(pub(crate) CorrelationId);

impl From<RequestOrigin> for Origin {
    fn from(origin: RequestOrigin) -> Self {
        Self {
            correlation: origin.0,
            causation: None,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequestOrigin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        let _ = parts;
        unimplemented!("read the id the request-id layer put in extensions")
    }
}

/// `Json<T>` whose rejection is `malformed_request` with a message that names the JSON path but
/// never quotes input. serde's own messages can quote values, and request bodies carry secrets.
#[derive(Debug)]
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T: serde::de::DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;
    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        let _ = (req, state);
        unimplemented!(
            "requires Content-Type: application/json (forces a CORS preflight cross-site)"
        )
    }
}

async fn session(
    State(app): State<SidecarState>,
    MaybeTailnetCaller(caller): MaybeTailnetCaller,
) -> Result<Json<SessionView>, ApiError> {
    Ok(Json(app.store.query(&caller, WhoAmI).await?))
}

async fn claim(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    origin: RequestOrigin,
    ApiJson(body): ApiJson<ClaimRequest>,
) -> Result<Json<ClaimResponse>, ApiError> {
    let command = ClaimServer {
        code: body.code,
        display_name: body.display_name,
        workspace_name: body.workspace_name,
    };
    let claimed = app
        .store
        .execute(&caller.into(), &origin.into(), command)
        .await?;
    Ok(Json(claimed.into()))
}

async fn issue_token(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    origin: RequestOrigin,
    Path(workspace): Path<WorkspaceId>,
    ApiJson(body): ApiJson<IssueTokenRequest>,
) -> Result<Json<TokenIssuedResponse>, ApiError> {
    let command = IssueEnrolmentToken {
        workspace,
        label: body.label,
        ttl: body.ttl,
    };
    let issued = app
        .store
        .execute(&caller.into(), &origin.into(), command)
        .await?;
    Ok(Json(issued.into()))
}

async fn list_tokens(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    Path(workspace): Path<WorkspaceId>,
) -> Result<Json<Vec<TokenSummary>>, ApiError> {
    Ok(Json(
        app.store
            .query(&caller.into(), ListEnrolmentTokens { workspace })
            .await?,
    ))
}

async fn revoke_token(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    origin: RequestOrigin,
    Path((workspace, token)): Path<(WorkspaceId, EnrolmentTokenId)>,
) -> Result<axum::http::StatusCode, ApiError> {
    app.store
        .execute(
            &caller.into(),
            &origin.into(),
            RevokeEnrolmentToken { workspace, token },
        )
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn revoke_worker(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    origin: RequestOrigin,
    Path((workspace, worker)): Path<(WorkspaceId, WorkerId)>,
) -> Result<axum::http::StatusCode, ApiError> {
    app.store
        .execute(
            &caller.into(),
            &origin.into(),
            RevokeWorker { workspace, worker },
        )
        .await?;
    // Live sessions notice on their next LogFeed wake-up and close with 4403; there is no direct
    // kick, so revocation from any path behaves the same way.
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Enrolment ignores any tailnet identity on the request: the token is the authority and the actor
/// is the token's reserved worker, so no ambient authority is used and CSRF cannot apply.
async fn enrol(
    State(app): State<SidecarState>,
    origin: RequestOrigin,
    ApiJson(body): ApiJson<EnrolRequest>,
) -> Result<Json<EnrolResponse>, ApiError> {
    let command = EnrolWorker {
        token: body.token,
        key: body.key,
        machine: body.machine,
        build: body.build,
    };
    let enrolled = app
        .store
        .execute(&Caller::Anonymous, &origin.into(), command)
        .await?;
    Ok(Json(EnrolResponse {
        worker: enrolled.worker,
        workspace: enrolled.workspace,
        name: enrolled.name,
    }))
}
