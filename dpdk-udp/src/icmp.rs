//! ICMP (Internet Control Message Protocol) implementation
//!
//! Handles ICMP echo request/reply (ping) functionality.

use std::net::Ipv4Addr;

use crate::{ETH_HEADER_LEN, ETH_TYPE_IPV4, IPV4_HEADER_LEN, ipv4_checksum};

// ============================================================================
// Constants
// ============================================================================

/// IP protocol number for ICMP
pub const IP_PROTO_ICMP: u8 = 1;

/// ICMP type: Echo Reply
pub const ICMP_TYPE_ECHO_REPLY: u8 = 0;

/// ICMP type: Echo Request
pub const ICMP_TYPE_ECHO_REQUEST: u8 = 8;

/// ICMP code for echo messages
pub const ICMP_CODE_ECHO: u8 = 0;

/// ICMP header size (type + code + checksum + identifier + sequence)
pub const ICMP_HEADER_LEN: usize = 8;

/// Minimum ICMP packet size
pub const MIN_ICMP_PACKET_LEN: usize = ETH_HEADER_LEN + IPV4_HEADER_LEN + ICMP_HEADER_LEN;

// ============================================================================
// ICMP Packet Structure
// ============================================================================

/// Parsed ICMP packet
#[derive(Debug, Clone)]
pub struct IcmpPacket {
    /// Source MAC address
    pub src_mac: [u8; 6],
    /// Destination MAC address
    pub dst_mac: [u8; 6],
    /// Source IP address
    pub src_ip: Ipv4Addr,
    /// Destination IP address
    pub dst_ip: Ipv4Addr,
    /// TTL from IP header
    pub ttl: u8,
    /// ICMP type
    pub icmp_type: u8,
    /// ICMP code
    pub icmp_code: u8,
    /// ICMP checksum
    pub checksum: u16,
    /// Identifier (for echo request/reply)
    pub identifier: u16,
    /// Sequence number (for echo request/reply)
    pub sequence: u16,
    /// Payload data
    pub payload: Vec<u8>,
}

impl IcmpPacket {
    /// Check if this is an echo request (ping)
    pub fn is_echo_request(&self) -> bool {
        self.icmp_type == ICMP_TYPE_ECHO_REQUEST && self.icmp_code == ICMP_CODE_ECHO
    }

    /// Check if this is an echo reply (pong)
    pub fn is_echo_reply(&self) -> bool {
        self.icmp_type == ICMP_TYPE_ECHO_REPLY && self.icmp_code == ICMP_CODE_ECHO
    }

    /// Create an echo reply for this echo request
    pub fn make_echo_reply(&self, reply_src_mac: [u8; 6]) -> Option<IcmpPacket> {
        if !self.is_echo_request() {
            return None;
        }

        Some(IcmpPacket {
            src_mac: reply_src_mac,
            dst_mac: self.src_mac,
            src_ip: self.dst_ip,
            dst_ip: self.src_ip,
            ttl: 64,
            icmp_type: ICMP_TYPE_ECHO_REPLY,
            icmp_code: ICMP_CODE_ECHO,
            checksum: 0, // Will be calculated when building
            identifier: self.identifier,
            sequence: self.sequence,
            payload: self.payload.clone(),
        })
    }
}

// ============================================================================
// Checksum
// ============================================================================

/// Calculate ICMP checksum
///
/// The checksum is the 16-bit one's complement of the one's complement sum
/// of the ICMP message starting with the ICMP Type.
pub fn icmp_checksum(icmp_header_and_data: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    // Sum all 16-bit words
    for i in (0..icmp_header_and_data.len()).step_by(2) {
        let word = if i + 1 < icmp_header_and_data.len() {
            ((icmp_header_and_data[i] as u32) << 8) | (icmp_header_and_data[i + 1] as u32)
        } else {
            (icmp_header_and_data[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    // Fold 32-bit sum to 16 bits
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    // One's complement
    !(sum as u16)
}

// ============================================================================
// Parsing
// ============================================================================

/// Parse an ICMP packet from a raw Ethernet frame
///
/// Returns None if the frame is not a valid ICMP packet
pub fn parse_icmp_packet(frame: &[u8]) -> Option<IcmpPacket> {
    // Minimum size check
    if frame.len() < MIN_ICMP_PACKET_LEN {
        return None;
    }

    // Parse Ethernet header
    let dst_mac: [u8; 6] = frame[0..6].try_into().ok()?;
    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    // Only handle IPv4
    if ethertype != ETH_TYPE_IPV4 {
        return None;
    }

    // Parse IPv4 header
    let ip_header = &frame[ETH_HEADER_LEN..];

    // Check IP version
    let version = (ip_header[0] >> 4) & 0x0F;
    if version != 4 {
        return None;
    }

    // Get IP header length
    let ihl = (ip_header[0] & 0x0F) as usize;
    let ip_header_len = ihl * 4;
    if ip_header_len < 20 {
        return None;
    }

    // Check protocol
    let protocol = ip_header[9];
    if protocol != IP_PROTO_ICMP {
        return None;
    }

    let ttl = ip_header[8];
    let src_ip = Ipv4Addr::new(ip_header[12], ip_header[13], ip_header[14], ip_header[15]);
    let dst_ip = Ipv4Addr::new(ip_header[16], ip_header[17], ip_header[18], ip_header[19]);

    // Parse ICMP header
    let icmp_start = ETH_HEADER_LEN + ip_header_len;
    if frame.len() < icmp_start + ICMP_HEADER_LEN {
        return None;
    }

    let icmp = &frame[icmp_start..];
    let icmp_type = icmp[0];
    let icmp_code = icmp[1];
    let checksum = u16::from_be_bytes([icmp[2], icmp[3]]);
    let identifier = u16::from_be_bytes([icmp[4], icmp[5]]);
    let sequence = u16::from_be_bytes([icmp[6], icmp[7]]);

    // Extract payload
    let payload = if frame.len() > icmp_start + ICMP_HEADER_LEN {
        frame[icmp_start + ICMP_HEADER_LEN..].to_vec()
    } else {
        Vec::new()
    };

    Some(IcmpPacket {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        ttl,
        icmp_type,
        icmp_code,
        checksum,
        identifier,
        sequence,
        payload,
    })
}

// ============================================================================
// Building
// ============================================================================

/// Build an ICMP packet into a raw Ethernet frame
pub fn build_icmp_frame(packet: &IcmpPacket) -> Vec<u8> {
    let total_len = ETH_HEADER_LEN + IPV4_HEADER_LEN + ICMP_HEADER_LEN + packet.payload.len();
    let mut frame = vec![0u8; total_len];

    // Ethernet header
    frame[0..6].copy_from_slice(&packet.dst_mac);
    frame[6..12].copy_from_slice(&packet.src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

    // IPv4 header
    let ip_start = ETH_HEADER_LEN;
    let ip_total_len = (IPV4_HEADER_LEN + ICMP_HEADER_LEN + packet.payload.len()) as u16;

    frame[ip_start] = 0x45; // Version 4, IHL 5
    frame[ip_start + 1] = 0x00; // DSCP/ECN
    frame[ip_start + 2..ip_start + 4].copy_from_slice(&ip_total_len.to_be_bytes());
    frame[ip_start + 4..ip_start + 6].copy_from_slice(&[0x00, 0x00]); // Identification
    frame[ip_start + 6..ip_start + 8].copy_from_slice(&[0x40, 0x00]); // Flags (DF) + Fragment
    frame[ip_start + 8] = packet.ttl;
    frame[ip_start + 9] = IP_PROTO_ICMP;
    frame[ip_start + 10..ip_start + 12].copy_from_slice(&[0x00, 0x00]); // Checksum placeholder
    frame[ip_start + 12..ip_start + 16].copy_from_slice(&packet.src_ip.octets());
    frame[ip_start + 16..ip_start + 20].copy_from_slice(&packet.dst_ip.octets());

    // Calculate IP checksum
    let ip_cksum = ipv4_checksum(&frame[ip_start..ip_start + IPV4_HEADER_LEN]);
    frame[ip_start + 10..ip_start + 12].copy_from_slice(&ip_cksum.to_be_bytes());

    // ICMP header
    let icmp_start = ETH_HEADER_LEN + IPV4_HEADER_LEN;
    frame[icmp_start] = packet.icmp_type;
    frame[icmp_start + 1] = packet.icmp_code;
    frame[icmp_start + 2..icmp_start + 4].copy_from_slice(&[0x00, 0x00]); // Checksum placeholder
    frame[icmp_start + 4..icmp_start + 6].copy_from_slice(&packet.identifier.to_be_bytes());
    frame[icmp_start + 6..icmp_start + 8].copy_from_slice(&packet.sequence.to_be_bytes());

    // Payload
    if !packet.payload.is_empty() {
        frame[icmp_start + ICMP_HEADER_LEN..].copy_from_slice(&packet.payload);
    }

    // Calculate ICMP checksum (over ICMP header + payload)
    let icmp_cksum = icmp_checksum(&frame[icmp_start..]);
    frame[icmp_start + 2..icmp_start + 4].copy_from_slice(&icmp_cksum.to_be_bytes());

    frame
}

/// Build an echo request frame
pub fn build_echo_request(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> Vec<u8> {
    let packet = IcmpPacket {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        ttl: 64,
        icmp_type: ICMP_TYPE_ECHO_REQUEST,
        icmp_code: ICMP_CODE_ECHO,
        checksum: 0,
        identifier,
        sequence,
        payload: payload.to_vec(),
    };
    build_icmp_frame(&packet)
}

/// Build an echo reply frame
pub fn build_echo_reply(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> Vec<u8> {
    let packet = IcmpPacket {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        ttl: 64,
        icmp_type: ICMP_TYPE_ECHO_REPLY,
        icmp_code: ICMP_CODE_ECHO,
        checksum: 0,
        identifier,
        sequence,
        payload: payload.to_vec(),
    };
    build_icmp_frame(&packet)
}

// ============================================================================
// ICMP Handler
// ============================================================================

/// Handles ICMP protocol operations
pub struct IcmpHandler {
    /// Our MAC address
    pub local_mac: [u8; 6],
    /// Our IP addresses
    pub local_ips: Vec<Ipv4Addr>,
}

impl IcmpHandler {
    /// Create a new ICMP handler
    pub fn new(local_mac: [u8; 6], local_ip: Ipv4Addr) -> Self {
        Self {
            local_mac,
            local_ips: vec![local_ip],
        }
    }

    /// Add a local IP address
    pub fn add_local_ip(&mut self, ip: Ipv4Addr) {
        if !self.local_ips.contains(&ip) {
            self.local_ips.push(ip);
        }
    }

    /// Process an incoming ICMP packet
    ///
    /// Returns an echo reply frame if this was an echo request for our IP,
    /// or None if no response is needed.
    pub fn process_icmp(&self, frame: &[u8]) -> Option<Vec<u8>> {
        let packet = parse_icmp_packet(frame)?;

        // Only respond to echo requests for our IPs
        if packet.is_echo_request() && self.local_ips.contains(&packet.dst_ip) {
            let reply = packet.make_echo_reply(self.local_mac)?;
            return Some(build_icmp_frame(&reply));
        }

        None
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_icmp_frame(
        src_mac: [u8; 6],
        dst_mac: [u8; 6],
        src_ip: Ipv4Addr,
        dst_ip: Ipv4Addr,
        icmp_type: u8,
        icmp_code: u8,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let total_len = MIN_ICMP_PACKET_LEN + payload.len();
        let mut frame = vec![0u8; total_len];

        // Ethernet
        frame[0..6].copy_from_slice(&dst_mac);
        frame[6..12].copy_from_slice(&src_mac);
        frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

        // IP
        let ip_start = ETH_HEADER_LEN;
        let ip_total_len = (IPV4_HEADER_LEN + ICMP_HEADER_LEN + payload.len()) as u16;
        frame[ip_start] = 0x45;
        frame[ip_start + 2..ip_start + 4].copy_from_slice(&ip_total_len.to_be_bytes());
        frame[ip_start + 8] = 64; // TTL
        frame[ip_start + 9] = IP_PROTO_ICMP;
        frame[ip_start + 12..ip_start + 16].copy_from_slice(&src_ip.octets());
        frame[ip_start + 16..ip_start + 20].copy_from_slice(&dst_ip.octets());

        // ICMP
        let icmp_start = ETH_HEADER_LEN + IPV4_HEADER_LEN;
        frame[icmp_start] = icmp_type;
        frame[icmp_start + 1] = icmp_code;
        frame[icmp_start + 4..icmp_start + 6].copy_from_slice(&identifier.to_be_bytes());
        frame[icmp_start + 6..icmp_start + 8].copy_from_slice(&sequence.to_be_bytes());

        // Payload
        if !payload.is_empty() {
            frame[icmp_start + ICMP_HEADER_LEN..].copy_from_slice(payload);
        }

        // Calculate ICMP checksum
        let icmp_cksum = icmp_checksum(&frame[icmp_start..]);
        frame[icmp_start + 2..icmp_start + 4].copy_from_slice(&icmp_cksum.to_be_bytes());

        frame
    }

    #[test]
    fn test_icmp_constants() {
        assert_eq!(IP_PROTO_ICMP, 1);
        assert_eq!(ICMP_TYPE_ECHO_REPLY, 0);
        assert_eq!(ICMP_TYPE_ECHO_REQUEST, 8);
        assert_eq!(ICMP_HEADER_LEN, 8);
    }

    #[test]
    fn test_parse_echo_request() {
        let src_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let dst_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let src_ip = Ipv4Addr::new(192, 168, 1, 100);
        let dst_ip = Ipv4Addr::new(192, 168, 1, 1);

        let frame = build_test_icmp_frame(
            src_mac,
            dst_mac,
            src_ip,
            dst_ip,
            ICMP_TYPE_ECHO_REQUEST,
            ICMP_CODE_ECHO,
            0x1234,
            0x0001,
            b"ping payload",
        );

        let parsed = parse_icmp_packet(&frame);
        assert!(parsed.is_some());

        let p = parsed.unwrap();
        assert!(p.is_echo_request());
        assert!(!p.is_echo_reply());
        assert_eq!(p.src_ip, src_ip);
        assert_eq!(p.dst_ip, dst_ip);
        assert_eq!(p.identifier, 0x1234);
        assert_eq!(p.sequence, 0x0001);
        assert_eq!(p.payload, b"ping payload");
    }

    #[test]
    fn test_parse_echo_reply() {
        let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let dst_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let src_ip = Ipv4Addr::new(192, 168, 1, 1);
        let dst_ip = Ipv4Addr::new(192, 168, 1, 100);

        let frame = build_test_icmp_frame(
            src_mac,
            dst_mac,
            src_ip,
            dst_ip,
            ICMP_TYPE_ECHO_REPLY,
            ICMP_CODE_ECHO,
            0x1234,
            0x0001,
            b"pong",
        );

        let parsed = parse_icmp_packet(&frame);
        assert!(parsed.is_some());

        let p = parsed.unwrap();
        assert!(!p.is_echo_request());
        assert!(p.is_echo_reply());
    }

    #[test]
    fn test_parse_invalid_frame() {
        // Too short
        let short = [0u8; 10];
        assert!(parse_icmp_packet(&short).is_none());

        // Wrong protocol
        let mut frame = build_test_icmp_frame(
            [1; 6], [2; 6],
            Ipv4Addr::new(1, 2, 3, 4),
            Ipv4Addr::new(5, 6, 7, 8),
            ICMP_TYPE_ECHO_REQUEST,
            0,
            0,
            0,
            b"",
        );
        frame[ETH_HEADER_LEN + 9] = 17; // Change to UDP
        assert!(parse_icmp_packet(&frame).is_none());
    }

    #[test]
    fn test_make_echo_reply() {
        let src_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let dst_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let src_ip = Ipv4Addr::new(192, 168, 1, 100);
        let dst_ip = Ipv4Addr::new(192, 168, 1, 1);

        let request = IcmpPacket {
            src_mac,
            dst_mac,
            src_ip,
            dst_ip,
            ttl: 64,
            icmp_type: ICMP_TYPE_ECHO_REQUEST,
            icmp_code: ICMP_CODE_ECHO,
            checksum: 0,
            identifier: 0x1234,
            sequence: 42,
            payload: b"test data".to_vec(),
        };

        let reply_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let reply = request.make_echo_reply(reply_mac);
        assert!(reply.is_some());

        let r = reply.unwrap();
        assert!(r.is_echo_reply());
        assert_eq!(r.src_mac, reply_mac);
        assert_eq!(r.dst_mac, src_mac); // Reply goes back to requester
        assert_eq!(r.src_ip, dst_ip); // Our IP
        assert_eq!(r.dst_ip, src_ip); // Requester's IP
        assert_eq!(r.identifier, 0x1234); // Same identifier
        assert_eq!(r.sequence, 42); // Same sequence
        assert_eq!(r.payload, b"test data"); // Same payload
    }

    #[test]
    fn test_make_echo_reply_not_request() {
        let reply_packet = IcmpPacket {
            src_mac: [1; 6],
            dst_mac: [2; 6],
            src_ip: Ipv4Addr::new(1, 2, 3, 4),
            dst_ip: Ipv4Addr::new(5, 6, 7, 8),
            ttl: 64,
            icmp_type: ICMP_TYPE_ECHO_REPLY, // Already a reply
            icmp_code: ICMP_CODE_ECHO,
            checksum: 0,
            identifier: 0,
            sequence: 0,
            payload: vec![],
        };

        // Can't make a reply from a reply
        assert!(reply_packet.make_echo_reply([3; 6]).is_none());
    }

    #[test]
    fn test_build_and_parse_roundtrip() {
        let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let dst_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let src_ip = Ipv4Addr::new(10, 0, 0, 1);
        let dst_ip = Ipv4Addr::new(10, 0, 0, 2);
        let payload = b"roundtrip test payload";

        let frame = build_echo_request(
            src_mac,
            dst_mac,
            src_ip,
            dst_ip,
            0xABCD,
            0x0042,
            payload,
        );

        let parsed = parse_icmp_packet(&frame);
        assert!(parsed.is_some());

        let p = parsed.unwrap();
        assert!(p.is_echo_request());
        assert_eq!(p.src_mac, src_mac);
        assert_eq!(p.dst_mac, dst_mac);
        assert_eq!(p.src_ip, src_ip);
        assert_eq!(p.dst_ip, dst_ip);
        assert_eq!(p.identifier, 0xABCD);
        assert_eq!(p.sequence, 0x0042);
        assert_eq!(p.payload, payload);
    }

    #[test]
    fn test_icmp_checksum() {
        // Simple checksum test with known values
        let data = [
            0x08, 0x00, // Type, Code
            0x00, 0x00, // Checksum placeholder
            0x12, 0x34, // Identifier
            0x00, 0x01, // Sequence
        ];
        let checksum = icmp_checksum(&data);
        assert_ne!(checksum, 0);

        // Verify: checksum of data with correct checksum should be 0
        let mut with_checksum = data.clone();
        with_checksum[2..4].copy_from_slice(&checksum.to_be_bytes());
        assert_eq!(icmp_checksum(&with_checksum), 0);
    }

    #[test]
    fn test_icmp_handler_echo_reply() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = IcmpHandler::new(local_mac, local_ip);

        let requester_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let requester_ip = Ipv4Addr::new(192, 168, 1, 100);

        // Create echo request
        let request = build_echo_request(
            requester_mac,
            local_mac,
            requester_ip,
            local_ip,
            0x1234,
            1,
            b"ping",
        );

        // Process it
        let reply = handler.process_icmp(&request);
        assert!(reply.is_some());

        // Verify reply
        let reply_parsed = parse_icmp_packet(&reply.unwrap());
        assert!(reply_parsed.is_some());

        let r = reply_parsed.unwrap();
        assert!(r.is_echo_reply());
        assert_eq!(r.src_ip, local_ip);
        assert_eq!(r.dst_ip, requester_ip);
        assert_eq!(r.identifier, 0x1234);
        assert_eq!(r.sequence, 1);
        assert_eq!(r.payload, b"ping");
    }

    #[test]
    fn test_icmp_handler_ignores_other_ips() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = IcmpHandler::new(local_mac, local_ip);

        let other_ip = Ipv4Addr::new(192, 168, 1, 99);
        let requester_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let requester_ip = Ipv4Addr::new(192, 168, 1, 100);

        // Create echo request for a different IP
        let request = build_echo_request(
            requester_mac,
            local_mac,
            requester_ip,
            other_ip, // Not our IP
            0x1234,
            1,
            b"ping",
        );

        // Should not respond
        let reply = handler.process_icmp(&request);
        assert!(reply.is_none());
    }

    #[test]
    fn test_icmp_handler_multiple_ips() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip1 = Ipv4Addr::new(192, 168, 1, 1);
        let local_ip2 = Ipv4Addr::new(10, 0, 0, 1);

        let mut handler = IcmpHandler::new(local_mac, local_ip1);
        handler.add_local_ip(local_ip2);

        let requester_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let requester_ip = Ipv4Addr::new(192, 168, 1, 100);

        // Should respond to both IPs
        let request1 = build_echo_request(
            requester_mac, local_mac, requester_ip, local_ip1, 1, 1, b"",
        );
        assert!(handler.process_icmp(&request1).is_some());

        let request2 = build_echo_request(
            requester_mac, local_mac, requester_ip, local_ip2, 2, 1, b"",
        );
        assert!(handler.process_icmp(&request2).is_some());
    }
}
