//! Time as a value. abc-core never reads a clock: the store reads its [`Clock`] once per request
//! and passes the instant in, so expiry rules are pure and tests set time explicitly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};
use ts_rs::TS;

/// An instant in UTC, with microsecond precision to match Postgres `timestamptz`. On the wire it
/// is an RFC 3339 string.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(transparent)]
#[ts(type = "string")]
pub struct Timestamp(
    #[serde(with = "time::serde::rfc3339")]
    #[schemars(with = "String")]
    OffsetDateTime,
);

impl Timestamp {
    /// Truncates to microseconds and converts to UTC, so values round-trip through Postgres
    /// unchanged. Replay equality depends on that.
    #[must_use]
    pub fn from_utc(at: OffsetDateTime) -> Self {
        let utc = at.to_offset(UtcOffset::UTC);
        // A microsecond count read from a valid instant is always a valid microsecond count.
        Self(utc.replace_microsecond(utc.microsecond()).unwrap_or(utc))
    }

    /// The underlying value, for storage.
    #[must_use]
    pub const fn as_offset(self) -> OffsetDateTime {
        self.0
    }

    /// `self + duration`, or `None` when the result is not a representable instant.
    #[must_use]
    pub fn checked_add(self, duration: time::Duration) -> Option<Self> {
        self.0.checked_add(duration).map(Self::from_utc)
    }
}

/// A source of the current time. The store holds one: the system clock in production and
/// `ManualClock` in tests (abc-store `testing` feature).
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn from_utc_truncates_to_microseconds_and_converts_to_utc() {
        let at = datetime!(2026-01-01 12:00:00.123_456_789 +02:00);
        let stamp = Timestamp::from_utc(at);
        assert_eq!(
            stamp.as_offset(),
            datetime!(2026-01-01 10:00:00.123_456 UTC),
            "nanoseconds are dropped and the offset becomes UTC"
        );
        assert_eq!(stamp.as_offset().offset(), UtcOffset::UTC, "stored in UTC");
    }

    #[test]
    fn checked_add_adds_or_reports_overflow() {
        let at = Timestamp::from_utc(datetime!(2026-01-01 00:00:00 UTC));
        assert_eq!(
            at.checked_add(time::Duration::minutes(30)),
            Some(Timestamp::from_utc(datetime!(2026-01-01 00:30:00 UTC))),
            "thirty minutes later"
        );
        assert_eq!(
            at.checked_add(time::Duration::MAX),
            None,
            "an instant past the representable range is None"
        );
    }

    #[test]
    fn the_wire_form_is_rfc3339() {
        let at = Timestamp::from_utc(datetime!(2026-01-01 00:00:00.5 UTC));
        let json = serde_json::to_string(&at).unwrap();
        assert_eq!(json, r#""2026-01-01T00:00:00.5Z""#, "RFC 3339 in UTC");
        assert_eq!(
            serde_json::from_str::<Timestamp>(&json).unwrap(),
            at,
            "and reads back"
        );
    }
}
