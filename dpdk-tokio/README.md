# dpdk-stdlib-tokio

Async DPDK-accelerated replacement for `tokio::net::UdpSocket`.

## Overview

This crate provides async Tokio integration for DPDK networking. The compat layer gives you a drop-in replacement for `tokio::net::UdpSocket` — same API, DPDK-accelerated when available, with automatic fallback to standard Tokio sockets.

## Quick Start

```toml
[dependencies]
dpdk-stdlib-tokio = { version = "0.1", features = ["dpdk"] }
tokio = { version = "1", features = ["full"] }
```

```rust
// Replace tokio::net::UdpSocket — same API, DPDK-accelerated
use dpdk_tokio::compat::tokio::UdpSocket;

let socket = UdpSocket::bind("0.0.0.0:9000").await?;
socket.send_to(b"hello", "192.168.1.100:9000").await?;

let mut buf = [0u8; 1500];
let (len, addr) = socket.recv_from(&mut buf).await?;
```

## Features

- `dpdk` — Enables DPDK backend support via `dpdk-stdlib-udp`. Without this feature, only standard Tokio sockets are available.

## Compat Layer

The `compat` module provides drop-in replacements:

- **`dpdk_tokio::compat::tokio::UdpSocket`** — Replaces `tokio::net::UdpSocket`. Tries DPDK first, falls back to Tokio.
- **`dpdk_tokio::compat::net::UdpSocket`** — Replaces `std::net::UdpSocket`. Tries DPDK first, falls back to std.

## Async Trait

For generic async UDP code, use the `AsyncUdpSocket` trait:

```rust
use dpdk_tokio::{AsyncUdpSocket, SocketConfig};

async fn echo(socket: &dyn AsyncUdpSocket) {
    let mut buf = [0u8; 1500];
    let (len, addr) = socket.recv_from(&mut buf).await.unwrap();
    socket.send_to(&buf[..len], addr).await.unwrap();
}
```

## License

MIT
