# dpdk-stdlib-rust

Drop-in DPDK-accelerated replacements for `std::net::UdpSocket` and `tokio::net::UdpSocket`. Bypass the Linux kernel network stack for high-throughput packet processing, with automatic fallback when DPDK is unavailable.

## Why

Traditional Linux networking routes every packet through the kernel: syscalls, context switches, interrupts, and the full TCP/IP stack. For high-packet-rate workloads (DNS servers, load balancers, packet processors), this overhead becomes the bottleneck.

DPDK (Data Plane Development Kit) bypasses the kernel entirely using userspace drivers and polling. This eliminates syscalls and context switches, achieving:

- **10-100x higher packet rates** — millions of packets/sec per core
- **Microsecond-level latency** instead of milliseconds
- **Zero kernel overhead** for packet I/O

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

### Multi-Core Pipeline

By default, `UdpSocket::bind()` runs in single-threaded run-to-completion mode — one core handles NIC polling, protocol processing, and application logic. This is the lowest-latency path and is optimal for low-to-moderate packet rates.

For high-throughput workloads, use the builder API to enable the multi-core pipeline:

```rust
use dpdk_udp::UdpSocket;

// Auto-detect: multi-core when enough cores + real DPDK are available
let socket = UdpSocket::builder()
    .bind("0.0.0.0:9000")?;

// Explicit: 2 worker threads per RX queue
let socket = UdpSocket::builder()
    .workers_per_queue(2)
    .bind("0.0.0.0:9000")?;

// Explicit: 4 RSS queues, 2 workers each (12 cores total: 4 RX + 8 workers)
let socket = UdpSocket::builder()
    .rx_queues(4)
    .workers_per_queue(2)
    .bind("0.0.0.0:9000")?;

// Force simple mode: no pipeline threads, lowest latency
let socket = UdpSocket::builder()
    .workers_per_queue(0)
    .bind("0.0.0.0:9000")?;
```

When the pipeline is active, the data flow is:

```
NIC → RX lcore (ARP/ICMP inline) → SPSC rings → N workers → MPSC app_ring → recv_from()
send_to() → TX ring → RX lcore → NIC
```

Environment variables override builder settings: `DPDK_RX_QUEUES`, `DPDK_WORKERS_PER_QUEUE`.

Query the active topology at runtime:

```rust
if socket.is_run_to_completion() {
    println!("Simple mode (single-threaded)");
} else if let Some(plan) = socket.topology_plan() {
    println!("Pipeline: {} RX queues, {} workers/queue", plan.rx_queues, plan.workers_per_queue);
}
```

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
- Topology detection for multi-core scaling (NUMA-aware queue/worker planning)

**dpdk-tokio** — Async layer providing `tokio::net::UdpSocket`-compatible API with poll-based I/O. Includes a compat module (`dpdk_tokio::compat::tokio`) for zero-change migration from Tokio sockets.

### Packet Path

**Run-to-completion (default):**

TX: `send_to()` → build frame → backend `send_frame()` → NIC.

RX: Backend `recv_frames()` → parse headers → ARP/ICMP inline → UDP payload to caller.

**Multi-core pipeline (when configured via builder):**

TX: `send_to()` → build frame → enqueue to TX ring → RX lcore drains → NIC.

RX: RX lcore polls NIC → ARP/ICMP inline → SPSC fan-out to workers → workers parse UDP → MPSC app_ring → `recv_from()` dequeues.

All ring communication uses lock-free SPSC/MPSC rings with cache-line-padded atomics for zero contention between cores.

Two packet construction paths exist by design: `build_udp_packet(&mut Mbuf)` writes directly into DPDK mbufs (zero-copy), while `build_udp_frame() -> Vec<u8>` produces owned bytes for the generic backend path. Both emit identical wire-format frames.

## Development

### Build and Test

```bash
# Build everything (works without DPDK - uses stubs)
cargo build

# Run 180+ unit tests (no DPDK required)
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
| 140,000 | 140,000 | 0% | 139,000 | 0.7% |
| 350,000 | 350,000 | 0% | 339,540 | 3.0% |
| 700,000 | 635,566 | 9.2% | 327,703 | 53.2% |

### 512-byte packets

| Target PPS | rust-dpdk RX | Drop | Kernel RX | Drop |
|-----------|-------------|------|----------|------|
| 70,000 | 70,000 | 0% | 69,000 | 1.4% |
| 140,000 | 140,000 | 0% | 139,000 | 0.7% |
| 350,000 | 350,000 | 0% | 318,596 | 9.0% |
| 700,000 | 606,699 | 13.3% | 312,455 | 55.4% |

### 1400-byte packets (near MTU)

| Target PPS | rust-dpdk RX | Drop | Kernel RX | Drop |
|-----------|-------------|------|----------|------|
| 70,000 | 70,000 | 0% | 69,000 | 1.4% |
| 140,000 | 140,000 | 0% | 138,997 | 0.7% |
| 350,000 | 350,000 | 0% | 299,882 | 14.3% |
| 700,000 | 447,807 | 36.0% | 299,438 | 57.2% |

**Key takeaway**: At 350k pps, DPDK handles all three packet sizes with zero drops. The kernel stack is already losing 3-14% of packets at the same rate. At 700k pps the gap widens — DPDK delivers ~1.5-2x the throughput of kernel sockets.

## Status

- **Phase 1-5 complete** (see `API_COMPATIBILITY.md`)
- **Phase B (multi-core pipeline)**: Configurable worker fan-out with lock-free rings
- **std::net::UdpSocket**: 19/19 methods implemented
- **tokio::net::UdpSocket**: All async methods + poll API
- **ARP resolution** and **ICMP echo reply** support
- **Hardware checksum offload** (IPv4, UDP, TCP)
- **Backend abstraction** (DPDK, AF_PACKET, MMAP)
- **Multi-core scaling**: Configurable RX queues and worker threads per queue
- **Integration tests** on AWS EC2 (c6gn.large with ENA)

## DPDK Installation (Optional)

Development and testing work without DPDK. For production kernel bypass:

### Amazon Linux 2023

```bash
sudo ./scripts/install_dpdk_amazon_linux.sh
```

This installs DPDK 23.11 and configures hugepages.

### Verify DPDK

```bash
# Should show "real" not "stub"
cargo run -p echo -- --dpdk
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
