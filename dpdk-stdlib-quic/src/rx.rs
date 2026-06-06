//! Receive queue for delivering datagrams to the s2n-quic endpoint.

use crate::ecn::extract_ecn;
use crate::path_handle::DpdkPathHandle;
use dpdk_udp::{parse_udp_packet_ref, ETH_HEADER_LEN};
use s2n_quic_core::inet::{datagram, IpV4Address};
use s2n_quic_core::io::rx;
use s2n_quic_core::path::{self, Handle as _};
use std::net::SocketAddr;

/// A parsed inbound datagram ready for delivery to s2n-quic.
pub struct RxDatagram {
    pub header: datagram::Header<DpdkPathHandle>,
    pub payload: Vec<u8>,
}

/// Receive queue buffering parsed datagrams from `recv_frames()`.
pub struct DpdkRxQueue {
    datagrams: Vec<RxDatagram>,
}

impl DpdkRxQueue {
    pub fn new() -> Self {
        Self {
            datagrams: Vec::new(),
        }
    }

    pub fn push(&mut self, datagram: RxDatagram) {
        self.datagrams.push(datagram);
    }
}

impl rx::Queue for DpdkRxQueue {
    type Handle = DpdkPathHandle;

    fn for_each<F: FnMut(datagram::Header<Self::Handle>, &mut [u8])>(&mut self, mut on_packet: F) {
        for dgram in self.datagrams.drain(..) {
            let mut payload = dgram.payload;
            on_packet(dgram.header, &mut payload);
        }
    }

    fn is_empty(&self) -> bool {
        self.datagrams.is_empty()
    }
}

/// Parse a raw Ethernet frame into an `RxDatagram` for the s2n-quic endpoint.
///
/// Reuses `parse_udp_packet_ref` from `dpdk-udp` for validation and field extraction.
/// Extracts the ECN codepoint from the IPv4 TOS byte and constructs the path handle
/// with remote and local addresses.
///
/// Returns `None` if:
/// - The frame is not a valid IPv4/UDP datagram
/// - The destination port doesn't match `local_addr`'s port
pub fn parse_to_rx_datagram(frame: &[u8], local_addr: SocketAddr) -> Option<RxDatagram> {
    let parsed = parse_udp_packet_ref(frame)?;

    // Filter: only accept datagrams destined for our bound port
    let local_port = local_addr.port();
    if parsed.dst_port != local_port {
        return None;
    }

    // Extract ECN from TOS byte (offset ETH_HEADER_LEN + 1 in the IPv4 header)
    let tos_byte = frame[ETH_HEADER_LEN + 1];
    let ecn = extract_ecn(tos_byte);

    // Build remote address from parsed source IP:port
    let remote_addr = IpV4Address::from(parsed.src_ip.octets()).with_port(parsed.src_port);
    let remote = path::RemoteAddress::from(remote_addr);

    // Build local address from parsed destination IP:port
    let local = IpV4Address::from(parsed.dst_ip.octets()).with_port(parsed.dst_port);
    let local = path::LocalAddress::from(local);

    let mut path = DpdkPathHandle::from_remote_address(remote);
    path.set_local_address(local);

    let header = datagram::Header { path, ecn };
    let payload = parsed.payload.to_vec();

    Some(RxDatagram { header, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dpdk_udp::build_udp_frame_into_with_tos;
    use s2n_quic_core::inet::ExplicitCongestionNotification::*;
    use s2n_quic_core::io::rx::Queue as _;
    use s2n_quic_core::path::Handle as _;
    use std::net::Ipv4Addr;

    fn build_test_frame(
        src_ip: Ipv4Addr,
        dst_ip: Ipv4Addr,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
        tos: u8,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        build_udp_frame_into_with_tos(
            &mut out,
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
            &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            src_ip,
            dst_ip,
            src_port,
            dst_port,
            payload,
            64,
            tos,
        )
        .unwrap();
        out
    }

    #[test]
    fn valid_parse_produces_correct_datagram() {
        let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        let frame = build_test_frame(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            5000,
            4433,
            b"hello quic",
            0x02, // ECN Ect0
        );

        let dgram = parse_to_rx_datagram(&frame, local_addr).unwrap();

        // Verify payload
        assert_eq!(dgram.payload, b"hello quic");

        // Verify ECN
        assert_eq!(dgram.header.ecn, Ect0);

        // Verify remote address
        let remote = dgram.header.path.remote_address();
        let expected_remote =
            path::RemoteAddress::from(IpV4Address::from([10, 0, 0, 1]).with_port(5000));
        assert_eq!(remote, expected_remote);

        // Verify local address
        let local = dgram.header.path.local_address();
        let expected_local =
            path::LocalAddress::from(IpV4Address::from([10, 0, 0, 2]).with_port(4433));
        assert_eq!(local, expected_local);
    }

    #[test]
    fn wrong_dst_port_returns_none() {
        let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        let frame = build_test_frame(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            5000,
            9999, // wrong port
            b"wrong port",
            0x00,
        );

        assert!(parse_to_rx_datagram(&frame, local_addr).is_none());
    }

    #[test]
    fn non_ipv4_frame_returns_none() {
        // Build a frame with non-IPv4 EtherType (e.g., ARP = 0x0806)
        let mut frame = vec![0u8; 60];
        // Dst MAC
        frame[0..6].copy_from_slice(&[0xaa; 6]);
        // Src MAC
        frame[6..12].copy_from_slice(&[0xbb; 6]);
        // EtherType = ARP (0x0806)
        frame[12] = 0x08;
        frame[13] = 0x06;

        let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        assert!(parse_to_rx_datagram(&frame, local_addr).is_none());
    }

    #[test]
    fn truncated_frame_returns_none() {
        let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        // Too short to contain headers
        let frame = vec![0u8; 10];
        assert!(parse_to_rx_datagram(&frame, local_addr).is_none());
    }

    #[test]
    fn for_each_drains_all_datagrams() {
        let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();

        let mut queue = DpdkRxQueue::new();
        assert!(queue.is_empty());

        // Push 3 datagrams
        for i in 0..3u8 {
            let frame = build_test_frame(
                Ipv4Addr::new(10, 0, 0, 1),
                Ipv4Addr::new(10, 0, 0, 2),
                5000 + i as u16,
                4433,
                &[i; 4],
                0x00,
            );
            let dgram = parse_to_rx_datagram(&frame, local_addr).unwrap();
            queue.push(dgram);
        }

        assert!(!queue.is_empty());

        // Drain all via for_each
        let mut count = 0;
        queue.for_each(|_header, payload| {
            assert_eq!(payload.len(), 4);
            assert_eq!(payload[0], count as u8);
            count += 1;
        });

        assert_eq!(count, 3);
        assert!(queue.is_empty());
    }

    #[test]
    fn ecn_all_codepoints_extracted() {
        let local_addr: SocketAddr = "10.0.0.2:4433".parse().unwrap();

        for (tos, expected_ecn) in [(0x00, NotEct), (0x01, Ect1), (0x02, Ect0), (0x03, Ce)] {
            let frame = build_test_frame(
                Ipv4Addr::new(10, 0, 0, 1),
                Ipv4Addr::new(10, 0, 0, 2),
                5000,
                4433,
                b"ecn",
                tos,
            );
            let dgram = parse_to_rx_datagram(&frame, local_addr).unwrap();
            assert_eq!(dgram.header.ecn, expected_ecn, "TOS byte 0x{tos:02x}");
        }
    }
}
