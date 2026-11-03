//! Every reconnect decision, and nowhere else. Pure: no IO and no clock; jitter arrives as a drawn
//! [`Fraction`]. The matches are exhaustive with no wildcard arms, so a new close code, refusal or
//! way for a link to end fails the build until it is decided here.
//!
//! | What happened                                         | Verdict                                  |
//! | ----------------------------------------------------- | ---------------------------------------- |
//! | refused `invalid_worker_key`                          | stop: [`Terminal::InvalidKey`]           |
//! | refused `worker_revoked`; closed 4403                 | stop: [`Terminal::Revoked`]              |
//! | closed 4409 (this process holds one link at a time)   | stop: [`Terminal::Superseded`]           |
//! | `Reject`: no protocol version in common               | stop: [`Terminal::VersionMismatch`]      |
//! | the machine slept, on a link or waiting to dial again | dial again at once, backoff reset        |
//! | refused `rate_limited`                                | dial again after the backoff, at least the rate-limit floor |
//! | anything else: closed 4400, 4408, 4503 or another code; the link died, fell silent or timed out its handshake; the server broke the protocol; refused otherwise; the dial failed | dial again after the backoff |
//! | the worker is stopping                                | done                                     |
//!
//! Backoff: equal jitter. The `n`th consecutive retry waits `c/2 + draw * c/2`, where
//! `c = min(cap, base * 2^n)`; a link that stayed up for the stable timing resets `n`. Equal jitter
//! rather than full jitter because its floor is guaranteed: with the protocol's timings a worker
//! whose every link fails at once starts at most ten sessions in its first minute and four a
//! minute after that, well under the server's limit of 30 (a test checks the worst case).

use std::time::Duration;

use abc_protocol::link::CloseCode;
use abc_protocol::{SUPPORTED_VERSIONS, VersionRange};
use thiserror::Error;

use crate::Timings;
use crate::dial::Refusal;
use crate::session::{DropCause, How, ServerClose, SessionEnd};

/// What came of one attempt to hold a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome<'a> {
    /// The server answered the upgrade with a refusal.
    Refused(Refusal),
    /// The link could not be opened.
    DialFailed,
    /// The link was up and ended.
    Ended(&'a SessionEnd),
    /// The machine slept while the worker waited to dial again.
    Slept,
}

/// What to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Dial again after this long.
    RetryAfter(Duration),
    /// Stop: a person has to act.
    Stop(Terminal),
    /// Stop: the worker was asked to.
    Done,
}

/// The server says this worker must stop, and retrying cannot change that. The binary exits with
/// code 3 so a service manager stops restarting it. `Display` is the sentence the user reads.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Terminal {
    /// Refused `worker_revoked`, or closed 4403.
    #[error(
        "this worker was revoked; to use this machine again, enrol it with a new token: abc worker enrol --replace <url>"
    )]
    Revoked,
    /// Refused `invalid_worker_key`: the server does not know this key (another server, or one
    /// whose database was reset).
    #[error(
        "the server does not know this worker's key; enrol this machine again: abc worker enrol --replace <url>"
    )]
    InvalidKey,
    /// Closed 4409 while this process held its only link: another process connected with the
    /// same key.
    #[error(
        "another connection is using this worker's key (was the worker home copied to another machine?); enrol each machine with its own token"
    )]
    Superseded,
    /// `Reject`: the server and this build share no protocol version.
    #[error(
        "the server speaks protocol {server} and this worker speaks {worker}; upgrade whichever is older"
    )]
    VersionMismatch {
        /// The server's versions.
        server: VersionRange,
        /// This build's.
        worker: VersionRange,
    },
}

/// A number in `[0, 1)`, drawn by [`crate::Jitter`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Fraction(f64);

impl Fraction {
    /// Zero, the lowest draw.
    pub(crate) const ZERO: Self = Self(0.0);

    /// `value` if it lies in `[0, 1)`.
    pub(crate) fn new(value: f64) -> Option<Self> {
        (0.0..1.0).contains(&value).then_some(Self(value))
    }
}

/// The highest backoff exponent tracked; the cap is reached long before it.
const MAX_EXPONENT: u32 = 16;

/// The reconnect state: how many retries in a row so far.
#[derive(Debug, Default)]
pub(crate) struct Policy {
    exponent: u32,
}

impl Policy {
    /// Decides what follows `outcome`, drawing jitter from `draw`.
    pub(crate) fn decide(
        &mut self,
        outcome: Outcome<'_>,
        timings: &Timings,
        draw: Fraction,
    ) -> Verdict {
        let end = match outcome {
            Outcome::Refused(refusal) => {
                return match refusal {
                    Refusal::InvalidKey => Verdict::Stop(Terminal::InvalidKey),
                    Refusal::Revoked => Verdict::Stop(Terminal::Revoked),
                    Refusal::RateLimited => Verdict::RetryAfter(
                        self.backoff(timings, draw).max(timings.rate_limited_floor),
                    ),
                    Refusal::Unavailable | Refusal::Unexpected { .. } => {
                        Verdict::RetryAfter(self.backoff(timings, draw))
                    }
                };
            }
            Outcome::DialFailed => return Verdict::RetryAfter(self.backoff(timings, draw)),
            Outcome::Slept => return self.woke(),
            Outcome::Ended(end) => end,
        };
        if end.lived >= timings.stable_after {
            self.exponent = 0;
        }
        match &end.how {
            How::Closed(ServerClose::Abc(CloseCode::Revoked)) => Verdict::Stop(Terminal::Revoked),
            How::Closed(ServerClose::Abc(CloseCode::Superseded)) => {
                Verdict::Stop(Terminal::Superseded)
            }
            How::Rejected { supported } => Verdict::Stop(Terminal::VersionMismatch {
                server: *supported,
                worker: SUPPORTED_VERSIONS,
            }),
            How::Dropped(DropCause::Woke) => self.woke(),
            How::Closed(
                ServerClose::Abc(
                    CloseCode::HandshakeTimeout
                    | CloseCode::ProtocolViolation
                    | CloseCode::ServerShutdown,
                )
                | ServerClose::Other(_),
            )
            | How::Dropped(DropCause::Io | DropCause::Silent | DropCause::HandshakeTimeout)
            | How::Violation(_) => Verdict::RetryAfter(self.backoff(timings, draw)),
            How::Shutdown => Verdict::Done,
        }
    }

    /// After a sleep the network has likely changed and the server has long dropped the link:
    /// dial at once, and start the backoff over.
    fn woke(&mut self) -> Verdict {
        self.exponent = 0;
        Verdict::RetryAfter(Duration::ZERO)
    }

    /// The next delay, `c/2 + draw * c/2` with `c = min(cap, base * 2^n)`, and one more retry.
    fn backoff(&mut self, timings: &Timings, draw: Fraction) -> Duration {
        let ceiling = timings
            .backoff_base
            .checked_mul(2_u32.saturating_pow(self.exponent))
            .map_or(timings.backoff_cap, |ceiling| {
                ceiling.min(timings.backoff_cap)
            });
        self.exponent = self.exponent.saturating_add(1).min(MAX_EXPONENT);
        let half = ceiling.checked_div(2).unwrap_or_default();
        half.saturating_add(half.mul_f64(draw.0))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const ZERO: Fraction = Fraction::ZERO;
    const HALF: Fraction = Fraction(0.5);

    fn timings() -> Timings {
        Timings::production()
    }

    fn ended(lived: Duration, how: How) -> SessionEnd {
        SessionEnd { lived, how }
    }

    fn after(how: How) -> SessionEnd {
        ended(Duration::from_secs(1), how)
    }

    fn decide(policy: &mut Policy, end: &SessionEnd, draw: Fraction) -> Verdict {
        policy.decide(Outcome::Ended(end), &timings(), draw)
    }

    #[test]
    fn a_person_must_act_when_the_server_says_so() {
        let mut policy = Policy::default();
        let t = timings();
        assert_eq!(
            policy.decide(Outcome::Refused(Refusal::InvalidKey), &t, HALF),
            Verdict::Stop(Terminal::InvalidKey),
            "an unknown key"
        );
        assert_eq!(
            policy.decide(Outcome::Refused(Refusal::Revoked), &t, HALF),
            Verdict::Stop(Terminal::Revoked),
            "refused as revoked"
        );
        assert_eq!(
            decide(
                &mut policy,
                &after(How::Closed(ServerClose::Abc(CloseCode::Revoked))),
                HALF
            ),
            Verdict::Stop(Terminal::Revoked),
            "revoked while connected"
        );
        assert_eq!(
            decide(
                &mut policy,
                &after(How::Closed(ServerClose::Abc(CloseCode::Superseded))),
                HALF
            ),
            Verdict::Stop(Terminal::Superseded),
            "another holder of the key"
        );
        let server = VersionRange::single(abc_protocol::ProtocolVersion::new(9));
        assert_eq!(
            decide(
                &mut policy,
                &after(How::Rejected { supported: server }),
                HALF
            ),
            Verdict::Stop(Terminal::VersionMismatch {
                server,
                worker: SUPPORTED_VERSIONS
            }),
            "no common version"
        );
        assert_eq!(
            Terminal::VersionMismatch {
                server,
                worker: SUPPORTED_VERSIONS
            }
            .to_string(),
            "the server speaks protocol 9 and this worker speaks 1; upgrade whichever is older",
            "both sides are named"
        );
        assert_eq!(
            decide(&mut policy, &after(How::Shutdown), HALF),
            Verdict::Done,
            "a stop the worker asked for"
        );
    }

    #[test]
    fn everything_else_is_retried_with_a_growing_backoff() {
        let retried = [
            Outcome::Refused(Refusal::Unavailable),
            Outcome::Refused(Refusal::Unexpected {
                status: 404,
                code: None,
            }),
            Outcome::DialFailed,
        ];
        let ends = [
            How::Closed(ServerClose::Abc(CloseCode::HandshakeTimeout)),
            How::Closed(ServerClose::Abc(CloseCode::ProtocolViolation)),
            How::Closed(ServerClose::Abc(CloseCode::ServerShutdown)),
            How::Closed(ServerClose::Other(1000)),
            How::Closed(ServerClose::Other(1011)),
            How::Dropped(DropCause::Io),
            How::Dropped(DropCause::Silent),
            How::Dropped(DropCause::HandshakeTimeout),
            How::Violation("v"),
        ]
        .map(after);
        let outcomes = retried.into_iter().chain(ends.iter().map(Outcome::Ended));
        for outcome in outcomes {
            let mut policy = Policy::default();
            let delays: Vec<Verdict> = (0..9)
                .map(|_| policy.decide(outcome, &timings(), ZERO))
                .collect();
            let expected: Vec<Verdict> = [250, 500, 1000, 2000, 4000, 8000, 15_000, 15_000, 15_000]
                .into_iter()
                .map(|millis| Verdict::RetryAfter(Duration::from_millis(millis)))
                .collect();
            assert_eq!(
                delays, expected,
                "{outcome:?}: half of 0.5 s doubling up to half of 30 s at the lowest draw"
            );
        }
    }

    #[test]
    fn jitter_spreads_the_upper_half() {
        let mut policy = Policy::default();
        let draw = Fraction::new(0.5).unwrap();
        assert_eq!(
            policy.decide(Outcome::DialFailed, &timings(), draw),
            Verdict::RetryAfter(Duration::from_millis(375)),
            "half of 0.5 s plus half of the other half"
        );
        assert_eq!(Fraction::new(1.0), None, "1 is out of range");
        assert_eq!(Fraction::new(-0.1), None, "and so is a negative");
        assert_eq!(Fraction::new(0.0), Some(ZERO), "0 is in");
    }

    #[test]
    fn a_rate_limit_waits_at_least_its_floor() {
        let mut policy = Policy::default();
        assert_eq!(
            policy.decide(Outcome::Refused(Refusal::RateLimited), &timings(), ZERO),
            Verdict::RetryAfter(Duration::from_secs(15)),
            "the floor"
        );
    }

    #[test]
    fn a_stable_link_or_a_wake_resets_the_backoff() {
        let mut policy = Policy::default();
        for _ in 0..5 {
            policy.decide(Outcome::DialFailed, &timings(), ZERO);
        }
        let stable = ended(Duration::from_secs(60), How::Dropped(DropCause::Silent));
        assert_eq!(
            decide(&mut policy, &stable, ZERO),
            Verdict::RetryAfter(Duration::from_millis(250)),
            "a link up for a minute starts over"
        );
        for _ in 0..5 {
            policy.decide(Outcome::DialFailed, &timings(), ZERO);
        }
        let short = ended(Duration::from_secs(59), How::Dropped(DropCause::Silent));
        assert_eq!(
            decide(&mut policy, &short, ZERO),
            Verdict::RetryAfter(Duration::from_secs(15)),
            "a shorter one does not"
        );
        assert_eq!(
            decide(&mut policy, &after(How::Dropped(DropCause::Woke)), HALF),
            Verdict::RetryAfter(Duration::ZERO),
            "after sleep, dial at once"
        );
        assert_eq!(
            policy.decide(Outcome::DialFailed, &timings(), ZERO),
            Verdict::RetryAfter(Duration::from_millis(250)),
            "and start the backoff over"
        );
        for _ in 0..5 {
            policy.decide(Outcome::DialFailed, &timings(), ZERO);
        }
        assert_eq!(
            policy.decide(Outcome::Slept, &timings(), HALF),
            Verdict::RetryAfter(Duration::ZERO),
            "a sleep while waiting to dial again: dial at once"
        );
        assert_eq!(
            policy.decide(Outcome::DialFailed, &timings(), ZERO),
            Verdict::RetryAfter(Duration::from_millis(250)),
            "and start the backoff over too"
        );
    }

    #[test]
    fn the_exponent_stops_growing_at_the_cap() {
        let mut policy = Policy::default();
        for _ in 0..100 {
            policy.decide(Outcome::DialFailed, &timings(), ZERO);
        }
        assert_eq!(policy.exponent, MAX_EXPONENT, "bounded");
        assert_eq!(
            policy.decide(Outcome::DialFailed, &timings(), ZERO),
            Verdict::RetryAfter(Duration::from_secs(15)),
            "still half the cap"
        );
    }

    /// The server's limit on session starts a minute, for one worker.
    const SERVER_STARTS_PER_MINUTE: usize = 30;

    /// Proptest's defaults, with failing cases saved where this repository keeps them:
    /// `proptest-regressions/` beside the source file. `cargo xtask check` fixes the seed.
    fn proptest_config() -> ProptestConfig {
        ProptestConfig {
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::SourceParallel(
                    "proptest-regressions",
                ),
            )),
            ..ProptestConfig::default()
        }
    }

    proptest! {
        #![proptest_config(proptest_config())]

        /// However the draws fall, a worker whose every link fails at once (each costing one
        /// session start) stays under the server's limit in every minute.
        #[test]
        fn a_failing_worker_stays_under_the_servers_limit(
            draws in proptest::collection::vec(0.0_f64..1.0, 64),
        ) {
            let t = timings();
            let mut policy = Policy::default();
            let mut at = Duration::ZERO;
            let mut starts = Vec::new();
            for draw in draws {
                starts.push(at);
                let Verdict::RetryAfter(delay) = policy.decide(
                    Outcome::Ended(&after(How::Closed(ServerClose::Other(1011)))),
                    &t,
                    Fraction::new(draw).unwrap(),
                ) else {
                    panic!("a failed link is retried");
                };
                at = at.saturating_add(delay);
            }
            for (index, start) in starts.iter().enumerate() {
                let in_minute = starts[index..]
                    .iter()
                    .take_while(|later| **later < start.saturating_add(Duration::from_secs(60)))
                    .count();
                prop_assert!(
                    in_minute < SERVER_STARTS_PER_MINUTE / 2,
                    "{} starts in the minute from {:?}", in_minute, start
                );
            }
        }
    }
}
