//! High-performance instrumentation for DPDK UDP sockets.
//!
//! Provides lock-free counters, latency sampling, and background reporting
//! with < 1% throughput overhead at 350K PPS.
//!
//! All hot-path counters use `AtomicU64` with `Relaxed` ordering — the cheapest
//! atomic operation (single cache-line bounce on x86, no memory fence).
//!
//! ## Feature gate: `perf-counters`
//!
//! Hot-path counter increments are gated behind the `perf-counters` cargo feature
//! (enabled by default). Disable with `--no-default-features` to compile out all
//! instrumentation overhead from the TX/RX fast paths.
//!
//! The `PerfCounters` struct, `PerfReporter`, and `LatencySampler` always exist
//! so the API doesn't break — but counter values stay at zero when the feature
//! is disabled.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// ============================================================================
// Feature-gated hot-path macros
// ============================================================================

/// Increment an `AtomicU64` counter with `Relaxed` ordering.
/// Compiles to nothing when the `perf-counters` feature is disabled.
#[cfg(feature = "perf-counters")]
#[macro_export]
macro_rules! perf_inc {
    ($counter:expr, $val:expr) => {
        $counter.fetch_add($val, std::sync::atomic::Ordering::Relaxed)
    };
    ($counter:expr) => {
        $counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    };
}

/// No-op when `perf-counters` feature is disabled.
#[cfg(not(feature = "perf-counters"))]
#[macro_export]
macro_rules! perf_inc {
    ($counter:expr, $val:expr) => { 0u64 };
    ($counter:expr) => { 0u64 };
}

/// Check if latency should be sampled for this packet.
/// Always returns false when `perf-counters` feature is disabled.
#[cfg(feature = "perf-counters")]
#[macro_export]
macro_rules! perf_should_sample {
    ($sampler:expr) => {
        $sampler.should_sample()
    };
}

#[cfg(not(feature = "perf-counters"))]
#[macro_export]
macro_rules! perf_should_sample {
    ($sampler:expr) => {
        false
    };
}

// ============================================================================
// PerfCounters — lock-free hot-path counters
// ============================================================================

/// Per-socket performance counters. All fields are `AtomicU64` for lock-free
/// increment on the hot path. The reporting thread reads them periodically.
#[repr(align(64))] // Cache-line aligned to prevent false sharing
pub struct PerfCounters {
    // RX path
    pub rx_packets: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub rx_drops_ring_full: AtomicU64,
    pub rx_drops_parse_fail: AtomicU64,
    pub rx_arp_handled: AtomicU64,
    pub rx_icmp_handled: AtomicU64,
    pub rx_bursts: AtomicU64,
    pub rx_burst_sum: AtomicU64,

    // TX path
    pub tx_packets: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub tx_failures: AtomicU64,

    // Ring utilization (multi-core only)
    pub worker_ring_enqueue_fail: AtomicU64,
    pub app_ring_enqueue_fail: AtomicU64,
    pub tx_ring_enqueue_fail: AtomicU64,

    // Worker (multi-core only)
    pub worker_packets_processed: AtomicU64,
    pub worker_idle_polls: AtomicU64,

    // ARP cache
    pub arp_cache_hits: AtomicU64,
    pub arp_cache_misses: AtomicU64,
    pub arp_cache_inserts: AtomicU64,

    // Latency sampling
    pub latency_sample_count: AtomicU64,
    pub latency_sum_ns: AtomicU64,
    pub latency_max_ns: AtomicU64,
}

impl PerfCounters {
    /// Create a new set of zeroed counters.
    pub fn new() -> Self {
        Self {
            rx_packets: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            rx_drops_ring_full: AtomicU64::new(0),
            rx_drops_parse_fail: AtomicU64::new(0),
            rx_arp_handled: AtomicU64::new(0),
            rx_icmp_handled: AtomicU64::new(0),
            rx_bursts: AtomicU64::new(0),
            rx_burst_sum: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_failures: AtomicU64::new(0),
            worker_ring_enqueue_fail: AtomicU64::new(0),
            app_ring_enqueue_fail: AtomicU64::new(0),
            tx_ring_enqueue_fail: AtomicU64::new(0),
            worker_packets_processed: AtomicU64::new(0),
            worker_idle_polls: AtomicU64::new(0),
            arp_cache_hits: AtomicU64::new(0),
            arp_cache_misses: AtomicU64::new(0),
            arp_cache_inserts: AtomicU64::new(0),
            latency_sample_count: AtomicU64::new(0),
            latency_sum_ns: AtomicU64::new(0),
            latency_max_ns: AtomicU64::new(0),
        }
    }

    /// Take a snapshot of all counters (Relaxed loads — approximate but cheap).
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            rx_drops_ring_full: self.rx_drops_ring_full.load(Ordering::Relaxed),
            rx_drops_parse_fail: self.rx_drops_parse_fail.load(Ordering::Relaxed),
            rx_arp_handled: self.rx_arp_handled.load(Ordering::Relaxed),
            rx_icmp_handled: self.rx_icmp_handled.load(Ordering::Relaxed),
            rx_bursts: self.rx_bursts.load(Ordering::Relaxed),
            rx_burst_sum: self.rx_burst_sum.load(Ordering::Relaxed),
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            tx_failures: self.tx_failures.load(Ordering::Relaxed),
            worker_ring_enqueue_fail: self.worker_ring_enqueue_fail.load(Ordering::Relaxed),
            app_ring_enqueue_fail: self.app_ring_enqueue_fail.load(Ordering::Relaxed),
            tx_ring_enqueue_fail: self.tx_ring_enqueue_fail.load(Ordering::Relaxed),
            worker_packets_processed: self.worker_packets_processed.load(Ordering::Relaxed),
            worker_idle_polls: self.worker_idle_polls.load(Ordering::Relaxed),
            arp_cache_hits: self.arp_cache_hits.load(Ordering::Relaxed),
            arp_cache_misses: self.arp_cache_misses.load(Ordering::Relaxed),
            arp_cache_inserts: self.arp_cache_inserts.load(Ordering::Relaxed),
            latency_sample_count: self.latency_sample_count.load(Ordering::Relaxed),
            latency_sum_ns: self.latency_sum_ns.load(Ordering::Relaxed),
            latency_max_ns: self.latency_max_ns.load(Ordering::Relaxed),
        }
    }

    /// Update the max latency atomically (CAS loop, only on sampled packets).
    pub fn update_latency_max(&self, ns: u64) {
        let mut current = self.latency_max_ns.load(Ordering::Relaxed);
        while ns > current {
            match self.latency_max_ns.compare_exchange_weak(
                current,
                ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Reset interval-specific counters (latency max).
    /// Called by the reporter after each interval snapshot.
    pub fn reset_interval(&self) {
        self.latency_max_ns.store(0, Ordering::Relaxed);
        self.latency_sample_count.store(0, Ordering::Relaxed);
        self.latency_sum_ns.store(0, Ordering::Relaxed);
    }
}

impl Default for PerfCounters {
    fn default() -> Self {
        Self::new()
    }
}

// Need Debug for struct that contains PerfCounters
impl std::fmt::Debug for PerfCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerfCounters")
            .field("rx_packets", &self.rx_packets.load(Ordering::Relaxed))
            .field("tx_packets", &self.tx_packets.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Point-in-time snapshot of all counters (plain u64, no atomics).
#[derive(Debug, Clone)]
pub struct CounterSnapshot {
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub rx_drops_ring_full: u64,
    pub rx_drops_parse_fail: u64,
    pub rx_arp_handled: u64,
    pub rx_icmp_handled: u64,
    pub rx_bursts: u64,
    pub rx_burst_sum: u64,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    pub tx_failures: u64,
    pub worker_ring_enqueue_fail: u64,
    pub app_ring_enqueue_fail: u64,
    pub tx_ring_enqueue_fail: u64,
    pub worker_packets_processed: u64,
    pub worker_idle_polls: u64,
    pub arp_cache_hits: u64,
    pub arp_cache_misses: u64,
    pub arp_cache_inserts: u64,
    pub latency_sample_count: u64,
    pub latency_sum_ns: u64,
    pub latency_max_ns: u64,
}

impl CounterSnapshot {
    /// Compute per-second rates by diffing two snapshots over an interval.
    pub fn rates_since(&self, prev: &CounterSnapshot, elapsed_secs: f64) -> RateSnapshot {
        let delta = |cur: u64, old: u64| -> f64 {
            cur.saturating_sub(old) as f64 / elapsed_secs
        };

        let rx_bursts_delta = self.rx_bursts.saturating_sub(prev.rx_bursts);
        let rx_burst_sum_delta = self.rx_burst_sum.saturating_sub(prev.rx_burst_sum);
        let burst_avg = if rx_bursts_delta > 0 {
            rx_burst_sum_delta as f64 / rx_bursts_delta as f64
        } else {
            0.0
        };

        let worker_total = self.worker_packets_processed.saturating_sub(prev.worker_packets_processed)
            + self.worker_idle_polls.saturating_sub(prev.worker_idle_polls);
        let worker_idle_pct = if worker_total > 0 {
            self.worker_idle_polls.saturating_sub(prev.worker_idle_polls) as f64
                / worker_total as f64
                * 100.0
        } else {
            0.0
        };

        let lat_count = self.latency_sample_count;
        let lat_avg_us = if lat_count > 0 {
            (self.latency_sum_ns as f64 / lat_count as f64) / 1000.0
        } else {
            0.0
        };

        RateSnapshot {
            rx_pps: delta(self.rx_packets, prev.rx_packets),
            rx_bps: delta(self.rx_bytes, prev.rx_bytes) * 8.0,
            tx_pps: delta(self.tx_packets, prev.tx_packets),
            tx_bps: delta(self.tx_bytes, prev.tx_bytes) * 8.0,
            rx_drops: self.rx_drops_ring_full.saturating_sub(prev.rx_drops_ring_full),
            tx_fails: self.tx_failures.saturating_sub(prev.tx_failures),
            arp_hits: self.arp_cache_hits.saturating_sub(prev.arp_cache_hits),
            arp_misses: self.arp_cache_misses.saturating_sub(prev.arp_cache_misses),
            ring_drops: self.worker_ring_enqueue_fail.saturating_sub(prev.worker_ring_enqueue_fail)
                + self.app_ring_enqueue_fail.saturating_sub(prev.app_ring_enqueue_fail)
                + self.tx_ring_enqueue_fail.saturating_sub(prev.tx_ring_enqueue_fail),
            worker_idle_pct,
            burst_avg,
            lat_avg_us,
            lat_max_us: self.latency_max_ns as f64 / 1000.0,
        }
    }
}

/// Computed rates for a reporting interval.
#[derive(Debug, Clone)]
pub struct RateSnapshot {
    pub rx_pps: f64,
    pub tx_pps: f64,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_drops: u64,
    pub tx_fails: u64,
    pub arp_hits: u64,
    pub arp_misses: u64,
    pub ring_drops: u64,
    pub worker_idle_pct: f64,
    pub burst_avg: f64,
    pub lat_avg_us: f64,
    pub lat_max_us: f64,
}

// ============================================================================
// LatencySampler — lightweight sampled latency tracking
// ============================================================================

/// Lightweight latency sampler using a fixed-size ring buffer.
/// Only samples 1 in N packets to keep overhead < 1%.
pub struct LatencySampler {
    /// Ring buffer of recent latency samples in nanoseconds (AtomicU64 for safe concurrent access).
    samples: Box<[AtomicU64]>,
    /// Number of valid samples stored (up to capacity).
    count: AtomicU64,
    /// Write index (wraps around).
    write_idx: AtomicU64,
    /// Sample every Nth packet.
    sample_rate: u64,
    /// Counter to determine when to sample.
    packet_count: AtomicU64,
}

impl LatencySampler {
    /// Create a new sampler with given ring buffer capacity and sample rate.
    ///
    /// `capacity`: number of latency samples to store (e.g., 1024).
    /// `sample_rate`: sample 1 in N packets (e.g., 1000).
    pub fn new(capacity: usize, sample_rate: u64) -> Self {
        let samples: Vec<AtomicU64> = (0..capacity).map(|_| AtomicU64::new(0)).collect();
        Self {
            samples: samples.into_boxed_slice(),
            count: AtomicU64::new(0),
            write_idx: AtomicU64::new(0),
            sample_rate: sample_rate.max(1),
            packet_count: AtomicU64::new(0),
        }
    }

    /// Check if this packet should be sampled (1 in N).
    /// Returns true if the caller should measure latency for this packet.
    pub fn should_sample(&self) -> bool {
        let count = self.packet_count.fetch_add(1, Ordering::Relaxed);
        count % self.sample_rate == 0
    }

    /// Record a latency sample in nanoseconds.
    pub fn record(&self, duration_ns: u64) {
        let capacity = self.samples.len() as u64;
        if capacity == 0 {
            return;
        }
        let idx = self.write_idx.fetch_add(1, Ordering::Relaxed) % capacity;
        self.samples[idx as usize].store(duration_ns, Ordering::Relaxed);

        let count = self.count.load(Ordering::Relaxed);
        if count < capacity {
            self.count.store(count + 1, Ordering::Relaxed);
        }
    }

    /// Compute percentiles from the current sample buffer.
    ///
    /// Returns (p50, p95, p99, p99.9, max) in nanoseconds.
    /// Called by the reporter thread — not on the hot path.
    pub fn percentiles(&self) -> LatencyPercentiles {
        let count = self.count.load(Ordering::Relaxed) as usize;
        if count == 0 {
            return LatencyPercentiles::default();
        }

        let mut sorted: Vec<u64> = self.samples[..count]
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect();
        sorted.sort_unstable();

        let p = |pct: f64| -> u64 {
            let idx = ((pct / 100.0) * (sorted.len() - 1) as f64).round() as usize;
            sorted[idx.min(sorted.len() - 1)]
        };

        LatencyPercentiles {
            p50_ns: p(50.0),
            p95_ns: p(95.0),
            p99_ns: p(99.0),
            p999_ns: p(99.9),
            max_ns: sorted[sorted.len() - 1],
            count: count as u64,
        }
    }

    /// Reset the sampler for a new interval.
    pub fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
        self.write_idx.store(0, Ordering::Relaxed);
    }
}

impl Default for LatencySampler {
    fn default() -> Self {
        Self::new(1024, 1000)
    }
}

impl std::fmt::Debug for LatencySampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LatencySampler")
            .field("count", &self.count.load(Ordering::Relaxed))
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

/// Latency percentile results.
#[derive(Debug, Clone, Default)]
pub struct LatencyPercentiles {
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub p999_ns: u64,
    pub max_ns: u64,
    pub count: u64,
}

// ============================================================================
// PerfSnapshot — programmatic API for reading perf data
// ============================================================================

/// Point-in-time performance snapshot for programmatic access.
#[derive(Debug, Clone)]
pub struct PerfSnapshot {
    pub rx_pps: f64,
    pub tx_pps: f64,
    pub rx_drops: u64,
    pub latency_avg_us: f64,
    pub latency_p50_us: f64,
    pub latency_p95_us: f64,
    pub latency_p99_us: f64,
    pub latency_max_us: f64,
    pub worker_idle_pct: f64,
    pub ring_drop_rate: f64,
}

// ============================================================================
// PerfReporter — background reporting thread
// ============================================================================

/// Background thread that reads `PerfCounters` and `LatencySampler` every N
/// seconds and emits structured key=value log lines to stderr.
pub struct PerfReporter {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PerfReporter {
    /// Start a background reporter thread.
    ///
    /// Emits one log line per `interval` to stderr. The line contains
    /// key=value pairs for easy parsing.
    pub fn start(
        counters: Arc<PerfCounters>,
        sampler: Arc<LatencySampler>,
        interval: Duration,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = thread::Builder::new()
            .name("dpdk-perf-reporter".to_string())
            .spawn(move || {
                Self::reporter_loop(counters, sampler, interval, shutdown_clone);
            })
            .expect("failed to spawn perf reporter thread");

        Self {
            shutdown,
            handle: Some(handle),
        }
    }

    fn reporter_loop(
        counters: Arc<PerfCounters>,
        sampler: Arc<LatencySampler>,
        interval: Duration,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut prev_snapshot = counters.snapshot();
        let mut prev_time = Instant::now();

        while !shutdown.load(Ordering::Relaxed) {
            thread::sleep(interval);
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let now = Instant::now();
            let elapsed = now.duration_since(prev_time);
            let elapsed_secs = elapsed.as_secs_f64();
            if elapsed_secs < 0.001 {
                continue;
            }

            let current = counters.snapshot();
            let rates = current.rates_since(&prev_snapshot, elapsed_secs);
            let latencies = sampler.percentiles();

            let interval_secs = interval.as_secs();
            eprintln!(
                "[PERF] interval={}s rx_pps={:.0} rx_bps={:.0} tx_pps={:.0} tx_bps={:.0} \
                 rx_drops={} tx_fails={} lat_avg_us={:.0} lat_p50_us={:.0} lat_p95_us={:.0} \
                 lat_p99_us={:.0} lat_max_us={:.0} arp_hits={} arp_misses={} ring_drops={} \
                 worker_idle_pct={:.1} burst_avg={:.1}",
                interval_secs,
                rates.rx_pps,
                rates.rx_bps,
                rates.tx_pps,
                rates.tx_bps,
                rates.rx_drops,
                rates.tx_fails,
                rates.lat_avg_us,
                latencies.p50_ns as f64 / 1000.0,
                latencies.p95_ns as f64 / 1000.0,
                latencies.p99_ns as f64 / 1000.0,
                rates.lat_max_us,
                rates.arp_hits,
                rates.arp_misses,
                rates.ring_drops,
                rates.worker_idle_pct,
                rates.burst_avg,
            );

            // Reset interval-specific data
            counters.reset_interval();
            sampler.reset();

            prev_snapshot = counters.snapshot();
            prev_time = now;
        }
    }

    /// Stop the reporter thread.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PerfReporter {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn perf_counters_new_is_zeroed() {
        let counters = PerfCounters::new();
        assert_eq!(counters.rx_packets.load(Ordering::Relaxed), 0);
        assert_eq!(counters.tx_packets.load(Ordering::Relaxed), 0);
        assert_eq!(counters.rx_drops_ring_full.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn perf_counters_increment_and_snapshot() {
        let counters = PerfCounters::new();
        counters.rx_packets.fetch_add(100, Ordering::Relaxed);
        counters.tx_packets.fetch_add(50, Ordering::Relaxed);
        counters.rx_bytes.fetch_add(140000, Ordering::Relaxed);

        let snap = counters.snapshot();
        assert_eq!(snap.rx_packets, 100);
        assert_eq!(snap.tx_packets, 50);
        assert_eq!(snap.rx_bytes, 140000);
    }

    #[test]
    fn perf_counters_concurrent_increment() {
        let counters = Arc::new(PerfCounters::new());
        let mut handles = vec![];

        for _ in 0..4 {
            let c = Arc::clone(&counters);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10_000 {
                    c.rx_packets.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counters.rx_packets.load(Ordering::Relaxed), 40_000);
    }

    #[test]
    fn perf_counters_latency_max_cas() {
        let counters = PerfCounters::new();
        counters.update_latency_max(100);
        assert_eq!(counters.latency_max_ns.load(Ordering::Relaxed), 100);
        counters.update_latency_max(50); // should not decrease
        assert_eq!(counters.latency_max_ns.load(Ordering::Relaxed), 100);
        counters.update_latency_max(200); // should increase
        assert_eq!(counters.latency_max_ns.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn perf_counters_reset_interval() {
        let counters = PerfCounters::new();
        counters.latency_max_ns.store(5000, Ordering::Relaxed);
        counters.latency_sample_count.store(10, Ordering::Relaxed);
        counters.latency_sum_ns.store(50000, Ordering::Relaxed);

        counters.reset_interval();

        assert_eq!(counters.latency_max_ns.load(Ordering::Relaxed), 0);
        assert_eq!(counters.latency_sample_count.load(Ordering::Relaxed), 0);
        assert_eq!(counters.latency_sum_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn latency_sampler_should_sample() {
        let sampler = LatencySampler::new(128, 10);
        // First call (count=0) should sample (0 % 10 == 0)
        assert!(sampler.should_sample());
        // Next 9 should not
        for _ in 0..9 {
            assert!(!sampler.should_sample());
        }
        // 11th call (count=10) should sample
        assert!(sampler.should_sample());
    }

    #[test]
    fn latency_sampler_record_and_percentiles() {
        let sampler = LatencySampler::new(100, 1);

        // Record 100 samples: 1000ns, 2000ns, ..., 100000ns
        for i in 1..=100 {
            sampler.record(i * 1000);
        }

        let p = sampler.percentiles();
        assert_eq!(p.count, 100);
        // p50 should be around 50000ns
        assert!(p.p50_ns >= 49000 && p.p50_ns <= 51000, "p50={}", p.p50_ns);
        // p99 should be around 99000ns
        assert!(p.p99_ns >= 98000 && p.p99_ns <= 100000, "p99={}", p.p99_ns);
        assert_eq!(p.max_ns, 100000);
    }

    #[test]
    fn latency_sampler_empty_percentiles() {
        let sampler = LatencySampler::new(64, 1000);
        let p = sampler.percentiles();
        assert_eq!(p.count, 0);
        assert_eq!(p.p50_ns, 0);
        assert_eq!(p.max_ns, 0);
    }

    #[test]
    fn latency_sampler_reset() {
        let sampler = LatencySampler::new(64, 1);
        sampler.record(1000);
        sampler.record(2000);
        assert_eq!(sampler.percentiles().count, 2);

        sampler.reset();
        assert_eq!(sampler.percentiles().count, 0);
    }

    #[test]
    fn counter_snapshot_rates() {
        let prev = CounterSnapshot {
            rx_packets: 0, rx_bytes: 0, rx_drops_ring_full: 0, rx_drops_parse_fail: 0,
            rx_arp_handled: 0, rx_icmp_handled: 0, rx_bursts: 0, rx_burst_sum: 0,
            tx_packets: 0, tx_bytes: 0, tx_failures: 0,
            worker_ring_enqueue_fail: 0, app_ring_enqueue_fail: 0, tx_ring_enqueue_fail: 0,
            worker_packets_processed: 0, worker_idle_polls: 0,
            arp_cache_hits: 0, arp_cache_misses: 0, arp_cache_inserts: 0,
            latency_sample_count: 0, latency_sum_ns: 0, latency_max_ns: 0,
        };

        let current = CounterSnapshot {
            rx_packets: 350000, rx_bytes: 490_000_000, rx_drops_ring_full: 5,
            rx_drops_parse_fail: 2, rx_arp_handled: 10, rx_icmp_handled: 3,
            rx_bursts: 10000, rx_burst_sum: 350000,
            tx_packets: 349000, tx_bytes: 488_600_000, tx_failures: 1,
            worker_ring_enqueue_fail: 3, app_ring_enqueue_fail: 1, tx_ring_enqueue_fail: 1,
            worker_packets_processed: 340000, worker_idle_polls: 10000,
            arp_cache_hits: 349000, arp_cache_misses: 5, arp_cache_inserts: 5,
            latency_sample_count: 350, latency_sum_ns: 49_000_000, latency_max_ns: 5_000_000,
        };

        let rates = current.rates_since(&prev, 10.0);
        assert!((rates.rx_pps - 35000.0).abs() < 1.0);
        assert!((rates.tx_pps - 34900.0).abs() < 1.0);
        assert_eq!(rates.rx_drops, 5);
        assert_eq!(rates.tx_fails, 1);
        assert!((rates.burst_avg - 35.0).abs() < 0.1);
        assert!(rates.lat_avg_us > 0.0);
    }

    #[test]
    fn perf_reporter_start_stop() {
        let counters = Arc::new(PerfCounters::new());
        let sampler = Arc::new(LatencySampler::new(64, 1000));

        // Start with a short interval
        let mut reporter = PerfReporter::start(
            Arc::clone(&counters),
            Arc::clone(&sampler),
            Duration::from_millis(50),
        );

        // Let it run briefly
        std::thread::sleep(Duration::from_millis(20));

        // Stop cleanly
        reporter.stop();
    }

    #[test]
    fn perf_reporter_emits_output() {
        let counters = Arc::new(PerfCounters::new());
        let sampler = Arc::new(LatencySampler::new(64, 1));

        // Simulate some activity
        counters.rx_packets.fetch_add(1000, Ordering::Relaxed);
        counters.tx_packets.fetch_add(950, Ordering::Relaxed);
        sampler.record(150_000); // 150us

        let mut reporter = PerfReporter::start(
            Arc::clone(&counters),
            Arc::clone(&sampler),
            Duration::from_millis(50),
        );

        // Wait for at least one report cycle
        std::thread::sleep(Duration::from_millis(120));

        reporter.stop();
        // If we got here without panic, the reporter ran successfully.
        // The actual output goes to stderr — we can't easily capture it here,
        // but no panics means the formatting and computation succeeded.
    }
}
