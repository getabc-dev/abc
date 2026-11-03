//! One link, from the upgrade to its end. It owns the socket and reports how the link ended; it
//! never decides what happens next (that is [`crate::policy`]).
//!
//! ```text
//! send Hello { SUPPORTED_VERSIONS, build }
//! within the handshake timing:  Welcome { v } with v supported  -> connected
//!                               Reject { UnsupportedVersion }   -> Rejected
//!                               anything else                   -> as in the loop below
//! loop:  any frame                -> the link is alive (pings are answered by tungstenite)
//!        close frame              -> Closed(code)
//!        text or binary           -> close 4400 ourselves: protocol 1 has no server messages
//!                                    after Welcome, so one is a server bug -> Violation
//!        tick: wall clock ahead of the monotonic clock by the wake gap -> Dropped(Woke)
//!              no frame for the silence timing                         -> Dropped(Silent)
//!        shutdown                 -> close 1000 and wait briefly for the server's close
//! ```
//!
//! Sleep: tokio's timers and `Instant` stop while the machine sleeps, so after a wake a monotonic
//! timeout alone would not see the time slept, and the socket from before sleep is half-open.
//! Each tick compares wall-clock progress ([`crate::WallClock`]) with monotonic progress; wall
//! time ahead by more than the wake gap means the machine slept, and the link is dropped at once.
//! A clock step (NTP) costs one needless reconnect. A network change needs nothing of its own:
//! `WireGuard` keeps the TCP connection across roaming, and a link that does die falls silent.

use std::pin::Pin;
use std::time::{Duration, SystemTime};

use abc_protocol::link::CloseCode;
use abc_protocol::{
    Hello, ProtocolVersion, RejectReason, SUPPORTED_VERSIONS, ServerMessage, VersionRange,
    WorkerMessage,
};
use futures::{SinkExt as _, StreamExt as _};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode as WireCode;

use crate::Env;
use crate::dial::Link;

/// How a link ended, and how long it was up (for the backoff's reset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionEnd {
    /// From the upgrade to the end.
    pub(crate) lived: Duration,
    /// How.
    pub(crate) how: How,
}

/// How a link ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum How {
    /// The server closed it.
    Closed(ServerClose),
    /// It died under us.
    Dropped(DropCause),
    /// The server shares no protocol version with this build.
    Rejected {
        /// The versions the server speaks.
        supported: VersionRange,
    },
    /// The server broke the protocol, and the worker closed with 4400.
    Violation(&'static str),
    /// The worker is stopping, and closed the link cleanly.
    Shutdown,
}

/// How the server closed the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerClose {
    /// One of abc's close codes.
    Abc(CloseCode),
    /// Any other code: 1000 (the server timed the worker out, or closed after a reject), 1001,
    /// 1011, or none.
    Other(u16),
}

/// Why a link died under us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropCause {
    /// The socket failed or ended without a close frame.
    Io,
    /// No frame arrived for the silence timing.
    Silent,
    /// The machine slept.
    Woke,
    /// No answer to `Hello` within the handshake timing.
    HandshakeTimeout,
}

/// What the first message after `Hello` said.
enum Handshake {
    Welcome(ProtocolVersion),
    End(How),
}

/// Runs `link` until it ends or `shutdown` resolves.
pub(crate) async fn run(
    mut link: Link,
    env: &Env,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
) -> SessionEnd {
    let started = Instant::now();
    let how = match handshake(&mut link, env, shutdown).await {
        Handshake::Welcome(protocol) => {
            tracing::info!(protocol = %protocol, "connected");
            serve(&mut link, env, shutdown).await
        }
        Handshake::End(how) => how,
    };
    SessionEnd {
        lived: started.elapsed(),
        how,
    }
}

/// Sends `Hello` and reads the server's answer.
async fn handshake(
    link: &mut Link,
    env: &Env,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
) -> Handshake {
    let hello = WorkerMessage::Hello(Hello::new(env.build.clone()));
    let sent = match hello.encode() {
        Ok(text) => link.send(Message::Text(text.into())).await.is_ok(),
        Err(error) => {
            tracing::error!(error = %error, "could not encode hello");
            false
        }
    };
    if !sent {
        return Handshake::End(How::Dropped(DropCause::Io));
    }
    let answer = tokio::select! {
        answer = tokio::time::timeout(env.timings.handshake, link.next()) => answer,
        () = shutdown.as_mut() => return Handshake::End(close_for_shutdown(link, env).await),
    };
    let Ok(frame) = answer else {
        return Handshake::End(How::Dropped(DropCause::HandshakeTimeout));
    };
    match frame {
        Some(Ok(Message::Text(text))) => match ServerMessage::decode(&text) {
            Ok(ServerMessage::Welcome(welcome))
                if SUPPORTED_VERSIONS.contains(welcome.protocol) =>
            {
                Handshake::Welcome(welcome.protocol)
            }
            Ok(ServerMessage::Welcome(_)) => Handshake::End(
                violate(
                    link,
                    "the server chose a protocol version this worker did not offer",
                )
                .await,
            ),
            Ok(ServerMessage::Reject(reject)) => match reject.reason {
                RejectReason::UnsupportedVersion { supported } => {
                    Handshake::End(How::Rejected { supported })
                }
            },
            Err(_) => Handshake::End(
                violate(link, "the server's answer to hello was not a message").await,
            ),
        },
        Some(Ok(Message::Binary(_))) => {
            Handshake::End(violate(link, "the server answered hello with binary data").await)
        }
        other => match ended(other) {
            Some(how) => Handshake::End(how),
            // A ping or pong before the answer is not an answer. The server sends none, so one is
            // a violation rather than a reason to wait again.
            None => Handshake::End(violate(link, "the server did not answer hello first").await),
        },
    }
}

/// The connected loop.
async fn serve(
    link: &mut Link,
    env: &Env,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
) -> How {
    let timings = env.timings;
    let mut tick = tokio::time::interval(timings.watchdog_tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_frame = Instant::now();
    let mut clocks = (Instant::now(), env.wall.now());
    loop {
        tokio::select! {
            frame = link.next() => {
                last_frame = Instant::now();
                match frame {
                    Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                        return violate(link, "the server sent a message protocol 1 does not have").await;
                    }
                    other => if let Some(how) = ended(other) {
                        return how;
                    },
                }
            }
            _ = tick.tick() => {
                let now = (Instant::now(), env.wall.now());
                if slept(clocks, now, timings.wake_gap) {
                    return How::Dropped(DropCause::Woke);
                }
                clocks = now;
                if last_frame.elapsed() > timings.silence {
                    return How::Dropped(DropCause::Silent);
                }
            }
            () = shutdown.as_mut() => return close_for_shutdown(link, env).await,
        }
    }
}

/// Whether the wall clock ran ahead of the monotonic clock by more than `gap` between `before`
/// and `after`: the machine slept in between.
fn slept(before: (Instant, SystemTime), after: (Instant, SystemTime), gap: Duration) -> bool {
    let monotonic = after.0.saturating_duration_since(before.0);
    // A wall clock that went backwards slept for no time.
    let wall = after.1.duration_since(before.1).unwrap_or_default();
    wall.saturating_sub(monotonic) > gap
}

/// What a frame that is not a message means: `None` for a ping or pong, else how the link ended.
fn ended(frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) -> Option<How> {
    match frame {
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => None,
        Some(Ok(Message::Close(close))) => Some(How::Closed(
            close.map_or(ServerClose::Other(1005), |close| {
                classify(u16::from(close.code))
            }),
        )),
        // Text and binary are handled by the caller; a raw frame is never yielded when reading.
        Some(Ok(Message::Text(_) | Message::Binary(_) | Message::Frame(_)) | Err(_)) | None => {
            Some(How::Dropped(DropCause::Io))
        }
    }
}

/// Classifies a close code.
pub(crate) const fn classify(code: u16) -> ServerClose {
    match CloseCode::from_code(code) {
        Some(code) => ServerClose::Abc(code),
        None => ServerClose::Other(code),
    }
}

/// Closes the link with 4400 because the server broke the protocol.
async fn violate(link: &mut Link, what: &'static str) -> How {
    let frame = CloseFrame {
        code: WireCode::from(CloseCode::ProtocolViolation.code()),
        reason: what.into(),
    };
    // The server may already be gone; the link is over either way.
    let _unheard = link.close(Some(frame)).await;
    How::Violation(what)
}

/// Closes the link with 1000 and waits up to the close grace for the server's own close, so the
/// server records a clean end.
async fn close_for_shutdown(link: &mut Link, env: &Env) -> How {
    let frame = CloseFrame {
        code: WireCode::Normal,
        reason: "the worker is stopping".into(),
    };
    if link.send(Message::Close(Some(frame))).await.is_ok() {
        let drained = tokio::time::timeout(env.timings.close_grace, async {
            while let Some(Ok(answer)) = link.next().await {
                if matches!(answer, Message::Close(_)) {
                    break;
                }
            }
        })
        .await;
        if drained.is_err() {
            tracing::debug!("the server did not answer the close in time");
        }
    }
    How::Shutdown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_are_classified() {
        for code in CloseCode::ALL {
            assert_eq!(
                classify(code.code()),
                ServerClose::Abc(code),
                "{code:?} is abc's"
            );
        }
        for code in [1000, 1001, 1005, 1011, 4000, 4999] {
            assert_eq!(classify(code), ServerClose::Other(code), "{code} is not");
        }
    }

    #[test]
    fn a_wall_clock_ahead_of_the_monotonic_one_means_the_machine_slept() {
        let mono = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let gap = Duration::from_secs(10);
        let at = |mono_secs: u64, wall_secs: u64| {
            (
                mono + Duration::from_secs(mono_secs),
                wall + Duration::from_secs(wall_secs),
            )
        };
        assert!(
            !slept(at(0, 0), at(5, 5), gap),
            "both clocks moved together"
        );
        assert!(
            !slept(at(0, 0), at(5, 15), gap),
            "ten seconds ahead is within the gap"
        );
        assert!(
            slept(at(0, 0), at(5, 16), gap),
            "more than ten seconds ahead is a sleep"
        );
        assert!(
            !slept(at(0, 100), at(5, 0), gap),
            "a wall clock stepped back is no sleep"
        );
        assert!(
            !slept(at(0, 0), at(60, 5), gap),
            "a slow wall clock is no sleep"
        );
    }

    #[test]
    fn frames_that_are_not_messages_end_the_link_or_not() {
        assert_eq!(
            ended(Some(Ok(Message::Ping(Vec::new().into())))),
            None,
            "a ping"
        );
        assert_eq!(
            ended(Some(Ok(Message::Pong(Vec::new().into())))),
            None,
            "a pong"
        );
        assert_eq!(
            ended(Some(Ok(Message::Close(Some(CloseFrame {
                code: WireCode::from(4403),
                reason: "revoked".into(),
            }))))),
            Some(How::Closed(ServerClose::Abc(CloseCode::Revoked))),
            "a close frame names its code"
        );
        assert_eq!(
            ended(Some(Ok(Message::Close(None)))),
            Some(How::Closed(ServerClose::Other(1005))),
            "a close without a code is 1005, no status"
        );
        assert_eq!(
            ended(None),
            Some(How::Dropped(DropCause::Io)),
            "the end of the stream"
        );
        assert_eq!(
            ended(Some(Err(
                tokio_tungstenite::tungstenite::Error::ConnectionClosed
            ))),
            Some(How::Dropped(DropCause::Io)),
            "a socket error"
        );
    }
}
