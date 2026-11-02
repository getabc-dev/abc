//! Where the store's randomness comes from, without a panic in the path.
//!
//! Each request draws one 32-byte seed from an [`Entropy`] source, which can fail with a typed
//! error, and seeds a `ChaCha20` generator ([`ChaChaMint`]) that mints that request's ids and
//! secrets. `ChaCha20` seeded from the operating system is a cryptographically secure generator,
//! so a token or setup code minted from it is as unpredictable as one read from the OS directly.
//! Tests seed it from a fixed sequence instead, so every run mints the same values.

use std::fmt;
use std::sync::{Mutex, PoisonError};

use abc_core::decision::Mint;
use abc_core::time::Timestamp;
use rand::rngs::{ChaCha20Rng, SysRng};
use rand::{Rng as _, SeedableRng as _, TryRng as _};
use thiserror::Error;
use zeroize::Zeroizing;

/// A source of seeds for the store's per-request generators.
pub trait Entropy: Send + Sync + fmt::Debug {
    /// 32 fresh, unpredictable bytes.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyUnavailable`] when the source cannot produce them.
    fn seed(&self) -> Result<[u8; 32], EntropyUnavailable>;
}

/// The operating system's random number generator failed.
#[derive(Debug, Error)]
#[error("the operating system's random number generator failed")]
pub struct EntropyUnavailable(#[source] pub rand::rngs::SysError);

/// The operating system's generator.
#[derive(Debug)]
pub(crate) struct SystemEntropy;

impl Entropy for SystemEntropy {
    fn seed(&self) -> Result<[u8; 32], EntropyUnavailable> {
        let mut seed = [0; 32];
        SysRng
            .try_fill_bytes(&mut seed)
            .map_err(EntropyUnavailable)?;
        Ok(seed)
    }
}

/// A `ChaCha20` generator that mints ids and secrets: one per request, seeded from [`Entropy`].
pub(crate) struct ChaChaMint(Mutex<ChaCha20Rng>);

impl ChaChaMint {
    /// A generator seeded with `seed`.
    pub(crate) fn from_seed(seed: [u8; 32]) -> Self {
        Self(Mutex::new(ChaCha20Rng::from_seed(seed)))
    }

    fn fill(&self, bytes: &mut [u8]) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fill_bytes(bytes);
    }
}

impl fmt::Debug for ChaChaMint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChaChaMint(<seeded>)")
    }
}

impl Mint for ChaChaMint {
    fn uuid(&self, now: Timestamp) -> uuid::Uuid {
        let mut random = [0; 10];
        self.fill(&mut random);
        uuid_v7(now, &random)
    }

    fn secret(&self) -> Zeroizing<[u8; 32]> {
        let mut bytes = Zeroizing::new([0; 32]);
        self.fill(bytes.as_mut_slice());
        bytes
    }
}

/// A `UUIDv7` whose time part is `now` (milliseconds since the Unix epoch) and whose remaining
/// bits come from `random`.
pub(crate) fn uuid_v7(now: Timestamp, random: &[u8; 10]) -> uuid::Uuid {
    let millis = now.as_offset().unix_timestamp_nanos() / 1_000_000;
    uuid::Builder::from_unix_timestamp_millis(u64::try_from(millis).unwrap_or(0), random)
        .into_uuid()
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn system_entropy_gives_fresh_seeds() {
        let seed = SystemEntropy.seed().unwrap();
        assert_ne!(seed, SystemEntropy.seed().unwrap(), "every seed is fresh");
        assert_ne!(seed, [0; 32], "and not zero");
    }

    #[test]
    fn a_chacha_mint_makes_time_ordered_ids_and_repeats_for_a_seed() {
        let now = Timestamp::from_utc(datetime!(2026-01-01 00:00:01.5 UTC));
        let mint = ChaChaMint::from_seed([7; 32]);
        let first = mint.uuid(now);
        assert_ne!(first, mint.uuid(now), "every id is fresh");
        assert_eq!(first.get_version_num(), 7, "a UUIDv7");
        let (seconds, nanos) = first.get_timestamp().unwrap().to_unix();
        assert_eq!(
            (seconds, nanos),
            (1_767_225_601, 500_000_000),
            "its time is now, to the ms"
        );
        let again = ChaChaMint::from_seed([7; 32]);
        assert_eq!(again.uuid(now), first, "the same seed, the same sequence");
        let secret = mint.secret();
        assert_ne!(*secret, *mint.secret(), "every secret is fresh");
        assert_ne!(*secret, [0; 32], "and not zero");
        assert_eq!(
            format!("{mint:?}"),
            "ChaChaMint(<seeded>)",
            "Debug shows no state"
        );
    }
}
