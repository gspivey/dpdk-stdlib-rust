# Design Document

> **Target version**: s2n-quic 1.81.0 / s2n-quic-core 0.81.0

## Overview

The `dpdk-stdlib-quic` crate implements a native DPDK I/O provider for s2n-quic. It lives in the dpdk-stdlib-rust workspace at `dpdk-stdlib-quic/` and depends on `dpdk-udp` and `dpdk` (never `dpdk-tokio`). The provider owns an s2n-quic endpoint and drives it from a dedicated thread running a busy-poll event loop with DPDK `rx_burst`/`tx_burst` — no Tokio runtime involved.

The I/O loop itself is runtime-free; the application side (server.accept().await, client.connect().await) still needs an executor — a Tokio dev-dependency provides this for tests and benchmarks.

The design maps directly onto s2n-quic's provider model:
- `io::Provider` — the entrypoint trait; `start(self, endpoint)` spawns the event loop thread
- `io::rx::Queue` — our `DpdkRxQueue` buffers parsed datagrams from `recv_frames()` for the endpoint to consume
- `io::tx::Queue` — our `DpdkTxQueue` collects outbound datagrams from the endpoint and flushes them via `send_frame()`
- `path::Handle` — our `DpdkPathHandle` carries `(local_addr, remote_addr)` as `SocketAddress` values

## Architecture

```
┌────────────────────────────────────────────────────────────────────┐
│  Application (Server / Client) — requires a Tokio runtime           │
│  .with_io(DpdkProvider::builder().with_addr("0.0.0.0:4433").build())│
└───────────────────────────────────┬────────────────────────────────┘
                                │ start(self, endpoint)
                                ▼
┌────────────────────────────────────────────────────────────────────┐
│  DpdkProvider                                                       │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │  Event Loop Thread (runtime-free, see §5 Threading)           │  │
│  │                                                               │  │
│  │  loop {                                                       │  │
│  │    1. endpoint.poll_wakeups(&mut cx, &clock)                  │  │
│  │    2. backend.recv_frames(32) → parse → DpdkRxQueue           │  │
│  │       endpoint.receive(&mut rx_queue, &clock)                 │  │
│  │    3. endpoint.transmit(&mut tx_queue, &clock)                │  │
│  │       tx_queue.drain() → backend.send_frame(&frame)           │  │
│  │    4. sleep_until(endpoint.timeout())                         │  │
│  │  }                                                            │  │
│  └──────────────────────────────────────────────────────────────┘  │
└───────────────────────────────────┬────────────────────────────────┘
                                │
                                ▼
┌────────────────────────────────────────────────────────────────────┐
│  dpdk-udp: PacketBackend (DpdkBackend or stub)                      │
│  send_frame(&[u8]) / recv_frames(max) → Vec<Vec<u8>>                │
└────────────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### 1. DpdkProvider (implements `io::Provider`)

```rust
pub struct DpdkProvider {
    config: ProviderConfig,
    stats: Arc<ProviderStats>,
    shutdown: Arc<AtomicBool>,
}

impl s2n_quic::provider::io::Provider for DpdkProvider {
    type PathHandle = DpdkPathHandle;
    type Error = DpdkQuicError;

    fn start<E: Endpoint<PathHandle = Self::PathHandle>>(
        self,
        endpoint: E,
    ) -> Result<SocketAddress, Self::Error> {
        // 1. Initialize DPDK backend (EAL, port, mempool)
        // 2. Resolve gateway MAC (see §1a Gateway MAC)
        // 3. Bind to configured address
        // 4. Spawn event loop thread (moves endpoint + provider clones into thread)
        // 5. Return bound SocketAddress
    }
}
```

The `build()` method on `ProviderBuilder` creates both `Arc`s (stats + shutdown flag) upfront. The provider holds clones; `ProviderHandle` holds clones. On `start()`, the provider moves its clones into the spawned thread.

```rust
pub struct ProviderBuilder {
    bind_addr: SocketAddr,
    eal_args: Option<Vec<String>>,
    backend_config: BackendConfig,
    gateway_mac: Option<[u8; 6]>,  // explicit override; else kernel ARP seed
    max_rx_burst: usize,           // default 32
    max_tx_burst: usize,           // default 32
    busy_poll_budget: usize,       // iterations before cooldown
}

impl ProviderBuilder {
    pub fn build(self) -> (DpdkProvider, ProviderHandle) {
        let stats = Arc::new(ProviderStats::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let provider = DpdkProvider {
            config: self.into_config(),
            stats: Arc::clone(&stats),
            shutdown: Arc::clone(&shutdown),
        };
        let handle = ProviderHandle { stats, shutdown };
        (provider, handle)
    }
}
```

### 1a. Gateway MAC Acquisition

The real mechanism for resolving the next-hop (gateway) MAC address — there is **no** `NeighborResolver` or `dpdk-net` crate.

Two options (builder chooses):

1. **Explicit `--gateway-mac` parameter** on the builder (recommended for production): the operator provides the known gateway MAC via `ProviderBuilder::with_gateway_mac([u8; 6])`.

2. **Kernel ARP cache seed** (matching existing `dpdk-udp` behavior): On Linux, `seed_arp_cache_from_kernel` reads `/proc/net/arp` and populates an `ArpCache`. The gateway entry (populated by the kernel's default route) provides the MAC. In AWS VPC the gateway MAC is always present in the kernel ARP table.

For the provider, option (1) is the primary path. The builder stores the MAC; if not provided, `start()` falls back to reading the kernel ARP cache for the default gateway IP (same pattern as `dpdk-udp/src/lib.rs`). This avoids runtime ARP requests.

### 2. DpdkPathHandle (implements `path::Handle`)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DpdkPathHandle {
    remote: RemoteAddress,
    local: LocalAddress,
}

impl path::Handle for DpdkPathHandle {
    fn from_remote_address(remote: RemoteAddress) -> Self { ... }
    fn remote_address(&self) -> RemoteAddress { ... }
    fn set_remote_address(&mut self, remote: RemoteAddress) { ... }
    fn local_address(&self) -> LocalAddress { ... }
    fn set_local_address(&mut self, local: LocalAddress) { ... }
    fn eq(&self, other: &Self) -> bool { ... }
    fn strict_eq(&self, other: &Self) -> bool { ... }
    fn maybe_update(&mut self, other: &Self) { ... }
}
```

The `path::Handle` trait (supertrait bounds: `'static + Copy + Send + Debug`) returns `RemoteAddress`/`LocalAddress` by value (not references). Uses `s2n_quic_core::inet::SocketAddress` (variants `IpV4` / `IpV6` — capital V) per s2n-quic's type system. IPv6 variants are rejected at runtime with `UnsupportedAddressFamily`.

### 3. DpdkRxQueue (implements `io::rx::Queue`)

```rust
pub struct DpdkRxQueue {
    datagrams: Vec<RxDatagram>,
}

struct RxDatagram {
    header: Header<DpdkPathHandle>,
    payload: Vec<u8>,
}

impl DpdkRxQueue {
    pub fn new() -> Self { Self { datagrams: Vec::new() } }

    pub fn push(&mut self, datagram: RxDatagram) {
        self.datagrams.push(datagram);
    }
}

impl io::rx::Queue for DpdkRxQueue {
    type Handle = DpdkPathHandle;

    fn for_each<F: FnMut(Header<Self::Handle>, &mut [u8])>(&mut self, mut on_packet: F) {
        for dgram in self.datagrams.drain(..) {
            let mut payload = dgram.payload;
            on_packet(dgram.header, &mut payload);
        }
    }

    fn is_empty(&self) -> bool {
        self.datagrams.is_empty()
    }
}
```

`Header<Path>` fields are `{ path: DpdkPathHandle, ecn: ExplicitCongestionNotification }` — the local and remote addresses live inside the handle.

Populated by parsing raw Ethernet frames from `recv_frames()`:
1. Validate EtherType == IPv4
2. Parse IPv4 header — extract TOS byte for ECN, check protocol field
3. **If protocol == ICMP**: dispatch to `IcmpHandler::process_icmp_full()`, send any reply via backend, skip to next frame
4. **If protocol != UDP**: discard
5. Parse UDP header — extract src/dst port
6. Discard if dst_port != bound port
7. Construct `Header<DpdkPathHandle>` with remote/local addresses and ECN marking
8. Push `(header, payload_slice)` into queue

Reuses `parse_udp_packet_ref` from `dpdk-udp` for the actual parsing (zero-copy variant).

### 4. DpdkTxQueue (implements `io::tx::Queue`)

```rust
pub struct DpdkTxQueue {
    pending: Vec<TxDatagram>,
    capacity: usize,
    local_addr: SocketAddr,
    src_mac: [u8; 6],
    gateway_mac: [u8; 6],
    frame_buf: Vec<u8>,  // reusable buffer for build_udp_frame_into
}

struct TxDatagram {
    frame: Vec<u8>,  // complete Ethernet frame ready to send
}

impl io::tx::Queue for DpdkTxQueue {
    type Handle = DpdkPathHandle;

    const SUPPORTS_ECN: bool = true;
    const SUPPORTS_PACING: bool = false;  // v1: immediate drain; s2n-quic paces internally
    const SUPPORTS_FLOW_LABELS: bool = false;

    fn push<M: tx::Message<Handle = Self::Handle>>(
        &mut self,
        message: M,
    ) -> Result<tx::Outcome, tx::Error> {
        // 1. Extract remote address from message.path_handle()
        // 2. Get ECN marking from message.ecn()
        // 3. Compute TOS byte from ECN
        // 4. Allocate payload buffer, call message.write_payload(
        //        PayloadBuffer::new(&mut buf), gso_offset
        //    ) — advancing gso_offset for each segment
        // 5. For each segment: build_udp_frame_into_with_tos(...)
        // 6. Push frames to pending vec
        // Return Result<Outcome { len, index }, Error { EmptyPayload | UndersizedBuffer | AtCapacity }>
    }

    fn capacity(&self) -> usize {
        self.capacity - self.pending.len()
    }

    fn flush(&mut self) {
        // GSO boundary — ensures segments from different connections
        // aren't merged in the same tx_burst
    }
}
```

**`push()` return type**: `Result<Outcome { len: usize, index: usize }, Error>` where `Error` variants are `EmptyPayload`, `UndersizedBuffer`, `AtCapacity`.

**`write_payload` signature** (s2n-quic 1.81.0):
```rust
fn write_payload(
    &mut self,
    buffer: PayloadBuffer,
    gso_offset: usize,
) -> Result<usize, Error>;
```
The second argument is a GSO offset, not segment length. You drive GSO by calling `write_payload` repeatedly with advancing `gso_offset`. `PayloadBuffer::new(&mut buf)` is the constructor.

**GSO query**: `can_gso` is a method on `tx::Message`, not on the queue. The provider queries `message.can_gso(segment_len, segment_count)` inside `push()` to determine segmentation.

The `flush()` between connections ensures GSO segments stay isolated. The actual `send_frame()` calls happen at the end of each event loop iteration when the full tx_queue is drained.

### 5. Event Loop and Threading

**Threading model (v1)**: The event loop runs on any `std::thread`. `DpdkBackend` wraps the port in `Mutex<Port>`, which makes `rx_burst`/`tx_burst` thread-safe. The spawned thread gets `LCORE_ID_ANY` — this is acceptable because the Mutex serialization already prevents concurrent burst calls. There is no `rte_eal_remote_launch` or `rte_thread_register` binding in `dpdk-sys`.

**Accepted tradeoff**: This means the loop thread is not pinned to an EAL-registered lcore. For v1 this is correct and matches the existing thread-safe `Mutex<Port>` pattern used throughout the codebase. A future optimization could register the thread with EAL or run on the main lcore.

**No-op waker + uninterruptible sleep tradeoff (v1)**: The waker is a no-op (no thread unpark). During cooldown sleep (`thread::sleep`), the thread cannot be woken early by application-initiated sends. Worst-case app→send latency during cooldown is 1ms. A future improvement: use `thread::park_timeout` + a real `Waker` that calls `thread::unpark`.

```rust
fn event_loop<E: Endpoint<PathHandle = DpdkPathHandle>>(
    mut endpoint: E,
    backend: Arc<dyn PacketBackend>,
    local_addr: SocketAddr,
    config: LoopConfig,
    shutdown: Arc<AtomicBool>,
    stats: Arc<ProviderStats>,
    icmp_handler: IcmpHandler,
) {
    let clock = StdClock::new();
    let noop_waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&noop_waker);

    let mut rx_queue = DpdkRxQueue::new();
    let mut tx_queue = DpdkTxQueue::new(local_addr, config.max_tx_burst, ...);
    let mut idle_cycles: u32 = 0;

    loop {
        if shutdown.load(Ordering::Relaxed) { break; }

        // 1. Service wakeups — returns CloseError when all app handles dropped
        match endpoint.poll_wakeups(&mut cx, &clock) {
            Ok(_) => {}
            Err(_close) => break,  // secondary shutdown signal
        }
        stats.timer_wakeups.fetch_add(1, Ordering::Relaxed);

        // 2. RX: recv_frames → parse → ICMP dispatch → queue → endpoint.receive
        let rx_result = backend.recv_frames(config.max_rx_burst);
        stats.rx_burst_calls.fetch_add(1, Ordering::Relaxed);
        let rx_count = match rx_result {
            Ok(frames) => {
                let mut count = 0usize;
                for frame in &frames {
                    let protocol = frame.get(ETH_HEADER_LEN + 9).copied().unwrap_or(0);
                    if protocol == IP_PROTO_ICMP {
                        // Dispatch ICMP: echo replies + error reporting
                        if let Some(action) = icmp_handler.process_icmp_full(frame) {
                            match action {
                                IcmpAction::Reply(reply) => { let _ = backend.send_frame(&reply); }
                                IcmpAction::Error(_info) => { /* future: report to endpoint */ }
                            }
                        }
                        continue;
                    }
                    if let Some(dgram) = parse_to_rx_datagram(frame, local_addr) {
                        rx_queue.push(dgram);
                        count += 1;
                    }
                }
                stats.datagrams_received.fetch_add(count as u64, Ordering::Relaxed);
                count
            }
            Err(_) => {
                stats.rx_drops.fetch_add(1, Ordering::Relaxed);
                0
            }
        };

        if !rx_queue.is_empty() {
            endpoint.receive(&mut rx_queue, &clock);
        }

        // 3. TX: endpoint.transmit → tx_queue → send_frame
        endpoint.transmit(&mut tx_queue, &clock);
        stats.tx_burst_calls.fetch_add(1, Ordering::Relaxed);
        let mut tx_count = 0usize;
        for dgram in tx_queue.drain() {
            match backend.send_frame(&dgram.frame) {
                Ok(_) => tx_count += 1,
                Err(_) => { stats.tx_drops.fetch_add(1, Ordering::Relaxed); }
            }
        }
        stats.datagrams_transmitted.fetch_add(tx_count as u64, Ordering::Relaxed);

        // 4. Timer: check next deadline
        let work_done = rx_count > 0 || tx_count > 0;
        if work_done {
            idle_cycles = 0;
        } else {
            idle_cycles += 1;
            if idle_cycles > config.busy_poll_budget {
                if let Some(timeout) = endpoint.timeout() {
                    let now = clock.get_time();
                    if timeout > now {
                        let sleep_dur = Duration::from(timeout - now)
                            .min(Duration::from_millis(1));
                        std::thread::sleep(sleep_dur);
                    }
                } else {
                    std::thread::sleep(Duration::from_micros(100));
                }
                idle_cycles = 0;
            }
        }
    }
}
```

### 6. Clock

```rust
pub struct StdClock {
    epoch: Instant,
}

impl s2n_quic_core::time::Clock for StdClock {
    fn get_time(&self) -> Timestamp {
        let elapsed = self.epoch.elapsed();
        unsafe { Timestamp::from_duration(elapsed) }
    }
}
```

Uses `std::time::Instant` for monotonic time. The trait path is `s2n_quic_core::time::Clock`. s2n-quic's `Timestamp` is a `Duration` from an arbitrary epoch — `Instant::now()` at startup serves as that epoch.

### 7. ECN Handling

s2n-quic 1.81.0 defines ECN as:
```rust
#[repr(u8)]
pub enum ExplicitCongestionNotification {
    NotEct = 0b00,
    Ect1   = 0b01,
    Ect0   = 0b10,
    Ce     = 0b11,
}
```

Since the enum is `#[repr(u8)]` with values equal to the wire bits, use direct cast.

**RX path**: Extract the 2 low-order bits of the TOS field, transmute/cast to the enum:
```rust
let ecn_bits = frame[ip_offset + 1] & 0x03;
// Safety: ecn_bits is 0..=3, matching all enum variants
let ecn: ExplicitCongestionNotification = unsafe { std::mem::transmute(ecn_bits) };
```

**TX path**: When building outbound frames, the TOS byte's ECN bits are `ecn as u8`:
```rust
let tos = ecn as u8;  // Direct cast — repr(u8) guarantees correct wire encoding
```

Set `frame[ip_offset + 1] = tos` before computing the IPv4 header checksum (software recompute).

### 8. GSO/GRO Implementation

**GSO (TX)**: The provider queries `message.can_gso(segment_len, segment_count)` inside `push()`. When GSO is possible, the provider calls `message.write_payload(PayloadBuffer::new(&mut buf), gso_offset)` repeatedly with advancing `gso_offset`. Each written chunk becomes a separate Ethernet frame built via `build_udp_frame_into_with_tos`. All N frames are sent in a single `send_frame()` burst within one event loop iteration.

Our "GSO" is software segmentation into individual frames — which is what DPDK naturally expects.

**GRO (RX)**: Multiple datagrams returned by a single `recv_frames(32)` call are all parsed and queued in `DpdkRxQueue` before calling `endpoint.receive()`. This gives the endpoint a batch of datagrams in one shot — equivalent to GRO from the endpoint's perspective.

### 9. ProviderStats (Observability)

```rust
pub struct ProviderStats {
    pub rx_burst_calls: AtomicU64,
    pub tx_burst_calls: AtomicU64,
    pub datagrams_received: AtomicU64,
    pub datagrams_transmitted: AtomicU64,
    pub rx_drops: AtomicU64,        // recv_frames errors
    pub tx_drops: AtomicU64,        // send_frame failures
    pub timer_wakeups: AtomicU64,
}

impl ProviderStats {
    pub fn snapshot(&self) -> StatsSnapshot { ... }
}
```

`rx_drops` counts `recv_frames()` errors (the practical DPDK metric — individual frames are either delivered or not).

Both `Arc<ProviderStats>` and `Arc<AtomicBool>` (shutdown) are created in `build()`, not `start()`. The provider and handle each hold clones.

### 10. Shutdown

**`ProviderHandle` is the authoritative shutdown mechanism**:

1. `ProviderHandle::shutdown()` sets the `AtomicBool` flag — this is the primary external trigger.
2. The event loop checks the flag each iteration and breaks.
3. `poll_wakeups` returning `CloseError` (when all app connection/acceptor handles are dropped) is the **secondary** signal — the loop also breaks on this.
4. The endpoint is moved into and owned by the loop thread. The application holds connection and acceptor handles.
5. The thread joins within 100ms.
6. All backend resources are dropped when the `Arc<dyn PacketBackend>` refcount reaches zero.

```rust
pub struct ProviderHandle {
    stats: Arc<ProviderStats>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProviderHandle {
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }
}
```

### 11. Stub Mode and Loopback Backend

When DPDK is unavailable (stubs active), `DpdkBackend` returns empty frames on `recv_frames()` and succeeds on `send_frame()` without I/O. The provider initializes normally — the event loop runs but does no real work.

For testing the full Rx/Tx → handshake path without DPDK, a `LoopbackBackend` is provided:
```rust
pub struct LoopbackBackend {
    tx_to_rx: Mutex<VecDeque<Vec<u8>>>,
    mac: [u8; 6],
    promiscuous: AtomicBool,
    allmulticast: AtomicBool,
}

impl PacketBackend for LoopbackBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.tx_to_rx.lock().unwrap().push_back(frame.to_vec());
        Ok(frame.len())
    }
    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut q = self.tx_to_rx.lock().unwrap();
        let n = max_frames.min(q.len());
        Ok(q.drain(..n).collect())
    }
    fn mac_address(&self) -> [u8; 6] { self.mac }
    fn backend_name(&self) -> &'static str { "loopback" }
    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        self.promiscuous.store(enable, Ordering::Relaxed); Ok(())
    }
    fn is_promiscuous(&self) -> bool { self.promiscuous.load(Ordering::Relaxed) }
    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        self.allmulticast.store(enable, Ordering::Relaxed); Ok(())
    }
    fn is_allmulticast(&self) -> bool { self.allmulticast.load(Ordering::Relaxed) }
}
```

All 8 `PacketBackend` methods are implemented.

### 12. Frame Building (build_udp_frame_into_with_tos)

The new function lives in `dpdk-udp` (next to the existing `build_udp_frame_into`) — this is an additive, non-breaking change per Req 14.2. The `dpdk-stdlib-quic` crate calls it directly.

```rust
// dpdk-udp/src/lib.rs (new public function)
pub fn build_udp_frame_into_with_tos(
    out: &mut Vec<u8>,
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    ttl: u8,
    tos: u8,
) -> UdpResult<usize>
```

Identical to `build_udp_frame_into` except `frame[ip + 1] = tos` instead of `0x00`. The IPv4 header checksum is recomputed in software after setting TOS.

`dpdk-stdlib-quic/src/frame.rs` re-exports or wraps this for ergonomics within the crate.

### 13. Benchmark Binary

```
dpdk-stdlib-quic/src/bin/bench.rs
```

CLI arguments:
- `--provider=stock | native-dpdk`
- `--duration=<secs>` (default 10)
- `--streams=<n>` (default 1)
- `--payload-size=<bytes>` (default 1MB)

Both providers run the same workload: client opens N streams, sends `payload-size` bytes on each, server echoes. Metrics collected: total bytes transferred, elapsed time, throughput (Gbps), packets/sec, handshake latency, provider stats counters.

TLS is configured with a self-signed cert generated at runtime via `rcgen` (dev-dependency).

## Data Models

The primary data structures flowing through the system:

- **RxDatagram**: `{ header: Header<DpdkPathHandle>, payload: Vec<u8> }` — parsed inbound datagram
- **TxDatagram**: `{ frame: Vec<u8> }` — complete Ethernet frame ready to send
- **Header\<Path\>**: `{ path: DpdkPathHandle, ecn: ExplicitCongestionNotification }` — s2n-quic datagram header
- **DpdkPathHandle**: `{ remote: RemoteAddress, local: LocalAddress }` — path identification
- **StatsSnapshot**: `{ rx_burst_calls: u64, tx_burst_calls: u64, datagrams_received: u64, datagrams_transmitted: u64, rx_drops: u64, tx_drops: u64, timer_wakeups: u64 }` — point-in-time counter values
- **ProviderConfig**: Builder output containing bind_addr, gateway_mac, burst sizes, EAL args, backend config

## Data Flow

### Receive Path (RX)
```
NIC → DPDK rx_burst → recv_frames() → Vec<Vec<u8>>
  → check protocol field:
      ICMP → IcmpHandler::process_icmp_full() → reply/error handling
      UDP  → parse via parse_udp_packet_ref [validate ETH/IP/UDP, extract ECN]
           → DpdkRxQueue.push(RxDatagram { header: Header { path, ecn }, payload })
  → endpoint.receive(&mut rx_queue, &clock)
  → s2n-quic processes QUIC packets
```

### Transmit Path (TX)
```
s2n-quic → endpoint.transmit(&mut tx_queue, &clock)
  → tx_queue.push(message):
      message.write_payload(PayloadBuffer::new(&mut buf), gso_offset)
      build_udp_frame_into_with_tos(src, dst, payload, ttl, ecn as u8)
  → TxDatagram { frame: Vec<u8> }
  → tx_queue.drain() → backend.send_frame(&frame)
  → DPDK tx_burst → NIC
```

## Dependency Graph

```
dpdk-stdlib-quic
├── s2n-quic-core = "=0.81.0" (Endpoint, io::rx, io::tx, path::Handle, Clock, inet types)
├── s2n-quic = "=1.81.0"      (Server, Client, provider::io::Provider)
├── dpdk-udp                   (PacketBackend, DpdkBackend, build_udp_frame_into_with_tos, parse_udp_packet_ref, IcmpHandler)
├── dpdk                       (Eal, Port — for native DPDK init)
├── thiserror = "1"
├── futures = "0.3"            (noop_waker)
└── [dev] rcgen                (self-signed TLS certs for tests/bench)
     [dev] tokio = { features = ["full"] }  (app-side executor for handshake tests)
```

## File Layout

```
dpdk-stdlib-quic/
├── Cargo.toml
├── src/
│   ├── lib.rs              # Public API: DpdkProvider, ProviderBuilder, ProviderHandle
│   ├── provider.rs         # io::Provider impl, build(), start()
│   ├── event_loop.rs       # The core poll loop
│   ├── path_handle.rs      # DpdkPathHandle + path::Handle impl
│   ├── rx.rs               # DpdkRxQueue + io::rx::Queue impl + push()/new()
│   ├── tx.rs               # DpdkTxQueue + io::tx::Queue impl + GSO segmentation
│   ├── clock.rs            # StdClock + s2n_quic_core::time::Clock impl
│   ├── ecn.rs              # ECN extraction and TOS construction helpers (direct cast)
│   ├── frame.rs            # Re-exports/wraps dpdk-udp's build_udp_frame_into_with_tos
│   ├── stats.rs            # ProviderStats, StatsSnapshot, ProviderHandle
│   ├── error.rs            # DpdkQuicError type
│   ├── loopback.rs         # LoopbackBackend for testing (all 8 PacketBackend methods)
│   └── bin/
│       ├── bench.rs        # Two-way benchmark binary
│       └── quic-smoke.rs   # Walking skeleton: build provider, start stub, print OK, exit
├── tests/
│   ├── provider_init.rs       # Provider construction and stub-mode tests
│   ├── loopback_handshake.rs  # Full QUIC handshake over LoopbackBackend
│   ├── ecn_roundtrip.rs       # ECN marking preserved through RX/TX
│   └── gso_segmentation.rs   # GSO segmentation correctness
└── README.md
```

## Correctness Properties

### Property 1: ECN wire-encoding round-trip
For any `ExplicitCongestionNotification` variant, `extract_ecn(ecn_to_tos_bits(ecn)) == ecn`. Guarantees no data corruption in congestion-control signaling.

**Validates: Requirements 6.1, 6.2**

### Property 2: Clock monotonicity
For any sequence of `clock.get_time()` calls, each returned `Timestamp` is ≥ the previous. Prevents timer inversions that could stall loss detection.

**Validates: Requirements 3.1**

### Property 3: Rx address extraction preserves source and local
For any valid UDP frame with arbitrary source IP:port targeting the bound port, the resulting `Header<DpdkPathHandle>` contains a `remote` matching the frame's source and a `local` matching the provider's bound address.

**Validates: Requirements 5.1, 5.2**

### Property 4: All valid datagrams delivered regardless of source
For any source IP and any UDP payload of 1–1472 bytes destined for the bound port, the provider delivers it to the endpoint with payload bytes intact.

**Validates: Requirements 5.3, 5.4**

### Property 5: Invalid frames silently discarded
For any frame that is not a valid IPv4/UDP datagram for the bound port (wrong EtherType, wrong protocol, wrong dst_port, truncated), the rx queue remains empty after parsing.

**Validates: Requirements 5.5**

### Property 6: GSO segmentation correctness
For any payload of length L > segment_len where `message.can_gso(segment_len, count)` returns true, the tx queue produces exactly `ceil(L / segment_len)` frames, each with payload ≤ segment_len bytes, and concatenation of all payloads equals the original buffer.

**Validates: Requirements 7.1, 7.2**

### Property 7: Gateway MAC on all outbound frames
For any outbound frame produced by the tx queue, the Ethernet destination MAC is the configured gateway MAC — never a MAC derived from the peer's IP address.

**Validates: Requirements 2.5**

### Property 8: IPv6 rejection
For any `SocketAddress::IpV6` passed to the builder or encountered during operation, the provider returns `UnsupportedAddressFamily` rather than silently misbehaving.

**Validates: Requirements 13.3**

### Property 9: Counter consistency
For any sequence of operations, `stats.datagrams_received` equals the number of valid UDP datagrams delivered to `endpoint.receive()`, and `stats.datagrams_transmitted` equals the number of successful `send_frame()` calls.

**Validates: Requirements 9.1**

### Property 10: Shutdown completes within 100ms
After `ProviderHandle::shutdown()` is called, the event loop thread exits and `join()` returns within 100ms.

**Validates: Requirements 8.4**

## Performance Thesis and Known v1 Overhead

`PacketBackend`'s current API (`Vec<Vec<u8>>` on RX, per-frame `send_frame(&[u8])` on TX) introduces copy overhead:

- **RX**: Each frame is allocated as an owned `Vec<u8>` by `recv_frames`. A future optimization path is `parse_udp_packet_ref` directly on mbuf-backed slices (zero-copy RX).
- **TX**: Each frame is built into a `Vec<u8>` then copied into an mbuf by `send_frame`. A future optimization path is a batched `send_frames(&[&[u8]])` API or direct mbuf construction.

These are accepted v1 costs. The primary performance win comes from kernel bypass (no syscall overhead, no netfilter, no socket buffer copies). The copy overhead is bounded and predictable. Benchmarks will quantify the gap vs. the theoretical zero-copy ceiling.

## Error Handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum DpdkQuicError {
    #[error("DPDK initialization failed: {0}")]
    DpdkInit(String),

    #[error("Backend creation failed: {0}")]
    BackendInit(#[from] std::io::Error),

    #[error("Address family not supported: IPv6 requires a separate provider")]
    UnsupportedAddressFamily,

    #[error("Port bind failed: {0}")]
    BindFailed(String),

    #[error("Event loop terminated unexpectedly: {0}")]
    EventLoopCrash(String),
}
```

This satisfies `'static + Display + Send + Sync` as required by `io::Provider::Error`.

## Testing Strategy

1. **Unit tests** (no DPDK): Test frame parsing, ECN extraction (direct cast), GSO segmentation, TxQueue/RxQueue logic using synthetic frames
2. **Loopback integration** (no DPDK): Full QUIC handshake using `LoopbackBackend` — validates the entire provider works end-to-end. Requires `tokio` dev-dependency for the app-side executor.
3. **Stub mode** (no DPDK): Provider initializes cleanly, event loop runs without panic
4. **Walking skeleton CI** (no DPDK): `quic-smoke` binary builds provider in stub mode, prints OK, exits 0
5. **EC2 integration** (real DPDK): Two-instance test — one server, one client — validates real packet flow
6. **Benchmark** (real DPDK): Throughput comparison between stock and native-dpdk providers

Property-based testing is not applicable here — this is an I/O provider with external service integration; testing relies on unit tests, loopback integration, and EC2 integration.

## Non-Goals (Deferred)

- IPv6 support (APIs use `SocketAddress` for forward-compat but reject IPv6 at runtime)
- Connection migration between providers
- Multi-port / multi-queue distribution
- QUIC 0-RTT optimization at the provider level
- Hardware TLS offload
- Real Waker integration (v1 uses noop_waker + thread::sleep)
- EAL lcore thread registration (v1 uses std::thread with Mutex<Port>)
