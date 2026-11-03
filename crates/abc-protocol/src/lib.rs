//! Wire formats spoken between abc workers and the abc server.
//!
//! A worker opens every connection with a handshake. It sends [`WorkerMessage::Hello`] with the
//! range of protocol versions it speaks and the abc release it runs. The server picks a version
//! with [`negotiate`] and answers with [`ServerMessage::Welcome`], naming the version both sides
//! speak from then on, or with [`ServerMessage::Reject`] and the reason.
//!
//! Messages travel as JSON text, tagged by a `type` field. The handshake runs before the two sides
//! agree on a version, so every release has to be able to read it: decoders ignore fields they
//! don't know, which lets a newer peer add a field without breaking an older one. Renaming or
//! removing a field needs a new protocol version. Unknown message types and reject codes are
//! decoding errors.
//!
//! Before the handshake, a worker enrols once over HTTP ([`enrol`]) and from then on opens each
//! connection with its key in `Authorization: Bearer` ([`link`]), so no message here ever carries
//! a credential. Every HTTP error any client sees carries one of the stable codes in
//! [`api::ApiCode`].

pub mod api;
pub mod enrol;
pub mod link;
pub mod url;

use std::fmt;

use semver::Version;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1);

/// The protocol versions this build supports: [`PROTOCOL_VERSION`] alone.
pub const SUPPORTED_VERSIONS: VersionRange = VersionRange::single(PROTOCOL_VERSION);

/// One version of the worker-server protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(u32);

impl ProtocolVersion {
    /// The protocol version with the given number.
    #[must_use]
    pub const fn new(number: u32) -> Self {
        Self(number)
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An inclusive, non-empty range of protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawVersionRange")]
pub struct VersionRange {
    min: ProtocolVersion,
    max: ProtocolVersion,
}

impl VersionRange {
    /// The versions from `min` to `max`, both included.
    ///
    /// # Errors
    ///
    /// Returns [`InvertedRange`] when `min` is greater than `max`.
    pub const fn new(min: ProtocolVersion, max: ProtocolVersion) -> Result<Self, InvertedRange> {
        if min.0 > max.0 {
            Err(InvertedRange { min, max })
        } else {
            Ok(Self { min, max })
        }
    }

    /// The range holding only `version`.
    #[must_use]
    pub const fn single(version: ProtocolVersion) -> Self {
        Self {
            min: version,
            max: version,
        }
    }

    /// Whether `version` lies in this range.
    #[must_use]
    pub const fn contains(self, version: ProtocolVersion) -> bool {
        self.min.0 <= version.0 && version.0 <= self.max.0
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.min == self.max {
            write!(f, "{}", self.min)
        } else {
            write!(f, "{} to {}", self.min, self.max)
        }
    }
}

/// The unchecked form of a [`VersionRange`] read from the wire.
#[derive(Deserialize)]
struct RawVersionRange {
    min: ProtocolVersion,
    max: ProtocolVersion,
}

impl TryFrom<RawVersionRange> for VersionRange {
    type Error = InvertedRange;

    fn try_from(raw: RawVersionRange) -> Result<Self, Self::Error> {
        Self::new(raw.min, raw.max)
    }
}

/// A version range whose lower end is above its upper end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("protocol version range is inverted: min {min} is greater than max {max}")]
pub struct InvertedRange {
    min: ProtocolVersion,
    max: ProtocolVersion,
}

/// The server and the worker share no protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "no protocol version in common: the server supports {server}, the worker supports {worker}"
)]
pub struct NoCommonVersion {
    /// The versions the server supports.
    pub server: VersionRange,
    /// The versions the worker offered.
    pub worker: VersionRange,
}

/// Chooses the protocol version for a connection: the highest version that both the server and
/// the worker support.
///
/// ```
/// use abc_protocol::{ProtocolVersion, VersionRange, negotiate};
///
/// let server = VersionRange::new(ProtocolVersion::new(1), ProtocolVersion::new(3))?;
/// let worker = VersionRange::new(ProtocolVersion::new(2), ProtocolVersion::new(5))?;
/// assert_eq!(negotiate(server, worker)?, ProtocolVersion::new(3));
///
/// let old_worker = VersionRange::single(ProtocolVersion::new(0));
/// assert!(negotiate(server, old_worker).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns [`NoCommonVersion`] when the two ranges share no version.
pub fn negotiate(
    server: VersionRange,
    worker: VersionRange,
) -> Result<ProtocolVersion, NoCommonVersion> {
    let lowest_shared = server.min.max(worker.min);
    let highest_shared = server.max.min(worker.max);
    if lowest_shared <= highest_shared {
        Ok(highest_shared)
    } else {
        Err(NoCommonVersion { server, worker })
    }
}

/// A message a worker sends to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    /// Opens the handshake.
    Hello(Hello),
}

/// The worker's opening message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The protocol versions the worker speaks.
    pub protocol: VersionRange,
    /// The abc release the worker runs.
    pub build: Version,
}

impl Hello {
    /// The hello a worker of this build sends: [`SUPPORTED_VERSIONS`] and its release.
    #[must_use]
    pub const fn new(build: Version) -> Self {
        Self {
            protocol: SUPPORTED_VERSIONS,
            build,
        }
    }
}

/// A message the server sends to a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Accepts the handshake.
    Welcome(Welcome),
    /// Refuses the handshake.
    Reject(Reject),
}

/// The server's acceptance of a handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The protocol version both sides speak from now on.
    pub protocol: ProtocolVersion,
}

/// The server's refusal of a handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    /// Why the server refused.
    pub reason: RejectReason,
}

/// Why the server refused a handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum RejectReason {
    /// The worker offered no protocol version the server speaks.
    UnsupportedVersion {
        /// The protocol versions the server speaks.
        supported: VersionRange,
    },
}

/// A message could not be encoded as wire text.
#[derive(Debug, Error)]
#[error("could not encode the message as JSON")]
pub struct EncodeError(#[from] serde_json::Error);

/// Wire text is not a well-formed message.
#[derive(Debug, Error)]
#[error("malformed message")]
pub struct DecodeError(#[from] serde_json::Error);

impl WorkerMessage {
    /// Encodes the message as wire text.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError`] if serialization fails.
    pub fn encode(&self) -> Result<String, EncodeError> {
        encode(self)
    }

    /// Decodes a message from wire text.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] when `text` is not a well-formed worker message.
    pub fn decode(text: &str) -> Result<Self, DecodeError> {
        decode(text)
    }
}

impl ServerMessage {
    /// Encodes the message as wire text.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError`] if serialization fails.
    pub fn encode(&self) -> Result<String, EncodeError> {
        encode(self)
    }

    /// Decodes a message from wire text.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] when `text` is not a well-formed server message.
    pub fn decode(text: &str) -> Result<Self, DecodeError> {
        decode(text)
    }
}

fn encode<M: Serialize>(message: &M) -> Result<String, EncodeError> {
    Ok(serde_json::to_string(message)?)
}

fn decode<M: DeserializeOwned>(text: &str) -> Result<M, DecodeError> {
    Ok(serde_json::from_str(text)?)
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use proptest::prelude::*;
    use proptest::test_runner::FileFailurePersistence;
    use semver::{BuildMetadata, Prerelease};

    use super::*;

    fn version(number: u32) -> ProtocolVersion {
        ProtocolVersion::new(number)
    }

    fn range(min: u32, max: u32) -> VersionRange {
        VersionRange::new(version(min), version(max)).expect("test ranges are ordered")
    }

    fn decode_error_detail(error: &DecodeError) -> String {
        error
            .source()
            .expect("decode errors carry their cause")
            .to_string()
    }

    #[test]
    fn overlapping_ranges_choose_the_highest_shared_version() {
        assert_eq!(
            negotiate(range(1, 3), range(2, 5)),
            Ok(version(3)),
            "server 1-3 and worker 2-5 share 2 and 3"
        );
        assert_eq!(
            negotiate(range(2, 5), range(1, 3)),
            Ok(version(3)),
            "the choice does not depend on which side holds which range"
        );
    }

    #[test]
    fn nested_ranges_choose_the_top_of_the_inner_range() {
        assert_eq!(
            negotiate(range(1, 10), range(4, 6)),
            Ok(version(6)),
            "a worker range inside the server range is chosen from its top"
        );
    }

    #[test]
    fn ranges_touching_at_one_version_choose_it() {
        assert_eq!(
            negotiate(range(1, 2), range(2, 3)),
            Ok(version(2)),
            "ranges sharing only version 2 agree on 2"
        );
    }

    #[test]
    fn adjacent_ranges_share_no_version() {
        assert_eq!(
            negotiate(range(1, 2), range(3, 4)),
            Err(NoCommonVersion {
                server: range(1, 2),
                worker: range(3, 4),
            }),
            "1-2 and 3-4 touch but share nothing"
        );
    }

    #[test]
    fn disjoint_ranges_report_both_sides() {
        let error = negotiate(range(5, 6), range(1, 2)).expect_err("5-6 and 1-2 are disjoint");
        assert_eq!(
            error.to_string(),
            "no protocol version in common: the server supports 5 to 6, the worker supports 1 to 2",
            "the error names both ranges"
        );
    }

    #[test]
    fn single_versions_agree_only_when_equal() {
        assert_eq!(
            negotiate(
                VersionRange::single(version(4)),
                VersionRange::single(version(4))
            ),
            Ok(version(4)),
            "equal single versions agree"
        );
        let error = negotiate(
            VersionRange::single(version(4)),
            VersionRange::single(version(5)),
        )
        .expect_err("different single versions share nothing");
        assert_eq!(
            error.to_string(),
            "no protocol version in common: the server supports 4, the worker supports 5",
            "single-version ranges print as one number"
        );
    }

    #[test]
    fn inverted_range_is_rejected_at_construction() {
        let error = VersionRange::new(version(3), version(1)).expect_err("3 to 1 is inverted");
        assert_eq!(
            error,
            InvertedRange {
                min: version(3),
                max: version(1),
            },
            "the error carries both ends"
        );
        assert_eq!(
            error.to_string(),
            "protocol version range is inverted: min 3 is greater than max 1",
            "the error explains the inversion"
        );
    }

    #[test]
    fn this_build_offers_its_protocol_version() {
        let hello = Hello::new(Version::new(0, 1, 0));
        assert_eq!(
            hello.protocol,
            VersionRange::single(PROTOCOL_VERSION),
            "a worker offers exactly the version it was built with"
        );
        assert_eq!(
            negotiate(SUPPORTED_VERSIONS, hello.protocol),
            Ok(PROTOCOL_VERSION),
            "a worker and a server of the same build agree on PROTOCOL_VERSION"
        );
        insta::assert_snapshot!(
            WorkerMessage::Hello(hello).encode().unwrap(),
            @r#"{"type":"hello","protocol":{"min":1,"max":1},"build":"0.1.0"}"#
        );
    }

    #[test]
    fn hello_wire_format() {
        let hello = WorkerMessage::Hello(Hello {
            protocol: range(1, 2),
            build: Version::new(0, 1, 0),
        });
        insta::assert_snapshot!(
            hello.encode().unwrap(),
            @r#"{"type":"hello","protocol":{"min":1,"max":2},"build":"0.1.0"}"#
        );
    }

    #[test]
    fn welcome_wire_format() {
        let welcome = ServerMessage::Welcome(Welcome {
            protocol: version(2),
        });
        insta::assert_snapshot!(welcome.encode().unwrap(), @r#"{"type":"welcome","protocol":2}"#);
    }

    #[test]
    fn reject_wire_format() {
        let reject = ServerMessage::Reject(Reject {
            reason: RejectReason::UnsupportedVersion {
                supported: range(3, 4),
            },
        });
        insta::assert_snapshot!(
            reject.encode().unwrap(),
            @r#"{"type":"reject","reason":{"code":"unsupported_version","supported":{"min":3,"max":4}}}"#
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let text = r#"{"type":"hello","protocol":{"min":1,"max":2,"preferred":2},"build":"0.1.0","os":"linux"}"#;
        assert_eq!(
            WorkerMessage::decode(text).unwrap(),
            WorkerMessage::Hello(Hello {
                protocol: range(1, 2),
                build: Version::new(0, 1, 0),
            }),
            "fields from a newer peer must not break the handshake"
        );
    }

    #[test]
    fn unknown_message_type_is_an_error() {
        let error = WorkerMessage::decode(r#"{"type":"goodbye"}"#)
            .expect_err("goodbye is not a worker message");
        assert!(
            decode_error_detail(&error).contains("unknown variant `goodbye`"),
            "the cause names the unknown type: {error:?}"
        );
    }

    #[test]
    fn server_message_is_not_a_worker_message() {
        let error = WorkerMessage::decode(r#"{"type":"welcome","protocol":1}"#)
            .expect_err("welcome only travels from server to worker");
        assert!(
            decode_error_detail(&error).contains("unknown variant `welcome`"),
            "the cause names the misdirected type: {error:?}"
        );
    }

    #[test]
    fn unknown_reject_code_is_an_error() {
        let error = ServerMessage::decode(r#"{"type":"reject","reason":{"code":"busy"}}"#)
            .expect_err("busy is not a reject code");
        assert!(
            decode_error_detail(&error).contains("unknown variant `busy`"),
            "the cause names the unknown code: {error:?}"
        );
    }

    #[test]
    fn missing_type_is_an_error() {
        let error = WorkerMessage::decode(r#"{"protocol":{"min":1,"max":1},"build":"0.1.0"}"#)
            .expect_err("a message needs a type");
        assert!(
            decode_error_detail(&error).contains("missing field `type`"),
            "the cause names the missing tag: {error:?}"
        );
    }

    #[test]
    fn missing_field_is_an_error() {
        let error = WorkerMessage::decode(r#"{"type":"hello","protocol":{"min":1,"max":1}}"#)
            .expect_err("hello needs a build version");
        assert!(
            decode_error_detail(&error).contains("missing field `build`"),
            "the cause names the missing field: {error:?}"
        );
    }

    #[test]
    fn invalid_build_version_is_an_error() {
        let error =
            WorkerMessage::decode(r#"{"type":"hello","protocol":{"min":1,"max":1},"build":"one"}"#)
                .expect_err("one is not a semantic version");
        assert!(
            decode_error_detail(&error).contains("unexpected character"),
            "the cause comes from the version parser: {error:?}"
        );
    }

    #[test]
    fn negative_protocol_version_is_an_error() {
        WorkerMessage::decode(r#"{"type":"hello","protocol":{"min":-1,"max":1},"build":"0.1.0"}"#)
            .expect_err("protocol versions are unsigned");
    }

    #[test]
    fn inverted_range_on_the_wire_is_an_error() {
        let error = WorkerMessage::decode(
            r#"{"type":"hello","protocol":{"min":2,"max":1},"build":"0.1.0"}"#,
        )
        .expect_err("an inverted range cannot be decoded");
        assert!(
            decode_error_detail(&error).contains("min 2 is greater than max 1"),
            "the cause explains the inversion: {error:?}"
        );
    }

    #[test]
    fn malformed_json_is_an_error() {
        for text in [
            "",
            "{",
            "null",
            "[]",
            "\"hello\"",
            r#"{"type":"welcome","protocol":1} trailing"#,
        ] {
            ServerMessage::decode(text).expect_err(text);
        }
    }

    fn version_range() -> impl Strategy<Value = VersionRange> {
        (0_u32..20, 0_u32..20).prop_map(|(a, b)| range(a.min(b), a.max(b)))
    }

    fn build_version() -> impl Strategy<Value = Version> {
        let identifier = "([a-z][a-z0-9-]{0,7}(\\.[a-z][a-z0-9-]{0,7}){0,2})?";
        (
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            identifier,
            identifier,
        )
            .prop_map(|(major, minor, patch, pre, build)| Version {
                major,
                minor,
                patch,
                pre: Prerelease::new(&pre).expect("the pattern yields valid pre-release ids"),
                build: BuildMetadata::new(&build).expect("the pattern yields valid build ids"),
            })
    }

    fn worker_message() -> impl Strategy<Value = WorkerMessage> {
        (version_range(), build_version())
            .prop_map(|(protocol, build)| WorkerMessage::Hello(Hello { protocol, build }))
    }

    fn server_message() -> impl Strategy<Value = ServerMessage> {
        prop_oneof![
            any::<u32>().prop_map(|number| ServerMessage::Welcome(Welcome {
                protocol: version(number),
            })),
            version_range().prop_map(|supported| ServerMessage::Reject(Reject {
                reason: RejectReason::UnsupportedVersion { supported },
            })),
        ]
    }

    /// `text` with the byte at `at` (modulo its length) replaced by the ASCII `byte`, so the result
    /// stays text: a message that is almost right.
    fn corrupt(text: &str, at: usize, byte: u8) -> String {
        let mut bytes = text.as_bytes().to_vec();
        if let Some(slot) = at
            .checked_rem(bytes.len())
            .and_then(|slot| bytes.get_mut(slot))
        {
            *slot = byte & 0x7f;
        }
        String::from_utf8(bytes).unwrap_or_default()
    }

    /// Proptest's defaults, with failing cases saved where this repository keeps them:
    /// `proptest-regressions/` beside the source file. Commit those files; proptest replays them
    /// before new cases.
    fn proptest_config() -> ProptestConfig {
        ProptestConfig {
            failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
                "proptest-regressions",
            ))),
            ..ProptestConfig::default()
        }
    }

    proptest! {
        #![proptest_config(proptest_config())]

        #[test]
        fn worker_messages_survive_the_wire(message in worker_message()) {
            let text = message.encode().unwrap();
            prop_assert_eq!(WorkerMessage::decode(&text).unwrap(), message);
        }

        #[test]
        fn server_messages_survive_the_wire(message in server_message()) {
            let text = message.encode().unwrap();
            prop_assert_eq!(ServerMessage::decode(&text).unwrap(), message);
        }

        /// The fuzz target's property (`crates/abc-protocol/fuzz`), on every `check`: text from a
        /// peer not yet trusted, whether arbitrary or a valid message with one byte changed,
        /// decodes without panicking, and whatever decodes encodes and decodes back to itself.
        #[test]
        fn untrusted_text_decodes_or_is_refused_never_half_read(
            text in prop_oneof![
                any::<String>(),
                (worker_message(), any::<usize>(), any::<u8>()).prop_map(|(message, at, byte)| {
                    corrupt(&message.encode().unwrap(), at, byte)
                }),
                (server_message(), any::<usize>(), any::<u8>()).prop_map(|(message, at, byte)| {
                    corrupt(&message.encode().unwrap(), at, byte)
                }),
            ],
        ) {
            if let Ok(message) = WorkerMessage::decode(&text) {
                prop_assert_eq!(WorkerMessage::decode(&message.encode().unwrap()).unwrap(), message);
            }
            if let Ok(message) = ServerMessage::decode(&text) {
                prop_assert_eq!(ServerMessage::decode(&message.encode().unwrap()).unwrap(), message);
            }
        }

        #[test]
        fn negotiation_matches_the_highest_version_both_contain(
            server in version_range(),
            worker in version_range(),
        ) {
            let highest_common = (0_u32..20)
                .map(version)
                .filter(|&candidate| server.contains(candidate) && worker.contains(candidate))
                .max();
            let expected = highest_common.ok_or(NoCommonVersion { server, worker });
            prop_assert_eq!(negotiate(server, worker), expected);
        }
    }
}
