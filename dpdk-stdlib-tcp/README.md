# dpdk-stdlib-tcp

DPDK-accelerated TCP stack: drop-in replacements for `std::net::TcpStream` and `std::net::TcpListener`.

## Overview

A userspace TCP stack (IPv4) that bypasses the Linux kernel. A dedicated engine thread runs the TCP state machine and exchanges data with your `TcpStream`/`TcpListener` over SPSC rings:

- Three-way handshake, FIN teardown, RST validation
- Out-of-order reassembly, Nagle, delayed ACK, silly-window avoidance
- RTO with retransmission, persist, keepalive, TIME_WAIT
- Reno congestion control with fast retransmit

For async code, use the `tokio::net` drop-ins in [`dpdk-stdlib-tokio`](https://crates.io/crates/dpdk-stdlib-tokio).

## Quick Start

```toml
[dependencies]
dpdk-stdlib-tcp = "0.3"
```

```rust
use dpdk_stdlib_tcp::{init_dpdk_tcp_context, DpdkTcpRuntimeConfig, TcpListener};
use std::io::{Read, Write};

// Once per process, before any bind/connect.
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

`TcpStream::connect` works the same way, and `into_split` gives owned read/write halves.

## Requirements

- DPDK installed (found via `pkg-config`) and a NIC bound to `vfio-pci`, with hugepages configured. Without DPDK the crate builds against stubs, for development and tests.
- IPv4 only for now; IPv6 is planned.

Tested on AWS EC2 (c5n.large) on every PR: handshake, echo, shutdown, flow control, retransmit, interop with kernel `ncat`/`iperf3`, and parity with `std::net`. See the [repository](https://github.com/gspivey/dpdk-stdlib-rust).

## License

MIT
