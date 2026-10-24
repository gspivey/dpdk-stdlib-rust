//! Clock abstraction for deterministic testing of timer-driven behavior.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Clock trait for injectable time source.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// System clock delegating to `std::time::Instant::now()`.
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Mock clock for deterministic testing. Advances only via explicit calls.
pub struct MockClock {
    inner: Arc<Mutex<Instant>>,
}

impl MockClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Create with a specific starting instant.
    pub fn with_instant(instant: Instant) -> Self {
        Self {
            inner: Arc::new(Mutex::new(instant)),
        }
    }

    /// Advance the clock by the given duration.
    pub fn advance(&self, duration: Duration) {
        let mut t = self.inner.lock().unwrap();
        *t += duration;
    }

    /// Set the clock to an exact instant.
    pub fn set(&self, instant: Instant) {
        let mut t = self.inner.lock().unwrap();
        *t = instant;
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        *self.inner.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_monotonic() {
        let clock = SystemClock;
        let t1 = clock.now();
        let t2 = clock.now();
        assert!(t2 >= t1);
    }

    #[test]
    fn mock_clock_advance() {
        let clock = MockClock::new();
        let t1 = clock.now();
        clock.advance(Duration::from_secs(5));
        let t2 = clock.now();
        assert_eq!(t2 - t1, Duration::from_secs(5));
    }

    #[test]
    fn mock_clock_set() {
        let clock = MockClock::new();
        let start = clock.now();
        clock.advance(Duration::from_secs(10));
        let after_advance = clock.now();
        assert_eq!(after_advance - start, Duration::from_secs(10));
        clock.set(start);
        assert_eq!(clock.now(), start);
    }

    #[test]
    fn mock_clock_multiple_advances() {
        let clock = MockClock::new();
        let start = clock.now();
        clock.advance(Duration::from_millis(100));
        clock.advance(Duration::from_millis(200));
        clock.advance(Duration::from_millis(300));
        assert_eq!(clock.now() - start, Duration::from_millis(600));
    }
}
