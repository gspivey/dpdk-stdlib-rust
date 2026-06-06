//! Frame building helpers for the QUIC provider.
//!
//! Re-exports `build_udp_frame_into_with_tos` from `dpdk-udp` and provides a
//! convenience wrapper for the common QUIC TX pattern.

pub use dpdk_udp::{build_udp_frame_into_with_tos, ETH_HEADER_LEN, IPV4_HEADER_LEN, TOTAL_HEADER_LEN};
use dpdk_udp::UdpResult;
use std::net::Ipv4Addr;

/// Build a UDP frame with TOS/ECN marking for QUIC transmission.
///
/// Convenience wrapper around [`dpdk_udp::build_udp_frame_into_with_tos`] using
/// the typical QUIC provider parameters (src_mac, gateway_mac, addresses, payload, tos).
#[inline]
pub fn build_quic_frame(
    out: &mut Vec<u8>,
    src_mac: &[u8; 6],
    gateway_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    tos: u8,
) -> UdpResult<usize> {
    build_udp_frame_into_with_tos(
        out,
        src_mac,
        gateway_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        payload,
        64, // default TTL for QUIC
        tos,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecn::{ecn_to_tos_bits, extract_ecn};
    use dpdk_udp::ipv4_checksum;
    use s2n_quic_core::inet::ExplicitCongestionNotification::*;

    #[test]
    fn tos_byte_at_correct_offset() {
        let mut out = Vec::new();
        let tos = 0b10101110;
        build_quic_frame(
            &mut out,
            &[0x01; 6],
            &[0x02; 6],
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            4433, 443,
            b"hello",
            tos,
        )
        .unwrap();

        assert_eq!(out[ETH_HEADER_LEN + 1], tos);
    }

    #[test]
    fn checksum_valid_after_tos() {
        let mut out = Vec::new();
        build_quic_frame(
            &mut out,
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
            &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            Ipv4Addr::new(172, 16, 0, 1),
            Ipv4Addr::new(172, 16, 0, 2),
            5000, 6000,
            b"checksum test",
            0x02,
        )
        .unwrap();

        let ip_start = ETH_HEADER_LEN;
        let verify = ipv4_checksum(&out[ip_start..ip_start + IPV4_HEADER_LEN]);
        assert_eq!(verify, 0, "IPv4 checksum must verify to 0");
    }

    #[test]
    fn ecn_round_trip_through_frame() {
        for ecn in [NotEct, Ect1, Ect0, Ce] {
            let tos = ecn_to_tos_bits(ecn);
            let mut out = Vec::new();
            build_quic_frame(
                &mut out,
                &[0x01; 6],
                &[0x02; 6],
                Ipv4Addr::new(192, 168, 1, 1),
                Ipv4Addr::new(192, 168, 1, 2),
                1234, 5678,
                b"ecn",
                tos,
            )
            .unwrap();

            let recovered = extract_ecn(out[ETH_HEADER_LEN + 1]);
            assert_eq!(recovered, ecn, "ECN must survive frame round-trip");
        }
    }
}
