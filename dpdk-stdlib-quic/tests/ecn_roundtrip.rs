//! ECN round-trip integration test.
//!
//! Builds a frame with ECN marking via the TX path (build_udp_frame_into_with_tos),
//! parses it back via the RX path (parse_to_rx_datagram), and verifies that the
//! ECN codepoint is preserved for all 4 values.

use dpdk_stdlib_quic::ecn::{ecn_to_tos_bits, extract_ecn};
use dpdk_stdlib_quic::frame::build_quic_frame;
use dpdk_stdlib_quic::parse_to_rx_datagram;
use dpdk_udp::ETH_HEADER_LEN;
use s2n_quic_core::inet::ExplicitCongestionNotification;
use std::net::{Ipv4Addr, SocketAddr};

#[test]
fn ecn_roundtrip_all_codepoints() {
    let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();
    let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    let gateway_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

    for ecn in [
        ExplicitCongestionNotification::NotEct,
        ExplicitCongestionNotification::Ect1,
        ExplicitCongestionNotification::Ect0,
        ExplicitCongestionNotification::Ce,
    ] {
        let tos = ecn_to_tos_bits(ecn);

        // TX path: build frame with ECN marking
        let mut frame = Vec::new();
        build_quic_frame(
            &mut frame,
            &src_mac,
            &gateway_mac,
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            5000,
            4433,
            b"ecn integration test",
            tos,
        )
        .expect("frame build should succeed");

        // Verify TOS byte is set correctly in the raw frame
        assert_eq!(
            frame[ETH_HEADER_LEN + 1] & 0x03,
            tos,
            "TOS low bits must match ECN value for {:?}",
            ecn
        );

        // RX path: parse frame and extract ECN
        let dgram = parse_to_rx_datagram(&frame, local_addr)
            .expect("parse should succeed for valid frame");

        // Verify ECN codepoint is preserved
        assert_eq!(
            dgram.header.ecn, ecn,
            "ECN codepoint must survive TX→RX round-trip for {:?}",
            ecn
        );

        // Also verify via direct extraction from the raw TOS byte
        assert_eq!(
            extract_ecn(frame[ETH_HEADER_LEN + 1]),
            ecn,
            "Direct ECN extraction must match for {:?}",
            ecn
        );
    }
}

#[test]
fn ecn_roundtrip_with_dscp_bits_set() {
    // Verify ECN bits are preserved even when DSCP (upper 6 bits) are non-zero
    let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();
    let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    let gateway_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

    // ECN Ect0 = 0b10, combined with DSCP=0b101100 → TOS = 0b10110010 = 0xB2
    let ecn = ExplicitCongestionNotification::Ect0;
    let tos = 0b10110000 | ecn_to_tos_bits(ecn); // DSCP + ECN

    let mut frame = Vec::new();
    build_quic_frame(
        &mut frame,
        &src_mac,
        &gateway_mac,
        Ipv4Addr::new(10, 0, 0, 1),
        Ipv4Addr::new(10, 0, 0, 2),
        5000,
        4433,
        b"dscp+ecn",
        tos,
    )
    .unwrap();

    let dgram = parse_to_rx_datagram(&frame, local_addr).unwrap();
    assert_eq!(dgram.header.ecn, ecn);
}
