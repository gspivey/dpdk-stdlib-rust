# Requirements: Performance Optimization & Instrumentation

## Problem Statement

Performance benchmarks on c5n.2xlarge reveal critical gaps between our Rust DPDK implementation
and native DPDK (testpmd). The multi-core pipeline path is particularly degraded.

### Current Benchmark Data (1400B packets, c5n.2xlarge)

| Config              | 70K PPS     | 140K PPS    | 350K PPS           | 700K PPS           |
|---------------------|-------------|-------------|--------------------|--------------------|
| native-dpdk         | 80 us, 0%   | 73 us, 0%   | 80 us, 0%          | 999 us, 17.8%      |
| rust-dpdk (1-core)  | 154 us, 0%  | 157 us, 0%  | 239 us, 0%         | 1822 us, 21.9%     |
| rust-dpdk-multicore | 784 us, 0%  | 1380 us, 26%| **59,380 us, 75.8%**| **75,503 us, 87.3%** |
| rust-stdlib         | ~0 us, 1.5% | ~0 us, 0.7% | 461 us, 31%        | ~0 us, 65.2%       |

### Performance Gaps

1. **Single-core rust-dpdk vs native-dpdk**: ~2x latency overhead (154 us vs 80 us at 70K PPS)
2. **Multi-core pipeline**: ~10x worse than single-core at 70K, completely collapses above 140K PPS
3. **Multi-core max latency**: 175 ms — unacceptable for any real-time workload

## Requirements

### R1: Eliminate Multi-Core Pipeline Heap Allocations

The RX loop currently calls `recv_fn(32)` which returns `Vec<Vec<u8>>` — one heap allocation
per frame per batch. The frame is then `.clone()`d into each worker ring.

**Must**: Eliminate per-packet heap allocation on the RX→Worker hot path. Options include
pre-allocated slab pools, mbuf pass-through, or zero-copy ring slots.

**Acceptance**: `perf stat` or instrumentation shows zero allocator calls on the RX→Worker
path during steady-state benchmarks.

### R2: Reduce Single-Core Latency Gap

The single-core run-to-completion path has ~2x latency overhead vs native DPDK.

**Must**: Reduce average latency at 70K PPS from 154 us to ≤ 100 us (within 25% of native).

**Sources of overhead to investigate**:
- Mutex contention on `tx_buf`, `recv_queue`, `connected_addr`, `connection_state`
- UDP checksum computation in software (vs NIC offload)
- `parse_udp_packet_ref` vs direct header access
- `build_udp_frame_into` function call overhead
- ARP cache lookup overhead (`HashMap` with lock)

### R3: Fix Multi-Core Worker Spin-Loop Contention

Workers use `std::hint::spin_loop()` with no backoff when their SPSC ring is empty.
On a 2-vCPU instance, this causes the worker thread to compete with the RX thread for CPU.

**Must**: Implement adaptive polling — spin briefly (< 1 us), then yield, then sleep.
Workers should not burn CPU cycles when there's no work.

### R4: Eliminate MPSC CAS Contention on App Ring

Multiple workers CAS on the shared `app_ring` head. Under load, this creates contention
that scales poorly with worker count.

**Should**: Replace MPSC app_ring with per-worker SPSC rings that `recv_from()` polls
round-robin, or use a consumer-side merge strategy.

### R5: TX Path Optimization

Currently `send_to()` → TX ring → RX lcore → NIC adds a full ring hop and cross-thread
synchronization. For echo workloads this doubles the latency.

**Should**: Allow workers to transmit directly (each worker owns a TX queue) instead of
bouncing through the RX lcore's TX ring. Requires NIC multi-queue TX support.

### R6: High-Performance Instrumentation

**Must**: Add lightweight, always-on instrumentation that captures per-interval statistics
without impacting steady-state throughput. Logging interval must be configurable (default: 10s).

Required metrics:
- **RX**: packets received, bytes received, frames dropped (ring full)
- **TX**: packets sent, bytes sent, send failures
- **Latency**: min/avg/max/p99 per interval (sampled, not every packet)
- **Ring utilization**: current fill level, high-water mark, total enqueue failures
- **Worker**: packets processed, idle cycles ratio
- **ARP cache**: entries, hits, misses

**Must not**: Add more than 1% throughput overhead at 350K PPS.

**Implementation constraints**:
- Use `AtomicU64` counters — no locks on the hot path
- Aggregate statistics in a background reporting thread
- Output as structured log lines (parseable JSON or key=value format)
- Expose via a `PerfStats` struct accessible from the socket API

### R7: Latency Sampling Infrastructure

**Must**: Implement high-resolution latency sampling that measures the time from
`rx_burst` return to `recv_from()` return (internal pipeline latency).

**Implementation**: Use `std::time::Instant` on sampled packets (e.g., 1 in 1000).
Store in a fixed-size ring buffer. Report percentiles (p50, p95, p99, p99.9) at
each reporting interval.

**Must not**: Sample every packet — this adds ~100ns per packet from clock reads.

### R8: Perf Test Regression Gates

**Should**: Add performance regression detection to the perf test workflow. Compare
results against baseline thresholds:
- rust-dpdk single-core at 350K PPS: < 5% drop, < 300 us avg latency
- rust-dpdk-multicore at 350K PPS: < 10% drop, < 500 us avg latency
- No configuration should regress > 10% from its previous run

### R9: NIC Hardware Offload Utilization

**Should**: Verify and enable hardware checksum offload on both RX and TX paths.
Current code computes UDP/IPv4 checksums in software — if the NIC supports offload,
skip the computation.

**Acceptance**: Benchmark shows measurable latency reduction when offloads are active.
