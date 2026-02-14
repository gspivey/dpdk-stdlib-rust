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

// After
use dpdk_tokio::compat::net::UdpSocket;

// Code stays identical
let socket = UdpSocket::bind("0.0.0.0:9000")?;
socket.send_to(b"hello", "192.168.1.100:9000")?;
```

For async:

```rust
// Before
use tokio::net::UdpSocket;

// After
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
use dpdk_tokio::{SocketConfig, BackendType};

let config = SocketConfig {
    backend: BackendType::Dpdk,
    ..Default::default()
};
let socket = AsyncUdpSocket::bind_with_config("0.0.0.0:9000", config).await?;
```

## Development

### Build and Test

```bash
# Build everything (works without DPDK - uses stubs)
cargo build

# Run 133+ unit tests (no DPDK required)
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

See `CLAUDE.md` for agent instructions and `API_COMPATIBILITY.md` for API tracking.

## Architecture

```
Applications (echo, tokio-echo, test-client)
     │
     ├─ dpdk-tokio (async trait, compat layer, Tokio integration)
     │       │
     │       └─ dpdk-udp (UdpSocket, ARP, ICMP, backends)
     │               │
     │               ├─ DpdkBackend ──> dpdk (safe wrapper) ──> dpdk-sys (FFI)
     │               ├─ RawSocketBackend (AF_PACKET syscalls)
     │               └─ MmapBackend (AF_PACKET + ring buffers)
```

- **dpdk-sys**: Raw FFI bindings with stub fallback
- **dpdk**: Safe Rust wrapper (Port, Mbuf, Mempool, Queue)
- **dpdk-udp**: Protocol layer (sockets, packet parsing, ARP, ICMP)
- **dpdk-tokio**: Async support and drop-in compat layer

## Status

- ✅ **Phase 1-5 complete** (see `API_COMPATIBILITY.md`)
- ✅ **std::net::UdpSocket**: 19/19 methods implemented
- ✅ **tokio::net::UdpSocket**: All async methods + poll API
- ✅ **ARP resolution** and **ICMP echo reply** support
- ✅ **Hardware checksum offload** (IPv4, UDP, TCP)
- ✅ **Backend abstraction** (DPDK, AF_PACKET, MMAP)
- ✅ **Integration tests** on AWS EC2 (c6gn.large with ENA)

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
| macOS    | ✅        | ❌        | DPDK 23.11+ lacks macOS support |
| Linux    | ✅        | ✅        | Full DPDK functionality |
| Windows  | ❌        | ❌        | Not implemented |

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
