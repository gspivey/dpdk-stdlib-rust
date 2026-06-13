//! Timer wheel with 1ms granularity for TCP timer management.
//!
//! Supports 6 timer types: RTO, Persist, Keepalive, TimeWait, FinWait2, DelayedAck.
//! Each connection can have at most one active timer per type.

use std::collections::HashMap;
use std::time::Instant;

use crate::state::FourTuple;

/// The 6 timer types used by the TCP engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerType {
    /// Retransmission timeout.
    Rto,
    /// Zero-window persist probe.
    Persist,
    /// Keepalive probe.
    Keepalive,
    /// TIME_WAIT state (2×MSL = 120s).
    TimeWait,
    /// FIN_WAIT_2 timeout.
    FinWait2,
    /// Delayed ACK coalescing (200ms).
    DelayedAck,
}

/// A timer entry associating a connection and timer type with a deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerEntry {
    pub key: FourTuple,
    pub timer_type: TimerType,
    pub deadline: Instant,
}

/// Timer wheel with 1ms granularity.
///
/// Internally uses a sorted structure (HashMap of active timers keyed by
/// (FourTuple, TimerType)) for correctness and simplicity. The engine calls
/// `expired()` on each tick to collect fired timers.
pub struct TimerWheel {
    timers: HashMap<(FourTuple, TimerType), Instant>,
}

impl TimerWheel {
    /// Create an empty timer wheel.
    pub fn new() -> Self {
        Self {
            timers: HashMap::new(),
        }
    }

    /// Insert (or replace) a timer. If a timer of the same type already exists
    /// for this connection, it is replaced.
    pub fn insert(&mut self, key: FourTuple, timer_type: TimerType, deadline: Instant) {
        self.timers.insert((key, timer_type), deadline);
    }

    /// Cancel a specific timer. Returns true if the timer existed.
    pub fn cancel(&mut self, key: FourTuple, timer_type: TimerType) -> bool {
        self.timers.remove(&(key, timer_type)).is_some()
    }

    /// Cancel all timers for a given connection.
    pub fn cancel_all(&mut self, key: FourTuple) {
        self.timers.retain(|(k, _), _| *k != key);
    }

    /// Return all timers that have expired as of `now`, removing them.
    pub fn expired(&mut self, now: Instant) -> Vec<TimerEntry> {
        let mut fired = Vec::new();
        self.timers.retain(|(key, timer_type), deadline| {
            if *deadline <= now {
                fired.push(TimerEntry {
                    key: *key,
                    timer_type: *timer_type,
                    deadline: *deadline,
                });
                false
            } else {
                true
            }
        });
        fired
    }

    /// Return the earliest deadline across all active timers, or None if empty.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.values().min().copied()
    }

    /// Check if a specific timer is active.
    pub fn is_active(&self, key: FourTuple, timer_type: TimerType) -> bool {
        self.timers.contains_key(&(key, timer_type))
    }

    /// Number of active timers.
    pub fn len(&self) -> usize {
        self.timers.len()
    }

    /// Whether the wheel has no active timers.
    pub fn is_empty(&self) -> bool {
        self.timers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ft(port: u16) -> FourTuple {
        FourTuple {
            local: format!("10.0.0.1:{}", port).parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        }
    }

    #[test]
    fn insert_and_expire() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        wheel.insert(ft(1000), TimerType::Rto, now + Duration::from_millis(100));

        // Not yet expired
        let fired = wheel.expired(now + Duration::from_millis(99));
        assert!(fired.is_empty());
        assert_eq!(wheel.len(), 1);

        // Expired at exactly the deadline
        let fired = wheel.expired(now + Duration::from_millis(100));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].key, ft(1000));
        assert_eq!(fired[0].timer_type, TimerType::Rto);
        assert_eq!(wheel.len(), 0);
    }

    #[test]
    fn cancel_specific_timer() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        wheel.insert(ft(1000), TimerType::Rto, now + Duration::from_secs(1));
        wheel.insert(ft(1000), TimerType::Keepalive, now + Duration::from_secs(60));

        assert!(wheel.cancel(ft(1000), TimerType::Rto));
        assert!(!wheel.cancel(ft(1000), TimerType::Rto)); // already canceled
        assert_eq!(wheel.len(), 1);
        assert!(wheel.is_active(ft(1000), TimerType::Keepalive));
    }

    #[test]
    fn cancel_all_for_connection() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        wheel.insert(ft(1000), TimerType::Rto, now + Duration::from_secs(1));
        wheel.insert(ft(1000), TimerType::Persist, now + Duration::from_secs(2));
        wheel.insert(ft(2000), TimerType::Rto, now + Duration::from_secs(1));

        wheel.cancel_all(ft(1000));
        assert_eq!(wheel.len(), 1);
        assert!(wheel.is_active(ft(2000), TimerType::Rto));
    }

    #[test]
    fn replace_timer() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        wheel.insert(ft(1000), TimerType::Rto, now + Duration::from_millis(100));
        wheel.insert(ft(1000), TimerType::Rto, now + Duration::from_millis(200));

        // Only one timer (replaced)
        assert_eq!(wheel.len(), 1);

        // Should not fire at original deadline
        let fired = wheel.expired(now + Duration::from_millis(150));
        assert!(fired.is_empty());

        // Should fire at new deadline
        let fired = wheel.expired(now + Duration::from_millis(200));
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn next_deadline() {
        let mut wheel = TimerWheel::new();
        assert_eq!(wheel.next_deadline(), None);

        let now = Instant::now();
        wheel.insert(ft(1000), TimerType::Rto, now + Duration::from_millis(500));
        wheel.insert(ft(2000), TimerType::Keepalive, now + Duration::from_millis(100));
        wheel.insert(ft(3000), TimerType::TimeWait, now + Duration::from_secs(120));

        assert_eq!(wheel.next_deadline(), Some(now + Duration::from_millis(100)));
    }

    #[test]
    fn multiple_timers_fire_at_once() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        wheel.insert(ft(1000), TimerType::Rto, now + Duration::from_millis(50));
        wheel.insert(ft(2000), TimerType::Rto, now + Duration::from_millis(50));
        wheel.insert(ft(3000), TimerType::Rto, now + Duration::from_millis(100));

        let fired = wheel.expired(now + Duration::from_millis(50));
        assert_eq!(fired.len(), 2);
        assert_eq!(wheel.len(), 1); // ft(3000) still pending
    }

    #[test]
    fn all_timer_types() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        let key = ft(1000);

        wheel.insert(key, TimerType::Rto, now + Duration::from_millis(1));
        wheel.insert(key, TimerType::Persist, now + Duration::from_millis(2));
        wheel.insert(key, TimerType::Keepalive, now + Duration::from_millis(3));
        wheel.insert(key, TimerType::TimeWait, now + Duration::from_millis(4));
        wheel.insert(key, TimerType::FinWait2, now + Duration::from_millis(5));
        wheel.insert(key, TimerType::DelayedAck, now + Duration::from_millis(6));

        assert_eq!(wheel.len(), 6);

        let fired = wheel.expired(now + Duration::from_millis(6));
        assert_eq!(fired.len(), 6);
        assert!(wheel.is_empty());
    }

    #[test]
    fn is_active() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        let key = ft(1000);

        assert!(!wheel.is_active(key, TimerType::Rto));
        wheel.insert(key, TimerType::Rto, now + Duration::from_secs(1));
        assert!(wheel.is_active(key, TimerType::Rto));
        assert!(!wheel.is_active(key, TimerType::Persist));
    }

    #[test]
    fn one_ms_granularity() {
        let mut wheel = TimerWheel::new();
        let now = Instant::now();
        wheel.insert(ft(1000), TimerType::DelayedAck, now + Duration::from_millis(1));

        // Not expired at now
        let fired = wheel.expired(now);
        assert!(fired.is_empty());

        // Expired at now + 1ms
        let fired = wheel.expired(now + Duration::from_millis(1));
        assert_eq!(fired.len(), 1);
    }
}
