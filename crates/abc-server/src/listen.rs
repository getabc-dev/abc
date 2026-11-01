//! Listeners, and the rule that keeps identity off every path but one (D4).
//!
//! Tailscale Serve sets `Tailscale-User-*` headers and strips copies sent by clients, but anyone
//! who reaches the backend another way can set them. So:
//!
//! 1. **Identity routes can only be served on the sidecar socket.** [`SidecarApp`] wraps a private
//!    `Router`, built only by `api::sidecar_app`, and the only function that serves it is
//!    [`serve_sidecar`], which takes a [`SidecarSocket`]. No code path serves it over TCP. (Plain
//!    axum would allow that: `Router::with_state` erases the state type, so the newtype is what
//!    keeps the distinction.)
//! 2. **Only the sidecar can reach the socket.** [`SidecarSocket::bind`] refuses a parent directory
//!    with any "other" permission bit, removes a stale socket file, binds, and sets the socket to
//!    0660. In the compose file the directory is a volume mounted into exactly two containers: abc
//!    and `tailscale/tailscale`, whose serve config proxies `https://<name>.<tailnet>.ts.net/` to
//!    `unix:/run/abc-sidecar/abc.sock`.
//! 3. **There is no TCP listener with identity**, so a stray `ports:` mapping or a `0.0.0.0` bind
//!    exposes health checks at most, never forgeable identity.
//! 4. **Defence in depth against a shortcut.** `TailnetCaller` is `FromRequestParts<SidecarState>`
//!    only, so identity handlers cannot be mounted on the ops router (state `Readiness`). The ops
//!    router is built inside `ops::serve`, so no function serves a caller-supplied router over TCP.
//!    `clippy.toml` gains `disallowed-methods = ["axum::serve"]`, so code cannot bypass both
//!    newtypes by serving a router directly.

use std::path::Path;

use axum::Router;

use crate::ServeError;

/// The bound sidecar Unix socket. Constructed only through [`SidecarSocket::bind`].
#[derive(Debug)]
pub(crate) struct SidecarSocket {
    listener: tokio::net::UnixListener,
}

impl SidecarSocket {
    /// Checks the directory (exists, owned by this uid, `mode & 0o007 == 0`), removes a stale socket
    /// file, binds, and chmods the socket to 0660.
    pub(crate) fn bind(path: &Path) -> Result<Self, ServeError> {
        let _ = path;
        unimplemented!()
    }
}

/// The API router that trusts tailnet identity. Opaque outside this module and `api`.
#[derive(Debug)]
pub(crate) struct SidecarApp(Router);

impl SidecarApp {
    /// Only `api::sidecar_app` calls this.
    pub(crate) const fn new(router: Router) -> Self {
        Self(router)
    }
}

/// Serves the sidecar app on the sidecar socket with graceful shutdown (hyper-util auto builder
/// over `UnixListener`, HTTP/1.1, WebSocket upgrades enabled).
pub(crate) async fn serve_sidecar(
    socket: SidecarSocket,
    app: SidecarApp,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    let _ = (socket.listener, app.0, shutdown);
    unimplemented!()
}

/// The ops listener: health and readiness only.
#[derive(Debug)]
pub(crate) struct OpsListener {
    listener: tokio::net::TcpListener,
}

impl OpsListener {
    /// Binds `addr` (loopback by default; a non-loopback address is allowed for orchestrator
    /// probes because this router serves no data and reads no identity).
    pub(crate) async fn bind(addr: std::net::SocketAddr) -> Result<Self, ServeError> {
        let _ = addr;
        unimplemented!()
    }

    pub(crate) fn into_inner(self) -> tokio::net::TcpListener {
        self.listener
    }
}
