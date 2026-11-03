//! The worker's only network code, for enrolment and for the link: TCP to the enrolled URL's host,
//! then TLS that must verify the certificate for that host against [`crate::Trust`], then HTTP/1.1.
//! No redirects, no proxy variables, no identity headers: the server reads none from workers (the
//! key is a worker's only authenticator, and enrolment's authority is its token). The key is
//! written only after TLS has verified the server's name, so a wrong or spoofed server never sees
//! it. HTTP types stay in this module: refusals leave it as [`Refusal`] or [`crate::EnrolError`].

use std::time::Duration;

use abc_core::credential::WorkerKey;
use abc_core::secret::Secret;
use abc_protocol::api::{ApiCode, ApiErrorBody};
use abc_protocol::enrol::{ENROL_PATH, EnrolRequest, EnrolResponse};
use abc_protocol::link::{CONNECT_PATH, MAX_MESSAGE};
use abc_protocol::url::PublicUrl;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderValue};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::{EnrolError, Env};

/// An upgraded link, its messages and frames capped at [`MAX_MESSAGE`].
pub(crate) type Link = WebSocketStream<TlsStream<TcpStream>>;

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
        | ApiCode::EnrolmentTokenRequired
        | ApiCode::EnrolmentTokenUnknown
        | ApiCode::EnrolmentTokenExpired
        | ApiCode::EnrolmentTokenRevoked
        | ApiCode::EnrolmentTokenUsed
        | ApiCode::WorkerKeyInUse
        | ApiCode::EnrolmentTokenAlreadyRedeemed
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
        ApiCode::RateLimited => EnrolError::RateLimited,
        ApiCode::Unavailable => EnrolError::Unavailable,
        ApiCode::MalformedRequest
        | ApiCode::CrossSiteRequest
        | ApiCode::NotFound
        | ApiCode::MethodNotAllowed
        | ApiCode::TailnetIdentityRequired
        | ApiCode::InvalidWorkerKey
        | ApiCode::NotAMember
        | ApiCode::WorkerOnly
        | ApiCode::ServerAlreadyClaimed
        | ApiCode::SetupCodeRequired
        | ApiCode::SetupCodeMismatch
        | ApiCode::SetupCodeExpired
        | ApiCode::ServerOnly
        | ApiCode::UnknownWorker
        | ApiCode::EnrolmentTokenRequired
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

/// `POST /api/enrol`, bounded by the request timing.
pub(crate) async fn post_enrol(
    env: &Env,
    server: &PublicUrl,
    request: &EnrolRequest,
) -> Result<EnrolResponse, EnrolError> {
    let payload = serde_json::to_vec(request).map_err(|error| EnrolError::Encode(error.into()))?;
    let (status, answer) = within(env.timings.request, async {
        let stream = tls(env, server).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| DialError::Http(error.into()))?;
        // The connection ends when `sender` is dropped, after this one request.
        drop(tokio::spawn(connection));
        let host =
            HeaderValue::from_str(server.host()).map_err(|error| DialError::Http(error.into()))?;
        let post = hyper::Request::post(ENROL_PATH)
            .header(HOST, host)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload)))
            .map_err(|error| DialError::Http(error.into()))?;
        let response = sender
            .send_request(post)
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
    .map_err(EnrolError::Unreachable)?;
    if status == 200 {
        serde_json::from_slice(&answer).map_err(|_unreadable| EnrolError::NotAbc { status })
    } else {
        Err(enrol_refusal(status, api_error(&answer)))
    }
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
/// by the connect timing. A refusal is an answer, not an error.
pub(crate) async fn connect(
    env: &Env,
    server: &PublicUrl,
    key: &Secret<WorkerKey>,
) -> Result<Dialed, DialError> {
    within(env.timings.connect, async {
        let stream = tls(env, server).await?;
        let mut request = format!("wss://{}{CONNECT_PATH}", server.host())
            .into_client_request()
            .map_err(|error| DialError::Http(error.into()))?;
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", key.expose_secret().as_str()))
            .map_err(|error| DialError::Http(error.into()))?;
        bearer.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, bearer);
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        match tokio_tungstenite::client_async_with_config(request, stream, Some(config)).await {
            Ok((link, _)) => Ok(Dialed::Upgraded(Box::new(link))),
            Err(tungstenite::Error::Http(response)) => {
                let code = response
                    .body()
                    .as_deref()
                    .and_then(api_error)
                    .map(|body| body.error.code);
                Ok(Dialed::Refused(refusal(response.status().as_u16(), code)))
            }
            Err(error) => Err(DialError::Http(error.into())),
        }
    })
    .await
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
                ApiCode::RateLimited,
                "too many enrolment attempts from this machine; wait a minute and run the same command again",
            ),
            (
                ApiCode::Unavailable,
                "the server is starting, stopping or cannot reach its database; run the same command again",
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
