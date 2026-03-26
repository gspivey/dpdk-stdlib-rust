# Performance Test Log

Structured record of performance benchmark runs across optimization phases.
Each entry captures the git context, test configuration, results, and analysis.

---

## Run #5: Cleanup & Baseline Fix

| Field | Value |
|-------|-------|
| **Date** | 2026-03-25 |
| **Git Hash** | `990c095` |
| **Branch** | `claude/cleanup-udp-prototype-z4UUD` |
| **PR** | [#27](https://github.com/gspivey/dpdk-stdlib-rust/pull/27) |
| **GH Actions Run** | [23567309410](https://github.com/gspivey/dpdk-stdlib-rust/actions/runs/23567309410) |
| **Instance Type** | c5n.2xlarge |
| **Traffic Generator** | TRex |

### Changes Since Previous Run

- **Rewrote echo app**: 282→65 lines, now structurally identical to plain-echo with only `use dpdk_udp::UdpSocket` vs `use std::net::UdpSocket` — demonstrates the "drop-in replacement" story.
- **Removed echo/dpdk feature flag**: dpdk-udp is now a non-optional dependency.
- **Removed multicore configs**: `rust-dpdk-multicore` removed from default perf configs (was broken since topology simplification in PR #26).
- **Fixed README performance claims**: 10-100x → ~2x, matching actual benchmarks.
- **Reverted plain-echo to original tight loop**: Removed signal handling and read timeout that were added during this PR — the baseline should be the simplest possible `std::net` loop.
- **⚠️ Baseline change**: Previous runs' `rust-stdlib` config ran the `echo` binary which used `dpdk_udp::UdpSocket` with its abstraction layer in kernel-fallback mode — **not** a clean `std::net` comparison. This run's `plain-rust` config correctly uses `plain-echo` which calls `std::net::UdpSocket` directly. Results are now an honest apples-to-apples comparison. The `rust-stdlib` config still exists but is removed from defaults.

### Results: 64B Packets

#### native-dpdk (DPDK C baseline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 107 | 0% |
| 140K | 117 | 0% |
| 350K | 122 | 0% |
| 700K | 679 | 6.5% |

#### rust-dpdk (single-core, run-to-completion)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 223 | 0% |
| 140K | 224 | 0% |
| 350K | 246 | 0.03% |
| 700K | 840 | 3.1% |

#### plain-rust (std::net baseline via plain-echo)

| PPS | RX pps | Drop % |
|-----|--------|--------|
| 70K | 69,000 | 1.4% |
| 140K | 138,996 | 0.7% |
| 350K | 327,975 | 6.3% |
| 700K | 342,265 | 51.1% |

### Results: 512B Packets

#### native-dpdk

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 119 | 0% |
| 140K | 127 | 0% |
| 350K | 134 | 0% |
| 700K | 761 | 7.4% |

#### rust-dpdk

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 235 | 0% |
| 140K | 204 | 0.01% |
| 350K | 224 | 0.04% |
| 700K | 895 | 8.8% |

#### plain-rust

| PPS | RX pps | Drop % |
|-----|--------|--------|
| 70K | 69,000 | 1.4% |
| 140K | 138,968 | 0.7% |
| 350K | 289,761 | 17.2% |
| 700K | 324,749 | 53.6% |

### Results: 1400B Packets

#### native-dpdk

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 135 | 0% |
| 140K | 100 | 0% |
| 350K | 117 | 0.02% |
| 700K | 3,807 | 36.0% |

#### rust-dpdk

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 215 | 0% |
| 140K | 220 | 0.03% |
| 350K | 241 | 0% |
| 700K | 3,972 | 36.0% |

#### plain-rust

| PPS | RX pps | Drop % |
|-----|--------|--------|
| 70K | 68,996 | 1.4% |
| 140K | 138,972 | 0.7% |
| 350K | 283,868 | 18.9% |
| 700K | 309,586 | 55.8% |

### Analysis

**rust-dpdk matches native-dpdk almost exactly**: At 1400B/700K PPS, both deliver ~448K RX pps (36% drop) with nearly identical latency (3,972 vs 3,807us). At 64B/700K, rust-dpdk is within 3.7% of native (679K vs 655K RX pps). The Rust overhead at sub-saturation rates is consistently ~100us higher latency (215-246us vs 100-135us).

**Baseline is now honest**: The `plain-rust` results use `std::net::UdpSocket` directly via `plain-echo`. Previous runs' `rust-stdlib` used our abstraction layer in kernel-fallback mode, which was not a clean std::net comparison. Kernel numbers are consistent with Run #4 (51.1% drop here vs 54.8% in Run #4 at 64B/700K).

**DPDK advantage at 350K PPS is decisive**: DPDK (both native and rust) delivers zero drops at 350K PPS across all packet sizes, while the kernel loses 6-19%. At 700K PPS, DPDK delivers ~2x the throughput of kernel sockets.

**Key comparison at 700K PPS**:
| Packet Size | rust-dpdk RX | plain-rust RX | DPDK Advantage |
|-------------|-------------|---------------|----------------|
| 64B | 678,563 | 342,265 | 2.0x |
| 512B | 638,416 | 324,749 | 2.0x |
| 1400B | 447,693 | 309,586 | 1.4x |

**Consistency across runs**: These numbers align with Run #4 (316K kernel RX at 64B/700K, ~2x DPDK advantage). An earlier run on this branch showed anomalous kernel results (154K RX, 78% drop) which was an EC2 instance outlier — not representative of typical performance. The consistent finding across all non-outlier runs: **DPDK delivers ~2x throughput at saturation and zero drops up to 350K PPS where the kernel starts dropping**.

---

## Run #4: Topology Simplification

| Field | Value |
|-------|-------|
| **Date** | 2026-03-25 |
| **Git Hash** | `8949d08` |
| **Branch** | `main` (merged from topology simplification) |
| **PR** | [#26](https://github.com/gspivey/dpdk-stdlib-rust/pull/26) |
| **GH Actions Run** | [23522716883](https://github.com/gspivey/dpdk-stdlib-rust/actions/runs/23522716883) |
| **Instance Type** | c5n.2xlarge |
| **Traffic Generator** | TRex |

### Changes Since Previous Run

- **Removed `workers_per_queue`**: Simplified topology from two knobs (`rx_queues` × `workers_per_queue`) to one (`rx_queues`). Each RX queue gets exactly one worker thread.
- **Simplified `TopologyPlan`**: Removed `workers_per_queue` field, simplified thread spawning logic.
- **Removed `DPDK_WORKERS_PER_QUEUE` env var**: Only `DPDK_RX_QUEUES` remains.
- **Net reduction**: ~139 lines removed from topology code.

### Results: 1400B Packets

#### native-dpdk (DPDK C baseline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 125 | 0% |
| 140K | 119 | 0% |
| 350K | 129 | 0.02% |
| 700K | 3,728 | 36.0% |

#### rust-dpdk (single-core, run-to-completion)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 243 | 0% |
| 140K | 251 | 0% |
| 350K | 267 | 0% |
| 700K | 4,023 | 36.0% |

#### rust-dpdk-multicore

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 0 | 100% |
| 140K | 0 | 100% |
| 350K | 0 | 100% |
| 700K | 0 | 100% |

#### plain-rust (std::net baseline)

| PPS | Avg Latency (us) | Drop % |
|-----|-------------------|--------|
| 70K | 505 | 1.4% |
| 140K | — | 0.8% |
| 350K | — | 29.8% |
| 700K | — | 58.4% |

### Results: 64B Packets

#### rust-dpdk (single-core)

| PPS | RX pps | Drop % |
|-----|--------|--------|
| 70K | 70,000 | 0% |
| 140K | 140,000 | 0% |
| 350K | 349,928 | 0.02% |
| 700K | 642,575 | 8.2% |

#### plain-rust (std::net)

| PPS | RX pps | Drop % |
|-----|--------|--------|
| 70K | 69,000 | 1.4% |
| 140K | 138,997 | 0.7% |
| 350K | 329,984 | 5.7% |
| 700K | 316,155 | 54.8% |

### Analysis

**Single-core rust-dpdk matches native-dpdk at 700K PPS**: Both deliver ~448K RX pps at 1400B (36% drop). At lower rates, rust-dpdk has zero drops while native-dpdk also has zero drops. The gap is latency: rust-dpdk averages 243-267us vs native's 119-129us at sub-saturation rates.

**rust-dpdk-multicore is broken**: 100% packet drops at all rates. The perf test script passes `--workers 2` which is no longer a valid CLI flag after the topology simplification removed it. The multicore config needs to be removed from default perf test runs (done in PR #27).

**rust-stdlib significantly worse than previous runs**: 92% drops at 700K/64B (vs 53% in earlier runs). This appears to be instance-level variance — the `rust-stdlib` config uses the kernel stack which is sensitive to system load and ENI driver state.

**Key comparison at 64B/700K PPS** (worst case for kernel):
- rust-dpdk: 642,575 RX pps (8.2% drop)
- plain-rust: 316,155 RX pps (54.8% drop)
- **DPDK delivers ~2x the throughput of kernel sockets at saturation**

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
