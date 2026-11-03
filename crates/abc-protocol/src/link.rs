//! The worker link: a WebSocket the worker dials out to, authenticated before the upgrade.
//!
//! ```text
//! GET /api/worker/connect   Authorization: Bearer abc_wk_…   (Upgrade: websocket)
//!   401 + ApiErrorBody { invalid_worker_key | worker_revoked }  -> stop; tell the user to re-enrol
//!   101 -> worker sends Hello within HANDSHAKE_TIMEOUT -> Welcome | Reject (existing handshake)
//!       -> server pings every PING_INTERVAL; no pong for IDLE_TIMEOUT closes the session
//! ```
//!
//! The worker always dials; the server never connects to a worker. A worker holds at most one
//! link. When it ends, the worker decides from the close code (or the HTTP refusal) whether to
//! dial again after a backoff or to stop because a person has to act (`abc_worker`'s policy). A
//! newer session supersedes an older one for the same worker, so a half-open socket left from
//! before sleep never blocks the reconnect.

use std::time::Duration;

/// The WebSocket endpoint, relative to the server's tailnet URL.
pub const CONNECT_PATH: &str = "/api/worker/connect";

/// The worker must send `Hello` within this long after the upgrade.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The server pings this often.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);

/// A session with no pong for this long is closed as timed out (the laptop slept).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest message or frame either side accepts; both sides enforce it.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// Application close codes (4000 to 4999). The worker branches on them; any other code, 1000
/// included, means "dial again after a backoff".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCode {
    /// The worker did not send `Hello` in time. Retry.
    HandshakeTimeout,
    /// The worker sent something other than the expected message. Retry; report if it repeats.
    ProtocolViolation,
    /// The worker was revoked while connected. Stop retrying.
    Revoked,
    /// A newer session for this worker replaced this one. Do not reconnect from this process.
    Superseded,
    /// The server is shutting down. Retry with backoff.
    ServerShutdown,
}

impl CloseCode {
    /// The numeric code on the wire.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::HandshakeTimeout => 4408,
            Self::ProtocolViolation => 4400,
            Self::Revoked => 4403,
            Self::Superseded => 4409,
            Self::ServerShutdown => 4503,
        }
    }

    /// Every close code.
    pub const ALL: [Self; 5] = [
        Self::HandshakeTimeout,
        Self::ProtocolViolation,
        Self::Revoked,
        Self::Superseded,
        Self::ServerShutdown,
    ];

    /// Parses a received close code. Unknown codes are `None` (treat as retryable).
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            4408 => Some(Self::HandshakeTimeout),
            4400 => Some(Self::ProtocolViolation),
            4403 => Some(Self::Revoked),
            4409 => Some(Self::Superseded),
            4503 => Some(Self::ServerShutdown),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_at_most_64_kib() {
        assert_eq!(MAX_MESSAGE, 65_536, "both ends enforce this one number");
    }

    #[test]
    fn close_codes_round_trip_and_are_application_codes() {
        for code in CloseCode::ALL {
            assert_eq!(CloseCode::from_code(code.code()), Some(code), "{code:?}");
            assert!(
                (4000..5000).contains(&code.code()),
                "{code:?} is an application code"
            );
        }
        assert_eq!(
            CloseCode::ALL.map(CloseCode::code),
            [4408, 4400, 4403, 4409, 4503],
            "the wire values the worker branches on"
        );
        assert_eq!(
            CloseCode::from_code(1000),
            None,
            "a normal close is not ours"
        );
        assert_eq!(
            CloseCode::from_code(4999),
            None,
            "unknown codes are retryable"
        );
    }

    #[test]
    fn the_timings_fit_together() {
        assert!(
            PING_INTERVAL < IDLE_TIMEOUT,
            "several pings fit in the idle window"
        );
        assert!(
            HANDSHAKE_TIMEOUT < IDLE_TIMEOUT,
            "the handshake is shorter still"
        );
    }
}
