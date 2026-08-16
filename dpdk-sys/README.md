# dpdk-stdlib-sys

Raw FFI bindings for DPDK with stub support for building without DPDK installed.

## Overview

This crate provides low-level Rust FFI bindings to the [DPDK](https://www.dpdk.org/) C library. It uses `bindgen` to generate bindings when DPDK is installed, and ships with complete stub implementations so everything compiles and tests pass on any platform — no DPDK required.

## How It Works

The build script (`build.rs`) auto-detects DPDK via `pkg-config`:

- **DPDK found**: Generates real FFI bindings via bindgen, links against DPDK libraries.
- **DPDK not found**: Compiles stub implementations that return safe defaults (empty MAC addresses, zero-length bursts, successful init).

Check at runtime which mode is active:

```rust
if dpdk_sys::is_stub() {
    println!("Running with stubs (no real DPDK)");
}

if dpdk_sys::is_real_dpdk() {
    println!("Running with real DPDK");
}
```

## Usage

This crate is a dependency of [`dpdk-stdlib`](https://crates.io/crates/dpdk-stdlib) and is not typically used directly. If you need raw DPDK FFI access:

```toml
[dependencies]
dpdk-stdlib-sys = "0.1"
```

## Covered DPDK APIs

- EAL initialization and cleanup (`rte_eal_init`, `rte_eal_cleanup`)
- Ethernet device management (`rte_eth_dev_*`)
- Mbuf and mempool operations (`rte_pktmbuf_*`, `rte_mempool_*`)
- RX/TX burst functions (`rte_eth_rx_burst`, `rte_eth_tx_burst`)
- Promiscuous and allmulticast mode
- Device statistics
- MAC address retrieval

## Platform Support

| Platform | Stubs | Real DPDK |
|----------|-------|-----------|
| macOS | Yes | No |
| Linux | Yes | Yes |

## License

MIT
