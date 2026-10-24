# dpdk-stdlib-rust

[![dpdk-stdlib-udp](https://img.shields.io/crates/v/dpdk-stdlib-udp.svg?label=dpdk-stdlib-udp)](https://crates.io/crates/dpdk-stdlib-udp)
[![dpdk-stdlib-tcp](https://img.shields.io/crates/v/dpdk-stdlib-tcp.svg?label=dpdk-stdlib-tcp)](https://crates.io/crates/dpdk-stdlib-tcp)
[![dpdk-stdlib-quic](https://img.shields.io/crates/v/dpdk-stdlib-quic.svg?label=dpdk-stdlib-quic)](https://crates.io/crates/dpdk-stdlib-quic)
[![dpdk-stdlib-tokio](https://img.shields.io/crates/v/dpdk-stdlib-tokio.svg?label=dpdk-stdlib-tokio)](https://crates.io/crates/dpdk-stdlib-tokio)
[![dpdk-stdlib](https://img.shields.io/crates/v/dpdk-stdlib.svg?label=dpdk-stdlib)](https://crates.io/crates/dpdk-stdlib)
[![Rust](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/rust.yml/badge.svg?branch=development)](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/rust.yml?query=branch%3Adevelopment)
[![Integration Tests (x86, nightly)](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/integration-tests.yml/badge.svg?branch=development&event=schedule)](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/integration-tests.yml?query=event%3Aschedule)
[![Integration Tests (Graviton, nightly)](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/integration-tests-graviton.yml/badge.svg?branch=development&event=schedule)](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/integration-tests-graviton.yml?query=event%3Aschedule)
[![QUIC Integration (weekly)](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/quic-integration-tests.yml/badge.svg?branch=development&event=schedule)](https://github.com/gspivey/dpdk-stdlib-rust/actions/workflows/quic-integration-tests.yml?query=event%3Aschedule)

Drop-in DPDK-accelerated replacements for Rust's UDP and TCP sockets (`std::net` and `tokio::net`), plus a native DPDK I/O provider for [s2n-quic](https://github.com/aws/s2n-quic). Bypass the Linux kernel network stack for high-throughput packet processing, with automatic fallback when DPDK is unavailable.

## Why

Traditional Linux networking routes every packet through the kernel: syscalls, context switches, interrupts, and the full TCP/IP stack. For high-packet-rate workloads (DNS servers, load balancers, packet processors), this overhead becomes the bottleneck.

DPDK (Data Plane Development Kit) bypasses the kernel entirely using userspace drivers and polling. This eliminates syscalls and context switches, achieving:

- **~2x higher packet throughput** at saturation (700K PPS: DPDK delivers ~640-680K RX while kernel delivers ~310-340K)
- **Zero packet drops** up to 350K PPS where the kernel starts dropping
- **Zero kernel overhead** for packet I/O — no syscalls, no context switches

**But DPDK's C API is complex and unsafe.** This project wraps DPDK in safe Rust with a familiar `std::net` API, so you get kernel bypass without rewriting your application.

## Features

- **UDP**: 100% API-compatible with `std::net::UdpSocket` and `tokio::net::UdpSocket`, IPv4 and IPv6
- **TCP** (IPv4): userspace TCP stack with drop-in `TcpStream`/`TcpListener` for `std::net` and `tokio::net`
- **QUIC** (IPv4): `s2n_quic::provider::io::Provider` over DPDK, no Tokio in the I/O path
- **Multiple backends**: DPDK (kernel bypass), AF_PACKET (raw sockets), AF_PACKET+MMAP (zero-copy)
- **Automatic fallback**: Works without DPDK installed (development, testing, CI)
- **Hardware offload**: IPv4/IPv6 UDP checksum and VLAN offload on supported NICs
- **Protocol support**: ARP, NDP, ICMP/ICMPv6, 802.1Q VLAN, GUE/VXLAN/GENEVE tunnel endpoints
- **Tested on real hardware**: every PR runs on AWS EC2 x86 (c5n) and Graviton (c7g) with DPDK

## Quick Start

### Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
# Sync UDP (std::net::UdpSocket drop-in)
dpdk-udp = { version = "0.3", package = "dpdk-stdlib-udp" }

# Async UDP and TCP (tokio::net drop-ins)
dpdk-tokio = { version = "0.3", package = "dpdk-stdlib-tokio", features = ["dpdk"] }

# Sync TCP (std::net::TcpStream / TcpListener drop-in)
dpdk-stdlib-tcp = "0.3"

# QUIC (s2n-quic I/O provider)
dpdk-stdlib-quic = "0.3"
```

The `package` rename keeps your `use` statements short: `use dpdk_udp::UdpSocket` and `use dpdk_tokio::compat::tokio::UdpSocket`.

| Crate | What it is |
|-------|------------|
| [`dpdk-stdlib-udp`](https://crates.io/crates/dpdk-stdlib-udp) | `UdpSocket` drop-in, backends, ARP/NDP/ICMP, tunnels |
| [`dpdk-stdlib-tcp`](https://crates.io/crates/dpdk-stdlib-tcp) | Userspace TCP stack: `TcpStream`, `TcpListener` |
| [`dpdk-stdlib-quic`](https://crates.io/crates/dpdk-stdlib-quic) | DPDK I/O provider for s2n-quic |
| [`dpdk-stdlib-tokio`](https://crates.io/crates/dpdk-stdlib-tokio) | Async UDP and TCP drop-ins for `tokio::net` |
| [`dpdk-stdlib-net`](https://crates.io/crates/dpdk-stdlib-net) | Shared `PacketBackend` trait and backends |
| [`dpdk-stdlib`](https://crates.io/crates/dpdk-stdlib) | Safe DPDK wrappers (EAL, Port, Mbuf, Mempool) |
| [`dpdk-stdlib-sys`](https://crates.io/crates/dpdk-stdlib-sys) | Raw DPDK FFI bindings, with stubs when DPDK is absent |

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

#### TCP

Start the DPDK TCP runtime once, then use `TcpListener`/`TcpStream` like `std::net`:

```rust
use dpdk_stdlib_tcp::{init_dpdk_tcp_context, DpdkTcpRuntimeConfig, TcpListener};
use std::io::{Read, Write};

init_dpdk_tcp_context(DpdkTcpRuntimeConfig {
    local_ip: "10.0.1.50".parse().unwrap(),
    gateway_mac: Some([0x02, 0xff, 0xec, 0x01, 0x65, 0x2d]), // AWS VPC: always the gateway MAC
    ..Default::default()
})?;

let listener = TcpListener::bind("10.0.1.50:8080")?;
for stream in listener.incoming() {
    let mut stream = stream?;
    let mut buf = [0u8; 1460];
    let n = stream.read(&mut buf)?;
    stream.write_all(&buf[..n])?;
}
```

Async TCP uses `dpdk_tokio::compat::{TcpStream, TcpListener}` (same API as `tokio::net`). IPv4 goes through DPDK; IPv6 falls back to Tokio.

#### QUIC

`dpdk-stdlib-quic` plugs into s2n-quic as its I/O provider:

```rust
use dpdk_stdlib_quic::DpdkProvider;
use s2n_quic::Server;

let (provider, _handle) = DpdkProvider::builder()
    .with_addr("10.0.1.50:4433".parse()?)
    .with_gateway_mac([0x02, 0xff, 0xec, 0x01, 0x65, 0x2d])
    .build();

let mut server = Server::builder()
    .with_tls((cert_pem, key_pem))?
    .with_io(provider)?
    .start()?;

while let Some(conn) = server.accept().await {
    // handle the connection
}
```

The crate pins `s2n-quic = "=1.81.0"`, because the provider implements s2n-quic's lower-level I/O traits.

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
│     Applications (echo, tokio-echo, tcp-echo, quic-echo-server)  │
├──────────────────────────────────────────────────────────────────┤
│  dpdk-stdlib-tokio  Async compat: UDP + TCP tokio::net drop-ins  │
├─────────────────────┬────────────────────┬───────────────────────┤
│  dpdk-stdlib-quic   │  dpdk-stdlib-udp   │  dpdk-stdlib-tcp      │
│  s2n-quic provider  │  UdpSocket, ARP,   │  TCP engine, sockets  │
│                     │  NDP, ICMP, tunnels│                       │
├─────────────────────┴────────────────────┴───────────────────────┤
│  dpdk-stdlib-net    PacketBackend trait                          │
│               ┌──────────────┬────────────────┬────────────────┐ │
│               │ DpdkBackend  │ RawSocket      │ RawSocket+MMAP │ │
├───────────────┴──────────────┴────────────────┴────────────────┴─┤
│  dpdk-stdlib        Safe wrapper (Port, Mbuf, Mempool, Queue)    │
├──────────────────────────────────────────────────────────────────┤
│  dpdk-stdlib-sys    Raw FFI bindings + stubs (no DPDK required)  │
└──────────────────────────────────────────────────────────────────┘
                            │
                    ┌───────┴────────┐
                    │  DPDK Library  │  (optional, kernel bypass)
                    └────────────────┘
```

### Crate Breakdown

**dpdk-stdlib-sys** — Raw FFI bindings generated by bindgen when DPDK is installed. Ships with full stub implementations so everything compiles and tests pass without DPDK. Build script auto-detects DPDK via `pkg-config`.

**dpdk-stdlib** — Safe Rust wrappers around EAL initialization, Port configuration, Mbuf/Mempool management, and RX/TX queues. Handles hardware offload capability detection and NUMA-aware resource allocation.

**dpdk-stdlib-net** — The `PacketBackend` trait and its implementations, shared by UDP, TCP and QUIC:
- `DpdkBackend` — userspace DPDK with kernel bypass and direct mbuf writes
- `RawSocketBackend` — Linux AF_PACKET with optional PACKET_MMAP ring buffers

**dpdk-stdlib-udp** — The UDP crate. Contains:
- `UdpSocket` with the full `std::net::UdpSocket` API (19/19 methods), IPv4 and IPv6
- ARP/NDP resolution and ICMP/ICMPv6, all backend-agnostic
- VLAN, subnet-aware routing, and GUE/VXLAN/GENEVE tunnel endpoints

**dpdk-stdlib-tcp** — A userspace TCP stack (IPv4). A dedicated engine thread runs the TCP state machine (handshake, reordering, Nagle/delayed ACK, RTO, persist, keepalive, TIME_WAIT, Reno congestion control) and talks to `TcpStream`/`TcpListener` over SPSC rings.

**dpdk-stdlib-quic** — Implements `s2n_quic::provider::io::Provider` (IPv4). A busy-poll event loop thread drives the s2n-quic endpoint straight from `rx_burst`/`tx_burst`, with ECN and GSO.

**dpdk-stdlib-tokio** — Async layer: `tokio::net::UdpSocket`, `TcpStream` and `TcpListener` drop-ins under `dpdk_tokio::compat`, for zero-change migration from Tokio sockets.

### Packet Path

TX: `send_to()` → build frame → backend `send_frame()` → NIC.

RX: Backend `recv_frames()` → parse headers → ARP/ICMP inline → UDP payload to caller.

Two packet construction paths exist by design: `build_udp_packet(&mut Mbuf)` writes directly into DPDK mbufs (zero-copy), while `build_udp_frame() -> Vec<u8>` produces owned bytes for the generic backend path. Both emit identical wire-format frames.

### NIC Hardware Offloads

DPDK represents every in-flight packet as an `rte_mbuf` — a metadata header that sits in front of the packet data in a contiguous memory region:

```
┌─────────────────────────────────────────────┐
│  rte_mbuf (metadata header)                 │
│  ├─ ol_flags:     u64  (offload flags)      │
│  ├─ vlan_tci:     u16  (VLAN tag)           │
│  ├─ tx_offload:   u64  (packed bit-field)   │
│  │   ├─ l2_len:   7 bits  (Ethernet hdr)    │
│  │   ├─ l3_len:   9 bits  (IP hdr)          │
│  │   └─ l4_len:   8 bits  (UDP/TCP hdr)     │
│  ├─ data_len:     u16                       │
│  └─ ...                                     │
├─────────────────────────────────────────────┤
│  Packet data (frame bytes)                  │
│  [dst MAC | src MAC | ethertype | IP | UDP  │
│   | payload ...]                            │
└─────────────────────────────────────────────┘
```

Hardware offloads work by reading/writing mbuf metadata fields instead of modifying packet bytes. The NIC performs the actual work at line rate in hardware, driven entirely by what the software writes to these metadata fields.

**Checksum offload (TX):** The software builds the frame with a zeroed IPv4 checksum field and a pseudo-header checksum in the UDP checksum field, then sets mbuf metadata telling the NIC where each header starts:

```
mbuf.tx_offload  = l2_len=14, l3_len=20, l4_len=8
mbuf.ol_flags   |= RTE_MBUF_F_TX_IPV4           (this is an IPv4 packet)
                 | RTE_MBUF_F_TX_IP_CKSUM        (compute IPv4 header checksum)
                 | RTE_MBUF_F_TX_UDP_CKSUM       (compute UDP checksum)
```

The NIC reads `tx_offload` to locate the checksum fields in the packet data, computes the correct values, and writes them directly into the frame as it goes out on the wire. Software never touches the final checksum — it's computed in hardware at line rate.

**VLAN offload (TX):** The software builds an **untagged** frame (no 0x8100 tag in the bytes) and sets the VLAN TCI in mbuf metadata:

```
mbuf.vlan_tci    = 100                           (VID=100, PCP=0, DEI=0)
mbuf.ol_flags   |= RTE_MBUF_F_TX_VLAN           (insert 802.1Q tag)
```

The NIC inserts the 4-byte VLAN tag (`[0x8100 | TCI]`) between the source MAC and ethertype as the frame leaves the wire. The packet data buffer is never modified.

**VLAN offload (RX):** When the NIC receives a VLAN-tagged frame, it strips the 4-byte tag before writing the frame to memory and stores the tag in mbuf metadata:

```
mbuf.vlan_tci    = 100                           (stripped VID)
mbuf.ol_flags   |= RTE_MBUF_F_RX_VLAN_STRIPPED  (tag was removed from frame)
```

The packet data in the buffer is untagged (ethertype is `0x0800` for IPv4, not `0x8100`), but the VLAN ID is available from `mbuf.vlan_tci`. Our RX path passes this directly to the VLAN filtering logic — no frame reconstruction or extra allocation needed.

Both offloads fall back to software automatically when the NIC doesn't support them. Query support at runtime via `has_tx_ipv4_cksum_offload()`, `has_tx_vlan_offload()`, etc.

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

# Run 1,000+ unit and property tests (no DPDK required)
cargo test

# Run specific crate tests
cargo test -p dpdk-stdlib-udp
```

### Local Development Setup

No DPDK installation needed. The stub system provides mock implementations so all tests pass on macOS, Linux, or CI without dedicated hardware.

### Integration Testing

Integration tests run on real DPDK hardware: a pair of AWS EC2 instances per run, x86 (c5n.large) and Graviton (c7g.large).

| Workflow | Runs | Covers |
|----------|------|--------|
| Integration Tests (x86, Graviton) | PRs touching code, CI or deploy files; nightly on `development` | UDP tiers 1-3 (x86 also TCP: handshake, echo, shutdown, flow control, retransmit, kernel interop, std parity) |
| QUIC Integration Tests | PRs touching code; weekly on `development` | QUIC handshake + bidirectional echo |

TCP and QUIC are blocking: a failure fails CI. Nightlies skip a commit that has already passed. To run the suite by hand:

```bash
# Validate locally + trigger EC2 integration tests
./scripts/ci-validate.sh
```

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

A **high-performance endpoint library** that replaces Rust's UDP and TCP sockets, and s2n-quic's I/O, with DPDK kernel bypass. Designed for applications that are the **source or destination** of traffic and need maximum packet throughput with minimum latency. Think: DNS servers, game servers, telemetry collectors, financial feed handlers, echo/relay services, QUIC endpoints.

### What This Is Not

This is **not a general-purpose network stack**. It does not replace the Linux kernel's networking subsystem. It is not a router, firewall, load balancer, or network function. Applications that need connection tracking, netfilter integration, or namespace isolation should use the kernel.

### What's Implemented

**Core API & Backends**

| Feature | Status | Notes |
|---------|--------|-------|
| `std::net::UdpSocket` API | 19/19 methods | Full API compatibility |
| `tokio::net::UdpSocket` API | Complete | All async + poll methods |
| Multiple backends | 3 backends | DPDK, AF_PACKET, AF_PACKET+MMAP |
| Hardware checksum offload | Complete | TX NIC offload (`IP_CKSUM`, `UDP_CKSUM`), RX software validation · [#32](https://github.com/gspivey/dpdk-stdlib-rust/pull/32) |
| Hardware VLAN offload | Complete | NIC tag insert/strip, software fallback, force-software option · [#37](https://github.com/gspivey/dpdk-stdlib-rust/pull/37) |
| Ephemeral port allocation | Complete | Linux-compatible range (32768–60999) |
| Socket timeouts | Complete | Read and write deadlines |
| RX backpressure + drop counters | Complete | `SO_RCVBUF`-style byte limit, atomic `recv_drops()`, 256 KiB default · [#33](https://github.com/gspivey/dpdk-stdlib-rust/pull/33) |
| Connected socket filtering | Complete | Buffers non-matching packets |
| Multicast join/leave | Basic | IPv4 only, simplified group tracking |

**IPv4**

| Feature | Status | Notes |
|---------|--------|-------|
| IPv4 UDP send/receive | Complete | Ethernet/IPv4/UDP frame build and parse |
| ARP resolution | Complete | Cache, auto-request, kernel ARP seeding, gratuitous ARP on bind · [#34](https://github.com/gspivey/dpdk-stdlib-rust/pull/34) |
| ICMP echo reply | Complete | Auto-responds to ping |
| ICMP error handling | Complete | Dest Unreachable, Time Exceeded, etc. via `take_error()` · [#35](https://github.com/gspivey/dpdk-stdlib-rust/pull/35) |
| 802.1Q VLAN | Complete | Access/Trunk/PortTagging modes, all protocol handlers covered · [#36](https://github.com/gspivey/dpdk-stdlib-rust/pull/36) |
| Subnet-aware routing | Complete | LPM routes, OS auto-detect from `/proc/net/route`, configurable gateway/MTU · [#29](https://github.com/gspivey/dpdk-stdlib-rust/pull/29) [#30](https://github.com/gspivey/dpdk-stdlib-rust/pull/30) |
| Jumbo frames | Complete | Configurable MTU up to 9001 bytes · [#32](https://github.com/gspivey/dpdk-stdlib-rust/pull/32) |

**IPv6**

| Feature | Status | Notes |
|---------|--------|-------|
| IPv6 header build/parse | Complete | 40-byte fixed header, extension-header chain walk · [#49](https://github.com/gspivey/dpdk-stdlib-rust/pull/49) |
| IPv6 UDP send/receive | Complete | `SocketAddrV6` through all socket methods, `only_v6` option · [#62](https://github.com/gspivey/dpdk-stdlib-rust/pull/62) |
| IPv6 UDP checksum | Complete | Mandatory pseudo-header checksum, zero-checksum rejection per RFC 8200 · [#61](https://github.com/gspivey/dpdk-stdlib-rust/pull/61) |
| IPv6 hardware offload | Complete | `RTE_MBUF_F_TX_IPV6` + `UDP_CKSUM`, RX `PKT_RX_L4_CKSUM_GOOD` · [#55](https://github.com/gspivey/dpdk-stdlib-rust/pull/55) |
| IPv6 link-local / scope IDs | Complete | `fe80::/10` handling, `%ifindex` scope, solicited-node multicast MAC · [#54](https://github.com/gspivey/dpdk-stdlib-rust/pull/54) |
| NDP | Complete | Neighbor Solicitation/Advertisement, atomic cache, gratuitous NA on bind, kernel seeding · [#59](https://github.com/gspivey/dpdk-stdlib-rust/pull/59) |
| ICMPv6 echo reply | Complete | Auto-responds to `ping6` · [#56](https://github.com/gspivey/dpdk-stdlib-rust/pull/56) |
| ICMPv6 error handling | Complete | Dest Unreachable, Packet Too Big (with MTU), Time Exceeded · [#58](https://github.com/gspivey/dpdk-stdlib-rust/pull/58) |

**Tunneling**

| Feature | Status | Notes |
|---------|--------|-------|
| GUE endpoint | Complete | RFC 8470 L3-over-UDP, IPv4/IPv6 outer · [#42](https://github.com/gspivey/dpdk-stdlib-rust/pull/42) |
| VXLAN endpoint | Complete | RFC 7348, 24-bit VNI, inner Ethernet, per-VNI filtering · [#50](https://github.com/gspivey/dpdk-stdlib-rust/pull/50) |
| GENEVE endpoint | Complete | RFC 8926, 24-bit VNI, TLV options, inner Ethernet · [#51](https://github.com/gspivey/dpdk-stdlib-rust/pull/51) |
| IPv6 outer for all encap protocols | Complete | VXLAN/GENEVE/GUE with IPv6 outer headers, mandatory UDP6 checksum · [#60](https://github.com/gspivey/dpdk-stdlib-rust/pull/60) |

**TCP** (`dpdk-stdlib-tcp`, IPv4)

| Feature | Status | Notes |
|---------|--------|-------|
| `std::net::TcpStream` / `TcpListener` | Complete | Connect, accept, read/write, shutdown, timeouts, options, `split` |
| `tokio::net::TcpStream` / `TcpListener` | Complete | `AsyncRead`/`AsyncWrite` in `dpdk-stdlib-tokio`; IPv6 falls back to Tokio |
| Engine | Complete | Handshake, out-of-order reassembly, FIN/RST, Nagle, delayed ACK, SWS avoidance, RTO, persist, keepalive, TIME_WAIT |
| Congestion control | Complete | Reno with fast retransmit |
| EC2 integration | Complete, blocking | Handshake, echo, shutdown, flow control, retransmit, interop with kernel `ncat`/`iperf3`, `std` parity |

**QUIC** (`dpdk-stdlib-quic`, IPv4)

| Feature | Status | Notes |
|---------|--------|-------|
| s2n-quic `io::Provider` | Complete | Busy-poll event loop, no Tokio in the I/O path |
| ECN and GSO | Complete | TOS/ECN marking on TX and RX, GSO segmentation |
| EC2 integration | Complete, blocking | Handshake + bidirectional echo, DPDK ↔ DPDK |

### What's Not Implemented

The following features are absent or incomplete. TCP and QUIC over IPv6 are the next planned additions; the rest are intentional scope exclusions.

| Feature | Status | Notes |
|---------|--------|-------|
| **TCP over IPv6** | Planned | IPv4 only today — see [ROADMAP.md](ROADMAP.md) |
| **QUIC over IPv6** | Planned | IPv4 only today — see [ROADMAP.md](ROADMAP.md) |
| **IP fragmentation/reassembly** | Not planned | Modern networks use PMTUD; DF always set |
| **SO_REUSEPORT** | Not planned | Use RSS queues for multi-socket steering |
| **GSO/GRO for UDP sockets** | Not planned | `rx_burst`/`tx_burst` amortizes per-packet costs |
| **Netfilter / iptables** | Not planned | DPDK bypasses kernel; use Security Groups / upstream ACLs |
| **Network namespaces** | Not planned | Container isolation is a kernel concern |
| **BPF/XDP** | Not planned | Not applicable to userspace DPDK |
| **TOS/DSCP** | Not planned | Trivial to add; most DPDK deployments use dedicated NICs |
| **Cork / MSG_MORE** | Not planned | `tx_burst` already batches at NIC level |

### Current Environment Assumptions

Integration testing runs on **AWS EC2 with VPC networking**, which has specific properties that simplify our implementation:

- AWS VPC is **L3-routed, not L2-switched** — all traffic (even same-subnet) transits a virtual router
- ARP always resolves to the **VPC gateway MAC**, never the peer's actual MAC
- No VLANs at the VPC level (our VLAN support is for non-AWS environments), no broadcast domains, no real L2 switching
- Gateway is always at `subnet_base + 1` (e.g., `10.0.1.1`)

**On physical hardware**, the subnet-aware routing table handles L2/L3 routing decisions automatically. On Linux, `UdpSocket::bind()` auto-detects the subnet, gateway, and ARP entries from `/proc/net/route` and `/proc/net/arp`. For non-standard topologies, use `NetworkConfig` to configure routing explicitly. See `docs/routing.md`.

## Roadmap

All completed work is captured in the [What's Implemented](#whats-implemented) tables above. The detailed, agent-shippable task list lives in **[ROADMAP.md](ROADMAP.md)**.

### Next Major Features

TCP and QUIC (IPv4) shipped in v0.3.0. Next:

**TCP engine hardening** — RFC 6191 SYN-in-TIME_WAIT reuse, RFC 793 RST for an unacceptable ACK in SYN_SENT, and keeping MACs on a reopened connection.

**Test depth** — real in-path loss for the TCP retransmit tests, and TCP integration tests on Graviton.

**TCP IPv6** — IPv6 address support in the TCP stack (additive over IPv4 TCP).

**QUIC IPv6** — IPv6 address support in the QUIC provider (additive over IPv4 QUIC).

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
- 2x c5n.large instances (sender/receiver); the Graviton stack uses c7g.large
- Dual ENIs (management + DPDK)
- SSM access (no SSH keys needed)

See `deploy/README.md` for details.

## License

MIT License - see LICENSE file for details.
