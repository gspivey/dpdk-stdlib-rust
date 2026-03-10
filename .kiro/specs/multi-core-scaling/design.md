# Design Document: Multi-Core Scaling & Shared Memory IPC

## Overview

This design extends dpdk-stdlib-rust to use multiple CPU cores and support multi-process shared memory — while keeping the user-facing API (`UdpSocket::bind()` / `recv_from()` / `send_to()`) completely unchanged. The core philosophy: **topology is configuration, not code**.

Two features share one design because they compose naturally:

1. **Multi-core (RSS + Pipeline)** — Each NIC RSS queue gets a dedicated RX lcore; each RX lcore pipelines frames to worker lcores via lock-free SPSC rings. Single-stream workloads collapse to 1 queue + N workers. Multi-stream workloads fan out across queues.

2. **Shared Memory IPC** — A daemon process owns the NIC and the multi-core topology. Application processes connect via hugepage-backed shared memory rings, receiving/sending frames through the daemon. `ShmBackend` implements the existing `PacketBackend` trait — no trait changes needed.

## Architecture

### Multi-Core Topology

```
NIC (RSS enabled)
├── RSS Q0 ──► RX Lcore 0 ──► SPSC ring ──► Worker Lcore 4 ──┐
├── RSS Q1 ──► RX Lcore 1 ──► SPSC ring ──► Worker Lcore 5 ──┤
├── RSS Q2 ──► RX Lcore 2 ──► SPSC ring ──► Worker Lcore 6 ──┤ MPSC
└── RSS Q3 ──► RX Lcore 3 ──► SPSC ring ──► Worker Lcore 7 ──┘ ring
                                                                 │
                                                                 ▼
                                                         app_ring (to recv_from)

TX path (reverse):
  send_to() ──► tx_ring ──► RX Lcore N (owns the NIC queue) ──► NIC TX
```

**Run-to-completion fallback:** On 1-2 vCPU instances (or under stubs), the pipeline collapses: the single RX lcore does everything inline, no rings, no extra threads. This is the current behavior — zero overhead for small instances.

### Shared Memory Multi-Process

```
App Process A                    Daemon Process                   App Process B
┌─────────────┐                 ┌──────────────────┐             ┌─────────────┐
│ UdpSocket   │                 │ NIC (DPDK)       │             │ UdpSocket   │
│  ShmBackend │                 │  RX poll loop    │             │  ShmBackend │
│  ┌────────┐ │  hugepage shm   │  ┌────────────┐ │  hugepage    │ ┌────────┐ │
│  │ rx_ring │◄├────────────────┤──┤ classifier │ ├──────────────►│ rx_ring │ │
│  │ tx_ring │─├────────────────►──┤ (dst_port) │ ◄──────────────┤ tx_ring │ │
│  └────────┘ │                 │  └────────────┘ │              │ └────────┘ │
└─────────────┘                 │  TX drain loop   │             └─────────────┘
                                │  (dequeue app TX │
                                │   rings, burst)  │
                                └──────────────────┘
```

### Backend Selection in `bind()`

```
UdpSocket::bind(addr)
  │
  ├─ Am I the DPDK primary process?
  │   YES ──► DpdkBackend (direct, multi-core topology)
  │   NO ──┐
  │         ├─ Is a daemon running? (check /var/run/dpdk-stdlib.sock)
  │         │   YES ──► ShmBackend (shared memory rings)
  │         │   NO ──┐
  │         │         └─► RawSocketBackend (AF_PACKET fallback)
  └─────────┘
```

Three tiers of performance, zero API changes, automatic selection.

## Detailed Design

### 1. DpdkResources Extension

The existing `DpdkResources` struct (internal to `UdpSocket`) is extended to manage the multi-core topology:

```rust
// Inside dpdk-udp/src/lib.rs (or new file dpdk-udp/src/topology.rs)

struct DpdkResources {
    // ... existing fields (port, mempool, mac, etc.) ...

    /// Multi-core topology (None = run-to-completion on current thread)
    topology: Option<MultiCoreTopology>,
}

struct MultiCoreTopology {
    /// One per RSS queue — each runs on a dedicated lcore
    rx_cores: Vec<RxCore>,

    /// Worker cores that receive frames from RX cores
    workers: Vec<WorkerCore>,

    /// All workers enqueue processed packets here; recv_from() dequeues
    app_ring: Arc<MpscRing<ProcessedPacket>>,
}

struct RxCore {
    queue_id: u16,
    lcore_id: u32,
    /// SPSC rings to worker cores fed by this RX core
    worker_rings: Vec<Arc<SpscRing<RawFrame>>>,
    /// Handle to the spawned lcore thread
    handle: Option<JoinHandle<()>>,
}

struct WorkerCore {
    lcore_id: u32,
    /// Receives raw frames from parent RX core
    rx_ring: Arc<SpscRing<RawFrame>>,
    /// Sends processed packets to the application
    app_ring: Arc<MpscRing<ProcessedPacket>>,
    /// TX ring back to the RX core that owns the NIC queue
    tx_ring: Arc<SpscRing<TxFrame>>,
    handle: Option<JoinHandle<()>>,
}
```

### 2. Topology Auto-Detection

```rust
// dpdk-udp/src/topology.rs

pub(crate) fn detect_topology(config: &TopologyConfig) -> TopologyPlan {
    let available_lcores = eal_lcore_count();
    let nic_max_queues = port_max_rx_queues(port_id);
    let numa_nodes = detect_numa_layout();

    // Apply configuration precedence: builder > env > auto
    let rx_queues = config.rx_queues
        .or_else(|| env::var("DPDK_RX_QUEUES").ok()?.parse().ok())
        .unwrap_or_else(|| auto_detect_queues(available_lcores, nic_max_queues));

    let workers_per_queue = config.workers_per_queue
        .or_else(|| env::var("DPDK_WORKERS_PER_QUEUE").ok()?.parse().ok())
        .unwrap_or_else(|| auto_detect_workers(available_lcores, rx_queues));

    TopologyPlan { rx_queues, workers_per_queue, numa_nodes }
}

fn auto_detect_queues(lcores: usize, nic_max: u16) -> u16 {
    match lcores {
        0..=2 => 1,                                    // run-to-completion
        3..=4 => min(2, nic_max),                      // small pipeline
        n     => min((n / 2) as u16, nic_max),         // half for RX, half for workers
    }
}

fn auto_detect_workers(lcores: usize, rx_queues: u16) -> u16 {
    let remaining = lcores.saturating_sub(rx_queues as usize);
    if remaining == 0 { return 0; }  // run-to-completion
    (remaining / rx_queues as usize).max(1) as u16
}
```

**NUMA awareness:** When assigning lcores to roles, the topology planner groups each RX core and its workers on the same NUMA node. This avoids cross-socket memory traffic on the ring buffers.

### 3. Lock-Free Ring Buffers

Two ring types, both backed by cache-line-aligned slots:

```rust
// dpdk-udp/src/ring.rs

/// Single-Producer Single-Consumer ring (RX core → Worker, Worker → TX)
/// Fast path: no atomics beyond relaxed load/store + acquire/release fences
pub struct SpscRing<T> {
    head: CachePadded<AtomicU64>,   // written by producer
    tail: CachePadded<AtomicU64>,   // written by consumer
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    capacity: u64,                  // power of 2
}

/// Multi-Producer Single-Consumer ring (N workers → app recv_from)
/// Uses a two-phase commit: claim slot with CAS, then publish
pub struct MpscRing<T> {
    head: CachePadded<AtomicU64>,   // CAS by producers
    committed: CachePadded<AtomicU64>, // tracks published slots
    tail: CachePadded<AtomicU64>,   // read by consumer
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    capacity: u64,
}
```

**Why not `rte_ring`?** We could use DPDK's built-in `rte_ring`, but our own Rust implementation: (a) works under stubs for testing, (b) avoids FFI overhead on the hot path, (c) gives us type safety. For the shared memory IPC rings (cross-process), we use a simpler layout over `mmap` (see Section 5).

### 4. Builder API

```rust
// dpdk-udp/src/lib.rs

impl UdpSocket {
    /// Optional builder for explicit topology control.
    /// Most users should use `UdpSocket::bind()` which auto-detects.
    pub fn builder() -> UdpSocketBuilder {
        UdpSocketBuilder::new()
    }
}

pub struct UdpSocketBuilder {
    rx_queues: Option<u16>,
    workers_per_queue: Option<u16>,
    backend_type: Option<BackendType>,
    // ... existing BackendConfig fields ...
}

impl UdpSocketBuilder {
    pub fn bind(self, addr: impl ToSocketAddrs) -> UdpResult<UdpSocket> { ... }
    pub fn rx_queues(mut self, n: u16) -> Self { self.rx_queues = Some(n); self }
    pub fn workers_per_queue(mut self, n: u16) -> Self { self.workers_per_queue = Some(n); self }
}
```

The builder feeds into the same `detect_topology()` function, with explicit values taking priority.

### 5. Shared Memory Backend

#### 5a. Daemon

```rust
// dpdk-udp/src/daemon.rs

/// Start the DPDK packet daemon. Blocks forever.
/// Owns the NIC, runs RX/TX poll loops, multiplexes to app processes.
pub fn serve() -> UdpResult<()> { ... }

pub struct ServeConfig {
    pub interface: Option<String>,
    pub hugepage_path: PathBuf,       // default: /dev/hugepages/dpdk-stdlib
    pub control_socket: PathBuf,      // default: /var/run/dpdk-stdlib.sock
    pub max_clients: usize,           // default: 64
}
```

The daemon:
1. Initializes DPDK EAL and configures the multi-core topology (reuses Section 1-2)
2. Listens on a Unix domain socket for app registrations
3. On registration: allocates a pair of hugepage-backed SPSC rings (RX + TX) for the app, returns the shm paths
4. RX poll loop: classifies frames by `dst_port`, enqueues to the matching app's RX ring
5. TX drain loop: dequeues from each app's TX ring, transmits via NIC

#### 5b. ShmBackend

```rust
// dpdk-udp/src/backend_shm.rs

pub struct ShmBackend {
    rx_ring: MappedRing,    // daemon writes, app reads (SPSC)
    tx_ring: MappedRing,    // app writes, daemon reads (SPSC)
    mac: [u8; 6],           // learned from daemon at connect time
    control: UnixStream,    // for registration/teardown
}

impl PacketBackend for ShmBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.tx_ring.enqueue(frame)
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        self.rx_ring.dequeue_batch(max_frames)
    }

    fn mac_address(&self) -> [u8; 6] { self.mac }
    fn backend_name(&self) -> &'static str { "shared-memory" }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        // Send control message to daemon
        self.control_request(ControlMsg::SetPromiscuous(enable))
    }

    fn is_promiscuous(&self) -> bool {
        self.control_query(ControlMsg::IsPromiscuous)
    }

    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        self.control_request(ControlMsg::SetAllmulticast(enable))
    }

    fn is_allmulticast(&self) -> bool {
        self.control_query(ControlMsg::IsAllmulticast)
    }
}
```

#### 5c. Shared Memory Ring Layout (Cross-Language)

```
Hugepage file: /dev/hugepages/dpdk-stdlib/app-{pid}-rx

Offset  Size    Field
──────  ──────  ────────────────────────────────────
0x0000  8       head (u64, atomic, written by producer)
0x0040  8       tail (u64, atomic, written by consumer)
0x0080  8       capacity (u64, power of 2, immutable after creation)
0x00C0  8       slot_size (u64, = 4 + MTU_MAX, immutable)
0x0100  ...     [padding to 256-byte alignment]
0x0100  N*slot  slot array

Each slot:
  Offset  Size       Field
  0       4          frame_length (u32, little-endian; 0 = empty)
  4       slot_size  frame_data (Ethernet frame bytes)

Cache-line padding (64 bytes) between head and tail prevents false sharing.
Producer protocol: write frame_data, write frame_length, store-release head.
Consumer protocol: load-acquire tail, read frame_length, read frame_data, store-release tail.
```

This layout is deliberately simple: any language with `mmap` + atomic load/store can implement a consumer or producer. A `dpdk-stdlib-shm.h` C header will be provided.

### 6. Integration with Existing Code

#### What changes

| Component | Change |
|-----------|--------|
| `DpdkResources` | Extended with optional `MultiCoreTopology` |
| `UdpSocket::bind()` | Adds daemon detection (shared memory path) |
| `UdpSocket::recv_from()` | When topology is active: dequeue from `app_ring` instead of inline poll |
| `UdpSocket::send_to()` | When topology is active: enqueue to `tx_ring` instead of inline TX |
| `UdpSocket` (new) | `builder()` method |
| `dpdk/src/eal.rs` | Expose lcore enumeration, NUMA topology queries |
| `dpdk/src/port.rs` | Support multi-queue port configuration (already partially there) |

#### What does NOT change

| Component | Why |
|-----------|-----|
| `PacketBackend` trait | `ShmBackend` implements it as-is |
| `SocketBackend` enum | `ShmBackend` goes through the existing `Generic(Arc<dyn PacketBackend>)` variant |
| ARP / ICMP handlers | They operate on `&[u8]` — backend-agnostic by design |
| `dpdk-tokio` compat layer | Wraps `UdpSocket` — transparent to topology |
| `dpdk-sys` stubs | Multi-core is a no-op under stubs (single-threaded) |
| AF_PACKET backend | Stays single-threaded (it's the fallback path) |
| Public API signatures | `bind()`, `recv_from()`, `send_to()` — all unchanged |

### 7. Stub Behavior

Under stubs (`dpdk_sys::is_stub() == true`):
- `detect_topology()` always returns a single-core, run-to-completion plan
- No lcore threads are spawned
- `app_ring` is not used; `recv_from()` calls the inline poll path (current behavior)
- All 133+ existing tests pass unchanged
- New multi-core tests use mock rings to verify enqueue/dequeue logic without real DPDK

### 8. Error Handling

| Scenario | Behavior |
|----------|----------|
| NIC doesn't support RSS | Fall back to 1 queue, pipeline workers only |
| Fewer lcores than requested queues | Clamp to available lcores, warn via log |
| Daemon not running (shm path) | Fall through to AF_PACKET fallback |
| App process crashes (shm) | Daemon detects closed control socket, reclaims rings |
| Daemon crashes | App `recv_from()` returns `io::Error` (broken pipe); app can reconnect or fall back |
| Ring full (backpressure) | Producer spins briefly, then returns `WouldBlock`; caller retries |

## New Files

| File | Purpose |
|------|---------|
| `dpdk-udp/src/topology.rs` | `MultiCoreTopology`, `TopologyPlan`, `detect_topology()`, lcore loop functions |
| `dpdk-udp/src/ring.rs` | `SpscRing<T>`, `MpscRing<T>` — in-process lock-free rings |
| `dpdk-udp/src/daemon.rs` | `serve()`, `ServeConfig`, daemon main loops, client registration |
| `dpdk-udp/src/backend_shm.rs` | `ShmBackend` implementing `PacketBackend`, `MappedRing` (mmap'd cross-process ring) |
| `dpdk-udp/src/shm_ring.rs` | Cross-process shared memory ring layout, producer/consumer protocols |
| `include/dpdk-stdlib-shm.h` | C header for cross-language shared memory ring consumers |

## Implementation Phases

The work is ordered so each phase is independently shippable and testable:

### Phase A: Lock-Free Rings + Topology Planner
- Implement `SpscRing` and `MpscRing` in `ring.rs` with comprehensive unit tests
- Implement `detect_topology()` and `TopologyPlan` in `topology.rs`
- Add `UdpSocketBuilder` with `rx_queues()` / `workers_per_queue()`
- All tests pass under stubs (topology returns run-to-completion plan)

### Phase B: Multi-Core RX/TX Pipeline
- Extend `DpdkResources` with `MultiCoreTopology`
- Implement RX lcore loops: poll NIC queue → enqueue to worker SPSC ring
- Implement Worker lcore loops: dequeue from SPSC → protocol handling → enqueue to MPSC app_ring
- Wire `recv_from()` to dequeue from `app_ring` when topology is active
- Wire `send_to()` to enqueue to TX ring → RX lcore → NIC
- Integration test: multi-queue echo server on EC2 with RSS traffic

### Phase C: Shared Memory Daemon + ShmBackend
- Implement daemon (`serve()`) with Unix socket registration
- Implement `MappedRing` (hugepage-backed cross-process SPSC ring)
- Implement `ShmBackend` as a `PacketBackend`
- Extend `bind()` with daemon detection logic
- Integration test: daemon + two app processes sharing one NIC

### Phase D: Cross-Language + Polish
- Publish `dpdk-stdlib-shm.h` C header
- Add environment variable configuration (`DPDK_RX_QUEUES`, etc.)
- Performance benchmarks: single-core vs multi-core vs shared-memory
- Documentation and examples
