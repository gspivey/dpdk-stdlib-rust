# dpdk-stdlib-udp

Drop-in DPDK-accelerated replacement for `std::net::UdpSocket`.

## Overview

This crate provides a `UdpSocket` that is 100% API-compatible with `std::net::UdpSocket` (all 19 methods) but uses DPDK kernel bypass for packet I/O when available. When DPDK is not installed, it automatically falls back to AF_PACKET raw sockets.

## Quick Start

```toml
[dependencies]
dpdk-stdlib-udp = "0.1"
```

```rust
// Replace std::net::UdpSocket with dpdk_udp::UdpSocket — same API
use dpdk_udp::UdpSocket;

let socket = UdpSocket::bind("0.0.0.0:9000")?;
socket.send_to(b"hello", "192.168.1.100:9000")?;

let mut buf = [0u8; 1500];
let (len, addr) = socket.recv_from(&mut buf)?;
```

## Backends

Three packet I/O backends, selectable at bind time:

| Backend | Requires | Performance |
|---------|----------|-------------|
| **DPDK** | DPDK installed, dedicated NIC | Highest — full kernel bypass |
| **AF_PACKET+MMAP** | Linux, raw socket capability | High — zero-copy ring buffers |
| **AF_PACKET** | Linux, raw socket capability | Medium — syscall-based raw sockets |

```rust
use dpdk_udp::{UdpSocket, BackendConfig, BackendType};

let config = BackendConfig {
    backend_type: BackendType::Dpdk,
    ..Default::default()
};
let socket = UdpSocket::bind_with_backend("0.0.0.0:9000", config)?;
```

## Protocol Support

- **ARP**: Automatic resolution with caching. Pre-populate entries via `add_arp_entry()`.
- **ICMP**: Inline echo reply — the socket responds to pings without application involvement.

Both protocol handlers are backend-agnostic and operate on raw `&[u8]` frames.

## Constants

```rust
pub const MAX_UDP_PAYLOAD: usize = 1472;  // MTU 1500 - 20 IPv4 - 8 UDP
pub const TOTAL_HEADER_LEN: usize = 42;   // 14 Eth + 20 IPv4 + 8 UDP
```

## Features

- `perf-counters` (default) — Atomic packet counters on TX/RX hot paths. Disable with `--no-default-features` for latency-critical deployments.

## License

MIT
