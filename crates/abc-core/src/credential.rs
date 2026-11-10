//! The formats of every bearer secret abc issues, in one place: how each is generated, parsed and
//! digested. The server stores only [`Digest`]s, never plaintext.
//!
//! | Secret             | Format                                 | Minted by              | Entropy  |
//! | ------------------ | -------------------------------------- | ---------------------- | -------- |
//! | [`SetupCode`]      | `XXXXX-XXXXX-XXXXX-XXXXX` (Crockford)  | `abc server setup-code` | 100 bits |
//! | [`EnrolmentToken`] | `abc_enr_` + 52 base32 characters      | the owner's request    | 256 bits |
//! | [`WorkerKey`]      | `abc_wk_` + 52 base32 characters       | **the worker itself**  | 256 bits |
//!
//! An [`ApprovalCode`] is not a secret: it lets a person match the enrolment request in their
//! terminal with the one on the Approve page.
//!
//! The prefixes let gitleaks and similar scanners recognise a leaked abc secret (I2). All three
//! carry full entropy, so a plain SHA-256 digest is enough: brute force is infeasible, so no salt
//! and no slow hash are needed. Digest comparison is constant-time ([`Digest::matches`]).
//!
//! Base32 here is Crockford's alphabet (no I, L, O or U), upper case in setup codes, which people
//! type, and lower case in tokens and keys, which people paste.

use std::fmt;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use crate::decision::Mint;
use crate::secret::Secret;

/// Crockford's base32 alphabet, upper case.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Encodes the first `count` five-bit groups of `bytes`, most significant bit first. Groups past
/// the end of `bytes` read zero bits.
fn base32(bytes: &[u8], count: usize, lower: bool) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(count));
    for group in 0..count {
        let mut value = 0_usize;
        for bit in group.saturating_mul(5)..group.saturating_mul(5).saturating_add(5) {
            let set = bytes
                .get(bit / 8)
                .is_some_and(|byte| byte & (0x80 >> (bit % 8)) != 0);
            value = (value << 1).saturating_add(usize::from(set));
        }
        let symbol = char::from(CROCKFORD.get(value).copied().unwrap_or(b'0'));
        out.push(if lower {
            symbol.to_ascii_lowercase()
        } else {
            symbol
        });
    }
    out
}

/// Whether `symbol` is in the lower-case alphabet tokens and keys use.
fn is_token_symbol(symbol: u8) -> bool {
    CROCKFORD.contains(&symbol.to_ascii_uppercase()) && !symbol.is_ascii_uppercase()
}

/// A SHA-256 digest of a credential. Safe to store and log; never sent to the web app (read models
/// carry no digests). The marker type `K` keeps a token digest from being compared with a
/// worker-key digest. On the wire (the enrolment request) and in event payloads it is lowercase
/// hex.
pub struct Digest<K>([u8; 32], PhantomData<K>);

impl<K> Digest<K> {
    /// The digest of `text`.
    fn of(text: &str) -> Self {
        Self(Sha256::digest(text.as_bytes()).into(), PhantomData)
    }

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
        Self(bytes, PhantomData)
    }

    /// Rebuilds a digest from a stored byte string.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedDigest`] unless `bytes` is exactly 32 long.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, MalformedDigest> {
        <[u8; 32]>::try_from(bytes)
            .ok()
            .map(Self::from_bytes)
            .ok_or(MalformedDigest)
    }
}

// Derives would put bounds on `K`, an uninhabited marker.
impl<K> Clone for Digest<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K> Copy for Digest<K> {}
impl<K> PartialEq for Digest<K> {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other)
    }
}
impl<K> Eq for Digest<K> {}

impl<K> fmt::Debug for Digest<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = data_encoding::HEXLOWER.encode(self.0.get(..4).unwrap_or_default());
        write!(f, "Digest({hex}…)")
    }
}

impl<K> Serialize for Digest<K> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&data_encoding::HEXLOWER.encode(&self.0))
    }
}

impl<'de, K> Deserialize<'de> for Digest<K> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        data_encoding::HEXLOWER
            .decode(text.as_bytes())
            .ok()
            .and_then(|bytes| Self::from_slice(&bytes).ok())
            .ok_or_else(|| serde::de::Error::custom(MalformedDigest))
    }
}

/// A stored or transmitted digest that is not 32 bytes (64 lowercase hex characters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("malformed digest: expected 32 bytes as 64 lowercase hex characters")]
pub struct MalformedDigest;

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

/// The number of characters in a setup code, without dashes.
const SETUP_CODE_LEN: usize = 20;

/// The one-time code that lets the host operator claim an unclaimed server. Held in its
/// normalized form: 20 upper-case Crockford characters, no dashes.
#[derive(Zeroize, Deserialize)]
#[serde(try_from = "String")]
pub struct SetupCode(String);

impl SetupCode {
    /// Mints a fresh code: 100 random bits.
    #[must_use]
    pub fn generate(mint: &dyn Mint) -> Secret<Self> {
        let entropy = mint.secret();
        let mut code = base32(entropy.as_slice(), SETUP_CODE_LEN, false);
        Secret::new(Self(std::mem::take(&mut *code)))
    }

    /// Normalizes what a person typed: upper-cases, drops spaces and dashes, and reads I and L as
    /// 1 and O as 0.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedCredential`] unless 20 Crockford characters remain.
    pub fn parse(typed: &str) -> Result<Secret<Self>, MalformedCredential> {
        let mut normalized = Zeroizing::new(String::with_capacity(SETUP_CODE_LEN));
        for symbol in typed.chars() {
            match symbol.to_ascii_uppercase() {
                ' ' | '-' => {}
                'I' | 'L' => normalized.push('1'),
                'O' => normalized.push('0'),
                upper if u8::try_from(upper).is_ok_and(|byte| CROCKFORD.contains(&byte)) => {
                    normalized.push(upper);
                }
                _ => return Err(Self::MALFORMED),
            }
        }
        if normalized.len() == SETUP_CODE_LEN {
            Ok(Secret::new(Self(std::mem::take(&mut *normalized))))
        } else {
            Err(Self::MALFORMED)
        }
    }

    const MALFORMED: MalformedCredential = MalformedCredential { kind: "setup code" };

    /// The digest the log records and the claim compares against: SHA-256 of the normalized form.
    #[must_use]
    pub fn digest(&self) -> SetupCodeDigest {
        Digest::of(&self.0)
    }

    /// The code as shown to the operator: four groups of five, joined by dashes.
    #[must_use]
    pub fn display_form(&self) -> Zeroizing<String> {
        let mut shown = Zeroizing::new(String::with_capacity(SETUP_CODE_LEN.saturating_add(3)));
        for (index, symbol) in self.0.chars().enumerate() {
            if index > 0 && index % 5 == 0 {
                shown.push('-');
            }
            shown.push(symbol);
        }
        shown
    }
}

impl TryFrom<String> for SetupCode {
    type Error = MalformedCredential;

    fn try_from(mut raw: String) -> Result<Self, Self::Error> {
        let parsed = Self::parse(&raw);
        raw.zeroize();
        parsed.map(|secret| Self(secret.expose_secret().0.clone()))
    }
}

/// The number of base32 characters after a token's or key's prefix: 256 bits.
const BODY_LEN: usize = 52;

/// Generates `prefix` + 52 lower-case base32 characters from the mint's 32 secret bytes.
fn generate_prefixed(mint: &dyn Mint, prefix: &str) -> Zeroizing<String> {
    let entropy = mint.secret();
    let body = base32(entropy.as_slice(), BODY_LEN, true);
    let mut text = Zeroizing::new(String::with_capacity(prefix.len().saturating_add(BODY_LEN)));
    text.push_str(prefix);
    text.push_str(&body);
    text
}

/// Whether `text` is `prefix` followed by exactly 52 lower-case base32 characters.
fn is_prefixed(text: &str, prefix: &str) -> bool {
    text.strip_prefix(prefix)
        .is_some_and(|body| body.len() == BODY_LEN && body.bytes().all(is_token_symbol))
}

/// A one-time token that lets one machine enrol as a worker in one workspace.
#[derive(Zeroize, Deserialize)]
#[serde(try_from = "String")]
pub struct EnrolmentToken(String);

impl EnrolmentToken {
    /// The fixed prefix that marks an abc enrolment token.
    pub const PREFIX: &'static str = "abc_enr_";

    /// Mints a fresh token from 32 bytes of the mint's entropy and returns it with its digest.
    #[must_use]
    pub fn generate(mint: &dyn Mint) -> (Secret<Self>, EnrolmentTokenDigest) {
        let mut text = generate_prefixed(mint, Self::PREFIX);
        let token = Self(std::mem::take(&mut *text));
        let digest = token.digest();
        (Secret::new(token), digest)
    }

    /// Parses a token someone pasted.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedCredential`] when the prefix or the body is wrong.
    pub fn parse(text: &str) -> Result<Secret<Self>, MalformedCredential> {
        Self::try_from(text.to_owned()).map(Secret::new)
    }

    /// SHA-256 over the full string, prefix included.
    #[must_use]
    pub fn digest(&self) -> EnrolmentTokenDigest {
        Digest::of(&self.0)
    }

    /// The token as text, for the enrolment request and the command the web app shows.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The long-lived credential a worker presents on every connection. The worker generates it
/// itself and sends only its digest at enrolment, so the server never handles a key it would have
/// to return, store or replay.
#[derive(Zeroize, Deserialize)]
#[serde(try_from = "String")]
pub struct WorkerKey(String);

impl WorkerKey {
    /// The fixed prefix that marks an abc worker key.
    pub const PREFIX: &'static str = "abc_wk_";

    /// Mints a fresh key. The worker calls this with an OS-entropy mint.
    #[must_use]
    pub fn generate(mint: &dyn Mint) -> Secret<Self> {
        let mut text = generate_prefixed(mint, Self::PREFIX);
        Secret::new(Self(std::mem::take(&mut *text)))
    }

    /// Parses an `Authorization: Bearer` value or a stored key.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedCredential`] when the prefix or the body is wrong.
    pub fn parse(text: &str) -> Result<Secret<Self>, MalformedCredential> {
        Self::try_from(text.to_owned()).map(Secret::new)
    }

    /// SHA-256 over the full string, prefix included.
    #[must_use]
    pub fn digest(&self) -> WorkerKeyDigest {
        Digest::of(&self.0)
    }

    /// The key as text, for an `Authorization: Bearer` header or the worker's own key file.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The number of characters in an approval code.
const APPROVAL_CODE_LEN: usize = 8;

/// The short code a waiting `abc worker setup` and the Approve page both show, so the owner can
/// check that the request they approve is the one their terminal made: 8 upper-case Crockford
/// characters (40 bits), shown `XXXX-XXXX`. Not a secret: it grants nothing, and the log and
/// views keep it in the clear.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
pub struct ApprovalCode(String);

impl ApprovalCode {
    /// Mints a code from the mint's entropy.
    #[must_use]
    pub fn generate(mint: &dyn Mint) -> Self {
        let entropy = mint.secret();
        let mut code = base32(entropy.as_slice(), APPROVAL_CODE_LEN, false);
        Self(std::mem::take(&mut *code))
    }

    /// Reads a stored or sent code, with or without its dash.
    ///
    /// # Errors
    ///
    /// Returns [`MalformedCredential`] unless 8 upper-case Crockford characters remain.
    pub fn parse(text: &str) -> Result<Self, MalformedCredential> {
        let code: String = text.chars().filter(|&symbol| symbol != '-').collect();
        let valid = code.len() == APPROVAL_CODE_LEN
            && code
                .bytes()
                .all(|byte| CROCKFORD.contains(&byte) && !byte.is_ascii_lowercase());
        if valid {
            Ok(Self(code))
        } else {
            Err(MalformedCredential {
                kind: "approval code",
            })
        }
    }

    /// The code as people see it: two groups of four, joined by a dash.
    #[must_use]
    pub fn display_form(&self) -> String {
        let (first, second) = self.0.split_at(APPROVAL_CODE_LEN / 2);
        format!("{first}-{second}")
    }
}

impl TryFrom<String> for ApprovalCode {
    type Error = MalformedCredential;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ApprovalCode> for String {
    fn from(code: ApprovalCode) -> Self {
        code.display_form()
    }
}

impl fmt::Display for ApprovalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_form())
    }
}

// TryFrom<String> and Serialize for the prefixed credentials. Serialize exists only because
// serialize_exposed needs it on the inner type; the Secret wrapper still has none.
macro_rules! prefixed_credential_strings {
    ($name:ident, $kind:literal) => {
        impl TryFrom<String> for $name {
            type Error = MalformedCredential;

            fn try_from(mut raw: String) -> Result<Self, Self::Error> {
                if is_prefixed(&raw, Self::PREFIX) {
                    Ok(Self(raw))
                } else {
                    raw.zeroize();
                    Err(MalformedCredential { kind: $kind })
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }
    };
}
prefixed_credential_strings!(EnrolmentToken, "enrolment token");
prefixed_credential_strings!(WorkerKey, "worker key");

// The credential types hold plaintext, so their `Debug` is redacted like `Secret`'s: a credential
// that escapes its `Secret` wrapper still prints nothing.
macro_rules! redacted_debug {
    ($($name:ident),*) => {$(
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }
    )*};
}
redacted_debug!(SetupCode, EnrolmentToken, WorkerKey);

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;
    use crate::time::Timestamp;

    /// A mint whose secret bytes are fixed.
    #[derive(Debug)]
    struct FixedMint([u8; 32]);

    impl Mint for FixedMint {
        fn uuid(&self, _now: Timestamp) -> Uuid {
            Uuid::nil()
        }

        fn secret(&self) -> Zeroizing<[u8; 32]> {
            Zeroizing::new(self.0)
        }
    }

    #[test]
    fn base32_reads_five_bits_at_a_time_from_the_top() {
        assert_eq!(
            *base32(&[0b1111_1000], 1, false),
            "Z",
            "11111 is the last symbol"
        );
        assert_eq!(
            *base32(&[0b0000_1000], 2, false),
            "10",
            "00001 then 000 and zero bits"
        );
        assert_eq!(
            *base32(&[0xFF], 3, true),
            "zw0",
            "11111 111 then zero bits past the end, lower case"
        );
    }

    #[test]
    fn a_generated_setup_code_has_the_documented_shape() {
        let code = SetupCode::generate(&FixedMint([0xA5; 32]));
        let shown = code.expose_secret().display_form();
        assert_eq!(
            *shown, "MPJTB-9D5MP-JTB9D-5MPJT",
            "deterministic from the mint"
        );
        let reparsed = SetupCode::parse(&shown).unwrap();
        assert!(
            reparsed
                .expose_secret()
                .digest()
                .matches(&code.expose_secret().digest()),
            "the display form parses back to the same code"
        );
    }

    #[test]
    fn setup_code_parsing_forgives_case_spaces_dashes_and_lookalikes() {
        let canonical = SetupCode::parse("10ABC-DEFGH-JKMNP-QRSTV").unwrap();
        let typed = SetupCode::parse(" lo abc defgh-jkmnp qrstv ").unwrap();
        assert!(
            typed
                .expose_secret()
                .digest()
                .matches(&canonical.expose_secret().digest()),
            "l reads as 1, o as 0, case and separators do not matter"
        );
        let with_i = SetupCode::parse("I0ABCDEFGHJKMNPQRSTV").unwrap();
        assert_eq!(
            with_i.expose_secret().display_form(),
            canonical.expose_secret().display_form(),
            "I reads as 1"
        );
    }

    #[test]
    fn setup_code_parsing_rejects_wrong_lengths_and_symbols() {
        for typed in [
            "",
            "10ABC-DEFGH-JKMNP-QRST",
            "10ABC-DEFGH-JKMNP-QRSTVW",
            "10ABC-DEFGH-JKMNP-QRSTU",
            "10ABC-DEFGH-JKMNP-QRST!",
            "10ABC-DEFGH-JKMNP-QRSTé",
        ] {
            let error = SetupCode::parse(typed).err();
            assert_eq!(
                error,
                Some(MalformedCredential { kind: "setup code" }),
                "{typed:?} is not a setup code"
            );
        }
        assert_eq!(
            MalformedCredential { kind: "setup code" }.to_string(),
            "malformed setup code: expected the format abc issues",
            "the message never contains the input"
        );
    }

    #[test]
    fn setup_codes_deserialize_and_digest_their_normalized_form() {
        let code: SetupCode = serde_json::from_str(r#""10abc-defgh-jkmnp-qrstv""#).unwrap();
        assert_eq!(
            code.digest().as_bytes(),
            &<[u8; 32]>::from(Sha256::digest(b"10ABCDEFGHJKMNPQRSTV")),
            "the digest covers the normalized text"
        );
        assert!(
            serde_json::from_str::<SetupCode>(r#""short""#).is_err(),
            "malformed text does not deserialize"
        );
    }

    #[test]
    fn generated_tokens_and_keys_parse_and_carry_their_prefix() {
        let (token, digest) = EnrolmentToken::generate(&FixedMint([7; 32]));
        let text = token.expose_secret().as_str().to_owned();
        assert_eq!(
            serde_json::to_value(token.expose_secret()).unwrap(),
            text.as_str(),
            "the text is what serializes"
        );
        assert!(text.starts_with("abc_enr_"), "tokens carry their prefix");
        assert_eq!(text.len(), 8 + 52, "prefix plus 52 characters");
        assert!(
            EnrolmentToken::parse(&text)
                .unwrap()
                .expose_secret()
                .digest()
                .matches(&digest),
            "the returned digest is the token's"
        );

        let key = WorkerKey::generate(&FixedMint([9; 32]));
        let key_text = key.expose_secret().as_str();
        assert!(key_text.starts_with("abc_wk_"), "keys carry their prefix");
        assert_eq!(key_text.len(), 7 + 52, "prefix plus 52 characters");
        assert_eq!(
            WorkerKey::parse(key_text).unwrap().expose_secret().digest(),
            key.expose_secret().digest(),
            "a key parses back to itself"
        );
    }

    #[test]
    fn tokens_and_keys_reject_wrong_prefixes_lengths_and_symbols() {
        let body = "0".repeat(52);
        for text in [
            format!("abc_wk_{body}"),
            format!("abc_enr_{}", "0".repeat(51)),
            format!("abc_enr_{}", "0".repeat(53)),
            format!("abc_enr_{}u", "0".repeat(51)),
            format!("abc_enr_{}A", "0".repeat(51)),
            body.clone(),
        ] {
            assert_eq!(
                EnrolmentToken::parse(&text).err(),
                Some(MalformedCredential {
                    kind: "enrolment token"
                }),
                "{text} is not an enrolment token"
            );
        }
        assert!(
            EnrolmentToken::parse(&format!("abc_enr_{body}")).is_ok(),
            "a valid token"
        );
        assert_eq!(
            WorkerKey::parse(&format!("abc_enr_{body}")).err(),
            Some(MalformedCredential { kind: "worker key" }),
            "a token is not a key"
        );
        assert_eq!(
            serde_json::from_str::<WorkerKey>("\"abc_wk_\"")
                .err()
                .map(|error| error.to_string()),
            Some("malformed worker key: expected the format abc issues".to_owned()),
            "serde errors do not echo the input"
        );
    }

    #[test]
    fn credentials_never_print_their_plaintext() {
        let key = WorkerKey::generate(&FixedMint([1; 32]));
        let code = SetupCode::generate(&FixedMint([1; 32]));
        let (token, _) = EnrolmentToken::generate(&FixedMint([1; 32]));
        assert_eq!(
            format!("{:?}", key.expose_secret()),
            "WorkerKey(<redacted>)",
            "key"
        );
        assert_eq!(
            format!("{:?}", code.expose_secret()),
            "SetupCode(<redacted>)",
            "code"
        );
        assert_eq!(
            format!("{:?}", token.expose_secret()),
            "EnrolmentToken(<redacted>)",
            "token"
        );
    }

    #[test]
    fn digests_are_hex_on_the_wire_and_compare_by_value() {
        let key = WorkerKey::generate(&FixedMint([2; 32]));
        let digest = key.expose_secret().digest();
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json.len(), 64 + 2, "64 hex characters in quotes");
        assert_eq!(
            serde_json::from_str::<WorkerKeyDigest>(&json).unwrap(),
            digest,
            "hex reads back"
        );
        let first = data_encoding::HEXLOWER.encode(&digest.as_bytes()[..4]);
        assert_eq!(
            format!("{digest:?}"),
            format!("Digest({first}…)"),
            "Debug shows a prefix"
        );
        for bad in [
            r#""""#,
            r#""zz""#,
            &format!("\"{}\"", "A".repeat(64)),
            &format!("\"{}\"", "a".repeat(62)),
        ] {
            assert_eq!(
                serde_json::from_str::<WorkerKeyDigest>(bad)
                    .err()
                    .map(|e| e.to_string().starts_with("malformed digest")),
                Some(true),
                "{bad} is not a digest"
            );
        }
        let other = WorkerKey::generate(&FixedMint([3; 32]))
            .expose_secret()
            .digest();
        assert!(!digest.matches(&other), "different keys, different digests");
        assert_ne!(digest, other, "PartialEq agrees with matches");
        assert_eq!(
            WorkerKeyDigest::from_slice(digest.as_bytes()),
            Ok(digest),
            "32 stored bytes rebuild the digest"
        );
        assert_eq!(
            WorkerKeyDigest::from_slice(&[0; 31]),
            Err(MalformedDigest),
            "31 bytes are not a digest"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::SourceParallel("proptest-regressions"),
            )),
            ..ProptestConfig::default()
        })]

        #[test]
        fn every_generated_credential_parses(bytes in any::<[u8; 32]>()) {
            let mint = FixedMint(bytes);
            let code = SetupCode::generate(&mint);
            let shown = code.expose_secret().display_form();
            prop_assert!(SetupCode::parse(&shown).is_ok());
            let (token, digest) = EnrolmentToken::generate(&mint);
            let text = token.expose_secret().0.clone();
            prop_assert_eq!(EnrolmentToken::parse(&text).unwrap().expose_secret().digest(), digest);
            let key = WorkerKey::generate(&mint);
            prop_assert!(WorkerKey::parse(key.expose_secret().as_str()).is_ok());
        }
    }

    #[test]
    fn approval_codes_are_eight_crockford_characters_shown_in_two_groups() {
        let code = ApprovalCode::generate(&FixedMint([0xA5; 32]));
        assert_eq!(
            code.display_form(),
            "MPJT-B9D5",
            "40 bits, upper case, a dash in the middle"
        );
        assert_eq!(code.to_string(), "MPJT-B9D5", "Display is the shown form");
        assert_eq!(
            ApprovalCode::parse("MPJTB9D5"),
            Ok(code.clone()),
            "the dash is optional"
        );
        assert_eq!(
            serde_json::to_string(&code).unwrap(),
            r#""MPJT-B9D5""#,
            "the wire carries the shown form"
        );
        assert_eq!(
            serde_json::from_str::<ApprovalCode>(r#""MPJT-B9D5""#).unwrap(),
            code,
            "and reads it back"
        );
        for bad in ["MPJT-B9D", "MPJT-B9D55", "mpjt-b9d5", "MPJT-B9DU"] {
            assert!(ApprovalCode::parse(bad).is_err(), "{bad}");
        }
    }
}
