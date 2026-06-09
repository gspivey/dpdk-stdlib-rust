//! Provider initialization integration tests.
//!
//! Tests: provider construction with default config, stub mode start
//! (via LoopbackBackend), and IPv6 address rejection.

use dpdk_stdlib_quic::loopback::LoopbackBackend;
use dpdk_stdlib_quic::{DpdkProvider};
use dpdk_udp::PacketBackend;
use s2n_quic::Server;
use std::sync::Arc;

/// Generate self-signed TLS cert+key PEM strings via rcgen.
fn generate_tls_pair() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen cert generation failed");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
    (cert_pem, key_pem)
}

#[test]
fn provider_construction_default_config() {
    let (_provider, handle) = DpdkProvider::builder().build();
    let stats = handle.stats();
    assert_eq!(stats.rx_burst_calls, 0);
    assert_eq!(stats.datagrams_received, 0);
    assert_eq!(stats.datagrams_transmitted, 0);
    assert_eq!(stats.timer_wakeups, 0);
    assert_eq!(stats.rx_drops, 0);
    assert_eq!(stats.tx_drops, 0);
}

#[test]
fn provider_start_stub_mode_no_error() {
    let (cert_pem, key_pem) = generate_tls_pair();
    let backend = Arc::new(LoopbackBackend::new());

    let (provider, mut handle) = DpdkProvider::builder()
        .with_addr("127.0.0.1:0".parse().unwrap())
        .with_gateway_mac(backend.mac_address())
        .with_backend(Arc::clone(&backend) as Arc<_>)
        .build();

    // Build a real s2n-quic Server using our provider
    let server = Server::builder()
        .with_tls((cert_pem.as_str(), key_pem.as_str()))
        .expect("TLS config failed")
        .with_io(provider)
        .expect("provider start failed")
        .start();

    assert!(server.is_ok(), "server start must succeed in stub mode");

    // Give the event loop a moment to start, then shut down cleanly
    std::thread::sleep(std::time::Duration::from_millis(50));
    handle.shutdown();
}

#[test]
fn provider_ipv6_returns_unsupported_address_family() {
    let (cert_pem, key_pem) = generate_tls_pair();

    let (provider, _handle) = DpdkProvider::builder()
        .with_addr("[::1]:4433".parse().unwrap())
        .build();

    // The IPv6 rejection happens inside provider.start() which is called
    // during .start() on the server builder.
    let builder = Server::builder()
        .with_tls((cert_pem.as_str(), key_pem.as_str()))
        .unwrap()
        .with_io(provider)
        .unwrap();

    let result = builder.start();
    assert!(result.is_err(), "IPv6 address should be rejected");
}
