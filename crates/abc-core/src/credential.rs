//! The formats of every bearer secret abc issues, in one place: how each is generated, parsed and
//! digested. The server stores only [`Digest`]s, never plaintext.
//!
//! | Secret          | Format                               | Minted by            | Entropy  |
//! | --------------- | ------------------------------------ | -------------------- | -------- |
//! | [`SetupCode`]   | `XXXXX-XXXXX-XXXXX-XXXXX` (Crockford) | the store, at open   | 100 bits |
//! | [`EnrolmentToken`] | `abc_enr_` + 52 base32 chars      | the owner's request  | 256 bits |
//! | [`WorkerKey`]   | `abc_wk_` + 52 base32 chars          | **the worker itself** | 256 bits |
//!
//! The prefixes let gitleaks and similar scanners recognise a leaked abc secret (I2). All three
//! carry full entropy, so a plain SHA-256 digest is enough. Brute force is infeasible, so no
//! salt and no slow hash are needed. Digest comparison is constant-time ([`Digest::matches`]).

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use crate::decision::Mint;
use crate::secret::Secret;

/// A SHA-256 digest of a credential. Safe to store and log; never sent to the web app (read models
/// carry no digests). The marker type `K` keeps a token digest from being compared with a
/// worker-key digest. On the wire (the enrolment request) and in event payloads it is lowercase
/// hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest<K>([u8; 32], std::marker::PhantomData<K>);

impl<K> Serialize for Digest<K> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let _ = serializer;
        unimplemented!("lowercase hex of the 32 bytes")
    }
}

impl<'de, K> Deserialize<'de> for Digest<K> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let _ = deserializer;
        unimplemented!("exactly 64 lowercase hex characters")
    }
}

impl<K> Digest<K> {
    /// Constant-time equality, for comparing a presented credential with a stored digest.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }

    /// The raw bytes, for a `bytea` column.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Rebuilds a digest read from storage.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes, std::marker::PhantomData)
    }
}

impl<K> std::fmt::Debug for Digest<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = f;
        unimplemented!("first 8 hex chars, e.g. Digest(3fa4c2e1…)")
    }
}

/// Marker for [`SetupCode`] digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetupCodeKind {}
/// Marker for [`EnrolmentToken`] digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnrolmentTokenKind {}
/// Marker for [`WorkerKey`] digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerKeyKind {}

/// The digest of the owner setup code.
pub type SetupCodeDigest = Digest<SetupCodeKind>;
/// The digest of an enrolment token, which is what the log stores.
pub type EnrolmentTokenDigest = Digest<EnrolmentTokenKind>;
/// The digest of a worker key: what the worker sends at enrolment and what the log stores.
pub type WorkerKeyDigest = Digest<WorkerKeyKind>;

/// Text that is not a well-formed abc credential. The message never contains the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("malformed {kind}: expected the format abc issues")]
pub struct MalformedCredential {
    /// Which credential was expected, for the message.
    pub kind: &'static str,
}

/// The one-time code that lets the host operator claim an unclaimed server.
#[derive(Clone, Zeroize, Deserialize)]
#[serde(try_from = "String")]
pub struct SetupCode(String);

impl SetupCode {
    /// Mints a fresh code: 100 random bits, grouped for typing.
    #[must_use]
    pub fn generate(mint: &dyn Mint) -> Secret<Self> {
        let _ = mint;
        unimplemented!("13 bytes from mint.secret(), Crockford base32, 4 groups of 5")
    }

    /// Normalizes what a person typed: case-folds, drops spaces and dashes, maps I/L to 1 and O to 0.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedCredential`] unless 20 Crockford characters remain.
    pub fn parse(typed: &str) -> Result<Secret<Self>, MalformedCredential> {
        let _ = typed;
        unimplemented!()
    }

    /// The digest the store keeps in memory and compares against.
    #[must_use]
    pub fn digest(&self) -> SetupCodeDigest {
        unimplemented!("sha256 of the normalized form")
    }

    /// The code as shown to the operator, with dashes.
    #[must_use]
    pub fn display_form(&self) -> Zeroizing<String> {
        unimplemented!()
    }
}

impl TryFrom<String> for SetupCode {
    type Error = MalformedCredential;
    fn try_from(mut raw: String) -> Result<Self, Self::Error> {
        let parsed = Self::parse(&raw);
        raw.zeroize();
        let _ = parsed;
        unimplemented!("unwrap the Secret's inner value without cloning (mem::take)")
    }
}

/// A one-time token that lets one machine enrol as a worker in one workspace.
#[derive(Clone, Zeroize, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnrolmentToken(String);

impl EnrolmentToken {
    /// The fixed prefix that marks an abc enrolment token.
    pub const PREFIX: &'static str = "abc_enr_";

    /// Mints a fresh token from 32 bytes of the mint's entropy and returns it with its digest.
    #[must_use]
    pub fn generate(mint: &dyn Mint) -> (Secret<Self>, EnrolmentTokenDigest) {
        let _ = mint;
        unimplemented!()
    }

    /// SHA-256 over the full string, prefix included.
    #[must_use]
    pub fn digest(&self) -> EnrolmentTokenDigest {
        unimplemented!()
    }
}

/// The long-lived credential a worker presents on every connection. The worker generates it
/// itself (PR 4) and sends only its digest at enrolment, so the server never handles a key it
/// would have to return, store or replay.
#[derive(Clone, Zeroize, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkerKey(String);

impl WorkerKey {
    /// The fixed prefix that marks an abc worker key.
    pub const PREFIX: &'static str = "abc_wk_";

    /// Mints a fresh key. The worker calls this with an OS-entropy mint.
    #[must_use]
    pub fn generate(mint: &dyn Mint) -> Secret<Self> {
        let _ = mint;
        unimplemented!()
    }

    /// Parses an `Authorization: Bearer` value or a stored key.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedCredential`] when the prefix or the body is wrong.
    pub fn parse(text: &str) -> Result<Secret<Self>, MalformedCredential> {
        let _ = text;
        unimplemented!()
    }

    /// SHA-256 over the full string, prefix included.
    #[must_use]
    pub fn digest(&self) -> WorkerKeyDigest {
        unimplemented!()
    }
}

// TryFrom<String> / Into<String> for EnrolmentToken and WorkerKey: validate the prefix and the
// base32 body; zeroize the input String on failure. Into<String> exists only because
// serialize_exposed needs Serialize on the inner type; the Secret wrapper still has none.
macro_rules! prefixed_credential_strings {
    ($name:ident, $kind:literal) => {
        impl TryFrom<String> for $name {
            type Error = MalformedCredential;
            fn try_from(raw: String) -> Result<Self, Self::Error> {
                let _ = (raw, $kind);
                unimplemented!("check PREFIX and the 52-character base32 body; zeroize on failure")
            }
        }

        impl From<$name> for String {
            fn from(credential: $name) -> Self {
                let _ = credential;
                unimplemented!("move the inner String out without copying")
            }
        }
    };
}
prefixed_credential_strings!(EnrolmentToken, "enrolment token");
prefixed_credential_strings!(WorkerKey, "worker key");

// The credential types hold plaintext, so their `Debug` is redacted like `Secret`'s: a credential
// that escapes its `Secret` wrapper (a `Vec<EnrolmentToken>` in a debug log) still prints nothing.
macro_rules! redacted_debug {
    ($($name:ident),*) => {$(
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }
    )*};
}
redacted_debug!(SetupCode, EnrolmentToken, WorkerKey);
