//! The worker link: a WebSocket the worker dials out to, authenticated before the upgrade.
//!
//! ```text
//! GET /api/worker/connect   Authorization: Bearer abc_wk_…   (Upgrade: websocket)
//!   401 + ApiErrorBody { invalid_worker_key | worker_revoked }  -> stop; tell the user to re-enrol
//!   101 -> worker sends Hello within HANDSHAKE_TIMEOUT -> Welcome | Reject (existing handshake)
//!       -> server pings every PING_INTERVAL; no pong for IDLE_TIMEOUT closes the session
//! ```
//!
//! Reconnect (worker side, PR 4): full-jitter backoff from 0.5 s, doubling up to 30 s, reset after a
//! session that lived longer than a minute. Reconnect immediately when the network changes after
//! sleep. A newer session supersedes an older one for the same worker, so a half-open socket left
//! from before sleep never blocks the reconnect.

use std::time::Duration;

/// The WebSocket endpoint, relative to the server's tailnet URL.
pub const CONNECT_PATH: &str = "/api/worker/connect";

/// The worker must send `Hello` within this long after the upgrade.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The server pings this often.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);

/// A session with no pong for this long is closed as timed out (the laptop slept).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Application close codes (4000 to 4999). The worker branches on them.
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
