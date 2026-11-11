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
#[ts(type = "string")]
pub struct TailnetLogin(String);

impl TailnetLogin {
    /// The longest login accepted, in bytes.
    const MAX_LEN: usize = 254;

    /// Parses a header value: 1 to 254 bytes of printable ASCII with no spaces.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] for anything else.
    pub fn parse(raw: &str) -> Result<Self, InvalidValue> {
        let valid = (1..=Self::MAX_LEN).contains(&raw.len())
            && raw.bytes().all(|byte| byte.is_ascii_graphic());
        if valid {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InvalidValue {
                field: "tailnet login",
                rule: "1 to 254 printable ASCII characters without spaces",
            })
        }
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

/// Trims `raw`, then requires 1 to `max` characters with no control characters.
fn bounded(raw: &str, max: usize, field: &'static str) -> Result<String, InvalidValue> {
    let trimmed = raw.trim();
    let length = trimmed.chars().count();
    if (1..=max).contains(&length) && !trimmed.chars().any(char::is_control) {
        Ok(trimmed.to_owned())
    } else {
        Err(InvalidValue {
            field,
            rule: "1 to 64 characters after trimming, with no control characters",
        })
    }
}

/// The longest name or label, in characters. The views' CHECK constraints use the same bound.
const NAME_MAX: usize = 64;

macro_rules! bounded_text {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
        #[serde(try_from = "String")]
        #[ts(type = "string")]
        // The JSON Schema carries the bounds, in characters (code points) after trimming, and the
        // web app's one validator reads them (`web/src/names.ts`).
        #[schemars(extend("minLength" = 1, "maxLength" = NAME_MAX))]
        pub struct $name(String);

        impl $name {
            /// Trims, then requires 1 to 64 characters with no control characters.
            ///
            /// # Errors
            ///
            /// Returns [`InvalidValue`] when the trimmed text breaks that rule.
            pub fn parse(raw: &str) -> Result<Self, InvalidValue> {
                bounded(raw, NAME_MAX, $field).map(Self)
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
    DisplayName,
    "display name"
);
bounded_text!(
    /// A workspace's name.
    WorkspaceName,
    "workspace name"
);
bounded_text!(
    /// A note the owner attaches to an enrolment token, such as "work laptop". It becomes the
    /// enrolled worker's display name.
    MachineLabel,
    "machine label"
);

/// A tailnet node's `MagicDNS` name, such as `macbook.tail1234.ts.net`, as abc's own tailscaled
/// reports it for the address a request came from. Like [`TailnetLogin`] it means something only
/// when abc-server built it, from its own tailscaled's answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(try_from = "String")]
#[ts(type = "string")]
pub struct TailnetNode(String);

impl TailnetNode {
    /// The longest DNS name, in bytes.
    const MAX_LEN: usize = 253;

    /// Parses a node name: 1 to 253 bytes of letters, digits, `-` and `.`, without the trailing
    /// dot of a fully qualified name (one is dropped).
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] for anything else.
    pub fn parse(raw: &str) -> Result<Self, InvalidValue> {
        let name = raw.strip_suffix('.').unwrap_or(raw);
        let valid = (1..=Self::MAX_LEN).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.');
        if valid {
            Ok(Self(name.to_ascii_lowercase()))
        } else {
            Err(InvalidValue {
                field: "tailnet node",
                rule: "1 to 253 letters, digits, dashes and dots",
            })
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TailnetNode {
    type Error = InvalidValue;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

/// Who asked: the tailnet person Serve asserted and the node abc's tailscaled says the request
/// came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
pub struct Requester {
    /// The person's login.
    pub login: TailnetLogin,
    /// Their node.
    pub node: TailnetNode,
}

/// A label obeys the same rule as a display name, so it always is one.
impl From<MachineLabel> for DisplayName {
    fn from(label: MachineLabel) -> Self {
        Self(label.0)
    }
}

/// What a worker reports about its machine at enrolment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
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
pub enum Os {
    /// macOS.
    Macos,
    /// Linux.
    Linux,
}

impl Os {
    /// The stored name, as in the `worker` view's CHECK constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }

    /// Reads a stored name.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] for a name this build does not know.
    pub fn parse(stored: &str) -> Result<Self, InvalidValue> {
        match stored {
            "macos" => Ok(Self::Macos),
            "linux" => Ok(Self::Linux),
            _ => Err(InvalidValue {
                field: "operating system",
                rule: "macos or linux",
            }),
        }
    }
}

/// The CPU architectures abc ships for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    /// 64-bit ARM.
    Aarch64,
    /// 64-bit x86.
    X86_64,
}

impl Arch {
    /// The stored name, as in the `worker` view's CHECK constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }

    /// Reads a stored name.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] for a name this build does not know.
    pub fn parse(stored: &str) -> Result<Self, InvalidValue> {
        match stored {
            "aarch64" => Ok(Self::Aarch64),
            "x86_64" => Ok(Self::X86_64),
            _ => Err(InvalidValue {
                field: "architecture",
                rule: "aarch64 or x86_64",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_are_printable_ascii_without_spaces() {
        assert_eq!(
            TailnetLogin::parse("alice@example.com").unwrap().as_str(),
            "alice@example.com",
            "an email login"
        );
        assert!(
            TailnetLogin::parse(&"a".repeat(254)).is_ok(),
            "254 bytes is the limit"
        );
        for bad in ["", "alice smith", "alice\n", "ålice", &"a".repeat(255)] {
            assert_eq!(
                TailnetLogin::parse(bad),
                Err(InvalidValue {
                    field: "tailnet login",
                    rule: "1 to 254 printable ASCII characters without spaces",
                }),
                "{bad:?} is not a login"
            );
        }
        assert_eq!(
            serde_json::from_str::<TailnetLogin>(r#""bob@github""#).unwrap(),
            TailnetLogin::parse("bob@github").unwrap(),
            "logins deserialize through parse"
        );
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(
            DisplayName::parse("  Alice  ").unwrap().as_str(),
            "Alice",
            "surrounding space is trimmed"
        );
        assert!(
            DisplayName::parse(&"é".repeat(64)).is_ok(),
            "64 characters, not bytes"
        );
        for bad in ["", "   ", "tab\there", &"x".repeat(65)] {
            assert_eq!(
                WorkspaceName::parse(bad),
                Err(InvalidValue {
                    field: "workspace name",
                    rule: "1 to 64 characters after trimming, with no control characters",
                }),
                "{bad:?} is not a name"
            );
        }
        assert_eq!(
            InvalidValue {
                field: "machine label",
                rule: "a rule"
            }
            .to_string(),
            "invalid machine label: a rule",
            "the message names the field and rule, never the value"
        );
        assert_eq!(
            serde_json::from_str::<MachineLabel>(r#"" work laptop ""#).unwrap(),
            MachineLabel::parse("work laptop").unwrap(),
            "labels deserialize through parse"
        );
        let name: DisplayName = MachineLabel::parse("laptop").unwrap().into();
        assert_eq!(name.as_str(), "laptop", "a label becomes a display name");
    }

    #[test]
    fn os_and_arch_round_trip_their_stored_names() {
        for os in [Os::Macos, Os::Linux] {
            assert_eq!(Os::parse(os.as_str()), Ok(os), "{os:?}");
        }
        for arch in [Arch::Aarch64, Arch::X86_64] {
            assert_eq!(Arch::parse(arch.as_str()), Ok(arch), "{arch:?}");
        }
        assert!(Os::parse("windows").is_err(), "unknown OS");
        assert!(Arch::parse("riscv64").is_err(), "unknown architecture");
        assert_eq!(
            serde_json::to_string(&MachineInfo {
                hostname: DisplayName::parse("mbp").unwrap(),
                os: Os::Macos,
                arch: Arch::X86_64,
            })
            .unwrap(),
            r#"{"hostname":"mbp","os":"macos","arch":"x86_64"}"#,
            "the wire names match the stored names"
        );
    }

    #[test]
    fn node_names_are_dns_names_without_the_trailing_dot() {
        assert_eq!(
            TailnetNode::parse("MacBook.tail1234.ts.net.")
                .unwrap()
                .as_str(),
            "macbook.tail1234.ts.net",
            "lower case, the root dot dropped"
        );
        assert_eq!(
            TailnetNode::parse(&"a".repeat(253)).unwrap().as_str().len(),
            253,
            "the longest"
        );
        for bad in ["", ".", "has space.ts.net", "under_score", &"a".repeat(254)] {
            assert_eq!(
                TailnetNode::parse(bad).unwrap_err().to_string(),
                "invalid tailnet node: 1 to 253 letters, digits, dashes and dots",
                "{bad:?}"
            );
        }
        assert!(
            serde_json::from_str::<TailnetNode>(r#""bad name""#).is_err(),
            "parsed on the wire too"
        );
    }
}
