//! Opt-in idle backoff for the DPDK receive loops (tokio-udp-perf Req 5.4).
//!
//! Default is OFF: the receive loop is then exactly the 0.3.0 spin
//! (`try_recv_from` + `yield_now`). When enabled with threshold `d`, the loop keeps
//! spinning with `yield_now` until the socket has seen no packet for `max(d, 2 ms)`,
//! then escalates to fixed ~1 ms `tokio::time::sleep` steps. The idle clock restarts
//! at "just received" on every call, so a loaded receiver never sleeps.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Minimum idle threshold; smaller configured values are treated as this.
pub(crate) const MIN_IDLE_THRESHOLD: Duration = Duration::from_millis(2);
/// Fixed sleep step once idle (tokio's timer resolution is ~1 ms).
pub(crate) const SLEEP_STEP: Duration = Duration::from_millis(1);

/// Sentinel meaning "backoff disabled".
const OFF: u64 = u64::MAX;

/// Lock-free holder of the configured `Option<Duration>`.
pub(crate) struct IdleBackoff(AtomicU64);

impl IdleBackoff {
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(OFF))
    }

    pub(crate) fn set(&self, d: Option<Duration>) {
        let v = match d {
            // Saturate below the sentinel so a huge Duration stays "enabled".
            Some(d) => u64::try_from(d.as_nanos()).unwrap_or(OFF - 1).min(OFF - 1),
            None => OFF,
        };
        self.0.store(v, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> Option<Duration> {
        match self.0.load(Ordering::Relaxed) {
            OFF => None,
            n => Some(Duration::from_nanos(n)),
        }
    }

    /// Effective idle threshold: `max(d, 2 ms)`, or `None` when disabled.
    pub(crate) fn threshold(&self) -> Option<Duration> {
        self.get().map(|d| d.max(MIN_IDLE_THRESHOLD))
    }
}

/// Run `poll` until it yields a value, pacing empty polls per the backoff policy.
///
/// Disabled: `poll` + `yield_now` (byte-for-byte the 0.3.0 loop). Enabled: `yield_now`
/// until idle for the threshold, then fixed 1 ms sleeps. Returns immediately on a
/// first-poll success without touching the clock or timer.
pub(crate) async fn recv_with_backoff<T>(
    backoff: &IdleBackoff,
    mut poll: impl FnMut() -> io::Result<Option<T>>,
) -> io::Result<T> {
    if let Some(r) = poll()? {
        return Ok(r);
    }
    let threshold = backoff.threshold();
    let idle_since = threshold.map(|_| Instant::now());
    loop {
        match (threshold, idle_since) {
            (Some(t), Some(since)) if since.elapsed() >= t => {
                ::tokio::time::sleep(SLEEP_STEP).await
            }
            _ => ::tokio::task::yield_now().await,
        }
        if let Some(r) = poll()? {
            return Ok(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_off_and_roundtrip() {
        let b = IdleBackoff::new();
        assert_eq!(b.get(), None);
        assert_eq!(b.threshold(), None);
        b.set(Some(Duration::from_millis(5)));
        assert_eq!(b.get(), Some(Duration::from_millis(5)));
        b.set(None);
        assert_eq!(b.get(), None);
    }

    #[test]
    fn threshold_is_clamped_to_two_ms() {
        let b = IdleBackoff::new();
        b.set(Some(Duration::from_micros(10)));
        assert_eq!(b.get(), Some(Duration::from_micros(10)));
        assert_eq!(b.threshold(), Some(MIN_IDLE_THRESHOLD));
        b.set(Some(Duration::ZERO));
        assert_eq!(b.threshold(), Some(MIN_IDLE_THRESHOLD));
        b.set(Some(Duration::from_millis(7)));
        assert_eq!(b.threshold(), Some(Duration::from_millis(7)));
    }

    #[test]
    fn huge_duration_stays_enabled() {
        let b = IdleBackoff::new();
        b.set(Some(Duration::MAX));
        assert!(b.get().is_some());
    }

    #[tokio::test]
    async fn first_poll_success_returns_immediately() {
        let b = IdleBackoff::new();
        b.set(Some(Duration::from_millis(2)));
        let mut calls = 0;
        let v = recv_with_backoff(&b, || {
            calls += 1;
            Ok(Some(7))
        })
        .await
        .unwrap();
        assert_eq!((v, calls), (7, 1));
    }

    /// Disabled (default): never touches the timer, so it works on a runtime built
    /// without the time driver (a sleep there would panic).
    #[test]
    fn disabled_never_touches_the_timer() {
        let rt = ::tokio::runtime::Builder::new_current_thread().build().unwrap();
        let b = IdleBackoff::new();
        let mut polls = 0u32;
        let r = rt.block_on(recv_with_backoff(&b, || {
            polls += 1;
            Ok((polls >= 200).then_some(polls))
        }));
        assert_eq!(r.unwrap(), 200);
    }

    /// Enabled: once idle past the threshold the loop sleeps in ~1 ms steps. Between 50 ms
    /// and 150 ms of idleness a pure yield spin would poll many thousands of times; with the
    /// backoff it polls at most ~1 per ms.
    #[tokio::test]
    async fn enabled_sleeps_after_threshold_and_still_returns() {
        let b = IdleBackoff::new();
        b.set(Some(Duration::from_millis(2)));
        let start = Instant::now();
        let mut late_polls = 0u64;
        let r = recv_with_backoff(&b, || {
            let e = start.elapsed();
            if e >= Duration::from_millis(50) {
                late_polls += 1;
            }
            Ok((e >= Duration::from_millis(150)).then_some(()))
        })
        .await;
        assert!(r.is_ok());
        assert!(late_polls <= 110, "polled {late_polls} times in the sleep stage");
    }

    #[tokio::test]
    async fn errors_propagate() {
        let b = IdleBackoff::new();
        let r: io::Result<()> =
            recv_with_backoff(&b, || Err(io::Error::new(io::ErrorKind::Other, "boom"))).await;
        assert!(r.is_err());
    }
}
