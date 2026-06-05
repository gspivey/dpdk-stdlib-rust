//! Loopback backend tests.

use dpdk_stdlib_quic::loopback::LoopbackBackend;
use dpdk_udp::PacketBackend;

#[test]
fn loopback_send_recv_roundtrip() {
    let backend = LoopbackBackend::new();
    let frame = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
    backend.send_frame(&frame).unwrap();
    let received = backend.recv_frames(10).unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0], frame);
}

#[test]
fn loopback_recv_empty() {
    let backend = LoopbackBackend::new();
    let received = backend.recv_frames(10).unwrap();
    assert!(received.is_empty());
}

#[test]
fn loopback_mac_address() {
    let backend = LoopbackBackend::new();
    let mac = backend.mac_address();
    assert_eq!(mac[0], 0x02); // locally administered
}

#[test]
fn loopback_backend_name() {
    let backend = LoopbackBackend::new();
    assert_eq!(backend.backend_name(), "loopback");
}

#[test]
fn loopback_promiscuous() {
    let backend = LoopbackBackend::new();
    assert!(!backend.is_promiscuous());
    backend.set_promiscuous(true).unwrap();
    assert!(backend.is_promiscuous());
    backend.set_promiscuous(false).unwrap();
    assert!(!backend.is_promiscuous());
}

#[test]
fn loopback_allmulticast() {
    let backend = LoopbackBackend::new();
    assert!(!backend.is_allmulticast());
    backend.set_allmulticast(true).unwrap();
    assert!(backend.is_allmulticast());
    backend.set_allmulticast(false).unwrap();
    assert!(!backend.is_allmulticast());
}

#[test]
fn loopback_multiple_frames() {
    let backend = LoopbackBackend::new();
    backend.send_frame(&[1, 2, 3]).unwrap();
    backend.send_frame(&[4, 5, 6]).unwrap();
    backend.send_frame(&[7, 8, 9]).unwrap();

    // Receive with limit 2
    let received = backend.recv_frames(2).unwrap();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0], vec![1, 2, 3]);
    assert_eq!(received[1], vec![4, 5, 6]);

    // Remaining frame still there
    let received = backend.recv_frames(10).unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0], vec![7, 8, 9]);
}

#[test]
fn loopback_rx_readiness() {
    use dpdk_udp::RxReadiness;
    let backend = LoopbackBackend::new();
    match backend.rx_readiness() {
        RxReadiness::PollOnly => {} // expected
        _ => panic!("Expected PollOnly rx_readiness for LoopbackBackend"),
    }
}
