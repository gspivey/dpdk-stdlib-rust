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

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_quic_core::time::Clock;

    #[test]
    fn clock_monotonicity() {
        let clock = StdClock::new();
        let t1 = clock.get_time();
        // Spin briefly to ensure time advances
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t2 = clock.get_time();
        assert!(t2 > t1, "clock must be monotonically increasing");
    }
}
