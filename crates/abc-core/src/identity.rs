//! Validated value types for names and identities. Parse once at the boundary, trust inside.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

/// A value that failed validation. `field` names which input was wrong. The value itself is never
/// echoed, because some of these fields come from untrusted headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invalid {field}: {rule}")]
pub struct InvalidValue {
    /// The input that failed, such as `tailnet login`.
    pub field: &'static str,
    /// The rule it broke, such as `1 to 254 printable ASCII characters`.
    pub rule: &'static str,
}

/// A person's Tailscale login, as Tailscale Serve asserts it in `Tailscale-User-Login`, for example
/// `alice@example.com` or `alice@github`.
///
/// Holding one proves nothing by itself. It means something only when abc-server's sidecar
/// extractor built it from a request that arrived on the sidecar Unix socket. abc-store turns it
/// into a `Principal` inside the request's transaction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(try_from = "String")]
#[ts(export, type = "string")]
pub struct TailnetLogin(String);

impl TailnetLogin {
    /// Parses a header value: 1 to 254 bytes of printable ASCII with no spaces.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] for anything else.
    pub fn parse(raw: &str) -> Result<Self, InvalidValue> {
        let _ = raw;
        unimplemented!()
    }

    /// The login text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TailnetLogin {
    type Error = InvalidValue;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

macro_rules! bounded_text {
    ($(#[$meta:meta])* $name:ident, $field:literal, $max:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
        #[serde(try_from = "String")]
        #[ts(export, type = "string")]
        pub struct $name(String);

        impl $name {
            /// Trims, then requires 1 to the maximum length in characters, with no control characters.
            ///
            /// # Errors
            ///
            /// Returns [`InvalidValue`] when the trimmed text breaks that rule.
            pub fn parse(raw: &str) -> Result<Self, InvalidValue> {
                let _ = (raw, $field, $max);
                unimplemented!()
            }

            /// The text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidValue;
            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::parse(&raw)
            }
        }
    };
}

bounded_text!(
    /// How a person or worker is shown: an actor's display name.
    DisplayName, "display name", 64
);
bounded_text!(
    /// A workspace's name.
    WorkspaceName, "workspace name", 64
);
bounded_text!(
    /// A note the owner attaches to an enrolment token, such as "work laptop".
    MachineLabel, "machine label", 64
);

/// What a worker reports about its machine at enrolment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export)]
pub struct MachineInfo {
    /// The machine's hostname, which becomes the worker's default display name.
    pub hostname: DisplayName,
    /// The operating system.
    pub os: Os,
    /// The CPU architecture.
    pub arch: Arch,
}

/// The operating systems abc ships for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Os {
    /// macOS.
    Macos,
    /// Linux.
    Linux,
}

/// The CPU architectures abc ships for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Arch {
    /// 64-bit ARM.
    Aarch64,
    /// 64-bit x86.
    X86_64,
}
