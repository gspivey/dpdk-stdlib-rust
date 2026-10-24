# dpdk-stdlib-net

Shared networking layer for the dpdk-stdlib crates: the `PacketBackend` trait and its implementations.

## Overview

`PacketBackend` abstracts raw Ethernet frame I/O (`send_frame`, `recv_frames`, MAC address, promiscuous/allmulticast) so the protocol crates don't depend on any one way of reaching the NIC:

- **`DpdkBackend`** — userspace DPDK with kernel bypass
- **`RawSocketBackend`** — Linux AF_PACKET raw sockets, with optional PACKET_MMAP ring buffers

It is used by [`dpdk-stdlib-udp`](https://crates.io/crates/dpdk-stdlib-udp), [`dpdk-stdlib-tcp`](https://crates.io/crates/dpdk-stdlib-tcp) and [`dpdk-stdlib-quic`](https://crates.io/crates/dpdk-stdlib-quic). Most applications use those crates rather than this one directly.

## Quick Start

```toml
[dependencies]
dpdk-stdlib-net = "0.3"
```

Implement `PacketBackend` to plug a custom transport (for example a test loopback) into the UDP, TCP or QUIC stacks.

See the [repository](https://github.com/gspivey/dpdk-stdlib-rust) for details.

## License

MIT
