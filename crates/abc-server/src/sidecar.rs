//! Turning sidecar headers into a `Caller`: the only place in abc that reads `Tailscale-User-*`.
//!
//! [`TailnetCaller`] implements `FromRequestParts<SidecarState>` and nothing else, so a handler
//! that asks for one compiles only on the sidecar router, and the sidecar router is served only on
//! the sidecar socket (see `listen`).
//!
//! **CSRF and DNS rebinding.** Identity injected by the proxy is ambient authority, like a cookie:
//! any web page the user visits can make their browser send requests to the abc URL, and Serve will
//! attach their identity. So the extractor refuses with `cross_site_request`:
//! * any request that does not carry what Serve sends a `unix:` target: `Host: localhost`, the
//!   public host in `X-Forwarded-Host` and `X-Forwarded-Proto: https`, each once. Serve rewrites
//!   `Host` to `localhost` and sets both forwarded headers itself, replacing any a client sent
//!   (`ipn/ipnlocal/serve.go`), so `X-Forwarded-Host` is the `Host` the browser sent and is as
//!   trustworthy as the identity headers. A rebinding domain resolves to the server but cannot
//!   make the browser send the public host, so it fails this check;
//! * for unsafe methods: `Sec-Fetch-Site` present and not `same-origin`, or `Origin` present and
//!   not the public origin.
//!
//! Non-browser clients send neither `Origin` nor `Sec-Fetch-Site` and pass. `ApiJson` requires
//! `Content-Type: application/json`, which forces a CORS preflight, and the server answers none, so
//! a cross-origin page can neither send a JSON body nor read a response. Doing this inside the
//! extractor means a handler cannot hold a `TailnetCaller` from a cross-site request.

use axum::extract::FromRequestParts;
use axum::http::header::{HOST, ORIGIN};
use axum::http::request::Parts;

use abc_core::identity::{Requester, TailnetLogin};
use abc_store::Caller;

use abc_protocol::api::ApiCode;

use crate::api::error::ApiError;
use crate::api::{SidecarState, Source};
use crate::whois::WhoisError;

/// The header Tailscale Serve sets with the user's login.
const LOGIN_HEADER: &str = "tailscale-user-login";

/// The header browsers set to say which site a request came from.
const FETCH_SITE_HEADER: &str = "sec-fetch-site";

/// A tailnet person, as the sidecar asserted, on a same-origin request.
#[derive(Debug, Clone)]
pub(crate) struct TailnetCaller {
    pub(crate) login: TailnetLogin,
}

impl From<TailnetCaller> for Caller {
    fn from(caller: TailnetCaller) -> Self {
        Self::Tailnet(caller.login)
    }
}

/// The `Host` Serve sends to a `unix:` target, whatever host the client named.
const PROXIED_HOST: &str = "localhost";

/// The header Serve sets to the `Host` the client sent.
const FORWARDED_HOST_HEADER: &str = "x-forwarded-host";

/// The header Serve sets to `https` for a request that arrived over TLS.
const FORWARDED_PROTO_HEADER: &str = "x-forwarded-proto";

/// The value of header `name` when the request carries it exactly once.
fn single<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
    let mut values = parts.headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    }
}

/// Refuses a request that did not come through Serve for the public URL, or an unsafe one sent
/// from another site.
fn same_origin(parts: &Parts, state: &SidecarState) -> Result<(), ApiError> {
    if !single(parts, HOST.as_str()).is_some_and(|host| host.eq_ignore_ascii_case(PROXIED_HOST)) {
        return Err(ApiError::cross_site(
            "this request did not come through Tailscale Serve, which sends Host: localhost",
        ));
    }
    if !single(parts, FORWARDED_HOST_HEADER)
        .is_some_and(|host| host.eq_ignore_ascii_case(state.public_url.host()))
    {
        return Err(ApiError::cross_site(
            "this request names a host other than the server's public URL",
        ));
    }
    if single(parts, FORWARDED_PROTO_HEADER) != Some("https") {
        return Err(ApiError::cross_site(
            "this request did not reach Tailscale Serve over HTTPS",
        ));
    }
    if parts.method.is_safe() {
        return Ok(());
    }
    let site = parts.headers.get(FETCH_SITE_HEADER);
    if site.is_some_and(|site| site.as_bytes() != b"same-origin") {
        return Err(ApiError::cross_site("this request came from another site"));
    }
    let origin = parts.headers.get(ORIGIN);
    if origin.is_some_and(|origin| origin.as_bytes() != state.public_url.origin().as_bytes()) {
        return Err(ApiError::cross_site(
            "this request came from another origin",
        ));
    }
    Ok(())
}

/// The login Serve asserted: none for a tagged device or Funnel, and an error for a duplicate or
/// malformed header.
fn asserted_login(parts: &Parts) -> Result<Option<TailnetLogin>, ApiError> {
    let mut values = parts.headers.get_all(LOGIN_HEADER).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => value
            .to_str()
            .ok()
            .and_then(|login| TailnetLogin::parse(login).ok())
            .map(|login| {
                tracing::Span::current().record("login", login.as_str());
                Some(login)
            })
            .ok_or_else(|| ApiError::malformed("the Tailscale-User-Login header is malformed")),
        (Some(_), Some(_)) => Err(ApiError::malformed(
            "the request carries more than one Tailscale-User-Login header",
        )),
    }
}

impl FromRequestParts<SidecarState> for TailnetCaller {
    type Rejection = ApiError;

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "axum's extractor trait is async; this one has nothing to wait for"
    )]
    async fn from_request_parts(parts: &mut Parts, state: &SidecarState) -> Result<Self, ApiError> {
        same_origin(parts, state)?;
        asserted_login(parts)?
            .map(|login| Self { login })
            .ok_or_else(|| {
                ApiError::new(
                    ApiCode::TailnetIdentityRequired,
                    "this needs a signed-in tailnet person; tagged devices have no identity",
                )
            })
    }
}

/// Like [`TailnetCaller`], but a missing identity yields `Caller::Anonymous` instead of an error.
/// Used by `GET /api/session`, so the web app can show "no tailnet identity" on a tagged device.
#[derive(Debug)]
pub(crate) struct MaybeTailnetCaller(pub(crate) Caller);

impl FromRequestParts<SidecarState> for MaybeTailnetCaller {
    type Rejection = ApiError;

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "axum's extractor trait is async; this one has nothing to wait for"
    )]
    async fn from_request_parts(parts: &mut Parts, state: &SidecarState) -> Result<Self, ApiError> {
        same_origin(parts, state)?;
        Ok(Self(
            asserted_login(parts)?.map_or(Caller::Anonymous, Caller::Tailnet),
        ))
    }
}

/// A tailnet person and the node their request came from: the login Serve asserted, and the node
/// abc's own tailscaled says owns the address Serve forwarded. Only enrolment requests need it.
#[derive(Debug, Clone)]
pub(crate) struct TailnetRequester(pub(crate) Requester);

impl FromRequestParts<SidecarState> for TailnetRequester {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &SidecarState) -> Result<Self, ApiError> {
        let TailnetCaller { login } = TailnetCaller::from_request_parts(parts, state).await?;
        let Ok(Source(source)) = Source::from_request_parts(parts, state).await;
        match state.whois.node(&source, &login).await {
            Ok(node) => Ok(Self(Requester { login, node })),
            Err(WhoisError::Off) => Err(ApiError::new(
                ApiCode::ApprovalUnavailable,
                "this server cannot approve a machine in the browser; enrol it with a token: \
                 abc worker enrol --token-file",
            )),
            Err(error) => {
                tracing::warn!(%error, "could not name the node a request came from");
                Err(ApiError::new(
                    ApiCode::TailnetNodeUnknown,
                    "the server could not tell which tailnet node this request came from; check \
                     that this machine is on the tailnet and that the server's Tailscale is \
                     running",
                ))
            }
        }
    }
}
