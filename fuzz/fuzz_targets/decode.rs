//! Decodes arbitrary bytes as the messages both sides read from the wire: the server reads
//! [`WorkerMessage`]s and the worker reads [`ServerMessage`]s, both from a peer it does not trust
//! yet. Decoding must never panic, and a message that decodes must encode and decode back to
//! itself, so nothing is lost or changed between two peers of the same build.
#![no_main]
#![expect(
    clippy::panic,
    reason = "a fuzz target reports a broken property by panicking"
)]

use std::fmt::Debug;

use abc_protocol::{DecodeError, EncodeError, ServerMessage, WorkerMessage};

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    round_trips(text, WorkerMessage::decode, WorkerMessage::encode);
    round_trips(text, ServerMessage::decode, ServerMessage::encode);
});

/// If `text` decodes, its encoding decodes to the same message.
fn round_trips<M: PartialEq + Debug>(
    text: &str,
    decode: fn(&str) -> Result<M, DecodeError>,
    encode: fn(&M) -> Result<String, EncodeError>,
) {
    let Ok(message) = decode(text) else {
        return;
    };
    let encoded = encode(&message)
        .unwrap_or_else(|error| panic!("a decoded message must encode: {error}: {message:?}"));
    let again = decode(&encoded)
        .unwrap_or_else(|error| panic!("an encoded message must decode: {error}: {encoded}"));
    assert_eq!(again, message, "a message changed on the wire");
}
