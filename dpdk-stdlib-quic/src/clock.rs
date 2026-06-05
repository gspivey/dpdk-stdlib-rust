//! Monotonic clock for s2n-quic timer servicing.

use s2n_quic_core::time::{self, Timestamp};
use std::time::Instant;

/// Monotonic clock wrapping `std::time::Instant`.
pub struct StdClock {
    epoch: Instant,
}

impl StdClock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl time::Clock for StdClock {
    fn get_time(&self) -> Timestamp {
        let elapsed = self.epoch.elapsed();
        unsafe { Timestamp::from_duration(elapsed) }
    }
}
