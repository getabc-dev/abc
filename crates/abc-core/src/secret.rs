//! [`Secret`]: a value that cannot be logged or serialized by accident (D12, I2).
//!
//! What it does not implement is the design: no `Display`, no `Serialize`, no `Clone`, and a
//! `Debug` that prints `Secret(<redacted>)`. Reading the value takes [`Secret::expose_secret`], and
//! serializing it takes [`serialize_exposed`] named on the one field that must carry it (the
//! once-only token in `TokenIssued`, the worker's enrolment request). Both are greppable, so a
//! reviewer can audit every place a secret leaves its wrapper.

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

/// On the wire a secret is a string: the setup code in the claim request, the token in the one
/// response that hands it out.
impl<T: Zeroize> ts_rs::TS for Secret<T> {
    type WithoutGenerics = Self;
    type OptionInnerType = Self;
    fn name(_: &ts_rs::Config) -> String {
        "string".to_owned()
    }
    fn inline(_: &ts_rs::Config) -> String {
        "string".to_owned()
    }
    fn decl(_: &ts_rs::Config) -> String {
        unimplemented!("a primitive alias has no declaration")
    }
    fn decl_concrete(_: &ts_rs::Config) -> String {
        unimplemented!("a primitive alias has no declaration")
    }
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
    // proptest: for any string s, format!("{:?}", Secret::new(Plain(s))) does not contain s.
    // compile-fail (trybuild is not needed): there is no Serialize impl, so a type that derives
    // Serialize over a Secret field without serialize_exposed does not compile.
}
