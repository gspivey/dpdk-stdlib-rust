# AGENTS.md - Repository Guide for AI Agents

## Project Overview

**dpdk-stdlib-rust** is a production-ready, API-compatible DPDK-accelerated networking stack in Rust. It provides drop-in replacements for `std::net::UdpSocket` and `tokio::net::UdpSocket` that bypass the Linux kernel network stack using DPDK userspace networking, with automatic fallback to AF_PACKET raw sockets when DPDK is unavailable.

Detailed requirements, designs and tasks live in .kiro/specs/**/*.md
Whenever you are working on a task from the .kiro/specs sub directory you MUST read the design and requirements file that goes with it. For example:
You are working on a task from `.kiro/specs/ec2-integration-tests/tasks.md` You must read `.kiro/specs/ec2-integration-tests/design.md` and `.kiro/specs/ec2-integration-tests/requirements.md` before starting any task work.
Once you complete the task you must update `.kiro/specs/ec2-integration-tests/tasks.md` marking any tasks complete.

## Repository Layout

```
dpdk-stdlib-rust/
├── dpdk-sys/          # Raw FFI bindings to DPDK C library (bindgen + stubs)
├── dpdk/              # Safe Rust wrapper around dpdk-sys
├── dpdk-udp/          # UDP protocol layer (sockets, ARP, ICMP, backends)
├── dpdk-tokio/        # Async Tokio integration with compat layer
├── apps/
│   ├── echo/          # Sync echo server demo
│   ├── test-client/   # UDP test client
│   └── tokio-echo/    # Async echo server demo
├── deploy/            # AWS CDK deployment (c6gn.large with dual ENIs)
├── scripts/           # DPDK setup scripts for Amazon Linux
├── API_COMPATIBILITY.md  # Detailed API tracking document
└── README.md
```

## Crate Dependency Graph

```
dpdk-sys  (FFI bindings / stubs)
  └── dpdk  (safe wrapper: Eal, Port, Mbuf, Mempool, Queue)
        └── dpdk-udp  (UdpSocket, ARP, ICMP, PacketBackend)
              └── dpdk-tokio  (async trait, compat layer, macros)
                    └── apps/*  (demo applications)
```

## Architecture & Design Decisions

### 1. Stub-First Development

`dpdk-sys` provides full stub implementations when real DPDK is not installed. This means:
- **All 133+ tests run without DPDK installed** (macOS, CI, Linux without DPDK)
- Stubs return sensible defaults (empty MAC, zero-length rx_burst, successful init)
- Use `dpdk_sys::is_stub()` / `dpdk_sys::is_real_dpdk()` to check at runtime
- Build script (`dpdk-sys/build.rs`) auto-detects DPDK via `pkg-config`

### 2. Backend Abstraction (Phase 5)

The `PacketBackend` trait in `dpdk-udp/src/backend.rs` abstracts raw packet I/O:

```rust
pub trait PacketBackend: Send + Sync {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize>;
    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>>;
    fn mac_address(&self) -> [u8; 6];
    fn backend_name(&self) -> &'static str;
    fn set_promiscuous(&self, enable: bool) -> io::Result<()>;
    fn is_promiscuous(&self) -> bool;
    fn set_allmulticast(&self, enable: bool) -> io::Result<()>;
    fn is_allmulticast(&self) -> bool;
}
```

Three implementations exist:
- **DpdkBackend** (`backend_dpdk.rs`) - Userspace DPDK with kernel bypass
- **RawSocketBackend** (`backend_raw.rs`) - Linux AF_PACKET raw sockets
- **RawSocketBackend with MMAP** - AF_PACKET + PACKET_MMAP ring buffers for zero-copy

`UdpSocket` internally uses a `SocketBackend` enum that dispatches to either the legacy DPDK path or a generic `Arc<dyn PacketBackend>`:

```rust
enum SocketBackend {
    Dpdk(Arc<DpdkResources>),      // Legacy direct DPDK path
    Generic(Arc<dyn PacketBackend>), // Any backend via trait
}
```

### 3. Dual Packet Building

Two packet construction paths exist by design:
- `build_udp_packet(&mut Mbuf, ...)` - Writes directly into DPDK mbuf (zero-copy for DPDK path)
- `build_udp_frame(...)` -> `Vec<u8>` - Backend-agnostic, returns owned frame bytes

Both produce identical Ethernet frames (14B Eth + 20B IPv4 + 8B UDP + payload).

### 4. Protocol Handlers Are Backend-Agnostic

ARP (`arp.rs`) and ICMP (`icmp.rs`) operate on `&[u8]` slices and return `Vec<u8>` or `[u8; 42]`. They work identically regardless of which backend produced the packet data. Do NOT couple them to any specific backend.

### 5. API Compatibility Contract

This project maintains 100% API compatibility with:
- `std::net::UdpSocket` (all 19 methods)
- `tokio::net::UdpSocket` (all async + poll methods)

The compat layer lives in `dpdk-tokio/src/compat/`. Changing method signatures in `UdpSocket` or the async trait breaks this contract. Always verify against `API_COMPATIBILITY.md`.

### 6. Hardware Offload Support

The DPDK backend supports hardware checksum offloading (IPv4, UDP, TCP) on both RX and TX paths. Offload capabilities are queried from the NIC at port init and exposed via `has_tx_ipv4_cksum_offload()` etc. on `UdpSocket`.

## Key Constants

```rust
pub const MAX_UDP_PAYLOAD: usize = 1472;    // MTU 1500 - 20 IPv4 - 8 UDP
pub const ETH_HEADER_LEN: usize = 14;
pub const IPV4_HEADER_LEN: usize = 20;
pub const UDP_HEADER_LEN: usize = 8;
pub const TOTAL_HEADER_LEN: usize = 42;     // ETH + IPv4 + UDP
```

## Steering Rules for Agents

### DO:
- **Run `cargo build` and `cargo test` from the workspace root** after any changes
- **Read files before modifying them** - understand existing patterns first
- **Maintain backward compatibility** - `UdpSocket::bind()` must keep working as before
- **Keep protocol handlers (ARP, ICMP) backend-agnostic** - they work on `&[u8]`, not mbufs
- **Check API_COMPATIBILITY.md** before changing any public API surface
- **Use the stub system** - tests must pass without real DPDK (`cargo test` on any platform)
- **Follow existing error patterns** - `UdpError`/`UdpResult` in dpdk-udp, `DpdkError`/`DpdkResult` in dpdk, `io::Result` for backend trait methods

### DON'T:
- **Don't add DPDK-specific types to the `PacketBackend` trait** - it operates on `&[u8]` / `Vec<u8>`
- **Don't modify `dpdk-sys/src/stubs.rs`** without understanding that it affects ALL tests
- **Don't break the crate dependency direction** - lower crates must never depend on higher ones
- **Don't add `unsafe` code** outside of `dpdk-sys` and `ring_buffer.rs` without justification
- **Don't assume DPDK is available** - always handle the stub/fallback case
- **Don't change method signatures on `UdpSocket`** without updating the compat layer in `dpdk-tokio`

## Build & Test

```bash
# Build everything (works without DPDK installed - uses stubs)
cargo build

# Run all tests (133+ tests, no DPDK required)
cargo test

# Run specific crate tests
cargo test -p dpdk-udp
cargo test -p dpdk

# Run with real DPDK (requires DPDK installed + pkg-config)
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build
```

## Implementation Phases (All Complete)

1. **Phase 1** - Core DPDK FFI bindings and safe wrappers
2. **Phase 2** - UDP socket with full `std::net::UdpSocket` API
3. **Phase 3** - Async Tokio integration with `tokio::net::UdpSocket` API
4. **Phase 4** - ARP resolution and ICMP echo reply support
5. **Phase 5** - Backend abstraction (PacketBackend trait, AF_PACKET, MMAP ring buffers)

See `API_COMPATIBILITY.md` for detailed tracking of each phase.

## File Quick Reference

| File | Purpose | Lines |
|------|---------|-------|
| `dpdk-udp/src/lib.rs` | UdpSocket, packet build/parse, socket backend dispatch | ~1700 |
| `dpdk-udp/src/arp.rs` | ARP handler, cache, packet parsing | ~686 |
| `dpdk-udp/src/icmp.rs` | ICMP handler, echo reply | ~696 |
| `dpdk-udp/src/backend.rs` | PacketBackend trait, BackendConfig, BackendType | ~159 |
| `dpdk-udp/src/backend_dpdk.rs` | DPDK backend implementation | ~238 |
| `dpdk-udp/src/backend_raw.rs` | AF_PACKET raw socket backend | ~350 |
| `dpdk-udp/src/ring_buffer.rs` | PACKET_MMAP ring buffer structures | ~330 |
| `dpdk/src/port.rs` | Port config, MAC address, offloads | Key type |
| `dpdk/src/mbuf.rs` | Mbuf and Mempool wrappers | Key type |
| `dpdk-tokio/src/lib.rs` | Async UDP trait, macros, bind helpers | Entry point |
| `dpdk-tokio/src/compat/` | Drop-in std/tokio socket replacements | Compat layer |
