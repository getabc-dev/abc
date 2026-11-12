//! The worker's only network code, for enrolment and for the link: TCP to the enrolled URL's host,
//! then TLS that must verify the certificate for that host against [`crate::Trust`], then HTTP/1.1.
//! No redirects, no proxy variables, no identity headers: the server reads none from workers (the
//! key is a worker's only authenticator, and enrolment's authority is its token). The key is
//! written only after TLS has verified the server's name, so a wrong or spoofed server never sees
//! it. HTTP types stay in this module: refusals leave it as [`Refusal`] or [`crate::EnrolError`].

use std::time::Duration;

use abc_core::credential::WorkerKey;
use abc_core::id::{EnrolmentRequestId, WorkspaceId};
use abc_core::query::{RequestState, SessionView};
use abc_core::secret::Secret;
use abc_protocol::api::{ApiCode, ApiErrorBody};
use abc_protocol::enrol::{
    ApprovalRequest, ApprovalRequested, ENROL_PATH, EnrolRequest, EnrolResponse,
    REQUEST_STATUS_PATH, REQUESTS_PATH,
};
use abc_protocol::link::{CONNECT_PATH, MAX_MESSAGE};
use abc_protocol::url::PublicUrl;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty, Full, Limited};
use hyper::StatusCode;
use hyper::header::{
    AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue, SEC_WEBSOCKET_ACCEPT,
    SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION,
    UPGRADE,
};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use crate::{EnrolError, Env};

/// An upgraded link, its messages and frames capped at [`MAX_MESSAGE`].
pub(crate) type Link = WebSocketStream<TokioIo<Upgraded>>;

/// The largest error or enrolment body read from the server.
const MAX_BODY: usize = 64 * 1024;

/// The link could not be opened. Always worth retrying; the cause chain is logged.
#[derive(Debug, Error)]
pub enum DialError {
    /// The name did not resolve: `MagicDNS` is not answering, so Tailscale is probably not running
    /// on this machine.
    #[error("cannot resolve {name} (is Tailscale running on this machine?)")]
    Resolve {
        /// The name.
        name: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// No TCP connection.
    #[error("cannot connect to {host}")]
    Connect {
        /// `host[:port]`.
        host: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The certificate does not verify for the enrolled host, or TLS failed.
    #[error(
        "TLS with {host} failed; the worker sends its key only to a server whose certificate names it"
    )]
    Tls {
        /// `host[:port]`.
        host: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP exchange failed.
    #[error("the HTTP exchange with the server failed")]
    Http(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The server answered 101 without completing the WebSocket handshake.
    #[error("the server's upgrade is not a WebSocket handshake: {0}")]
    Upgrade(&'static str),
    /// It did not complete in time.
    #[error("the server did not answer within {} s", .0.as_secs_f64())]
    Timeout(Duration),
}

/// What the server answered instead of upgrading the link, in the worker's terms. Mapped from the
/// status and the body's `ApiCode` in one exhaustive match ([`refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `invalid_worker_key`: the server does not know this key.
    InvalidKey,
    /// `worker_revoked`.
    Revoked,
    /// `rate_limited`: too many sockets or session starts for this worker.
    RateLimited,
    /// `unavailable`, or a gateway error from Serve while the server is down or restarting.
    Unavailable,
    /// Anything else. Retried, and logged with what it was.
    Unexpected {
        /// The HTTP status.
        status: u16,
        /// The code, when the body was abc's error format.
        code: Option<ApiCode>,
    },
}

/// Classifies a refused upgrade.
pub(crate) const fn refusal(status: u16, code: Option<ApiCode>) -> Refusal {
    let Some(code) = code else {
        return match status {
            502..=504 => Refusal::Unavailable,
            _ => Refusal::Unexpected { status, code },
        };
    };
    match code {
        ApiCode::InvalidWorkerKey => Refusal::InvalidKey,
        ApiCode::WorkerRevoked => Refusal::Revoked,
        ApiCode::RateLimited => Refusal::RateLimited,
        ApiCode::Unavailable => Refusal::Unavailable,
        ApiCode::MalformedRequest
        | ApiCode::CrossSiteRequest
        | ApiCode::NotFound
        | ApiCode::MethodNotAllowed
        | ApiCode::TailnetIdentityRequired
        | ApiCode::NotAMember
        | ApiCode::WorkerOnly
        | ApiCode::ServerAlreadyClaimed
        | ApiCode::SetupCodeRequired
        | ApiCode::SetupCodeMismatch
        | ApiCode::SetupCodeExpired
        | ApiCode::ServerOnly
        | ApiCode::UnknownWorker
        | ApiCode::UnknownRun
        | ApiCode::EnrolmentTokenRequired
        | ApiCode::EnrolmentTokenUnknown
        | ApiCode::EnrolmentTokenExpired
        | ApiCode::EnrolmentTokenRevoked
        | ApiCode::EnrolmentTokenUsed
        | ApiCode::WorkerKeyInUse
        | ApiCode::EnrolmentTokenAlreadyRedeemed
        | ApiCode::EnrolmentRequestUnknown
        | ApiCode::TailnetNodeUnknown
        | ApiCode::ApprovalUnavailable
        | ApiCode::EnrolmentRequestExpired
        | ApiCode::EnrolmentRequestDenied
        | ApiCode::EnrolmentRequestApproved
        | ApiCode::Internal => Refusal::Unexpected {
            status,
            code: Some(code),
        },
    }
}

/// How an enrolment the server answered with an error is reported. The server's message is shown
/// as it is: it never contains a secret or echoes input.
pub(crate) fn enrol_refusal(status: u16, body: Option<ApiErrorBody>) -> EnrolError {
    let Some(ApiErrorBody { error }) = body else {
        return match status {
            502..=504 => EnrolError::Unavailable,
            _ => EnrolError::NotAbc { status },
        };
    };
    let refused = |message: String| EnrolError::Refused {
        code: error.code,
        message,
    };
    match error.code {
        ApiCode::EnrolmentTokenUnknown => EnrolError::TokenUnknown,
        ApiCode::EnrolmentTokenExpired => EnrolError::TokenExpired,
        ApiCode::EnrolmentTokenRevoked => EnrolError::TokenRevoked,
        ApiCode::EnrolmentTokenUsed | ApiCode::EnrolmentTokenAlreadyRedeemed => {
            EnrolError::TokenUsed
        }
        ApiCode::WorkerRevoked => EnrolError::Revoked,
        ApiCode::WorkerKeyInUse => EnrolError::KeyInUse,
        ApiCode::ApprovalUnavailable => EnrolError::ApprovalUnavailable,
        ApiCode::TailnetIdentityRequired => EnrolError::NoTailnetIdentity,
        ApiCode::RateLimited => EnrolError::RateLimited,
        ApiCode::Unavailable => EnrolError::Unavailable,
        // Its tailscaled may be restarting, or this machine just left the tailnet: retried alike.
        ApiCode::TailnetNodeUnknown => EnrolError::TailnetNodeUnknown,
        ApiCode::MalformedRequest
        | ApiCode::CrossSiteRequest
        | ApiCode::NotFound
        | ApiCode::MethodNotAllowed
        | ApiCode::InvalidWorkerKey
        | ApiCode::NotAMember
        | ApiCode::WorkerOnly
        | ApiCode::ServerAlreadyClaimed
        | ApiCode::SetupCodeRequired
        | ApiCode::SetupCodeMismatch
        | ApiCode::SetupCodeExpired
        | ApiCode::ServerOnly
        | ApiCode::UnknownWorker
        | ApiCode::UnknownRun
        | ApiCode::EnrolmentTokenRequired
        | ApiCode::EnrolmentRequestUnknown
        | ApiCode::EnrolmentRequestExpired
        | ApiCode::EnrolmentRequestDenied
        | ApiCode::EnrolmentRequestApproved
        | ApiCode::Internal => refused(error.message),
    }
}

/// Reads abc's error body, if that is what `body` holds.
fn api_error(body: &[u8]) -> Option<ApiErrorBody> {
    serde_json::from_slice(body).ok()
}

/// TCP and TLS to `server`, verifying its certificate for its host name.
async fn tls(env: &Env, server: &PublicUrl) -> Result<TlsStream<TcpStream>, DialError> {
    let addresses: Vec<_> = tokio::net::lookup_host((server.name(), server.port()))
        .await
        .map_err(|source| DialError::Resolve {
            name: server.name().to_owned(),
            source,
        })?
        .collect();
    let tcp = TcpStream::connect(addresses.as_slice())
        .await
        .map_err(|source| DialError::Connect {
            host: server.host().to_owned(),
            source,
        })?;
    // Frames are small and interactive; Nagle's delay buys nothing here. Failing to set it changes
    // only latency.
    if tcp.set_nodelay(true).is_err() {
        tracing::debug!("could not disable Nagle's algorithm on the link");
    }
    let name = ServerName::try_from(server.name().to_owned()).map_err(|error| DialError::Tls {
        host: server.host().to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error),
    })?;
    TlsConnector::from(env.trust.config())
        .connect(name, tcp)
        .await
        .map_err(|source| DialError::Tls {
            host: server.host().to_owned(),
            source,
        })
}

/// Runs `step` with a deadline of `limit`.
async fn within<T>(
    limit: Duration,
    step: impl Future<Output = Result<T, DialError>>,
) -> Result<T, DialError> {
    tokio::time::timeout(limit, step)
        .await
        .unwrap_or(Err(DialError::Timeout(limit)))
}

/// One JSON exchange with the server over a fresh verified connection, bounded by the request
/// timing: the status and the body (at most [`MAX_BODY`]).
async fn exchange(
    env: &Env,
    server: &PublicUrl,
    method: hyper::Method,
    path: &str,
    payload: Option<Vec<u8>>,
) -> Result<(u16, Bytes), DialError> {
    within(env.timings.request, async {
        let stream = tls(env, server).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| DialError::Http(error.into()))?;
        // The connection ends when `sender` is dropped, after this one request.
        drop(tokio::spawn(connection));
        let host =
            HeaderValue::from_str(server.host()).map_err(|error| DialError::Http(error.into()))?;
        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, host);
        let body = match payload {
            Some(payload) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Full::new(Bytes::from(payload))
            }
            None => Full::new(Bytes::new()),
        };
        let request = builder
            .body(body)
            .map_err(|error| DialError::Http(error.into()))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| DialError::Http(error.into()))?;
        let status = response.status().as_u16();
        let bytes = Limited::new(response.into_body(), MAX_BODY)
            .collect()
            .await
            .map_err(DialError::Http)?
            .to_bytes();
        Ok((status, bytes))
    })
    .await
}

/// Reads a 200 answer as `T`, or reports the refusal in enrolment's terms.
fn answer<T: serde::de::DeserializeOwned>(status: u16, body: &[u8]) -> Result<T, EnrolError> {
    if status == 200 {
        serde_json::from_slice(body).map_err(|_unreadable| EnrolError::NotAbc { status })
    } else {
        Err(enrol_refusal(status, api_error(body)))
    }
}

/// `POST /api/enrol`, bounded by the request timing.
pub(crate) async fn post_enrol(
    env: &Env,
    server: &PublicUrl,
    request: &EnrolRequest,
) -> Result<EnrolResponse, EnrolError> {
    let payload = serde_json::to_vec(request).map_err(|error| EnrolError::Encode(error.into()))?;
    let (status, body) = exchange(env, server, hyper::Method::POST, ENROL_PATH, Some(payload))
        .await
        .map_err(EnrolError::Unreachable)?;
    answer(status, &body)
}

/// `GET /api/session`: who the server says this machine's person is, through the machine's own
/// Tailscale.
pub(crate) async fn get_session(env: &Env, server: &PublicUrl) -> Result<SessionView, EnrolError> {
    let (status, body) = exchange(env, server, hyper::Method::GET, "/api/session", None)
        .await
        .map_err(EnrolError::Unreachable)?;
    answer(status, &body)
}

/// `POST /api/workspaces/{workspace}/enrolment-requests`.
pub(crate) async fn post_request(
    env: &Env,
    server: &PublicUrl,
    workspace: WorkspaceId,
    request: &ApprovalRequest,
) -> Result<ApprovalRequested, EnrolError> {
    let payload = serde_json::to_vec(request).map_err(|error| EnrolError::Encode(error.into()))?;
    let path = REQUESTS_PATH.replace("{workspace}", &workspace.to_string());
    let (status, body) = exchange(env, server, hyper::Method::POST, &path, Some(payload))
        .await
        .map_err(EnrolError::Unreachable)?;
    answer(status, &body)
}

/// `GET /api/enrolment-requests/{request}`.
pub(crate) async fn get_request_state(
    env: &Env,
    server: &PublicUrl,
    request: EnrolmentRequestId,
) -> Result<RequestState, EnrolError> {
    let path = REQUEST_STATUS_PATH.replace("{request}", &request.to_string());
    let (status, body) = exchange(env, server, hyper::Method::GET, &path, None)
        .await
        .map_err(EnrolError::Unreachable)?;
    answer(status, &body)
}

/// What dialling the link gave.
#[derive(Debug)]
pub(crate) enum Dialed {
    /// The link is open; the handshake comes next.
    Upgraded(Box<Link>),
    /// The server answered without upgrading.
    Refused(Refusal),
}

/// `GET /api/worker/connect` with the key in `Authorization: Bearer` (marked sensitive), bounded
/// by the connect timing. A refusal is an answer, not an error: its whole body is read, up to
/// [`MAX_BODY`], however many reads it arrives in (behind Serve the head and the body come
/// separately), so a revoked or unknown key is always recognised as one.
pub(crate) async fn connect(
    env: &Env,
    server: &PublicUrl,
    key: &Secret<WorkerKey>,
) -> Result<Dialed, DialError> {
    within(env.timings.connect, async {
        let stream = tls(env, server).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| DialError::Http(error.into()))?;
        // The connection hands its stream to the upgrade on a 101, and ends after a refusal.
        drop(tokio::spawn(connection.with_upgrades()));
        let nonce = handshake::client::generate_key();
        let request = upgrade_request(server, key, &nonce)?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| DialError::Http(error.into()))?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            let status = response.status().as_u16();
            let body = Limited::new(response.into_body(), MAX_BODY)
                .collect()
                .await
                .map_err(DialError::Http)?
                .to_bytes();
            let code = api_error(&body).map(|body| body.error.code);
            return Ok(Dialed::Refused(refusal(status, code)));
        }
        accepted(response.headers(), &nonce).map_err(DialError::Upgrade)?;
        let upgraded = hyper::upgrade::on(response)
            .await
            .map_err(|error| DialError::Http(error.into()))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let link =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, Some(config))
                .await;
        Ok(Dialed::Upgraded(Box::new(link)))
    })
    .await
}

/// The upgrade request: the key as a sensitive bearer, and a WebSocket handshake offering no
/// subprotocol and no extension.
fn upgrade_request(
    server: &PublicUrl,
    key: &Secret<WorkerKey>,
    nonce: &str,
) -> Result<hyper::Request<Empty<Bytes>>, DialError> {
    let host =
        HeaderValue::from_str(server.host()).map_err(|error| DialError::Http(error.into()))?;
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", key.expose_secret().as_str()))
        .map_err(|error| DialError::Http(error.into()))?;
    bearer.set_sensitive(true);
    hyper::Request::get(CONNECT_PATH)
        .header(HOST, host)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_VERSION, "13")
        .header(SEC_WEBSOCKET_KEY, nonce)
        .header(AUTHORIZATION, bearer)
        .body(Empty::new())
        .map_err(|error| DialError::Http(error.into()))
}

/// Whether a 101's headers complete the WebSocket handshake for `nonce` (RFC 6455 section 4.1):
/// `Upgrade: websocket`, a `Connection` naming `upgrade`, the accept key derived from the nonce,
/// and no subprotocol or extension, since the worker offered none. `Err` says which is wrong.
fn accepted(headers: &HeaderMap, nonce: &str) -> Result<(), &'static str> {
    let text = |name| {
        headers
            .get(name)
            .and_then(|value: &HeaderValue| value.to_str().ok())
    };
    if !text(UPGRADE).is_some_and(|value| value.eq_ignore_ascii_case("websocket")) {
        return Err("no `Upgrade: websocket`");
    }
    let names_upgrade = |value: &str| {
        value
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
    };
    if !text(CONNECTION).is_some_and(names_upgrade) {
        return Err("no `Connection: upgrade`");
    }
    if text(SEC_WEBSOCKET_ACCEPT) != Some(handshake::derive_accept_key(nonce.as_bytes()).as_str()) {
        return Err("the accept key does not match the request's");
    }
    if headers.contains_key(SEC_WEBSOCKET_PROTOCOL) {
        return Err("a subprotocol the worker did not offer");
    }
    if headers.contains_key(SEC_WEBSOCKET_EXTENSIONS) {
        return Err("an extension the worker did not offer");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use abc_protocol::api::ApiErrorDetail;

    use super::*;

    #[test]
    fn a_body_from_the_server_is_read_up_to_64_kib() {
        assert_eq!(
            MAX_BODY, 65_536,
            "room for any enrolment answer or error, and no more"
        );
    }

    #[test]
    fn every_refusal_is_classified() {
        let special = [
            (ApiCode::InvalidWorkerKey, Refusal::InvalidKey),
            (ApiCode::WorkerRevoked, Refusal::Revoked),
            (ApiCode::RateLimited, Refusal::RateLimited),
            (ApiCode::Unavailable, Refusal::Unavailable),
        ];
        for code in ApiCode::ALL {
            let expected = special.iter().find(|(special, _)| *special == code).map_or(
                Refusal::Unexpected {
                    status: 400,
                    code: Some(code),
                },
                |(_, refusal)| *refusal,
            );
            assert_eq!(refusal(400, Some(code)), expected, "{code:?}");
        }
        for status in [502, 503, 504] {
            assert_eq!(
                refusal(status, None),
                Refusal::Unavailable,
                "Serve's gateway errors while the server is down: {status}"
            );
        }
        for status in [400, 404, 500, 501, 505] {
            assert_eq!(
                refusal(status, None),
                Refusal::Unexpected { status, code: None },
                "a body that is not abc's: {status}"
            );
        }
    }

    #[test]
    fn only_a_complete_websocket_handshake_is_accepted() {
        // RFC 6455 section 1.3's example nonce and accept key.
        let nonce = "dGhlIHNhbXBsZSBub25jZQ==";
        let good = || {
            let mut headers = HeaderMap::new();
            headers.insert(UPGRADE, HeaderValue::from_static("WebSocket"));
            headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
            headers.insert(
                SEC_WEBSOCKET_ACCEPT,
                HeaderValue::from_static("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            );
            headers
        };
        assert_eq!(accepted(&good(), nonce), Ok(()), "a complete handshake");
        let changed = |change: &dyn Fn(&mut HeaderMap)| {
            let mut headers = good();
            change(&mut headers);
            accepted(&headers, nonce)
        };
        assert_eq!(
            changed(&|h| drop(h.remove(UPGRADE))),
            Err("no `Upgrade: websocket`"),
            "no upgrade"
        );
        assert_eq!(
            changed(&|h| drop(h.insert(UPGRADE, HeaderValue::from_static("h2c")))),
            Err("no `Upgrade: websocket`"),
            "an upgrade to something else"
        );
        assert_eq!(
            changed(&|h| drop(h.remove(CONNECTION))),
            Err("no `Connection: upgrade`"),
            "no connection header"
        );
        assert_eq!(
            changed(&|h| drop(
                h.insert(CONNECTION, HeaderValue::from_static("keep-alive, upgraded"))
            )),
            Err("no `Connection: upgrade`"),
            "a connection header without the upgrade token"
        );
        assert_eq!(
            changed(&|h| drop(h.remove(SEC_WEBSOCKET_ACCEPT))),
            Err("the accept key does not match the request's"),
            "no accept key"
        );
        assert_eq!(
            accepted(&good(), "AQIDBAUGBwgJCgsMDQ4PEC=="),
            Err("the accept key does not match the request's"),
            "an accept key for another nonce"
        );
        assert_eq!(
            changed(&|h| drop(h.insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("chat")))),
            Err("a subprotocol the worker did not offer"),
            "a subprotocol"
        );
        assert_eq!(
            changed(&|h| {
                drop(h.insert(
                    SEC_WEBSOCKET_EXTENSIONS,
                    HeaderValue::from_static("permessage-deflate"),
                ));
            }),
            Err("an extension the worker did not offer"),
            "an extension"
        );
    }

    #[test]
    fn the_upgrade_request_offers_a_plain_websocket_and_hides_the_key() {
        let server = PublicUrl::parse("https://abc.tail1234.ts.net").unwrap();
        let key = crate::testing::key(1);
        let request = upgrade_request(&server, &key, "bm9uY2U=").unwrap();
        assert_eq!(request.method(), hyper::Method::GET, "a GET");
        assert_eq!(request.uri().path(), CONNECT_PATH, "to the link's path");
        let header = |name| request.headers().get(name).unwrap().to_str().unwrap();
        assert_eq!(header(HOST), "abc.tail1234.ts.net", "for the enrolled host");
        assert_eq!(header(CONNECTION), "Upgrade", "asking for an upgrade");
        assert_eq!(header(UPGRADE), "websocket", "to a WebSocket");
        assert_eq!(header(SEC_WEBSOCKET_VERSION), "13", "of RFC 6455");
        assert_eq!(header(SEC_WEBSOCKET_KEY), "bm9uY2U=", "with the nonce");
        let bearer = request.headers().get(AUTHORIZATION).unwrap();
        assert_eq!(
            bearer.to_str().unwrap(),
            format!("Bearer {}", key.expose_secret().as_str()),
            "the key as a bearer"
        );
        assert!(bearer.is_sensitive(), "kept out of logs");
        assert!(
            !request.headers().contains_key(SEC_WEBSOCKET_PROTOCOL)
                && !request.headers().contains_key(SEC_WEBSOCKET_EXTENSIONS),
            "no subprotocol and no extension offered"
        );
    }

    fn body(code: ApiCode) -> ApiErrorBody {
        ApiErrorBody {
            error: ApiErrorDetail {
                code,
                message: "the server's words".to_owned(),
            },
        }
    }

    #[test]
    fn every_enrolment_refusal_is_explained() {
        let explained = [
            (
                ApiCode::EnrolmentTokenUnknown,
                "the server does not know this enrolment token; check that you copied all of it",
            ),
            (
                ApiCode::EnrolmentTokenExpired,
                "the enrolment token expired; ask the server's owner for a new one",
            ),
            (
                ApiCode::EnrolmentTokenRevoked,
                "the enrolment token was revoked; ask the server's owner for a new one",
            ),
            (
                ApiCode::EnrolmentTokenUsed,
                "another machine already used this enrolment token; ask the server's owner for a new one",
            ),
            (
                ApiCode::EnrolmentTokenAlreadyRedeemed,
                "another machine already used this enrolment token; ask the server's owner for a new one",
            ),
            (
                ApiCode::WorkerRevoked,
                "the worker this machine enrolled as was revoked; enrol again with a new token and --replace",
            ),
            (
                ApiCode::WorkerKeyInUse,
                "another worker already holds this machine's key (was the worker home copied?); enrol again with --replace",
            ),
            (
                ApiCode::ApprovalUnavailable,
                "the server cannot approve a machine in the browser; create an enrolment token on the Machines page and run `abc worker enrol <url> --token-file <file>` instead",
            ),
            (
                ApiCode::TailnetIdentityRequired,
                "the server does not know you: this machine is not signed in to Tailscale as a person (a tagged device has no identity); sign in as the person who owns the server, or enrol with a token: `abc worker enrol <url> --token-file <file>`",
            ),
            (
                ApiCode::RateLimited,
                "too many enrolment attempts from this machine; wait a minute and run the same command again",
            ),
            (
                ApiCode::Unavailable,
                "the server is starting, stopping or cannot reach its database; run the same command again",
            ),
            (
                ApiCode::TailnetNodeUnknown,
                "the server could not tell which Tailscale node this machine is; check that it is signed in to the server's tailnet (`tailscale status`), then run the same command again",
            ),
        ];
        for code in ApiCode::ALL {
            let error = enrol_refusal(400, Some(body(code)));
            match explained.iter().find(|(explained, _)| *explained == code) {
                Some((_, sentence)) => assert_eq!(error.to_string(), *sentence, "{code:?}"),
                None => assert!(
                    matches!(&error, EnrolError::Refused { code: got, message } if *got == code && message == "the server's words"),
                    "{code:?} is passed on with the server's message: {error:?}"
                ),
            }
        }
        assert!(
            matches!(enrol_refusal(502, None), EnrolError::Unavailable),
            "a gateway error from Serve"
        );
        assert!(
            matches!(enrol_refusal(404, None), EnrolError::NotAbc { status: 404 }),
            "anything else that is not abc's format"
        );
        let mut limited = body(ApiCode::RateLimited);
        limited.error.message = "m".to_owned();
        assert_eq!(
            api_error(br#"{"error":{"code":"rate_limited","message":"m"}}"#),
            Some(limited),
            "abc's error body is read"
        );
        assert_eq!(api_error(b"<html>"), None, "anything else is not");
    }
}
