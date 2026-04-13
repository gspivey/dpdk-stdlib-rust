# dpdk-stdlib-rust

Drop-in DPDK-accelerated replacements for `std::net::UdpSocket` and `tokio::net::UdpSocket`. Bypass the Linux kernel network stack for high-throughput packet processing, with automatic fallback when DPDK is unavailable.

## Why

Traditional Linux networking routes every packet through the kernel: syscalls, context switches, interrupts, and the full TCP/IP stack. For high-packet-rate workloads (DNS servers, load balancers, packet processors), this overhead becomes the bottleneck.

DPDK (Data Plane Development Kit) bypasses the kernel entirely using userspace drivers and polling. This eliminates syscalls and context switches, achieving:

- **~2x higher packet throughput** at saturation (700K PPS: DPDK delivers ~640-680K RX while kernel delivers ~310-340K)
- **Zero packet drops** up to 350K PPS where the kernel starts dropping
- **Zero kernel overhead** for packet I/O — no syscalls, no context switches

**But DPDK's C API is complex and unsafe.** This project wraps DPDK in safe Rust with a familiar `std::net` API, so you get kernel bypass without rewriting your application.

## Features

- **100% API-compatible** with `std::net::UdpSocket` and `tokio::net::UdpSocket`
- **Multiple backends**: DPDK (kernel bypass), AF_PACKET (raw sockets), AF_PACKET+MMAP (zero-copy)
- **Automatic fallback**: Works without DPDK installed (development, testing, CI)
- **Hardware offload**: IPv4/UDP checksum offloading on supported NICs
- **Protocol support**: ARP resolution, ICMP echo reply
- **Async runtime**: Full Tokio integration with poll-based API

## Quick Start

### As a Library

Replace your socket imports:

```rust
// Before
use std::net::UdpSocket;

// After — same API, DPDK-accelerated
use dpdk_udp::UdpSocket;

// Code stays identical
let socket = UdpSocket::bind("0.0.0.0:9000")?;
socket.send_to(b"hello", "192.168.1.100:9000")?;
```

For async:

```rust
// Before
use tokio::net::UdpSocket;

// After — same API, DPDK-accelerated
use dpdk_tokio::compat::tokio::UdpSocket;

// Code stays identical
let socket = UdpSocket::bind("0.0.0.0:9000").await?;
socket.send_to(b"hello", "192.168.1.100:9000").await?;
```

Backend selection is automatic: DPDK if available, otherwise AF_PACKET raw sockets.

### Running Examples

```bash
# Run async echo server (works anywhere, no DPDK required)
cargo run -p tokio-echo

# Test it
cargo run -p test-client -- --target 127.0.0.1 --port 9000
```

## Backend Selection

Three backends available (automatic selection by default):

| Backend | Requires | Performance | Use Case |
|---------|----------|-------------|----------|
| **DPDK** | DPDK installed, dedicated NIC | Highest (kernel bypass) | Production packet processing |
| **AF_PACKET+MMAP** | Linux raw sockets | High (zero-copy ring buffers) | Development, containers |
| **AF_PACKET** | Linux raw sockets | Medium (syscalls but no kernel stack) | Fallback, testing |

Configure explicitly:

```rust
use dpdk_udp::{UdpSocket, BackendConfig, BackendType};

let backend = BackendConfig {
    backend_type: BackendType::Dpdk,
    ..Default::default()
};
let socket = UdpSocket::bind_with_backend("0.0.0.0:9000", backend)?;
```

### NIC Port Detection

When you call `UdpSocket::bind()` (the simple API), the library uses **DPDK port 0** — the first NIC that DPDK enumerated during EAL initialization. DPDK discovers NICs by scanning the PCI bus for devices bound to a DPDK-compatible driver (`vfio-pci`, `igb_uio`, or `uio_pci_generic`). The order is deterministic: ports are numbered by PCI bus address, so the NIC at the lowest PCI address becomes port 0.

On most deployments this is the right choice — you bind one NIC to DPDK (leaving management traffic on a kernel-managed NIC), and port 0 is that NIC. On AWS EC2 with dual ENIs, the DPDK setup script binds only the secondary ENI to `vfio-pci`, so port 0 is always the data-plane NIC.

For multi-NIC DPDK setups (multiple NICs bound to DPDK drivers), use `BackendConfig` to select the port explicitly:

```rust
use dpdk_udp::{UdpSocket, BackendConfig};

// Use the second DPDK-managed NIC (port 1)
let backend = BackendConfig::new().with_dpdk(1);
let socket = UdpSocket::bind_with_backend("0.0.0.0:9000", backend)?;
```

You can query how many DPDK ports are available at runtime:

```rust
use dpdk::port::Port;

let count = Port::count_available();
println!("DPDK manages {} NIC ports", count);
```

### Advanced Backend Examples

NIC and backend selection is configured via `BackendConfig` in code. There is no CLI flag or environment variable for this — it is an API-level concern so that applications have full control over which NIC and backend they use.

```rust
use dpdk_udp::{UdpSocket, BackendConfig};

// DPDK on a specific port (e.g., second NIC)
let socket = UdpSocket::bind_with_backend(
    "0.0.0.0:9000",
    BackendConfig::new().with_dpdk(1),
)?;

// AF_PACKET raw socket on a named interface
let socket = UdpSocket::bind_with_backend(
    "0.0.0.0:9000",
    BackendConfig::new().with_raw_socket("eth1"),
)?;

// AF_PACKET with MMAP zero-copy ring buffers
let socket = UdpSocket::bind_with_backend(
    "0.0.0.0:9000",
    BackendConfig::new().with_raw_socket_mmap("eth1"),
)?;

// Combine routing, VLAN, and topology via the builder
use dpdk_udp::{NetworkConfig, VlanConfig};
use std::net::Ipv4Addr;

let socket = UdpSocket::builder()
    .network(
        NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 50), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
            .with_vlan(VlanConfig::new(100).access())
            .with_mtu(9001)
    )
    .bind("10.0.1.50:9000")?;

// Configure VLAN directly on an existing socket
let mut socket = UdpSocket::bind("0.0.0.0:9000")?;
socket.set_vlan(Some(VlanConfig::new(200).trunk(vec![100, 200], None)));
```

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│              Applications (echo, tokio-echo, test-client)        │
├──────────────────────────────────────────────────────────────────┤
│  dpdk-tokio   Async runtime, compat layer (std/tokio drop-ins)  │
├──────────────────────────────────────────────────────────────────┤
│  dpdk-udp     UdpSocket API, ARP, ICMP, packet parsing          │
│               ┌──────────────┬────────────────┬────────────────┐ │
│               │ DpdkBackend  │ RawSocket      │ RawSocket+MMAP │ │
├───────────────┴──────────────┴────────────────┴────────────────┤
│  dpdk         Safe wrapper (Port, Mbuf, Mempool, Queue)         │
├──────────────────────────────────────────────────────────────────┤
│  dpdk-sys     Raw FFI bindings + stubs (no DPDK required)       │
└──────────────────────────────────────────────────────────────────┘
                            │
                    ┌───────┴────────┐
                    │  DPDK Library  │  (optional, kernel bypass)
                    └────────────────┘
```

### Crate Breakdown

**dpdk-sys** — Raw FFI bindings generated by bindgen when DPDK is installed. Ships with full stub implementations so everything compiles and tests pass without DPDK. Build script auto-detects DPDK via `pkg-config`.

**dpdk** — Safe Rust wrappers around EAL initialization, Port configuration, Mbuf/Mempool management, and RX/TX queues. Handles hardware offload capability detection and NUMA-aware resource allocation.

**dpdk-udp** — The core networking crate. Contains:
- `UdpSocket` with the full `std::net::UdpSocket` API (19/19 methods)
- `PacketBackend` trait abstracting raw packet I/O across backends
- `DpdkBackend` — userspace DPDK with kernel bypass and direct mbuf writes
- `RawSocketBackend` — Linux AF_PACKET with optional PACKET_MMAP ring buffers
- ARP resolution (cache + handler) and ICMP echo reply, both backend-agnostic
- Topology detection and NUMA-aware resource allocation

**dpdk-tokio** — Async layer providing `tokio::net::UdpSocket`-compatible API with poll-based I/O. Includes a compat module (`dpdk_tokio::compat::tokio`) for zero-change migration from Tokio sockets.

### Packet Path

TX: `send_to()` → build frame → backend `send_frame()` → NIC.

RX: Backend `recv_frames()` → parse headers → ARP/ICMP inline → UDP payload to caller.

Two packet construction paths exist by design: `build_udp_packet(&mut Mbuf)` writes directly into DPDK mbufs (zero-copy), while `build_udp_frame() -> Vec<u8>` produces owned bytes for the generic backend path. Both emit identical wire-format frames.

### RX Drop Hierarchy

An incoming packet can be dropped at five distinct layers between the wire and
the application. When diagnosing loss, narrow down *which* layer is dropping
before touching code — the fix is different at each one. The perf instrumentation
(`[PERF]` log lines + perf-test harness) exposes counters at every layer we own,
and the comparison table surfaces them as **NIC Drops** and **App Drops** columns:

| # | Layer | Dropped because... | Counter | Column |
|---|---|---|---|---|
| 1 | Wire / NIC ingress | AWS ENA rate limiter, VPC shaping, bad cabling, upstream congestion | — (not owned by this stack) | inferred: `(TX − RX) − NIC Drops − App Drops` |
| 2 | NIC RX descriptor ring (HW) | Software polled too slowly → ring fills → NIC has nowhere to DMA new packets | `rte_eth_stats.imissed` | **NIC Drops** |
| 2b | NIC RX refill (HW) | Mempool exhausted → can't hand a free mbuf to the NIC for the next DMA | `rte_eth_stats.rx_nombuf` | **NIC Drops** |
| 3 | dpdk-udp worker ring (SW, multi-core) | Internal SpscRing between RX worker thread and app thread is full | `PerfCounters.rx_drops_ring_full` | **App Drops** |
| 4 | dpdk-udp `recv_queue` (SW, per-socket) | Per-socket `SO_RCVBUF`-equivalent (4096 pkts / 256 KiB) is full — app isn't calling `recv_from` fast enough | `PerfCounters.rx_drops_buffer_full` | **App Drops** |

**How to read the columns in perf reports:**

- **NIC Drops > 0, App Drops ≈ 0** → the poller isn't calling `rte_eth_rx_burst` fast enough to drain the HW ring (layer 2), or the mempool is too small (layer 2b). Fix: faster polling loop, larger mempool, more RX queues.
- **NIC Drops ≈ 0, App Drops > 0** → the packet made it into the Rust stack but got stuck in the worker ring (layer 3) or the socket buffer (layer 4). Fix: faster consumer / larger `recv_queue` cap / move work off the app thread.
- **Both ≈ 0 but RX < TX** → loss is at layer 1 (wire), which we can't directly count. Cross-reference with `native-dpdk` at the same rate to confirm it's environmental rather than something the stack is doing.
- **Both > 0** → backpressure is propagating from app layer down through the stack. Start with layer 4, work down.

For async backends, note that the `tokio-dpdk` compat layer adds a `spawn_blocking` hop per `recv_from`/`send_to` call, which caps throughput around 40K pps and makes layer 2 saturate easily under load. For raw throughput use the sync `dpdk_udp::UdpSocket` directly.

## Development

### Build and Test

```bash
# Build everything (works without DPDK - uses stubs)
cargo build

# Run 260+ unit tests (no DPDK required)
cargo test

# Run specific crate tests
cargo test -p dpdk-udp
```

### Local Development Setup

No DPDK installation needed. The stub system provides mock implementations so all tests pass on macOS, Linux, or CI without dedicated hardware.

### Integration Testing

For changes touching networking or backends:

```bash
# Validate locally + trigger EC2 integration tests
./scripts/ci-validate.sh
```

This runs:
1. `cargo build && cargo test` locally
2. Pushes your branch
3. Triggers GitHub Actions workflow on real EC2 DPDK hardware
4. Waits for results (exits non-zero on failure)

**Do not create a PR until this passes.**

### Contributing

1. Create a feature branch: `git checkout -b feature/my-change`
2. Make changes with tests
3. Run `./scripts/ci-validate.sh` to validate
4. Push and create PR

See `API_COMPATIBILITY.md` for API tracking.

## Performance

Benchmarked on AWS c5n.2xlarge (8 vCPU, 25 Gbps ENA) using TRex traffic generator. Each test runs 30 seconds per rate step. "rust-dpdk" is this library with the DPDK backend; "kernel" is `std::net::UdpSocket`.

### 64-byte packets (worst case for kernel — max packet rate per byte)

| Target PPS | rust-dpdk RX | Drop | Kernel RX | Drop |
|-----------|-------------|------|----------|------|
| 70,000 | 70,000 | 0% | 69,000 | 1.4% |
| 140,000 | 140,000 | 0% | 138,996 | 0.7% |
| 350,000 | 349,903 | 0.03% | 327,975 | 6.3% |
| 700,000 | 678,563 | 3.1% | 342,265 | 51.1% |

### 512-byte packets

| Target PPS | rust-dpdk RX | Drop | Kernel RX | Drop |
|-----------|-------------|------|----------|------|
| 70,000 | 70,000 | 0% | 69,000 | 1.4% |
| 140,000 | 139,992 | 0.01% | 138,968 | 0.7% |
| 350,000 | 349,864 | 0.04% | 289,761 | 17.2% |
| 700,000 | 638,416 | 8.8% | 324,749 | 53.6% |

### 1400-byte packets (near MTU)

| Target PPS | rust-dpdk RX | Drop | Kernel RX | Drop |
|-----------|-------------|------|----------|------|
| 70,000 | 70,000 | 0% | 68,996 | 1.4% |
| 140,000 | 139,953 | 0.03% | 138,972 | 0.7% |
| 350,000 | 350,000 | 0% | 283,868 | 18.9% |
| 700,000 | 447,693 | 36.0% | 309,586 | 55.8% |

**Key takeaway**: At 350K PPS, DPDK handles all three packet sizes with near-zero drops while the kernel drops 6-19%. At 700K PPS, DPDK delivers ~2x the throughput of kernel sockets consistently across runs. The advantage is most pronounced at high packet rates where kernel overhead dominates.

See `docs/perf-test-log.md` for detailed benchmark history across optimization phases.

## Scope and Limitations

### What This Is

A **high-performance UDP endpoint library** that replaces `std::net::UdpSocket` with DPDK kernel bypass. Designed for applications that are the **source or destination** of UDP traffic and need maximum packet throughput with minimum latency. Think: DNS servers, game servers, telemetry collectors, financial feed handlers, echo/relay services.

### What This Is Not

This is **not a general-purpose network stack**. It does not replace the Linux kernel's networking subsystem. It is not a router, firewall, load balancer, or network function. Applications that need full TCP/IP semantics, connection tracking, netfilter integration, or namespace isolation should use the kernel.

### What's Implemented

| Feature | Status | Notes |
|---------|--------|-------|
| `std::net::UdpSocket` API | 19/19 methods | Full API compatibility |
| `tokio::net::UdpSocket` API | Complete | All async + poll methods |
| IPv4 UDP send/receive | Complete | Build and parse Ethernet/IPv4/UDP frames |
| ARP resolution | Complete | Cache with atomic fast-path, auto-request, kernel ARP seeding, gratuitous ARP on bind |
| ICMP echo reply | Complete | Auto-responds to ping |
| ICMP error handling | Complete | Dest Unreachable, Time Exceeded, etc. queued via `take_error()` |
| Hardware checksum offload | Complete | TX offload on capable NICs, RX validation on all packets |
| Hardware VLAN offload | Complete | NIC inserts/strips 802.1Q tags, software fallback, force-software option |
| Multiple backends | 3 backends | DPDK, AF_PACKET, AF_PACKET+MMAP |
| Ephemeral port allocation | Complete | Linux-compatible range (32768-60999) |
| RX backpressure + drop counters | Complete | `SO_RCVBUF`-style byte limit, atomic `recv_drops()`, 256 KiB default |
| Multicast join/leave | Basic | IPv4 only, simplified group tracking |
| Connected socket filtering | Complete | Buffers non-matching packets |
| Socket timeouts | Complete | Read and write deadlines |

### What's Not Implemented

The Linux kernel's UDP path (`net/ipv4/udp.c` and surrounding infrastructure) handles significantly more than raw packet I/O. The following kernel features have **no equivalent** in this library. Planned items are listed first, matching the Roadmap order below.

| Feature | Kernel | Us | Impact |
|---------|--------|-----|--------|
| **IPv6** | Full dual-stack | IPv4 only | Planned |
| **UDP encapsulation (VXLAN/GUE/GENEVE)** | Tunnel endpoint support | None | Planned |
| **IP fragmentation/reassembly** | Full fragment/reassembly | DF always set, packets > 1472 bytes rejected | Not planned |
| **SO_REUSEPORT** | Multiple sockets share a port with BPF-programmable steering | One socket per port | Not planned |
| **GSO/GRO** | Batch segmentation/coalescing for bulk transfers | Single-packet TX/RX | Not planned |
| **Netfilter / iptables** | Full hook chain (PREROUTING through POSTROUTING) | None — DPDK bypasses kernel entirely | Not planned |
| **Network namespaces** | Per-namespace socket/routing isolation | None | Not planned |
| **BPF/XDP** | Programmable packet processing at NIC driver level | None | Not planned |
| **TOS/DSCP** | `IP_TOS` socket option | Always 0x00 | Not planned |
| **Cork / MSG_MORE** | Accumulate multiple writes into one datagram | None | Not planned |

### Current Environment Assumptions

Integration testing runs on **AWS EC2 with VPC networking**, which has specific properties that simplify our implementation:

- AWS VPC is **L3-routed, not L2-switched** — all traffic (even same-subnet) transits a virtual router
- ARP always resolves to the **VPC gateway MAC**, never the peer's actual MAC
- No VLANs at the VPC level (our VLAN support is for non-AWS environments), no broadcast domains, no real L2 switching
- Gateway is always at `subnet_base + 1` (e.g., `10.0.1.1`)

**On physical hardware**, the subnet-aware routing table handles L2/L3 routing decisions automatically. On Linux, `UdpSocket::bind()` auto-detects the subnet, gateway, and ARP entries from `/proc/net/route` and `/proc/net/arp`. For non-standard topologies, use `NetworkConfig` to configure routing explicitly. See `docs/routing.md`.

## Roadmap

### Done

**Subnet-aware routing** — Subnet mask awareness with longest-prefix-match static routes, configurable default gateway, and MTU. Auto-detects subnet/gateway from `/proc/net/route` and seeds ARP cache from `/proc/net/arp` on Linux. Falls back to passthrough when detection fails. The library now runs correctly on bare-metal servers and on-premises environments. See `docs/routing.md`.

**Jumbo frames** — Configurable MTU via `NetworkConfig` (up to 9001 bytes). `send_to()` rejects payloads exceeding the MTU-derived limit. TxBuffer is always sized for jumbo frames to avoid reallocation.

**RX checksum validation** — IPv4 header and UDP checksums are verified in software on every received packet. Packets with corrupted headers or payloads are silently dropped before reaching the application. Handles UDP checksum-disabled (0) per RFC 768.

**TX hardware checksum offload** — When the NIC supports `CHECKSUM_PARTIAL` mode (e.g., ENA on AWS), the DPDK backend sets mbuf offload flags (`RTE_MBUF_F_TX_IP_CKSUM`, `RTE_MBUF_F_TX_UDP_CKSUM`) and writes the pseudo-header checksum so the NIC computes final checksums. Falls back to software checksums on NICs without offload or on non-DPDK backends.

**RX backpressure and drop counters** — Socket-level receive buffer accounting with a configurable byte limit (SO_RCVBUF equivalent) and lock-free atomic drop counters. Applications call `set_recv_buffer_size(bytes)` to tune the limit and `recv_drops()` to read a `RecvDropStats { packets, bytes }` snapshot for production monitoring. Drops are also surfaced via the existing `rx_drops_buffer_full` perf counter and rolled into the `rx_drops` rate on the perf reporter. Default is 256 KiB, mirroring Linux `net.core.rmem_default`.

**Gratuitous ARP** — Broadcasts an unsolicited ARP announcement on `bind()` so switches and routers learn the socket's MAC/IP mapping immediately, without waiting for inbound ARP requests. Configurable via `set_auto_garp(bool)` (enabled by default). Also available on-demand via `send_gratuitous_arp()` for failover or IP migration scenarios.

**ICMP error handling** — ICMP error messages (Destination Unreachable, Time Exceeded, Redirect, Parameter Problem) are parsed and matched back to the originating socket using the embedded original datagram header. Errors are queued per-socket and surfaced via `take_error()`, mirroring Linux `SO_ERROR` behavior. Supported error types: Port/Host/Network Unreachable (`ConnectionRefused`), Fragmentation Needed with Next-Hop MTU (`Other`), TTL Exceeded (`TimedOut`), Admin Prohibited (`PermissionDenied`), and Parameter Problem (`InvalidData`). The error queue is bounded (16 entries) to prevent ICMP flood amplification.

**VLAN (802.1Q)** — Full 802.1Q VLAN tag insert/strip with three operating modes matching Linux 8021q subinterface semantics. **Access mode**: RX accepts untagged + matching VID (strips tag), TX sends untagged. **Trunk mode**: RX accepts frames tagged with any VID in an allowed set (optional native VLAN for untagged), TX tags. **PortTagging mode** (default): RX only accepts matching VID (strips tag, drops untagged), TX always tags. Configurable per-socket via `set_vlan(Some(VlanConfig::new(100).access()))` or through `NetworkConfig::with_vlan()` on the builder. All protocol handlers (ARP, ICMP, UDP) handle VLAN-tagged frames. Checksum verification works correctly with VLAN-tagged frames.

**Hardware VLAN offload** — NIC-assisted VLAN tag insert (TX) and strip (RX) when the hardware supports it, following the same pattern as checksum offload. NIC capabilities are queried at port init (`RTE_ETH_TX_OFFLOAD_VLAN_INSERT`, `RTE_ETH_RX_OFFLOAD_VLAN_STRIP`). On TX, the DPDK backend sets `mbuf.vlan_tci` and `RTE_MBUF_F_TX_VLAN` so the NIC inserts the 802.1Q tag on the wire. On RX, the NIC strips the tag into `mbuf.vlan_tci` with `RTE_MBUF_F_RX_VLAN_STRIPPED`; the software stack re-inserts it for uniform VLAN filtering. Falls back to software insert/strip on NICs without support or non-DPDK backends. Configurable via `VlanConfig::with_force_software(true)` to force software mode even when hardware offload is available. Offload status queryable via `has_tx_vlan_offload()` / `has_rx_vlan_offload()`.

### Planned

**IPv6** — Full dual-stack support. IPv6 is required for modern networks and public-facing services. Includes NDP (Neighbor Discovery Protocol) to replace ARP, ICMPv6, and IPv6 header construction/parsing throughout the stack.

**UDP encapsulation (VXLAN/GUE/GENEVE)** — Support for UDP-based tunnel protocols. Enables the library to serve as a high-performance tunnel endpoint for overlay networks, which is a natural extension of DPDK's kernel-bypass advantage.

### Not Currently Planned

These are features the Linux kernel provides that we intentionally defer to the network infrastructure or consider out of scope:

- **IP fragmentation/reassembly** — Modern networks use PMTUD; fragmentation is rare and problematic
- **SO_REUSEPORT** — Use RSS to steer traffic to dedicated queues instead
- **GSO/GRO** — DPDK's `rx_burst`/`tx_burst` already amortize per-packet costs
- **Netfilter / iptables** — Rely on external filtering (Security Groups, hardware ACLs, upstream firewalls)
- **Network namespaces** — Container isolation is a kernel concern
- **BPF/XDP** — Not applicable to userspace DPDK. Hardware filtering is possible via DPDK's `rte_flow` API (FFI bindings exist but no safe wrapper yet — could be exposed if there is demand)
- **TOS/DSCP** — Trivial to add when needed; most DPDK deployments use dedicated NICs where QoS is handled by the network
- **Cork / MSG_MORE** — Scatter-gather send; low priority since DPDK's `tx_burst` already batches at the NIC level

If you think a feature should be included, open an issue or feel free to cut a PR.

## DPDK Installation (Optional)

Development and testing work without DPDK. For production kernel bypass:

### Amazon Linux 2023

```bash
sudo ./scripts/install_dpdk_amazon_linux.sh
```

This installs DPDK 23.11 and configures hugepages.

### Verify DPDK

```bash
# Run the echo server (uses real DPDK when installed, stubs otherwise)
cargo run -p echo -- --ip 0.0.0.0 --port 9000
```

### Platform Support

| Platform | Stub Mode | Real DPDK | Notes |
|----------|-----------|-----------|-------|
| macOS    | Yes       | No        | DPDK 23.11+ lacks macOS support |
| Linux    | Yes       | Yes       | Full DPDK functionality |
| Windows  | No        | No        | Not implemented |

## AWS Deployment

Deploy test infrastructure to EC2:

```bash
cd deploy/cdk
npm install
cdk deploy --profile your-aws-profile
```

This creates:
- 2x c6gn.large instances (sender/receiver)
- Dual ENIs (management + DPDK)
- SSM access (no SSH keys needed)

See `deploy/README.md` for details.

## License

MIT License - see LICENSE file for details.
