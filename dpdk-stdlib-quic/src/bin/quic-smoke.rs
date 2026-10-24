//! Walking-skeleton smoke test for the DPDK QUIC provider.
//!
//! Builds the provider in stub mode, verifies construction succeeds,
//! prints `QUIC_SMOKE_OK`, and exits 0.

fn main() {
    let addr: std::net::SocketAddr = "0.0.0.0:4433".parse().unwrap();

    let (_provider, mut handle) = dpdk_stdlib_quic::DpdkProvider::builder()
        .with_addr(addr)
        .build();

    // Verify provider and handle were constructed
    let stats = handle.stats();
    assert_eq!(stats.rx_burst_calls, 0);
    assert_eq!(stats.datagrams_received, 0);

    // Verify shutdown is clean on a non-started provider
    handle.shutdown();

    println!("QUIC_SMOKE_OK");
}
