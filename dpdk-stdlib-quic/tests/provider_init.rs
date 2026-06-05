//! Provider initialization tests (stub mode).

use dpdk_stdlib_quic::{DpdkProvider, ProviderHandle};

#[test]
fn provider_builds_without_panic() {
    let addr: std::net::SocketAddr = "0.0.0.0:4433".parse().unwrap();
    let (_provider, handle) = DpdkProvider::builder().with_addr(addr).build();
    let stats = handle.stats();
    assert_eq!(stats.rx_burst_calls, 0);
    assert_eq!(stats.datagrams_received, 0);
    assert_eq!(stats.datagrams_transmitted, 0);
}

#[test]
fn provider_shutdown_on_non_started() {
    let (_provider, mut handle) = DpdkProvider::builder().build();
    // Shutdown on a non-started provider should not panic
    handle.shutdown();
}

#[test]
fn provider_builder_defaults() {
    let (_provider, handle) = DpdkProvider::builder().build();
    let stats = handle.stats();
    assert_eq!(stats.timer_wakeups, 0);
    assert_eq!(stats.rx_drops, 0);
    assert_eq!(stats.tx_drops, 0);
}
