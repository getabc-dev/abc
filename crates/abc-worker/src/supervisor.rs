//! The worker's loop: dial, hold the link until it ends, ask [`crate::policy`] what next, wait,
//! and dial again, until the server says stop or the worker is asked to. The wait watches the
//! clocks as a session does: a machine that sleeps through a backoff dials at once on waking,
//! because tokio's timers stop during a sleep and would otherwise run on for the rest of it.

use std::pin::Pin;
use std::time::Duration;

use tokio::time::Instant;

use abc_core::credential::WorkerKey;
use abc_core::secret::Secret;
use abc_platform::telemetry::chain;
use abc_protocol::url::PublicUrl;

use crate::Env;
use crate::dial::{self, Dialed};
use crate::policy::{Outcome, Policy, Terminal, Verdict};
use crate::session::{self, How, SessionEnd};

/// Holds a link to `server` with `key` until the policy stops (`Err`) or `shutdown` resolves
/// (`Ok`).
pub(crate) async fn supervise(
    env: &Env,
    server: &PublicUrl,
    key: &Secret<WorkerKey>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Terminal> {
    let mut shutdown = std::pin::pin!(shutdown);
    let mut policy = Policy::default();
    loop {
        let attempt = hold(env, server, key, &mut shutdown).await;
        let outcome = match &attempt {
            Attempt::Refused(refusal) => Outcome::Refused(*refusal),
            Attempt::DialFailed => Outcome::DialFailed,
            Attempt::Ended(end) => Outcome::Ended(end),
            Attempt::Stopped => return Ok(()),
        };
        let mut verdict = policy.decide(outcome, &env.timings, env.jitter.draw());
        loop {
            match verdict {
                Verdict::RetryAfter(delay) => {
                    tracing::info!(
                        retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        "dialling the server again"
                    );
                    match wait(env, delay, &mut shutdown).await {
                        Waited::Elapsed => break,
                        Waited::Slept => {
                            tracing::info!("the machine slept while waiting to dial again");
                            verdict =
                                policy.decide(Outcome::Slept, &env.timings, env.jitter.draw());
                        }
                        Waited::Stopped => return Ok(()),
                    }
                }
                Verdict::Stop(terminal) => {
                    tracing::error!(reason = %terminal, "stopping: the server said this worker must stop");
                    return Err(terminal);
                }
                Verdict::Done => return Ok(()),
            }
        }
    }
}

/// How a wait before dialling again ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
    /// It ran its course.
    Elapsed,
    /// The machine slept during it.
    Slept,
    /// The worker was asked to stop.
    Stopped,
}

/// Waits `delay` of monotonic time, comparing the clocks every watchdog tick as a session does.
async fn wait(
    env: &Env,
    delay: Duration,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
) -> Waited {
    let timings = env.timings;
    let elapsed = tokio::time::sleep(delay);
    let mut elapsed = std::pin::pin!(elapsed);
    let mut tick = tokio::time::interval(timings.watchdog_tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut clocks = (Instant::now(), env.wall.now());
    loop {
        tokio::select! {
            () = elapsed.as_mut() => return Waited::Elapsed,
            _ = tick.tick() => {
                let now = (Instant::now(), env.wall.now());
                if session::slept(clocks, now, timings.wake_gap) {
                    return Waited::Slept;
                }
                clocks = now;
            }
            () = shutdown.as_mut() => return Waited::Stopped,
        }
    }
}

/// What one attempt came to.
#[derive(Debug)]
enum Attempt {
    Refused(dial::Refusal),
    DialFailed,
    Ended(SessionEnd),
    /// The worker was asked to stop while dialling.
    Stopped,
}

/// Dials once and, if the link opens, holds it until it ends; logs how it went.
async fn hold(
    env: &Env,
    server: &PublicUrl,
    key: &Secret<WorkerKey>,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
) -> Attempt {
    let dialed = tokio::select! {
        dialed = dial::connect(env, server, key) => dialed,
        () = shutdown.as_mut() => return Attempt::Stopped,
    };
    match dialed {
        Ok(Dialed::Upgraded(link)) => {
            let end = session::run(*link, env, shutdown).await;
            log_end(&end);
            Attempt::Ended(end)
        }
        Ok(Dialed::Refused(refusal)) => {
            tracing::warn!(refusal = ?refusal, "the server refused the link");
            Attempt::Refused(refusal)
        }
        Err(error) => {
            tracing::warn!(error = %chain(&error), "could not open the link");
            Attempt::DialFailed
        }
    }
}

fn log_end(end: &SessionEnd) {
    let lived_ms = u64::try_from(end.lived.as_millis()).unwrap_or(u64::MAX);
    match &end.how {
        How::Violation(what) => {
            tracing::error!(
                lived_ms,
                violation = what,
                "closed the link: the server broke the protocol"
            );
        }
        How::Shutdown => tracing::info!(lived_ms, "closed the link: the worker is stopping"),
        how @ (How::Closed(_) | How::Dropped(_) | How::Rejected { .. }) => {
            tracing::warn!(lived_ms, how = ?how, "the link ended");
        }
    }
}
