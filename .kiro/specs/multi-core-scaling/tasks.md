# Tasks: Multi-Core Scaling & Shared Memory IPC

## Phase A: Lock-Free Rings + Topology Planner

- [ ] **A1**: Implement `SpscRing<T>` in `dpdk-udp/src/ring.rs` — cache-padded head/tail, power-of-2 capacity, `enqueue()`/`dequeue()`/`dequeue_batch()` with acquire/release semantics
- [ ] **A2**: Implement `MpscRing<T>` in `dpdk-udp/src/ring.rs` — CAS-based multi-producer with two-phase commit (claim + publish), single-consumer dequeue
- [ ] **A3**: Unit tests for `SpscRing` — single-threaded correctness, capacity boundary, empty/full behavior
- [ ] **A4**: Unit tests for `MpscRing` — multi-threaded stress test (N producers, 1 consumer), ordering guarantees within a single producer
- [ ] **A5**: Implement `TopologyPlan` and `detect_topology()` in `dpdk-udp/src/topology.rs` — lcore enumeration, NUMA detection, auto-scaling logic
- [ ] **A6**: Implement `UdpSocketBuilder` with `rx_queues()`, `workers_per_queue()`, and `bind()` — feeds config into `detect_topology()`
- [ ] **A7**: Verify all 133+ existing tests still pass (run-to-completion path under stubs is unchanged)

## Phase B: Multi-Core RX/TX Pipeline

- [ ] **B1**: Extend `DpdkResources` with optional `MultiCoreTopology` — RX core and worker core structs, lcore thread handles
- [ ] **B2**: Implement RX lcore loop — `rte_eth_rx_burst()` on assigned queue, classify (ARP/ICMP handled inline), enqueue data frames to worker SPSC rings
- [ ] **B3**: Implement Worker lcore loop — dequeue from SPSC ring, protocol processing, enqueue `ProcessedPacket` to MPSC `app_ring`
- [ ] **B4**: Implement TX path — `send_to()` builds frame, enqueues to TX ring, RX lcore drains TX ring and calls `rte_eth_tx_burst()`
- [ ] **B5**: Wire `recv_from()` to dequeue from `app_ring` when `MultiCoreTopology` is active (fall back to inline poll when `None`)
- [ ] **B6**: Wire `send_to()` to route through TX ring when topology is active
- [ ] **B7**: Graceful shutdown — signal lcore threads to stop, join handles, drain rings
- [ ] **B8**: Integration test on EC2 — multi-queue RSS echo server, verify packets arrive on different queues, measure throughput vs single-core baseline

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
