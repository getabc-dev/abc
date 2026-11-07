//! Listeners, and the rule that keeps identity off every path but one (D4).
//!
//! Tailscale Serve sets `Tailscale-User-*` headers and strips copies sent by clients, but anyone
//! who reaches the backend another way can set them. So:
//!
//! 1. **Identity routes can only be served on the sidecar socket.** [`SidecarApp`] wraps a private
//!    `Router`, built only by `api::sidecar_app`, and the only function that serves it is
//!    [`serve_sidecar`], which takes a [`SidecarSocket`]. (Plain axum would allow serving it over
//!    TCP: `Router::with_state` erases the state type, so the newtype is what keeps the
//!    distinction.)
//! 2. **Only the sidecar can reach the socket.** [`SidecarSocket::bind`] refuses a parent directory
//!    with any "other" permission bit, removes a stale socket file, binds, and sets the socket to
//!    0660. `abc server setup` puts it in `<home>/run/` (0700), where only the user reaches it: abc
//!    server and abc's own tailscaled run as that user, and Serve proxies
//!    `https://<name>.<tailnet>.ts.net/` to `unix:<home>/run/abc.sock` (abc-host's `root`).
//! 3. **There is no TCP listener with identity**, so a stray `ports:` mapping or a `0.0.0.0` bind
//!    exposes health checks at most, never forgeable identity.
//! 4. **Defence in depth.** `TailnetCaller` is `FromRequestParts<SidecarState>` only, so identity
//!    handlers cannot be mounted on the ops router. The ops router is built inside `ops::serve`.
//!    `clippy.toml` disallows `axum::serve`, so the two functions here are the only places that
//!    serve a router.

use std::fs;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use axum::Router;
use tokio::net::{TcpListener, UnixListener};

use crate::ServeError;

/// The permission bits of the sidecar socket: the abc user and its group (the sidecar's).
const SOCKET_MODE: u32 = 0o660;

/// The bound sidecar Unix socket. Constructed only through [`SidecarSocket::bind`].
#[derive(Debug)]
pub(crate) struct SidecarSocket {
    listener: UnixListener,
}

impl SidecarSocket {
    /// Checks that the directory exists, gives "other" users no access and belongs to this
    /// process's user; refuses a socket another server is listening on and removes a stale one;
    /// binds, and sets the socket's mode to 0660.
    pub(crate) fn bind(path: &Path) -> Result<Self, ServeError> {
        let refuse = |reason| ServeError::SocketPath {
            path: path.to_owned(),
            reason,
        };
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .ok_or_else(|| refuse("the path has no directory"))?;
        let access = |source| ServeError::SocketAccess {
            path: path.to_owned(),
            source,
        };
        let dir_meta = match fs::metadata(dir) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(refuse("its directory does not exist"));
            }
            Err(source) => return Err(access(source)),
        };
        if !dir_meta.is_dir() {
            return Err(refuse("its parent is not a directory"));
        }
        let mode = dir_meta.permissions().mode() & 0o777;
        if mode & 0o007 != 0 {
            return Err(ServeError::SocketDirTooOpen {
                path: dir.to_owned(),
                mode,
            });
        }
        match fs::symlink_metadata(path) {
            Ok(existing) if existing.file_type().is_socket() => {
                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return Err(refuse("another server is listening on it"));
                }
                fs::remove_file(path).map_err(listener_error)?;
            }
            Ok(_) => return Err(refuse("a file that is not a socket is in the way")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(access(source)),
        }
        let listener = UnixListener::bind(path).map_err(listener_error)?;
        // A socket belongs to the user that bound it: this process's.
        let server = fs::metadata(path).map_err(listener_error)?.uid();
        if dir_meta.uid() != server {
            drop(listener);
            let _removed = fs::remove_file(path);
            return Err(ServeError::SocketDirNotOwned {
                path: dir.to_owned(),
                owner: dir_meta.uid(),
                server,
            });
        }
        fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))
            .map_err(listener_error)?;
        Ok(Self { listener })
    }
}

fn listener_error(source: io::Error) -> ServeError {
    ServeError::Listener {
        what: "sidecar",
        source,
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

/// Serves the sidecar app on the sidecar socket until `shutdown` resolves and every connection
/// it still tracks has finished.
pub(crate) async fn serve_sidecar(
    socket: SidecarSocket,
    app: SidecarApp,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place that serves the sidecar app, on the sidecar socket only"
    )]
    let serving = axum::serve(socket.listener, app.0).with_graceful_shutdown(shutdown);
    serving.await.map_err(listener_error)
}

/// The ops listener: health and readiness only.
#[derive(Debug)]
pub(crate) struct OpsListener {
    listener: TcpListener,
}

impl OpsListener {
    /// Binds `addr`: loopback by default. A non-loopback address is allowed for orchestrator
    /// probes, because this listener serves no data and reads no identity.
    pub(crate) async fn bind(addr: SocketAddr) -> Result<Self, ServeError> {
        let listener = TcpListener::bind(addr).await.map_err(ops_error)?;
        Ok(Self { listener })
    }

    /// The address it is bound to (the actual port when `addr` asked for port 0).
    pub(crate) fn local_addr(&self) -> Result<SocketAddr, ServeError> {
        self.listener.local_addr().map_err(ops_error)
    }

    /// Serves the ops router, built by `ops::serve`, until `shutdown` resolves.
    pub(crate) async fn serve(
        self,
        router: Router,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServeError> {
        #[expect(
            clippy::disallowed_methods,
            reason = "the one place that serves the ops router, which reads no identity"
        )]
        let serving = axum::serve(self.listener, router).with_graceful_shutdown(shutdown);
        serving.await.map_err(ops_error)
    }
}

fn ops_error(source: io::Error) -> ServeError {
    ServeError::Listener {
        what: "ops",
        source,
    }
}
