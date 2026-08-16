# dpdk-stdlib

Safe Rust wrappers for DPDK EAL, Port, Mbuf, Mempool, and RX/TX queues.

## Overview

This crate provides safe, idiomatic Rust abstractions over the raw DPDK FFI bindings in [`dpdk-stdlib-sys`](https://crates.io/crates/dpdk-stdlib-sys). It handles resource lifecycle, error conversion, and NUMA-aware allocation so higher-level crates don't need to touch unsafe code.

## Key Types

- **`Eal`** — DPDK Environment Abstraction Layer. Initializes DPDK, manages global state, and cleans up on drop.
- **`Port`** — Ethernet device (NIC). Configures RX/TX queues, starts/stops the device, and provides burst I/O.
- **`Mbuf`** — Message buffer. Zero-copy packet buffer with builder API for constructing Ethernet frames.
- **`Mempool`** — Pre-allocated pool of Mbufs. NUMA-aware, configurable size and cache.
- **`MacAddress`** — 6-byte MAC address with display formatting and broadcast/multicast detection.
- **`PortConfig`** — Builder for port configuration including RX/TX offloads.

## Usage

This crate is a dependency of [`dpdk-stdlib-udp`](https://crates.io/crates/dpdk-stdlib-udp) and is not typically used directly. If you need safe DPDK primitives:

```toml
[dependencies]
dpdk-stdlib = "0.1"
```

```rust
use dpdk::{Eal, Port, Mempool};
use dpdk::mbuf::MempoolConfig;
use dpdk::port::PortConfig;

// Initialize DPDK
let eal = Eal::new(&["myapp", "-l", "0-1", "--no-huge"])?;

// Create a mempool
let pool = Mempool::create("pool0", MempoolConfig::default())?;

// Configure and start a port
let port = Port::new(0, PortConfig::default(), &pool)?;
port.start()?;
```

## Hardware Offload

The crate queries NIC capabilities at port init and exposes them:

- IPv4 header checksum offload (RX and TX)
- UDP checksum offload (RX and TX)
- TCP checksum offload (RX and TX)

## License

MIT
