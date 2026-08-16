# Tasks: Multi-Core Scaling & Shared Memory IPC

## Phase A: Lock-Free Rings + Topology Planner

- [x] **A1**: Implement `SpscRing<T>` in `dpdk-udp/src/ring.rs` — cache-padded head/tail, power-of-2 capacity, `enqueue()`/`dequeue()`/`dequeue_batch()` with acquire/release semantics
- [x] **A2**: Implement `MpscRing<T>` in `dpdk-udp/src/ring.rs` — CAS-based multi-producer with two-phase commit (claim + publish), single-consumer dequeue
- [x] **A3**: Unit tests for `SpscRing` — single-threaded correctness, capacity boundary, empty/full behavior, cross-thread correctness, drop semantics
- [x] **A4**: Unit tests for `MpscRing` — multi-threaded stress test (4 producers × 25K items, 1 consumer), ordering guarantees within a single producer
- [x] **A4b**: Unit tests for fan-out (1 producer, N SPSC consumers) — round-robin distribution to 4 SPSC rings + full pipeline test (SPSC fan-out → MPSC aggregation)
- [x] **A5**: Implement `TopologyPlan` and `detect_topology()` in `dpdk-udp/src/topology.rs` — lcore enumeration, NUMA detection, auto-scaling logic (2/4/16/32 vCPU plans, NIC max clamping, stub passthrough)
- [x] **A6**: Implement `UdpSocketBuilder` with `rx_queues()`, `workers_per_queue()`, `backend_type()`, and `bind()` — feeds config into `detect_topology()`
- [x] **A7**: Verify all 168 tests pass (104 dpdk-udp + 51 dpdk + 8 dpdk-sys + 3 dpdk-tokio + 2 apps) — run-to-completion path under stubs is unchanged

## Phase B: Multi-Core RX/TX Pipeline

- [x] **B1**: Extend `UdpSocket` with optional `MultiCoreTopology` — RX core and worker core structs, lcore thread handles, `topology_plan()` and `is_run_to_completion()` query methods
- [x] **B2**: Implement RX lcore loop — poll backend for frames, classify (ARP/ICMP handled inline on RX core), distribute data frames round-robin to worker SPSC rings, drain TX ring and send outbound frames
- [x] **B3**: Implement Worker lcore loop — dequeue from SPSC ring, parse UDP, filter by local port, learn source MAC into ARP cache, enqueue `ProcessedPacket` to MPSC `app_ring`
- [x] **B4**: Implement TX path — `send_to()` builds frame, enqueues to TX ring (`SpscRing<TxFrame>`), RX lcore drains TX ring batch and calls send_fn
- [x] **B5**: Wire `recv_from()` — when `MultiCoreTopology` is active: dequeue from `app_ring` (pipeline path via `recv_from_pipeline`); when `None`: inline poll (original `recv_from_inline` path). Connected socket filtering works in both paths.
- [x] **B6**: Wire `send_to()` — when topology is active: enqueue `TxFrame` to `tx_ring` (RX lcore transmits); when `None`: direct `send_frame()` via backend
- [x] **B7**: Graceful shutdown — `AtomicBool` shutdown flag, `MultiCoreTopology::shutdown()` signals + joins all threads, `Drop` impl ensures cleanup
- [x] **B8**: Configurable worker fan-out — `UdpSocketBuilder.workers_per_queue(0)` forces run-to-completion (no pipeline, lowest latency); `workers_per_queue(N)` enables N workers per RX queue; under stubs, always run-to-completion regardless of config
- [ ] **B9**: Integration test on EC2 — multi-queue RSS echo server, verify packets arrive on different queues, measure throughput vs single-core baseline

## Phase C: Shared Memory Daemon + ShmBackend

- [ ] **C1**: Implement `MappedRing` in `dpdk-udp/src/shm_ring.rs` — hugepage-backed mmap'd ring with the documented binary layout (head/tail/capacity/slots)
- [ ] **C2**: Unit tests for `MappedRing` — create in `/tmp` (non-hugepage for testing), verify cross-thread producer/consumer correctness
- [ ] **C3**: Implement daemon in `dpdk-udp/src/daemon.rs` — `serve()` entry point, Unix socket listener, client registration (allocate ring pair, return shm paths + MAC)
- [ ] **C4**: Implement daemon RX classifier — inspect `dst_port` in incoming frames, route to matching client's RX ring (or drop if no match)
- [ ] **C5**: Implement daemon TX drain — iterate client TX rings, dequeue frames, burst to NIC
- [ ] **C6**: Implement `ShmBackend` in `dpdk-udp/src/backend_shm.rs` — `PacketBackend` impl using `MappedRing` for send/recv, Unix socket for control messages
- [ ] **C7**: Extend `UdpSocket::bind()` — after DPDK primary check fails, attempt `ShmBackend::connect()` before falling back to AF_PACKET
- [ ] **C8**: Client lifecycle management — daemon detects closed control sockets, reclaims ring memory, handles reconnection
- [ ] **C9**: Integration test on EC2 — daemon process + two client processes on same instance, each binding different ports, verify independent send/recv

## Phase D: Cross-Language + Polish

- [ ] **D1**: Write `include/dpdk-stdlib-shm.h` — C struct definitions for the ring layout, inline producer/consumer functions
- [ ] **D2**: Add environment variable support — `DPDK_RX_QUEUES`, `DPDK_WORKERS_PER_QUEUE` read in `detect_topology()`, documented in README
- [ ] **D3**: Performance benchmarks — single-core run-to-completion vs multi-core pipeline vs shared-memory, report packets/sec and latency percentiles
- [ ] **D4**: Update `API_COMPATIBILITY.md` — document `builder()` method, `serve()` entry point, `ShmBackend` as third backend option
- [ ] **D5**: Update `AGENTS.md` — add multi-core and shared memory to architecture section, update file quick reference
