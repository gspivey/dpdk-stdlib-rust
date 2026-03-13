# Tasks: Performance Optimization & Instrumentation

## Phase 1: Instrumentation (Visibility First)

- [x] **P1.1**: Implement `PerfCounters` struct in `dpdk-udp/src/perf.rs` — all `AtomicU64` fields, cache-line aligned, `new()`, `snapshot()`, `reset_interval()` methods
- [x] **P1.2**: Implement `LatencySampler` in `dpdk-udp/src/perf.rs` — fixed-size ring buffer, configurable sample rate (default 1:1000), `record(duration_ns)`, `percentiles() -> (p50, p95, p99, p99.9, max)`
- [x] **P1.3**: Implement `PerfReporter` background thread — reads counters + sampler every N seconds, computes rates by diffing snapshots, emits structured key=value log line to stderr
- [x] **P1.4**: Wire counters into `UdpSocket` — add `Arc<PerfCounters>` field, increment on send/recv/drop/arp/icmp paths, add `perf_counters()`, `enable_perf_reporting()`, `perf_snapshot()` API methods
- [x] **P1.5**: Wire counters into multi-core topology — increment `rx_drops_ring_full`, `worker_idle_polls`, `worker_packets_processed`, ring enqueue failures in `rx_loop` and `worker_loop`
- [x] **P1.6**: Wire latency sampling — timestamp at `rx_burst` return, timestamp at `recv_from()` return, record delta on sampled packets
- [x] **P1.7**: Add `--perf-interval <seconds>` flag to echo app — enables `enable_perf_reporting()` at startup, default 10s
- [x] **P1.8**: Unit tests for `PerfCounters` (concurrent increment + snapshot), `LatencySampler` (percentile accuracy), `PerfReporter` (output format)
- [ ] **P1.9**: Run perf benchmark with instrumentation enabled — verify < 1% throughput regression at 350K PPS vs uninstrumented baseline

## Phase 2: Quick Wins (Low-Risk, High-Impact)

- [x] **P2.1**: Implement adaptive polling in `rx_loop` and `worker_loop` — spin (64 iters) → yield (16 iters) → sleep(1us), reset on work found
- [x] **P2.2**: Replace `Mutex<Vec<u8>>` tx_buf with `UnsafeCell<Vec<u8>>` in run-to-completion mode — safe because RTC is single-threaded; add runtime assertion
- [x] **P2.3**: Add ARP cache fast-path — `AtomicU64` storing last (IP, MAC) pair for single-peer echo pattern, bypass HashMap on cache hit
- [x] **P2.4**: Benchmark Phase 2 changes — measure latency improvement on single-core path at 70K/140K/350K PPS

## Phase 3: Multi-Core Pipeline Redesign

- [x] **P3.1**: Implement `FramePool` slab allocator — pre-allocated contiguous buffer of N × frame_size bytes, SPSC free list, `alloc() -> Option<u32>`, `free(u32)`, `frame_mut(u32) -> &mut [u8]`
- [x] **P3.2**: Define `FrameRef { pool_idx: u32, len: u16 }` — replace `Vec<u8>` in worker SPSC rings with `FrameRef`, update `rx_loop` to allocate from pool and enqueue `FrameRef`
- [x] **P3.3**: Update `worker_loop` to use `FrameRef` — access frame data via pool, free frame index back to pool after processing
- [x] **P3.4**: Replace MPSC `app_ring` with per-worker SPSC app rings — `recv_from()` polls round-robin across worker app rings
- [ ] **P3.5**: Implement worker-direct TX — detect NIC TX queue count, assign TX queues to workers, workers call `port.tx_burst(queue_id)` directly instead of enqueuing to TX ring
- [x] **P3.6**: Fix RSS-aware worker affinity — each RSS queue maps 1:1 to its worker set, remove round-robin distribution
- [x] **P3.7**: Benchmark Phase 3 changes — measure multi-core latency + throughput at 70K/140K/350K/700K PPS, compare to single-core and native baselines
- [x] **P3.8**: Unit tests for `FramePool` — alloc/free cycle, pool exhaustion, concurrent access patterns

## Phase 4: Hardware Offload & Polish

- [ ] **P4.1**: Enable TX checksum offload — detect NIC offload capability at port init, skip software IPv4/UDP checksum in `build_udp_frame_into` when offload is active, set `ol_flags` on mbuf
- [ ] **P4.2**: Enable RX checksum offload verification — check `ol_flags` on received mbufs, skip software checksum validation when NIC verifies
- [ ] **P4.3**: Add perf regression gates to CI — compare benchmark results against baseline thresholds (defined in `scripts/perf-thresholds.json`), fail workflow if regression > 10%
- [ ] **P4.4**: Update perf test workflow to archive structured JSON results — enable historical comparison across commits
- [ ] **P4.5**: Document performance tuning guide — environment variables, builder config, expected throughput/latency ranges by instance type
