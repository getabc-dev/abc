//! [`Secret`]: a value that cannot be logged or serialized by accident (D12, I2).
//!
//! What it does not implement is the design: no `Display`, no `Serialize`, no `Clone`, and a
//! `Debug` that prints `Secret(<redacted>)`. Reading the value takes [`Secret::expose_secret`], and
//! serializing it takes [`serialize_exposed`] named on the one field that must carry it (the
//! once-only token in the issue response, the worker's enrolment request). Both are greppable, so
//! a reviewer can audit every place a secret leaves its wrapper.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroize;

/// A secret value, zeroized when dropped.
pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    /// Wraps `value`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrows the secret. Every call site is a place the plaintext leaves its wrapper.
    #[must_use]
    pub const fn expose_secret(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Deserializes through `T`'s own `Deserialize`. Every `T` used here (the credential formats)
/// parses from a `String` with an error that never repeats the input. A serde error message can
/// end up in a response or a log, so this matters.
impl<'de, T: Zeroize + Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

/// Serializes the plaintext. Use only as `#[serde(serialize_with = "serialize_exposed")]` on a
/// field whose whole purpose is to hand the secret to its holder.
///
/// # Errors
///
/// Returns the serializer's error.
pub fn serialize_exposed<T, S>(secret: &Secret<T>, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Zeroize + Serialize,
    S: Serializer,
{
    secret.expose_secret().serialize(serializer)
}

/// `{"type": "string", "writeOnly": true}`: a client sends it and never reads it back, except in
/// the one response that exists to hand it out.
impl<T: Zeroize> schemars::JsonSchema for Secret<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Secret".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string", "writeOnly": true })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde::Serialize;

    use super::*;

    #[derive(Zeroize, Deserialize, Serialize)]
    struct Plain(String);

    #[derive(Serialize)]
    struct Carrier {
        #[serde(serialize_with = "serialize_exposed")]
        secret: Secret<Plain>,
    }

    #[test]
    fn serialize_exposed_writes_the_plaintext() {
        let carrier = Carrier {
            secret: Secret::new(Plain("hunter2".to_owned())),
        };
        assert_eq!(
            serde_json::to_string(&carrier).unwrap(),
            r#"{"secret":"hunter2"}"#,
            "the one field that exists to carry it does"
        );
    }

    #[test]
    fn a_secret_deserializes_through_its_inner_type() {
        let secret: Secret<Plain> = serde_json::from_str(r#""s3cret""#).unwrap();
        assert_eq!(secret.expose_secret().0, "s3cret", "the value is kept");
    }

    #[test]
    fn the_schema_is_a_write_only_string() {
        let schema = schemars::schema_for!(Secret<Plain>);
        let value = serde_json::to_value(&schema).unwrap();
        assert_eq!(value["type"], "string", "on the wire a secret is a string");
        assert_eq!(
            value["writeOnly"], true,
            "clients send it and never read it"
        );
    }

    /// Records whether it was zeroized.
    struct Probe(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Zeroize for Probe {
        fn zeroize(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_zeroizes_the_value() {
        let zeroized = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let secret = Secret::new(Probe(std::sync::Arc::clone(&zeroized)));
        assert!(
            !zeroized.load(std::sync::atomic::Ordering::SeqCst),
            "a live secret is intact"
        );
        drop(secret);
        assert!(
            zeroized.load(std::sync::atomic::Ordering::SeqCst),
            "dropping a secret zeroizes it"
        );
    }

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

        #[test]
        fn debug_never_contains_the_value(value in ".{1,40}") {
            let printed = format!("{:?}", Secret::new(Plain(value)));
            prop_assert_eq!(printed, "Secret(<redacted>)");
        }
    }
}
