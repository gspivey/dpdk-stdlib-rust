# Design: Performance Optimization & Instrumentation

## Overview

This design addresses the performance gaps identified in benchmarking on c5n.2xlarge. The work
is organized into three tracks: **single-core hot path optimization**, **multi-core pipeline
redesign**, and **always-on instrumentation**. Each track is independently shippable.

## Architecture: Instrumentation

### PerfCounters (Lock-Free Hot-Path Counters)

```rust
// dpdk-udp/src/perf.rs

/// Per-socket performance counters. All fields are AtomicU64 for lock-free
/// increment on the hot path. The reporting thread reads them periodically.
#[repr(align(64))]  // Cache-line aligned to prevent false sharing
pub struct PerfCounters {
    // RX path
    pub rx_packets: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub rx_drops_ring_full: AtomicU64,      // frames dropped: worker ring full
    pub rx_drops_parse_fail: AtomicU64,     // frames that failed UDP parse
    pub rx_arp_handled: AtomicU64,
    pub rx_icmp_handled: AtomicU64,
    pub rx_bursts: AtomicU64,               // number of rx_burst calls
    pub rx_burst_sum: AtomicU64,            // sum of burst sizes (for avg)

    // TX path
    pub tx_packets: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub tx_failures: AtomicU64,

    // Ring utilization (multi-core only)
    pub worker_ring_enqueue_fail: AtomicU64, // total enqueue failures across workers
    pub app_ring_enqueue_fail: AtomicU64,
    pub tx_ring_enqueue_fail: AtomicU64,

    // Worker (multi-core only)
    pub worker_packets_processed: AtomicU64,
    pub worker_idle_polls: AtomicU64,       // polls that returned empty

    // ARP cache
    pub arp_cache_hits: AtomicU64,
    pub arp_cache_misses: AtomicU64,
    pub arp_cache_inserts: AtomicU64,

    // Latency sampling
    pub latency_sample_count: AtomicU64,
    pub latency_sum_ns: AtomicU64,          // sum of sampled latencies
    pub latency_max_ns: AtomicU64,          // max in current interval
}
```

**Design rationale**: Each counter is a simple `fetch_add(1, Relaxed)` — the cheapest atomic
operation (single cache-line bounce on x86, no memory fence). Relaxed ordering is fine because
we only need approximate counts for reporting, not precise happens-before.

### Latency Sampling

```rust
/// Lightweight latency sampler using reservoir sampling.
/// Only samples 1 in N packets to keep overhead < 1%.
pub struct LatencySampler {
    /// Ring buffer of recent latency samples (fixed capacity, e.g., 1024)
    samples: Box<[AtomicU64]>,
    write_idx: AtomicU64,
    /// Sample every Nth packet (e.g., 1000)
    sample_rate: u64,
    /// Counter to determine when to sample
    packet_count: AtomicU64,
}
```

**Sampling strategy**: On every Nth packet (`sample_rate`), call `Instant::now()` at the
rx_burst return point and at the recv_from() return point. Store the delta in nanoseconds.
At reporting time, sort the samples buffer and compute percentiles.

**Cost**: `Instant::now()` is ~20ns on Linux (vDSO). At 1-in-1000 sampling and 350K PPS,
that's 350 clock reads/sec — negligible.

### PerfReporter (Background Thread)

```rust
/// Background thread that reads PerfCounters every N seconds and emits
/// structured log lines.
pub struct PerfReporter {
    counters: Arc<PerfCounters>,
    sampler: Arc<LatencySampler>,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
```

**Output format** (one line per interval, key=value for easy parsing):

```
[PERF] interval=10s rx_pps=349823 rx_bps=3918018400 tx_pps=349801 tx_bps=3917772000 \
       rx_drops=0 tx_fails=0 lat_avg_us=142 lat_p50_us=89 lat_p95_us=312 lat_p99_us=1204 \
       lat_max_us=2891 arp_hits=349823 arp_misses=0 ring_drops=0 worker_idle_pct=12.3 \
       burst_avg=28.4
```

The reporter computes rates by diffing counter snapshots between intervals.

### API Surface

```rust
impl UdpSocket {
    /// Access live performance counters. Always available, zero-cost if not read.
    pub fn perf_counters(&self) -> &PerfCounters { ... }

    /// Start background performance reporting to stderr.
    /// Interval: how often to emit a log line (default: 10s).
    pub fn enable_perf_reporting(&self, interval: Duration) -> io::Result<()> { ... }

    /// Stop background reporting.
    pub fn disable_perf_reporting(&self) { ... }

    /// Get a snapshot of current performance statistics.
    pub fn perf_snapshot(&self) -> PerfSnapshot { ... }
}

/// Point-in-time performance snapshot for programmatic access.
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
```

## Architecture: Single-Core Optimizations

### Mutex Elimination on Hot Path

Current `send_to()` acquires `self.tx_buf.lock()` (Mutex) on every packet. The recv path
acquires `self.connected_addr.lock()` and `self.connection_state.write()`.

**Fix**: Replace `Mutex<Vec<u8>>` tx_buf with thread-local or `UnsafeCell` (single-thread
guarantee in run-to-completion mode). For connected_addr, use `AtomicU64` encoding
(4 bytes IP + 2 bytes port + 2 bytes flags) or read-copy-update.

### ARP Cache Lock Elimination

`ArpCache` currently uses `DashMap` or similar concurrent map. For the common case
(cache hit on known peer), this still involves atomic operations.

**Fix**: Add a "last-seen" fast path: cache the most recent (IP, MAC) pair in an
atomic-width value. If the lookup IP matches, return immediately. This covers the
echo-server pattern (single peer) with zero synchronization.

### Checksum Offload

If the NIC reports TX checksum offload capability (`DEV_TX_OFFLOAD_IPV4_CKSUM`,
`DEV_TX_OFFLOAD_UDP_CKSUM`), skip software checksum computation and set the
appropriate ol_flags on the mbuf.

**Expected savings**: ~50ns per packet (IPv4 checksum) + ~100ns per packet (UDP checksum).

## Architecture: Multi-Core Pipeline Redesign

### Problem Analysis

The current multi-core architecture has these bottlenecks (ordered by severity):

1. **Heap allocation per frame** (most severe): `recv_fn(32)` returns `Vec<Vec<u8>>`.
   Each frame is a separate heap allocation. Then `frame_data.clone()` copies it into
   the worker ring. That's 2 allocations + 1 copy per frame.

2. **MPSC CAS contention**: All workers CAS on the shared `app_ring.head`. Under load,
   this causes cache-line bouncing across cores.

3. **TX ring indirection**: Echo responses must traverse: Worker → TX ring → RX lcore → NIC.
   This adds a full ring hop (~500ns) plus cross-core latency.

4. **Spin-loop without backoff**: Workers burning CPU on empty rings steal cycles from the
   RX lcore on a 2-vCPU machine.

5. **Round-robin ignores RSS**: RSS hashes flows to queues, but round-robin distribution
   to workers destroys locality — the same flow's packets go to different workers.

### Fix 1: Zero-Copy Frame Passing via Slab Pool

Replace `Vec<Vec<u8>>` with a pre-allocated slab of frame buffers:

```rust
/// Pre-allocated pool of fixed-size frame buffers.
/// Frames are passed by index through the ring, not by value.
pub struct FramePool {
    /// Contiguous allocation: N frames × MTU_MAX bytes
    buffer: Box<[u8]>,
    frame_size: usize,  // 1514 (max Ethernet frame) or 2048 (power of 2)
    capacity: usize,
    /// Free list: SPSC ring of available frame indices
    free_list: SpscRing<u32>,
}

impl FramePool {
    /// Get a mutable reference to frame slot N
    fn frame_mut(&mut self, idx: u32) -> &mut [u8] { ... }

    /// Allocate a frame index from the pool
    fn alloc(&self) -> Option<u32> { self.free_list.dequeue() }

    /// Return a frame index to the pool
    fn free(&self, idx: u32) { let _ = self.free_list.enqueue(idx); }
}
```

The SPSC ring between RX and Worker now carries `FrameRef` (index + length) instead
of `Vec<u8>`:

```rust
#[derive(Clone, Copy)]
struct FrameRef {
    pool_idx: u32,   // index into FramePool
    len: u16,        // actual frame length
}
```

**Benefit**: Zero heap allocation after initialization. Frames are copied once (NIC DMA → pool
slot), passed by 6-byte reference through rings, and freed back to pool after processing.

### Fix 2: Per-Worker SPSC App Rings (Eliminate MPSC)

Replace the single MPSC `app_ring` with N per-worker SPSC rings. `recv_from()` polls
them round-robin:

```
Worker 0 ──► SPSC app_ring_0 ──┐
Worker 1 ──► SPSC app_ring_1 ──┤ recv_from() polls round-robin
Worker 2 ──► SPSC app_ring_2 ──┘
```

**Benefit**: Eliminates all CAS contention. Each ring is SPSC (cheapest possible
synchronization). `recv_from()` cycles through rings checking for data.

### Fix 3: Worker-Direct TX (Multi-Queue TX)

If the NIC supports multiple TX queues, assign each worker its own TX queue.
Workers transmit directly instead of bouncing through the RX lcore:

```
Before: Worker → TX ring → RX lcore → NIC TX Q0
After:  Worker → NIC TX Q[worker_id]  (direct, no ring hop)
```

**Benefit**: Eliminates ~500ns ring hop for TX. Halves echo latency on the multi-core path.

**Fallback**: If the NIC has fewer TX queues than workers, workers sharing a queue
use a Mutex<TxQueue> or funnel through a MPSC ring to a dedicated TX thread.

### Fix 4: Adaptive Polling with Backoff

Replace bare `spin_loop()` with a three-phase strategy:

```rust
const SPIN_ITERS: u32 = 64;
const YIELD_ITERS: u32 = 16;
const SLEEP_US: u64 = 1;

fn adaptive_wait(empty_polls: &mut u32) {
    *empty_polls += 1;
    if *empty_polls < SPIN_ITERS {
        std::hint::spin_loop();
    } else if *empty_polls < SPIN_ITERS + YIELD_ITERS {
        std::thread::yield_now();
    } else {
        std::thread::sleep(Duration::from_micros(SLEEP_US));
    }
}
// Reset empty_polls to 0 when work is found
```

**Benefit**: Workers don't steal CPU from RX lcore on small instances. Latency stays
low because the first 64 polls are spin-loops (~3 us total).

### Fix 5: RSS-Aware Worker Affinity

Instead of round-robin across all workers, pin each RSS queue to a specific worker
(or worker set). Packets from the same flow (same RSS hash) always go to the same
worker, preserving cache locality:

```
RSS Q0 → Worker 0 (1:1 mapping)
RSS Q1 → Worker 1
```

This is already the intended design in the spec but the current implementation uses
round-robin, which breaks RSS locality.

## Instrumentation Integration Points

### Where counters are incremented (hot path)

| Counter | Location | When |
|---------|----------|------|
| rx_packets | `process_frame_zerocopy` | After successful UDP parse |
| rx_bytes | `process_frame_zerocopy` | After successful UDP parse |
| rx_drops_ring_full | `rx_loop` | When all worker rings are full |
| tx_packets | `send_to_addr` | After successful `send_frame` |
| tx_bytes | `send_to_addr` | After successful `send_frame` |
| tx_failures | `send_to_addr` | On `send_frame` error |
| rx_bursts | `recv_from_internal` / `rx_loop` | After each `rx_burst` call |
| rx_burst_sum | `recv_from_internal` / `rx_loop` | Add burst size |
| worker_idle_polls | `worker_loop` | When `dequeue_batch` returns empty |
| arp_cache_hits | `ArpCache::get` | On cache hit |
| arp_cache_misses | `ArpCache::get` | On cache miss |

### Echo app integration

```rust
// apps/echo/src/main.rs

// Enable reporting at startup
socket.enable_perf_reporting(Duration::from_secs(10))?;

// Or with --perf-interval flag
#[arg(long, default_value = "10")]
perf_interval: u64,
```

Output appears on stderr every N seconds:
```
[PERF] interval=10s rx_pps=349823 tx_pps=349801 lat_avg_us=142 lat_p99_us=1204 ...
```

## Implementation Order

The work is designed so each piece is independently testable:

1. **Instrumentation first** (gives visibility into subsequent optimizations)
2. **Adaptive polling** (easy win, fixes 2-vCPU contention)
3. **Single-core mutex elimination** (improves baseline)
4. **Slab pool + zero-copy rings** (biggest multi-core win)
5. **Per-worker SPSC app rings** (eliminates MPSC contention)
6. **Worker-direct TX** (halves echo latency)
7. **Checksum offload** (small but measurable)
8. **Regression gates** (CI enforcement)
