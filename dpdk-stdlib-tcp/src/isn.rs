//! Initial Sequence Number generator per RFC 6528.
//!
//! Uses a per-boot 128-bit secret + SipHash-2-4 of the 4-tuple + M
//! (elapsed microseconds / 4 since boot) to produce unpredictable ISNs.

use std::hash::Hasher;
use std::time::Instant;

use siphasher::sip::SipHasher24;

use crate::clock::Clock;
use crate::seq::SeqNum;
use crate::state::FourTuple;

/// ISN generator with per-boot secret (RFC 6528).
pub struct IsnGenerator {
    secret: [u8; 16],
    boot_instant: Instant,
}

impl IsnGenerator {
    /// Create a new ISN generator with a random per-boot secret.
    pub fn new(clock: &dyn Clock) -> Self {
        let mut secret = [0u8; 16];
        getrandom::getrandom(&mut secret).expect("getrandom failed");
        Self {
            secret,
            boot_instant: clock.now(),
        }
    }

    /// Create with a known secret (for deterministic testing).
    #[cfg(test)]
    pub fn with_secret(secret: [u8; 16], boot_instant: Instant) -> Self {
        Self {
            secret,
            boot_instant,
        }
    }

    /// Generate an ISN for the given 4-tuple.
    /// M = elapsed µs since boot / 4 (wraps ~4.7 hours, fine for ISN).
    pub fn generate(&self, four_tuple: &FourTuple, clock: &dyn Clock) -> SeqNum {
        let elapsed = clock.now().duration_since(self.boot_instant);
        let m = (elapsed.as_micros() / 4) as u32;

        let key_lo = u64::from_le_bytes(self.secret[0..8].try_into().unwrap());
        let key_hi = u64::from_le_bytes(self.secret[8..16].try_into().unwrap());
        let mut hasher = SipHasher24::new_with_keys(key_lo, key_hi);
        hasher.write(&four_tuple.to_bytes());
        let hash = hasher.finish() as u32;

        SeqNum(m.wrapping_add(hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use std::time::Duration;

    #[test]
    fn isn_deterministic_for_same_inputs() {
        let clock = MockClock::new();
        let gen = IsnGenerator::with_secret([1u8; 16], clock.now());
        let ft = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let isn1 = gen.generate(&ft, &clock);
        let isn2 = gen.generate(&ft, &clock);
        assert_eq!(isn1, isn2);
    }

    #[test]
    fn isn_differs_for_different_tuples() {
        let clock = MockClock::new();
        let gen = IsnGenerator::with_secret([2u8; 16], clock.now());
        let ft1 = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let ft2 = FourTuple {
            local: "10.0.0.1:1235".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        assert_ne!(gen.generate(&ft1, &clock), gen.generate(&ft2, &clock));
    }

    #[test]
    fn isn_advances_with_time() {
        let clock = MockClock::new();
        let gen = IsnGenerator::with_secret([3u8; 16], clock.now());
        let ft = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let isn1 = gen.generate(&ft, &clock);
        clock.advance(Duration::from_micros(400)); // M advances by 100
        let isn2 = gen.generate(&ft, &clock);
        // ISN should have advanced (M component changed)
        assert_ne!(isn1, isn2);
    }

    #[test]
    fn isn_unpredictable_different_secrets() {
        let clock = MockClock::new();
        let gen1 = IsnGenerator::with_secret([4u8; 16], clock.now());
        let gen2 = IsnGenerator::with_secret([5u8; 16], clock.now());
        let ft = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        assert_ne!(gen1.generate(&ft, &clock), gen2.generate(&ft, &clock));
    }

    #[test]
    fn isn_new_uses_random_secret() {
        let clock = MockClock::new();
        let gen1 = IsnGenerator::new(&clock);
        let gen2 = IsnGenerator::new(&clock);
        let ft = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        // Overwhelmingly likely to differ (random secrets)
        assert_ne!(gen1.generate(&ft, &clock), gen2.generate(&ft, &clock));
    }
}
