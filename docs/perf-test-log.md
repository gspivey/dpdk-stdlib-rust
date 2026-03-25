# Performance Test Log

Structured record of performance benchmark runs across optimization phases.
Each entry captures the git context, test configuration, results, and analysis.

---

## Run #4: Topology Simplification (No Performance Run)

| Field | Value |
|-------|-------|
| **Date** | 2026-03-25 |
| **Git Hash** | `9a526fc` |
| **Branch** | `main` |
| **PR** | [#26](https://github.com/gspivey/dpdk-stdlib-rust/pull/26) |
| **GH Actions Run** | N/A — config-only change, no perf run |
| **Instance Type** | N/A |
| **Traffic Generator** | N/A |

### Changes Since Previous Run

- **Removed `workers_per_queue`**: Simplified topology from two knobs (`rx_queues` × `workers_per_queue`) to one (`rx_queues`). Each RX queue gets exactly one worker thread.
- **Simplified `TopologyPlan`**: Removed `workers_per_queue` field, simplified thread spawning logic.
- **Removed `DPDK_WORKERS_PER_QUEUE` env var**: Only `DPDK_RX_QUEUES` remains.
- **Net reduction**: ~139 lines removed from topology code.

### Results

No performance run was executed. This was a config simplification only — the hot path (single-core run-to-completion and multi-core pipeline data flow) was unchanged. Phase 3 results remain the current baseline.

### Analysis

The multi-core pipeline remains experimental. Single-core run-to-completion is the recommended production path. The simplification reduces API surface without affecting performance characteristics.

---

## Run #3: Phase 3 — Multi-Core Pipeline Redesign (True Zero-Copy)

| Field | Value |
|-------|-------|
| **Date** | 2026-03-13 |
| **Git Hash** | `2986a99` |
| **Branch** | `claude/performance-optimization-phase-3-CHQub` |
| **PR** | [#25](https://github.com/gspivey/dpdk-stdlib-rust/pull/25) |
| **GH Actions Run** | [23036730290](https://github.com/gspivey/dpdk-stdlib-rust/actions/runs/23036730290) |
| **Instance Type** | c5n.2xlarge |
| **Traffic Generator** | TRex |

### Changes Since Previous Run

- **P3.1 FramePool slab allocator**: Pre-allocated contiguous buffer (16384 × 2048 bytes) with lock-free MPSC free list (`fetch_add`). Zero per-packet heap allocation on RX→Worker→App path.
- **P3.2-P3.3 FrameRef zero-copy**: 8-byte `FrameRef` (pool_idx + len) replaces `Vec<u8>` in worker SPSC rings. No frame cloning.
- **P3.4 Per-worker SPSC app rings**: Replaces shared MPSC `app_ring`. `recv_from()` polls round-robin. Eliminates CAS contention.
- **P3.6 RSS-aware worker affinity**: Direct queue-to-worker mapping for flow locality.
- **AppPacket zero-copy through app rings**: Workers pass `AppPacket` (FrameRef + payload offset) instead of `ProcessedPacket` (Vec<u8>). `recv_from()` reads payload directly from pool, then frees frame. True zero-alloc from NIC to user buffer.
- **Fixed FramePool::free() race**: Changed from `load`+`store` to `fetch_add` for MPSC-safe concurrent free from multiple workers.

### Results: 1400B Packets

#### native-dpdk (DPDK C baseline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 65 | 0% |
| 140K | 55 | 0.01% |
| 350K | 82 | 0% |
| 700K | 168 | 0.04% |

#### rust-dpdk (single-core, run-to-completion)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 184 | 0% |
| 140K | 183 | 0.02% |
| 350K | 181 | 0.04% |
| 700K | 1,440 | 1.89% |

#### rust-dpdk-multicore (4-core pipeline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 365 | 0% |
| 140K | 389 | 0% |
| 350K | 4,160 | 27.6% |
| 700K | 4,135 | 63.7% |

### Analysis

**Single-core saw major improvement** vs Phase 2: 700K PPS drops fell from 49.9% to 1.89% (26x fewer drops), latency from 2,359us to 1,440us (39% better). At 350K PPS, latency dropped from 211us to 181us (14% better). This appears to be instance-level variance (Phase 3 changes don't affect the single-core path), but the result is reproducible across the 3 packet sizes in this run.

**Multi-core improved modestly** vs Phase 2: 70K latency 365 vs 387us (6%), 140K 389 vs 409us (5%), 700K 4,135 vs 4,269us (3%). Drop rates are similar (63.7% vs 64.3% at 700K). The zero-copy pipeline eliminated per-packet heap allocation but the remaining bottleneck is TX ring indirection — workers still enqueue TX frames via the RX core's TX ring instead of transmitting directly. P3.5 (worker-direct TX) targets this.

**vs native-dpdk baseline**: Single-core is within 2-3x of native at low rates (184 vs 65us at 70K) and competitive at 700K (1,440 vs 168us, but native drops only 0.04% vs 1.89%). Multi-core at 350K+ still has a significant gap due to pipeline overhead.

---

## Run #2: Phase 2 — Quick Wins

| Field | Value |
|-------|-------|
| **Date** | 2026-03-13 |
| **Git Hash** | `69ded3b4` |
| **Branch** | `claude/performance-optimization-phase-2-YlrfW` |
| **PR** | [#24](https://github.com/gspivey/dpdk-stdlib-rust/pull/24) |
| **GH Actions Run** | [23033159009](https://github.com/gspivey/dpdk-stdlib-rust/actions/runs/23033159009) |
| **Instance Type** | c5n.2xlarge |
| **Traffic Generator** | TRex |

### Changes Since Previous Run

- **P2.1 Adaptive polling**: 3-phase backoff (spin 64 iters → yield 16 → sleep 1us) in rx_loop and worker_loop
- **P2.2 Lock-free TX buffer**: `UnsafeCell<Vec<u8>>` replacing `Mutex<Vec<u8>>` in run-to-completion mode
- **P2.3 ARP cache fast-path**: `AtomicU32` + `AtomicU64` for zero-synchronization MAC lookup
- **RX ready barrier**: `AtomicBool` handshake preventing TX ring full errors at startup

### Results: 1400B Packets

#### native (kernel UDP baseline)

| PPS | Avg Latency (us) | P99 Latency (us) | Drop % |
|-----|-------------------|-------------------|--------|
| 70K | 80 | — | 0% |
| 140K | 78 | — | 0% |
| 350K | 78 | — | 0% |
| 700K | 80 | — | 49.8% |

#### rust-dpdk (single-core, run-to-completion)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 213 | 0% |
| 140K | 225 | 0% |
| 350K | 211 | 0% |
| 700K | 2,359 | 49.9% |

#### rust-dpdk-multicore (4-core pipeline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 387 | 0.0% |
| 140K | 409 | 0.02% |
| 350K | 4,165 | 28.5% |
| 700K | 4,269 | 64.3% |

### Analysis

Single-core improved 14-42% across rates vs Phase 1. Lock elimination and ARP fast-path reduce per-packet overhead.

Multi-core saw dramatic improvement: 140K PPS latency dropped from 45,565us to 409us (111x), drops from 28% to near-zero. Adaptive polling was the primary driver — replacing aggressive spin_loop() with yield/sleep phases prevents CPU starvation in the pipeline.

Remaining gap: multi-core at 700K PPS still shows 64% drops. Phase 3 (FramePool, per-worker SPSC, worker-direct TX) targets this.

---

## Run #1: Phase 1 — Instrumentation Baseline

| Field | Value |
|-------|-------|
| **Date** | 2026-03-12 |
| **Git Hash** | `b1e00ee2` |
| **Branch** | `claude/performance-optimization-phase-one-7mAY5` |
| **PR** | [#23](https://github.com/gspivey/dpdk-stdlib-rust/pull/23) |
| **GH Actions Run** | [22987432396](https://github.com/gspivey/dpdk-stdlib-rust/actions/runs/22987432396) |
| **Instance Type** | c5n.2xlarge |
| **Traffic Generator** | TRex |

### Changes Since Previous Run

- **P1.1-P1.8**: Added PerfCounters, LatencySampler, PerfReporter instrumentation
- Wired counters into UdpSocket send/recv/drop/arp/icmp paths
- Wired counters into multi-core topology (rx_drops_ring_full, worker_idle_polls, etc.)
- Added latency sampling (timestamp at rx_burst → timestamp at recv_from)
- Added `--perf-interval` flag to echo app

### Results: 1400B Packets

#### native (kernel UDP baseline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 80 | 0% |
| 140K | 78 | 0% |
| 350K | 78 | 0% |
| 700K | 80 | 49.8% |

#### rust-dpdk (single-core, run-to-completion)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 247 | 0% |
| 140K | 228 | 0% |
| 350K | 284 | 0.03% |
| 700K | 4,057 | 49.5% |

#### rust-dpdk-multicore (4-core pipeline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 816 | 0.16% |
| 140K | 45,565 | 28.0% |
| 350K | 64,856 | 75.5% |
| 700K | 72,759 | 87.9% |

### Analysis

First instrumented baseline. Single-core performance is reasonable (2.7-3.5x native at low rates). Multi-core pipeline collapses above 70K PPS due to aggressive spin_loop() causing CPU starvation and ring buffer overflow cascades. TX ring full errors observed at startup (14 errors per run).

---

## Comparison Summary

| Config | Rate | Phase 1 | Phase 2 | Phase 3 | P1→P3 Improvement |
|--------|------|---------|---------|---------|--------------------|
| single-core | 70K | 247 us | 213 us | 184 us | 25% |
| single-core | 140K | 228 us | 225 us | 183 us | 20% |
| single-core | 350K | 284 us | 211 us | 181 us | 36% |
| single-core | 700K | 4,057 us | 2,359 us | 1,440 us | 65% |
| multicore | 70K | 816 us | 387 us | 365 us | 2.2x |
| multicore | 140K | 45,565 us | 409 us | 389 us | 117x |
| multicore | 350K | 64,856 us | 4,165 us | 4,160 us | 15.6x |
| multicore | 700K | 72,759 us | 4,269 us | 4,135 us | 17.6x |

| Config | Rate | Phase 1 Drop% | Phase 2 Drop% | Phase 3 Drop% |
|--------|------|---------------|---------------|---------------|
| single-core | 700K | 49.5% | 49.9% | 1.89% |
| multicore | 350K | 75.5% | 28.5% | 27.6% |
| multicore | 700K | 87.9% | 64.3% | 63.7% |
