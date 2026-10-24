# dpdk-stdlib-quic

Native DPDK I/O provider for [s2n-quic](https://github.com/aws/s2n-quic): kernel-bypass QUIC transport.

## Overview

`DpdkProvider` implements `s2n_quic::provider::io::Provider`. It owns the s2n-quic endpoint and drives it from a dedicated busy-poll thread that calls `rx_burst`/`tx_burst` directly, with no Tokio runtime in the I/O path. It supports ECN marking and GSO segmentation, and answers ICMP echo on its own.

## Quick Start

```toml
[dependencies]
dpdk-stdlib-quic = "0.3"
s2n-quic = "=1.81.0"
```

```rust
use dpdk_stdlib_quic::DpdkProvider;
use s2n_quic::Server;

let (provider, _handle) = DpdkProvider::builder()
    .with_addr("10.0.1.50:4433".parse()?)
    .with_gateway_mac([0x02, 0xff, 0xec, 0x01, 0x65, 0x2d]) // AWS VPC: always the gateway MAC
    .build();

let mut server = Server::builder()
    .with_tls((cert_pem, key_pem))?
    .with_io(provider)?
    .start()?;

while let Some(conn) = server.accept().await {
    // handle the connection
}
```

Clients use `s2n_quic::Client` with the same provider. Binding port 0 picks an ephemeral port.

## Version pin

This crate pins `s2n-quic = "=1.81.0"` and `s2n-quic-core = "=0.81.0"`. The provider implements s2n-quic's lower-level I/O traits (`rx::Queue`, `tx::Queue`, `path::Handle`), which are not stable across s2n-quic releases, so your `s2n-quic` version must match.

## Requirements

- DPDK installed (found via `pkg-config`) and a NIC bound to `vfio-pci`, with hugepages configured. Without DPDK the crate builds against stubs, and a loopback backend is available for tests.
- IPv4 only for now; IPv6 is planned.

Tested on AWS EC2 (c5n.large) on every PR: handshake and bidirectional echo, DPDK to DPDK. See the [repository](https://github.com/gspivey/dpdk-stdlib-rust).

## License

MIT
