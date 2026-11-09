//! How a `claude -p` attempt ended, read from the `result` line of its stream-json output, and
//! what the supervisor does next: the run's restart rules, as pure functions of the run and the
//! time.
//!
//! Claude Code ends a `--output-format stream-json` run with one line of `"type": "result"`. Its
//! fields this reads: `is_error`, `api_error` (`usage_limit_reached` for a usage limit,
//! `connection_failed` when the API cannot be reached) and `result`, the text: the handoff, or a
//! limit's reset time (`You've hit your 5-hour limit · resets 10:40am (UTC)`), or the network
//! error (`... (EAI_AGAIN)`). The tests hold results recorded from real runs.

use serde::{Deserialize, Serialize};

/// The most attempts one run makes.
pub(crate) const MAX_ATTEMPTS: usize = 40;

/// The most attempts in a row that end in an error or without a handoff.
pub(crate) const MAX_FAILURES: usize = 3;

/// How long the network may stay down before the run gives up, in seconds.
pub(crate) const MAX_OUTAGE: i64 = 24 * 60 * 60;

/// How long to wait after a limit whose reset time cannot be read, in seconds.
const UNKNOWN_RESET_WAIT: i64 = 30 * 60;

/// How long after a limit's reset the next attempt starts, in seconds.
const AFTER_RESET: i64 = 60;

/// The first wait after a network failure, doubled after each failed probe up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: i64 = 30;
const MAX_BACKOFF: i64 = 15 * 60;

/// The wait after a failed attempt, per failure in a row, in seconds.
const FAILURE_WAIT: i64 = 60;

/// Words in a result's text that name a network failure.
const NETWORK_ERRORS: [&str; 6] = [
    "EAI_AGAIN",
    "ENOTFOUND",
    "ECONNREFUSED",
    "ECONNRESET",
    "ETIMEDOUT",
    "ENETUNREACH",
];

/// A run's verdict, as its handoff states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Verdict {
    Pass,
    Fail,
    Blocked,
}

impl Verdict {
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Blocked => "BLOCKED",
        }
    }
}

/// How an attempt ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub(crate) enum Outcome {
    /// The agent handed off.
    Done { verdict: Verdict },
    /// A usage limit, with the reset in seconds since the Unix epoch when the text gives it.
    Limit { reset: Option<i64>, message: String },
    /// The API could not be reached.
    Network { message: String },
    /// The session ended without an error and without a handoff.
    Unfinished { message: String },
    /// Any other error, or no result at all.
    Error { message: String },
    /// The supervisor stopped while the attempt ran: killed, or the host rebooted.
    Interrupted,
}

impl Outcome {
    /// Says why an attempt ended, for the next session's prompt and the user.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Done { verdict } => format!("handed off {}", verdict.word()),
            Self::Limit {
                reset: Some(reset), ..
            } => {
                format!("a usage limit, reset at {}", crate::progress::utc(*reset))
            }
            Self::Limit { reset: None, .. } => "a usage limit".to_owned(),
            Self::Network { message } => format!("a network failure: {message}"),
            Self::Unfinished { message } => {
                format!("the session ended without a handoff: {message}")
            }
            Self::Error { message } => format!("an error: {message}"),
            Self::Interrupted => "the supervisor stopped (killed, or the host rebooted)".to_owned(),
        }
    }

    const fn failed(&self) -> bool {
        matches!(self, Self::Unfinished { .. } | Self::Error { .. })
    }
}

/// The most of a result's text an outcome keeps, in characters.
const MESSAGE_CHARS: usize = 500;

fn cut(text: &str) -> String {
    text.trim().chars().take(MESSAGE_CHARS).collect()
}

/// How an attempt ended, from its stream's last `result` line, if it printed one, at `now` (in
/// seconds since the Unix epoch, UTC).
pub(crate) fn classify(result: Option<&serde_json::Value>, now: i64) -> Outcome {
    let Some(result) = result else {
        return Outcome::Error {
            message: "claude ended without a result line".to_owned(),
        };
    };
    let text = result
        .get("result")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let api_error = result.get("api_error").and_then(serde_json::Value::as_str);
    let is_error = result
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if api_error == Some("usage_limit_reached") {
        return Outcome::Limit {
            reset: reset(text, now),
            message: cut(text),
        };
    }
    if api_error == Some("connection_failed")
        || (is_error && NETWORK_ERRORS.iter().any(|code| text.contains(code)))
    {
        return Outcome::Network { message: cut(text) };
    }
    if is_error {
        let subtype = result
            .get("subtype")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no subtype");
        let what = api_error.unwrap_or(subtype);
        return Outcome::Error {
            message: cut(&format!("{what}: {text}")),
        };
    }
    match verdict(text) {
        Some(verdict) => Outcome::Done { verdict },
        None => Outcome::Unfinished { message: cut(text) },
    }
}

/// The first of PASS, FAIL and BLOCKED that `text` names as a word.
pub(crate) fn verdict(text: &str) -> Option<Verdict> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .find_map(|word| match word {
            "PASS" => Some(Verdict::Pass),
            "FAIL" => Some(Verdict::Fail),
            "BLOCKED" => Some(Verdict::Blocked),
            _ => None,
        })
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// When a usage limit resets, from its text, `resets 10:40am (UTC)` or `resets Nov 12, 3pm
/// (UTC)`, in seconds since the Unix epoch, the next such time after `now`. Claude Code names the
/// time in its time zone, which in an agent VM is UTC (`TZ=UTC`); a time in any other zone is
/// not read.
pub(crate) fn reset(text: &str, now: i64) -> Option<i64> {
    let (_, rest) = text.split_once("resets ")?;
    let (when, rest) = rest.split_once(" (")?;
    let (zone, _) = rest.split_once(')')?;
    if !matches!(zone, "UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z") {
        return None;
    }
    let (date, clock) = match when.split_once(", ") {
        Some((date, clock)) => (Some(date), clock),
        None => (None, when),
    };
    let clock = clock.trim().to_ascii_lowercase();
    let (clock, afternoon) = if let Some(clock) = clock.strip_suffix("am") {
        (clock.to_owned(), false)
    } else {
        (clock.strip_suffix("pm")?.to_owned(), true)
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u8>().ok()?, minute.parse::<u8>().ok()?),
        None => (clock.parse::<u8>().ok()?, 0),
    };
    if !(1..=12).contains(&hour) {
        return None;
    }
    let hour = match (hour, afternoon) {
        (12, false) => 0,
        (12, true) => 12,
        (hour, false) => hour,
        (hour, true) => hour.checked_add(12)?,
    };
    let time = time::Time::from_hms(hour, minute, 0).ok()?;
    let now_at = time::OffsetDateTime::from_unix_timestamp(now).ok()?;
    let day = match date {
        None => now_at.date(),
        Some(date) => {
            let (month, day) = date.trim().split_once(' ')?;
            let month = MONTHS.iter().position(|name| *name == month)?;
            let month = time::Month::try_from(u8::try_from(month.checked_add(1)?).ok()?).ok()?;
            time::Date::from_calendar_date(now_at.year(), month, day.parse().ok()?).ok()?
        }
    };
    let mut at = day.with_time(time).assume_utc().unix_timestamp();
    // A time already past is the next one: tomorrow's for a clock time, next year's for a date.
    while at <= now {
        at = match date {
            None => at.checked_add(24 * 60 * 60)?,
            Some(_) => time::OffsetDateTime::from_unix_timestamp(at)
                .ok()?
                .replace_year(now_at.year().checked_add(1)?)
                .ok()?
                .unix_timestamp(),
        };
    }
    Some(at)
}

/// Why the supervisor waits before the next attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Cause {
    /// The run is starting: its first attempt, or the next after the supervisor stopped.
    Start,
    /// The last attempt failed.
    Retry,
    /// A usage limit, until its reset.
    Limit,
    /// The network is down; the supervisor probes the API before the next attempt.
    Network,
}

/// A network outage the run is waiting out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Outage {
    /// When the attempt that met it started, in seconds since the Unix epoch.
    pub(crate) since: i64,
    /// How many times the network was found down: the attempt that met it and failed probes.
    pub(crate) tries: u32,
}

/// What the supervisor does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Wait until `until`, in seconds since the Unix epoch, then attempt again; after a network
    /// failure, first probe the API.
    Wait {
        until: i64,
        cause: Cause,
        outage: Option<Outage>,
    },
    /// Done with the run, with its verdict.
    Done(Verdict),
    /// The run gives up, for this reason.
    GiveUp(String),
}

/// The wait after the `tries`-th time the network was found down.
fn backoff(tries: u32) -> i64 {
    let doublings = tries.saturating_sub(1).min(10);
    FIRST_BACKOFF
        .saturating_mul(2_i64.saturating_pow(doublings))
        .min(MAX_BACKOFF)
}

/// What follows an attempt that started at `started` and ended with `outcome` at `now`, the
/// earlier attempts having ended with `earlier` (oldest first), during `outage` if the network
/// was already down.
pub(crate) fn after_attempt(
    earlier: &[Outcome],
    outcome: &Outcome,
    started: i64,
    outage: Option<Outage>,
    now: i64,
) -> Next {
    let attempts = earlier.len().saturating_add(1);
    if let Outcome::Done { verdict } = outcome {
        return Next::Done(*verdict);
    }
    if attempts >= MAX_ATTEMPTS {
        return Next::GiveUp(format!(
            "{attempts} attempts, the most a run makes; the last ended with {}",
            outcome.describe()
        ));
    }
    match outcome {
        Outcome::Done { .. } | Outcome::Interrupted => Next::Wait {
            until: now,
            cause: Cause::Start,
            outage: None,
        },
        Outcome::Limit { reset, .. } => Next::Wait {
            until: reset.map_or(now.saturating_add(UNKNOWN_RESET_WAIT), |reset| {
                reset.saturating_add(AFTER_RESET).max(now)
            }),
            cause: Cause::Limit,
            outage: None,
        },
        Outcome::Network { .. } => {
            let outage = outage.unwrap_or(Outage {
                since: started,
                tries: 0,
            });
            network_down(outage, now)
        }
        Outcome::Unfinished { .. } | Outcome::Error { .. } => {
            let failures = earlier
                .iter()
                .rev()
                .take_while(|earlier| earlier.failed())
                .count()
                .saturating_add(1);
            if failures >= MAX_FAILURES {
                return Next::GiveUp(format!(
                    "{failures} attempts in a row failed; the last ended with {}",
                    outcome.describe()
                ));
            }
            Next::Wait {
                until: now.saturating_add(
                    FAILURE_WAIT.saturating_mul(i64::try_from(failures).unwrap_or(i64::MAX)),
                ),
                cause: Cause::Retry,
                outage: None,
            }
        }
    }
}

/// What follows finding the network down once more during `outage`, at `now`: a wait that
/// doubles each time, or giving up once it has been down for [`MAX_OUTAGE`].
pub(crate) fn network_down(outage: Outage, now: i64) -> Next {
    let outage = Outage {
        since: outage.since,
        tries: outage.tries.saturating_add(1),
    };
    if now.saturating_sub(outage.since) >= MAX_OUTAGE {
        return Next::GiveUp(format!(
            "the API has been unreachable since {}, {} hours",
            crate::progress::utc(outage.since),
            MAX_OUTAGE / 3600
        ));
    }
    Next::Wait {
        until: now.saturating_add(backoff(outage.tries)),
        cause: Cause::Network,
        outage: Some(outage),
    }
}
