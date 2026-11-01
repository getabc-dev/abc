//! Turning sidecar headers into a `Caller`: the only place in abc that reads `Tailscale-User-*`.
//!
//! [`TailnetCaller`] implements `FromRequestParts<SidecarState>` and nothing else, so a handler
//! that asks for one compiles only on the sidecar router, and the sidecar router is served only on
//! the sidecar socket (see `listen`).
//!
//! **CSRF and DNS rebinding.** Identity injected by the proxy is ambient authority, like a cookie:
//! any web page the user visits can make their browser send requests to the abc URL, and Serve will
//! attach their identity. So the extractor refuses with `cross_site_request`:
//! * any request whose `Host` is not the configured public host (a rebinding domain resolves to
//!   the server but cannot make the browser send the right `Host`);
//! * for unsafe methods (anything but GET, HEAD, OPTIONS): `Sec-Fetch-Site` present and not
//!   `same-origin`, or `Origin` present and not the public origin.
//!
//! Non-browser clients send neither `Origin` nor `Sec-Fetch-Site` and pass. `ApiJson` requires
//! `Content-Type: application/json`, which forces a CORS preflight, and the server answers none, so
//! a cross-origin page can neither send a JSON body nor read a response. Doing this inside the
//! extractor means a handler cannot hold a `TailnetCaller` from a cross-site request.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use abc_core::identity::{DisplayName, TailnetLogin};
use abc_store::Caller;

use crate::api::SidecarState;
use crate::api::error::ApiError;

/// The header Tailscale Serve sets with the user's login.
pub(crate) const LOGIN_HEADER: &str = "tailscale-user-login";
/// The header Tailscale Serve sets with the user's display name.
pub(crate) const NAME_HEADER: &str = "tailscale-user-name";

/// A tailnet person, as the sidecar asserted, on a same-origin request.
#[derive(Debug, Clone)]
pub(crate) struct TailnetCaller {
    pub(crate) login: TailnetLogin,
    /// From `Tailscale-User-Name`, used only as the claim form's default display name.
    pub(crate) name: Option<DisplayName>,
}

impl From<TailnetCaller> for Caller {
    fn from(caller: TailnetCaller) -> Self {
        Self::Tailnet(caller.login)
    }
}

impl FromRequestParts<SidecarState> for TailnetCaller {
    type Rejection = ApiError;

    /// 1. Host and same-origin guards against `state.public_url` (as above) -> `cross_site_request`.
    /// 2. Exactly one `Tailscale-User-Login` -> `TailnetLogin::parse`. A missing header (tagged
    ///    device, Funnel) gives `tailnet_identity_required`; a duplicate or invalid one gives
    ///    `malformed_request`.
    /// 3. Records `actor_login` on the request span (logins are identities, not secrets).
    async fn from_request_parts(parts: &mut Parts, state: &SidecarState) -> Result<Self, ApiError> {
        let _ = (parts, &state.public_url);
        unimplemented!()
    }
}

/// Like [`TailnetCaller`], but a missing identity yields `Caller::Anonymous` instead of an error.
/// Used by `GET /api/session`, so the web app can show "no tailnet identity" on a tagged device.
#[derive(Debug)]
pub(crate) struct MaybeTailnetCaller(pub(crate) Caller);

impl FromRequestParts<SidecarState> for MaybeTailnetCaller {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &SidecarState) -> Result<Self, ApiError> {
        let _ = (parts, state);
        unimplemented!()
    }
}
