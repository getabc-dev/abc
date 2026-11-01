//! Time as a value. abc-core never reads a clock: the store reads its [`Clock`] once per request
//! and passes the instant in, so expiry rules are pure and tests set time explicitly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ts_rs::TS;

/// An instant in UTC, with microsecond precision to match Postgres `timestamptz`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(transparent)]
#[ts(export, type = "string")]
pub struct Timestamp(#[schemars(with = "String")] OffsetDateTime);

impl Timestamp {
    /// Truncates to microseconds and converts to UTC, so values round-trip through Postgres
    /// unchanged. Replay equality depends on that.
    #[must_use]
    pub fn from_utc(at: OffsetDateTime) -> Self {
        let _ = at;
        unimplemented!("truncate to micros, to_offset(UTC)")
    }

    /// The underlying value, for storage.
    #[must_use]
    pub const fn as_offset(self) -> OffsetDateTime {
        self.0
    }

    /// `self + duration`, or `None` on overflow (no arithmetic side effects).
    #[must_use]
    pub fn checked_add(self, duration: time::Duration) -> Option<Self> {
        self.0.checked_add(duration).map(Self)
    }
}

/// A source of the current time. The store holds one: `SystemClock` in production and
/// `ManualClock` in tests (abc-store `testing` feature).
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current instant.
    fn now(&self) -> Timestamp;
}
