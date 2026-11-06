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
//!
//! Any other path outside `/api/` is the web app's ([`crate::web`]).

pub(crate) mod error;
pub(crate) mod types;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{
    DefaultBodyLimit, FromRequest, FromRequestParts, MatchedPath, Path, Request, State,
};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Json, Response};
use axum::routing::{delete, get, post};
use bytes::Bytes;
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use tracing::Instrument as _;
use tracing::field::Empty;

use abc_core::command::{
    ClaimServer, EnrolWorker, IssueEnrolmentToken, RevokeEnrolmentToken, RevokeWorker,
};
use abc_core::decision::Origin;
use abc_core::id::{CorrelationId, EnrolmentTokenId, WorkerId, WorkspaceId};
use abc_core::query::{ListEnrolmentTokens, SessionView, TokenSummary, WhoAmI};
use abc_protocol::api::ApiCode;
use abc_protocol::enrol::{ENROL_PATH, EnrolRequest, EnrolResponse};
use abc_protocol::link::CONNECT_PATH;
use abc_protocol::url::PublicUrl;
use abc_store::{Caller, Store};

use crate::limits::{Limits, RateLimiter};
use crate::listen::SidecarApp;
use crate::sessions::Sessions;
use crate::sidecar::{MaybeTailnetCaller, TailnetCaller};
use crate::web::{Answer, WebApp};
use crate::worker_link::LinkTimings;
use error::ApiError;
use types::{ClaimRequest, ClaimResponse, IssueTokenRequest, TokenIssuedResponse};

/// The largest request body accepted.
const BODY_LIMIT: usize = 64 * 1024;

/// How long a non-streaming request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared by every sidecar handler.
#[derive(Debug, Clone)]
pub(crate) struct SidecarState {
    pub(crate) store: Store,
    pub(crate) sessions: Sessions,
    /// The server's public tailnet URL (`ABC_PUBLIC_URL`). The identity extractor accepts only
    /// requests whose `X-Forwarded-Host`, which Serve sets, names it (DNS rebinding) and whose
    /// `Origin`, if any, equals it (CSRF).
    pub(crate) public_url: PublicUrl,
    pub(crate) link: LinkTimings,
    pub(crate) drain: Drain,
    pub(crate) limits: Limits,
    /// Enrolment attempts per source.
    pub(crate) enrolments: RateLimiter<String>,
    /// Session starts per worker.
    pub(crate) session_starts: RateLimiter<WorkerId>,
    /// The web app, which answers what no API route matches outside `/api/`.
    pub(crate) web: WebApp,
}

/// The server-wide signal that it is shutting down: live streams end when it fires.
#[derive(Debug, Clone)]
pub(crate) struct Drain(Arc<watch::Sender<bool>>);

impl Drain {
    pub(crate) fn new() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    pub(crate) fn start(&self) {
        self.0.send_replace(true);
    }

    /// Resolves once draining has started.
    pub(crate) async fn started(&self) {
        let mut draining = self.0.subscribe();
        // The sender lives as long as `self`.
        let _started = draining.wait_for(|draining| *draining).await;
    }
}

/// Builds the sidecar app: the routes; a request id per request, used as the events' correlation
/// id and echoed as `x-request-id`; a request span with method, route, status and latency (never
/// headers, raw URIs or bodies); a 64 KiB body limit; and a timeout on non-streaming routes.
pub(crate) fn sidecar_app(state: SidecarState) -> SidecarApp {
    let requests = Router::new()
        .route("/api/session", get(session))
        .route("/api/owner/claim", post(claim))
        .route(
            "/api/workspaces/{workspace}/enrolment-tokens",
            get(list_tokens).post(issue_token),
        )
        .route(
            "/api/workspaces/{workspace}/enrolment-tokens/{token}",
            delete(revoke_token),
        )
        .route(
            "/api/workspaces/{workspace}/workers/{worker}",
            delete(revoke_worker),
        )
        .route(ENROL_PATH, post(enrol))
        .layer(middleware::from_fn(deadline));
    let streams = Router::new()
        .route("/api/workspaces/{workspace}/live", get(crate::live::stream))
        .route(CONNECT_PATH, get(crate::worker_link::upgrade));
    let router = requests
        .merge(streams)
        .fallback(web_app)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(state.clone(), observe))
        .with_state(state);
    SidecarApp::new(router)
}

/// The web app's answer to a request no API route matched; the API's errors for what it does not
/// serve.
async fn web_app(State(app): State<SidecarState>, method: Method, uri: Uri) -> Response {
    match app.web.answer(&method, uri.path()) {
        Answer::File(response) => response,
        Answer::NotFound => ApiError::new(ApiCode::NotFound, "no such route").into_response(),
        Answer::MethodNotAllowed => method_not_allowed().await.into_response(),
    }
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        ApiCode::MethodNotAllowed,
        "this route does not take that method",
    )
}

/// Gives the request its id and span, and logs its end.
async fn observe(State(app): State<SidecarState>, mut request: Request, next: Next) -> Response {
    let id: CorrelationId = match app.store.fresh_id() {
        Ok(id) => id,
        Err(error) => return ApiError::from(error).into_response(),
    };
    request.extensions_mut().insert(RequestOrigin(id));
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let span = tracing::info_span!(
        "request",
        request_id = %id,
        method = %request.method(),
        route = %route,
        status = Empty,
        latency_ms = Empty,
        login = Empty,
        worker = Empty,
    );
    let started = Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    span.record("status", response.status().as_u16());
    span.record("latency_ms", latency);
    span.in_scope(|| tracing::info!("request finished"));
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// Fails a non-streaming request that takes longer than [`REQUEST_TIMEOUT`].
async fn deadline(request: Request, next: Next) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            ApiError::new(ApiCode::Unavailable, "the request took too long; retry").into_response()
        }
    }
}

/// The request's correlation id, minted by [`observe`].
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

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "axum's extractor trait is async; this one has nothing to wait for"
    )]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<Self>()
            .copied()
            .ok_or_else(|| ApiError::new(ApiCode::Internal, "the request has no id"))
    }
}

/// Where a request came from: the last `X-Forwarded-For` entry, which Serve appends with the
/// tailnet address it accepted the connection from (entries before it are the client's own claims).
/// Requests without one share a single source.
#[derive(Debug)]
pub(crate) struct Source(pub(crate) String);

impl<S: Send + Sync> FromRequestParts<S> for Source {
    type Rejection = std::convert::Infallible;

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "axum's extractor trait is async; this one has nothing to wait for"
    )]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let forwarded = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .rfind(|entry| !entry.is_empty());
        Ok(Self(forwarded.unwrap_or("unknown").to_owned()))
    }
}

/// `Path<T>` whose rejection is `malformed_request` in the API's error format.
#[derive(Debug)]
pub(crate) struct ApiPath<T>(pub(crate) T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for ApiPath<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .ok()
            .ok_or_else(|| ApiError::malformed("the path names an invalid id"))
    }
}

/// `Json<T>` that requires `Content-Type: application/json` (which forces a CORS preflight from
/// another site, and the server answers none) and whose rejection names the JSON path but never
/// quotes input: serde's own messages can quote values, and request bodies carry secrets.
#[derive(Debug)]
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let is_json = request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"));
        if !is_json {
            return Err(ApiError::malformed(
                "expected Content-Type: application/json",
            ));
        }
        let body = Bytes::from_request(request, state)
            .await
            .ok()
            .ok_or_else(|| {
                ApiError::malformed("the request body could not be read (64 KiB at most)")
            })?;
        let mut json = serde_json::Deserializer::from_slice(&body);
        let value = serde_path_to_error::deserialize(&mut json).map_err(|error| {
            if error.inner().is_data() {
                ApiError::malformed(format!("the request body is invalid at `{}`", error.path()))
            } else {
                ApiError::malformed("the request body is not valid JSON for this request")
            }
        })?;
        json.end()
            .ok()
            .ok_or_else(|| ApiError::malformed("the request body has data after the JSON"))?;
        Ok(Self(value))
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
    ApiPath(workspace): ApiPath<WorkspaceId>,
    ApiJson(body): ApiJson<IssueTokenRequest>,
) -> Result<([(HeaderName, HeaderValue); 1], Json<TokenIssuedResponse>), ApiError> {
    let command = IssueEnrolmentToken {
        workspace,
        label: body.label,
        ttl: body.ttl,
    };
    let issued = app
        .store
        .execute(&caller.into(), &origin.into(), command)
        .await?;
    // The one response that carries a secret is never stored by a cache.
    let no_store = [(CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(issued.into())))
}

async fn list_tokens(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    ApiPath(workspace): ApiPath<WorkspaceId>,
) -> Result<Json<Vec<TokenSummary>>, ApiError> {
    let tokens = app
        .store
        .query(&caller.into(), ListEnrolmentTokens { workspace })
        .await?;
    Ok(Json(tokens))
}

async fn revoke_token(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    origin: RequestOrigin,
    ApiPath((workspace, token)): ApiPath<(WorkspaceId, EnrolmentTokenId)>,
) -> Result<StatusCode, ApiError> {
    let command = RevokeEnrolmentToken { workspace, token };
    app.store
        .execute(&caller.into(), &origin.into(), command)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Live sessions notice on their next feed wake-up and close with 4403; there is no direct kick,
/// so a revocation from any path behaves the same way.
async fn revoke_worker(
    State(app): State<SidecarState>,
    caller: TailnetCaller,
    origin: RequestOrigin,
    ApiPath((workspace, worker)): ApiPath<(WorkspaceId, WorkerId)>,
) -> Result<StatusCode, ApiError> {
    let command = RevokeWorker { workspace, worker };
    app.store
        .execute(&caller.into(), &origin.into(), command)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Enrolment ignores any tailnet identity on the request: the token is the authority and the actor
/// is the token's reserved worker, so no ambient authority is used and CSRF cannot apply.
async fn enrol(
    State(app): State<SidecarState>,
    origin: RequestOrigin,
    source: Source,
    ApiJson(body): ApiJson<EnrolRequest>,
) -> Result<Json<EnrolResponse>, ApiError> {
    if !app.enrolments.allow(source.0) {
        return Err(ApiError::new(
            ApiCode::RateLimited,
            "too many enrolment attempts from this machine; wait a minute",
        ));
    }
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
