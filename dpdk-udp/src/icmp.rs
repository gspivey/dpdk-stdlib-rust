//! ICMP (Internet Control Message Protocol) implementation
//!
//! Handles ICMP echo request/reply (ping) and ICMP error processing.
//!
//! ICMP error messages (Destination Unreachable, Time Exceeded, etc.) carry the
//! IP header + first 8 bytes of the original datagram that triggered the error.
//! For UDP, those 8 bytes are the source and destination ports, which lets us
//! match errors back to the originating socket and surface them via `take_error()`.

use std::io;
use std::net::Ipv4Addr;

use crate::{ETH_HEADER_LEN, ETH_TYPE_IPV4, IPV4_HEADER_LEN, UDP_HEADER_LEN, ipv4_checksum};

// ============================================================================
// Constants
// ============================================================================

/// IP protocol number for ICMP
pub const IP_PROTO_ICMP: u8 = 1;

/// IP protocol number for UDP (used when parsing original datagram in ICMP errors)
pub const IP_PROTO_UDP: u8 = 17;

/// ICMP type: Echo Reply
pub const ICMP_TYPE_ECHO_REPLY: u8 = 0;

/// ICMP type: Destination Unreachable
pub const ICMP_TYPE_DEST_UNREACHABLE: u8 = 3;

/// ICMP type: Redirect
pub const ICMP_TYPE_REDIRECT: u8 = 5;

/// ICMP type: Echo Request
pub const ICMP_TYPE_ECHO_REQUEST: u8 = 8;

/// ICMP type: Time Exceeded
pub const ICMP_TYPE_TIME_EXCEEDED: u8 = 11;

/// ICMP type: Parameter Problem
pub const ICMP_TYPE_PARAMETER_PROBLEM: u8 = 12;

/// ICMP code for echo messages
pub const ICMP_CODE_ECHO: u8 = 0;

// Destination Unreachable codes (RFC 792 + RFC 1122)
/// Network Unreachable
pub const ICMP_CODE_NET_UNREACHABLE: u8 = 0;
/// Host Unreachable
pub const ICMP_CODE_HOST_UNREACHABLE: u8 = 1;
/// Protocol Unreachable
pub const ICMP_CODE_PROTO_UNREACHABLE: u8 = 2;
/// Port Unreachable
pub const ICMP_CODE_PORT_UNREACHABLE: u8 = 3;
/// Fragmentation Needed and DF Set (carries Next-Hop MTU in bytes 6-7)
pub const ICMP_CODE_FRAG_NEEDED: u8 = 4;
/// Source Route Failed
pub const ICMP_CODE_SOURCE_ROUTE_FAILED: u8 = 5;
/// Communication Administratively Prohibited (RFC 1812)
pub const ICMP_CODE_ADMIN_PROHIBITED: u8 = 13;

// Time Exceeded codes
/// TTL Exceeded in Transit
pub const ICMP_CODE_TTL_EXCEEDED: u8 = 0;
/// Fragment Reassembly Time Exceeded
pub const ICMP_CODE_FRAG_REASSEMBLY_EXCEEDED: u8 = 1;

/// ICMP header size (type + code + checksum + identifier/unused + sequence/mtu)
pub const ICMP_HEADER_LEN: usize = 8;

/// Minimum ICMP packet size (echo)
pub const MIN_ICMP_PACKET_LEN: usize = ETH_HEADER_LEN + IPV4_HEADER_LEN + ICMP_HEADER_LEN;

/// Minimum ICMP error packet size: outer headers + ICMP header + original IP header + 8 bytes of original transport
pub const MIN_ICMP_ERROR_PAYLOAD: usize = IPV4_HEADER_LEN + UDP_HEADER_LEN;

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

    /// Check if this is an ICMP error message (carries original datagram info).
    pub fn is_error(&self) -> bool {
        matches!(
            self.icmp_type,
            ICMP_TYPE_DEST_UNREACHABLE
                | ICMP_TYPE_TIME_EXCEEDED
                | ICMP_TYPE_REDIRECT
                | ICMP_TYPE_PARAMETER_PROBLEM
        )
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
// ICMP Error Handling
// ============================================================================

/// Parsed ICMP error with context from the original datagram.
///
/// ICMP error messages (types 3, 5, 11, 12) carry the IP header + first 8
/// bytes of the original packet that triggered the error. For UDP, those 8
/// bytes contain the source and destination ports.
#[derive(Debug, Clone)]
pub struct IcmpErrorInfo {
    /// ICMP error type (3 = Dest Unreachable, 11 = Time Exceeded, etc.)
    pub icmp_type: u8,
    /// ICMP error code (sub-type within the error category)
    pub icmp_code: u8,
    /// IP address of the router/host that generated the error
    pub error_source: Ipv4Addr,
    /// Original destination IP from the packet that triggered the error
    pub original_dst_ip: Ipv4Addr,
    /// Original source IP from the packet that triggered the error
    pub original_src_ip: Ipv4Addr,
    /// Original destination port (from the UDP header in the ICMP payload)
    pub original_dst_port: u16,
    /// Original source port (from the UDP header in the ICMP payload)
    pub original_src_port: u16,
    /// Next-Hop MTU (only valid for Fragmentation Needed, type 3 code 4)
    pub next_hop_mtu: u16,
}

impl IcmpErrorInfo {
    /// Convert this ICMP error into an `io::Error` matching Linux kernel behavior.
    ///
    /// Linux maps ICMP errors to errno values which are surfaced via `SO_ERROR`
    /// / `take_error()`. We replicate that mapping here.
    pub fn to_io_error(&self) -> io::Error {
        match (self.icmp_type, self.icmp_code) {
            (ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_NET_UNREACHABLE) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMP: network unreachable (from {})", self.error_source),
            ),
            (ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_HOST_UNREACHABLE) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMP: host unreachable (from {})", self.error_source),
            ),
            (ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_PROTO_UNREACHABLE)
            | (ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_PORT_UNREACHABLE) => io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!(
                    "ICMP: {} unreachable (from {})",
                    if self.icmp_code == ICMP_CODE_PORT_UNREACHABLE {
                        "port"
                    } else {
                        "protocol"
                    },
                    self.error_source,
                ),
            ),
            (ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_FRAG_NEEDED) => io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "ICMP: fragmentation needed, next-hop MTU {} (from {})",
                    self.next_hop_mtu, self.error_source
                ),
            ),
            (ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_ADMIN_PROHIBITED) => io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "ICMP: communication administratively prohibited (from {})",
                    self.error_source
                ),
            ),
            (ICMP_TYPE_DEST_UNREACHABLE, code) => io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "ICMP: destination unreachable code {} (from {})",
                    code, self.error_source
                ),
            ),
            (ICMP_TYPE_TIME_EXCEEDED, ICMP_CODE_TTL_EXCEEDED) => io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ICMP: TTL exceeded in transit (from {})", self.error_source),
            ),
            (ICMP_TYPE_TIME_EXCEEDED, ICMP_CODE_FRAG_REASSEMBLY_EXCEEDED) => io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "ICMP: fragment reassembly time exceeded (from {})",
                    self.error_source
                ),
            ),
            (ICMP_TYPE_REDIRECT, _) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMP: redirect (from {})", self.error_source),
            ),
            (ICMP_TYPE_PARAMETER_PROBLEM, _) => io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ICMP: parameter problem (from {})", self.error_source),
            ),
            (typ, code) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMP error type {} code {} (from {})", typ, code, self.error_source),
            ),
        }
    }
}

/// Parse an ICMP error message and extract the original datagram context.
///
/// ICMP error messages have this structure:
/// ```text
/// [Ethernet 14B][Outer IP 20B][ICMP Header 8B][Original IP Header 20B+][Original Transport 8B]
/// ```
///
/// We extract the original IP src/dst and the original UDP src/dst ports from
/// the embedded datagram, so the socket layer can match the error to the right
/// socket.
///
/// Returns `None` if the frame is not a valid ICMP error for a UDP datagram.
pub fn parse_icmp_error(frame: &[u8]) -> Option<IcmpErrorInfo> {
    // Detect VLAN tag and determine L3 offset.
    let layout = crate::detect_vlan(frame, None)?;
    let l3 = layout.l3_offset;

    if layout.ethertype != ETH_TYPE_IPV4 {
        return None;
    }

    // Minimum: L3 + outer IP(20) + ICMP header(8) + original IP(20) + original UDP ports(8)
    let min_len = l3 + IPV4_HEADER_LEN + ICMP_HEADER_LEN + MIN_ICMP_ERROR_PAYLOAD;
    if frame.len() < min_len {
        return None;
    }

    // Parse outer IP header
    let outer_ip = &frame[l3..];
    let version = (outer_ip[0] >> 4) & 0x0F;
    if version != 4 {
        return None;
    }
    let outer_ihl = (outer_ip[0] & 0x0F) as usize * 4;
    if outer_ihl < 20 {
        return None;
    }
    let protocol = outer_ip[9];
    if protocol != IP_PROTO_ICMP {
        return None;
    }
    let error_source = Ipv4Addr::new(outer_ip[12], outer_ip[13], outer_ip[14], outer_ip[15]);

    // Parse ICMP header
    let icmp_start = l3 + outer_ihl;
    if frame.len() < icmp_start + ICMP_HEADER_LEN + MIN_ICMP_ERROR_PAYLOAD {
        return None;
    }
    let icmp = &frame[icmp_start..];
    let icmp_type = icmp[0];
    let icmp_code = icmp[1];

    // Only process error types
    if !matches!(
        icmp_type,
        ICMP_TYPE_DEST_UNREACHABLE
            | ICMP_TYPE_TIME_EXCEEDED
            | ICMP_TYPE_REDIRECT
            | ICMP_TYPE_PARAMETER_PROBLEM
    ) {
        return None;
    }

    // For Fragmentation Needed (type 3, code 4), bytes 6-7 contain the Next-Hop MTU
    let next_hop_mtu = if icmp_type == ICMP_TYPE_DEST_UNREACHABLE && icmp_code == ICMP_CODE_FRAG_NEEDED {
        u16::from_be_bytes([icmp[6], icmp[7]])
    } else {
        0
    };

    // Parse the original IP header embedded in the ICMP payload
    let orig_ip_start = icmp_start + ICMP_HEADER_LEN;
    let orig_ip = &frame[orig_ip_start..];
    let orig_version = (orig_ip[0] >> 4) & 0x0F;
    if orig_version != 4 {
        return None;
    }
    let orig_ihl = (orig_ip[0] & 0x0F) as usize * 4;
    if orig_ihl < 20 {
        return None;
    }

    // Check that the original packet was UDP
    let orig_protocol = orig_ip[9];
    if orig_protocol != IP_PROTO_UDP {
        return None;
    }

    let original_src_ip = Ipv4Addr::new(orig_ip[12], orig_ip[13], orig_ip[14], orig_ip[15]);
    let original_dst_ip = Ipv4Addr::new(orig_ip[16], orig_ip[17], orig_ip[18], orig_ip[19]);

    // Extract original UDP ports (first 4 bytes of the original transport header)
    let orig_udp_start = orig_ip_start + orig_ihl;
    if frame.len() < orig_udp_start + 4 {
        return None;
    }
    let original_src_port = u16::from_be_bytes([frame[orig_udp_start], frame[orig_udp_start + 1]]);
    let original_dst_port = u16::from_be_bytes([frame[orig_udp_start + 2], frame[orig_udp_start + 3]]);

    Some(IcmpErrorInfo {
        icmp_type,
        icmp_code,
        error_source,
        original_dst_ip,
        original_src_ip,
        original_dst_port,
        original_src_port,
        next_hop_mtu,
    })
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
    // Detect VLAN tag and determine L3 offset.
    let layout = crate::detect_vlan(frame, None)?;
    let l3 = layout.l3_offset;

    if layout.ethertype != ETH_TYPE_IPV4 {
        return None;
    }

    // Minimum size: L3 + IP header + ICMP header
    if frame.len() < l3 + IPV4_HEADER_LEN + ICMP_HEADER_LEN {
        return None;
    }

    let dst_mac: [u8; 6] = frame[0..6].try_into().ok()?;
    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;

    let ip_header = &frame[l3..];
    let version = (ip_header[0] >> 4) & 0x0F;
    if version != 4 {
        return None;
    }
    let ihl = (ip_header[0] & 0x0F) as usize;
    let ip_header_len = ihl * 4;
    if ip_header_len < 20 {
        return None;
    }
    if ip_header[9] != IP_PROTO_ICMP {
        return None;
    }

    let ttl = ip_header[8];
    let src_ip = Ipv4Addr::new(ip_header[12], ip_header[13], ip_header[14], ip_header[15]);
    let dst_ip = Ipv4Addr::new(ip_header[16], ip_header[17], ip_header[18], ip_header[19]);

    let icmp_start = l3 + ip_header_len;
    if frame.len() < icmp_start + ICMP_HEADER_LEN {
        return None;
    }

    let icmp = &frame[icmp_start..];
    let icmp_type = icmp[0];
    let icmp_code = icmp[1];
    let checksum = u16::from_be_bytes([icmp[2], icmp[3]]);
    let identifier = u16::from_be_bytes([icmp[4], icmp[5]]);
    let sequence = u16::from_be_bytes([icmp[6], icmp[7]]);

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

/// Result of processing an ICMP packet: either a reply frame to send, or
/// an error to queue on the matching socket.
pub enum IcmpAction {
    /// An echo reply frame that should be transmitted back.
    Reply(Vec<u8>),
    /// An ICMP error that should be queued on the originating socket.
    Error(IcmpErrorInfo),
}

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

    /// Process an incoming ICMP packet (legacy API — echo only).
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

    /// Process an incoming ICMP packet, handling both echo requests and error messages.
    ///
    /// Returns `Some(IcmpAction::Reply(frame))` for echo requests addressed to us,
    /// or `Some(IcmpAction::Error(info))` for ICMP errors that reference a UDP
    /// datagram originating from one of our local IPs.
    pub fn process_icmp_full(&self, frame: &[u8]) -> Option<IcmpAction> {
        // Try echo request first (most common in-bound ICMP)
        if let Some(packet) = parse_icmp_packet(frame) {
            if packet.is_echo_request() && self.local_ips.contains(&packet.dst_ip) {
                let reply = packet.make_echo_reply(self.local_mac)?;
                return Some(IcmpAction::Reply(build_icmp_frame(&reply)));
            }
        }

        // Try ICMP error (type 3, 5, 11, 12 with embedded original datagram)
        if let Some(error_info) = parse_icmp_error(frame) {
            // Only accept errors about datagrams that originated from us
            if self.local_ips.contains(&error_info.original_src_ip) {
                return Some(IcmpAction::Error(error_info));
            }
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

    // ========================================================================
    // ICMP Error Parsing Tests
    // ========================================================================

    /// Build a synthetic ICMP error frame embedding an original UDP datagram header.
    ///
    /// Layout: [Eth 14][Outer IP 20][ICMP Hdr 8][Original IP 20][Original UDP 8]
    fn build_test_icmp_error_frame(
        src_mac: [u8; 6],
        dst_mac: [u8; 6],
        error_src_ip: Ipv4Addr,
        error_dst_ip: Ipv4Addr,
        icmp_type: u8,
        icmp_code: u8,
        next_hop_mtu: u16,
        orig_src_ip: Ipv4Addr,
        orig_dst_ip: Ipv4Addr,
        orig_src_port: u16,
        orig_dst_port: u16,
    ) -> Vec<u8> {
        // Total: Eth(14) + outer IP(20) + ICMP(8) + orig IP(20) + orig UDP(8) = 70
        let total = 14 + 20 + 8 + 20 + 8;
        let mut frame = vec![0u8; total];

        // Ethernet header
        frame[0..6].copy_from_slice(&dst_mac);
        frame[6..12].copy_from_slice(&src_mac);
        frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

        // Outer IP header
        let ip = 14;
        frame[ip] = 0x45;
        let outer_total_len = (20 + 8 + 20 + 8) as u16;
        frame[ip + 2..ip + 4].copy_from_slice(&outer_total_len.to_be_bytes());
        frame[ip + 8] = 64; // TTL
        frame[ip + 9] = IP_PROTO_ICMP;
        frame[ip + 12..ip + 16].copy_from_slice(&error_src_ip.octets());
        frame[ip + 16..ip + 20].copy_from_slice(&error_dst_ip.octets());

        // ICMP header
        let icmp = 34;
        frame[icmp] = icmp_type;
        frame[icmp + 1] = icmp_code;
        // bytes 4-5: unused (or pointer for param problem)
        // bytes 6-7: next-hop MTU for frag needed
        frame[icmp + 6..icmp + 8].copy_from_slice(&next_hop_mtu.to_be_bytes());

        // Original IP header (embedded in ICMP payload)
        let orig_ip = 42;
        frame[orig_ip] = 0x45;
        let orig_total_len = (20 + 8) as u16;
        frame[orig_ip + 2..orig_ip + 4].copy_from_slice(&orig_total_len.to_be_bytes());
        frame[orig_ip + 8] = 64;
        frame[orig_ip + 9] = IP_PROTO_UDP;
        frame[orig_ip + 12..orig_ip + 16].copy_from_slice(&orig_src_ip.octets());
        frame[orig_ip + 16..orig_ip + 20].copy_from_slice(&orig_dst_ip.octets());

        // Original UDP header (first 8 bytes)
        let orig_udp = 62;
        frame[orig_udp..orig_udp + 2].copy_from_slice(&orig_src_port.to_be_bytes());
        frame[orig_udp + 2..orig_udp + 4].copy_from_slice(&orig_dst_port.to_be_bytes());

        // Compute ICMP checksum
        let cksum = icmp_checksum(&frame[icmp..]);
        frame[icmp + 2..icmp + 4].copy_from_slice(&cksum.to_be_bytes());

        frame
    }

    #[test]
    fn test_parse_icmp_error_port_unreachable() {
        let router_ip = Ipv4Addr::new(10, 0, 1, 1);
        let our_ip = Ipv4Addr::new(10, 0, 1, 100);
        let peer_ip = Ipv4Addr::new(10, 0, 2, 200);

        let frame = build_test_icmp_error_frame(
            [0xaa; 6], [0xbb; 6],
            router_ip, our_ip,
            ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_PORT_UNREACHABLE, 0,
            our_ip, peer_ip, 12345, 9000,
        );

        let err = parse_icmp_error(&frame).expect("should parse");
        assert_eq!(err.icmp_type, ICMP_TYPE_DEST_UNREACHABLE);
        assert_eq!(err.icmp_code, ICMP_CODE_PORT_UNREACHABLE);
        assert_eq!(err.error_source, router_ip);
        assert_eq!(err.original_src_ip, our_ip);
        assert_eq!(err.original_dst_ip, peer_ip);
        assert_eq!(err.original_src_port, 12345);
        assert_eq!(err.original_dst_port, 9000);
        assert_eq!(err.next_hop_mtu, 0);
    }

    #[test]
    fn test_parse_icmp_error_frag_needed_with_mtu() {
        let router_ip = Ipv4Addr::new(10, 0, 1, 1);
        let our_ip = Ipv4Addr::new(10, 0, 1, 100);
        let peer_ip = Ipv4Addr::new(10, 0, 2, 200);

        let frame = build_test_icmp_error_frame(
            [0xaa; 6], [0xbb; 6],
            router_ip, our_ip,
            ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_FRAG_NEEDED, 1280,
            our_ip, peer_ip, 12345, 9000,
        );

        let err = parse_icmp_error(&frame).expect("should parse");
        assert_eq!(err.icmp_code, ICMP_CODE_FRAG_NEEDED);
        assert_eq!(err.next_hop_mtu, 1280);
    }

    #[test]
    fn test_parse_icmp_error_ttl_exceeded() {
        let router_ip = Ipv4Addr::new(10, 0, 1, 1);
        let our_ip = Ipv4Addr::new(10, 0, 1, 100);
        let peer_ip = Ipv4Addr::new(10, 0, 2, 200);

        let frame = build_test_icmp_error_frame(
            [0xaa; 6], [0xbb; 6],
            router_ip, our_ip,
            ICMP_TYPE_TIME_EXCEEDED, ICMP_CODE_TTL_EXCEEDED, 0,
            our_ip, peer_ip, 5000, 8080,
        );

        let err = parse_icmp_error(&frame).expect("should parse");
        assert_eq!(err.icmp_type, ICMP_TYPE_TIME_EXCEEDED);
        assert_eq!(err.icmp_code, ICMP_CODE_TTL_EXCEEDED);
        assert_eq!(err.original_src_port, 5000);
        assert_eq!(err.original_dst_port, 8080);
    }

    #[test]
    fn test_parse_icmp_error_rejects_echo() {
        // Echo request should not be parsed as an error
        let frame = build_echo_request(
            [0xaa; 6], [0xbb; 6],
            Ipv4Addr::new(1, 2, 3, 4),
            Ipv4Addr::new(5, 6, 7, 8),
            1, 1, b"hello",
        );
        assert!(parse_icmp_error(&frame).is_none());
    }

    #[test]
    fn test_parse_icmp_error_rejects_non_udp() {
        // Build an ICMP error whose original datagram is TCP (proto 6), not UDP
        let router_ip = Ipv4Addr::new(10, 0, 1, 1);
        let our_ip = Ipv4Addr::new(10, 0, 1, 100);
        let peer_ip = Ipv4Addr::new(10, 0, 2, 200);

        let mut frame = build_test_icmp_error_frame(
            [0xaa; 6], [0xbb; 6],
            router_ip, our_ip,
            ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_PORT_UNREACHABLE, 0,
            our_ip, peer_ip, 12345, 9000,
        );

        // Overwrite the original IP protocol from UDP(17) to TCP(6)
        frame[42 + 9] = 6;
        assert!(parse_icmp_error(&frame).is_none());
    }

    #[test]
    fn test_parse_icmp_error_too_short() {
        // Frame shorter than minimum ICMP error size
        let short = [0u8; 50];
        assert!(parse_icmp_error(&short).is_none());
    }

    #[test]
    fn test_icmp_error_to_io_error_types() {
        let base = IcmpErrorInfo {
            icmp_type: ICMP_TYPE_DEST_UNREACHABLE,
            icmp_code: ICMP_CODE_PORT_UNREACHABLE,
            error_source: Ipv4Addr::new(10, 0, 1, 1),
            original_dst_ip: Ipv4Addr::new(10, 0, 2, 200),
            original_src_ip: Ipv4Addr::new(10, 0, 1, 100),
            original_dst_port: 9000,
            original_src_port: 12345,
            next_hop_mtu: 0,
        };

        // Port unreachable -> ConnectionRefused
        let err = base.to_io_error();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);

        // Host unreachable -> Other (EHOSTUNREACH)
        let mut info = base.clone();
        info.icmp_code = ICMP_CODE_HOST_UNREACHABLE;
        assert_eq!(info.to_io_error().kind(), io::ErrorKind::Other);

        // Admin prohibited -> PermissionDenied
        let mut info = base.clone();
        info.icmp_code = ICMP_CODE_ADMIN_PROHIBITED;
        assert_eq!(info.to_io_error().kind(), io::ErrorKind::PermissionDenied);

        // TTL exceeded -> TimedOut
        let mut info = base.clone();
        info.icmp_type = ICMP_TYPE_TIME_EXCEEDED;
        info.icmp_code = ICMP_CODE_TTL_EXCEEDED;
        assert_eq!(info.to_io_error().kind(), io::ErrorKind::TimedOut);

        // Parameter problem -> InvalidData
        let mut info = base.clone();
        info.icmp_type = ICMP_TYPE_PARAMETER_PROBLEM;
        info.icmp_code = 0;
        assert_eq!(info.to_io_error().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_handler_process_icmp_full_echo() {
        let local_mac = [0x11; 6];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = IcmpHandler::new(local_mac, local_ip);

        let frame = build_echo_request(
            [0xaa; 6], local_mac,
            Ipv4Addr::new(192, 168, 1, 100), local_ip,
            1, 1, b"ping",
        );

        match handler.process_icmp_full(&frame) {
            Some(IcmpAction::Reply(_)) => {} // expected
            other => panic!("expected Reply, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_handler_process_icmp_full_error() {
        let local_mac = [0x11; 6];
        let local_ip = Ipv4Addr::new(10, 0, 1, 100);
        let handler = IcmpHandler::new(local_mac, local_ip);

        let frame = build_test_icmp_error_frame(
            [0xaa; 6], local_mac,
            Ipv4Addr::new(10, 0, 1, 1), local_ip,
            ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_HOST_UNREACHABLE, 0,
            local_ip, Ipv4Addr::new(10, 0, 2, 200), 5000, 9000,
        );

        match handler.process_icmp_full(&frame) {
            Some(IcmpAction::Error(info)) => {
                assert_eq!(info.icmp_type, ICMP_TYPE_DEST_UNREACHABLE);
                assert_eq!(info.icmp_code, ICMP_CODE_HOST_UNREACHABLE);
                assert_eq!(info.original_src_port, 5000);
            }
            other => panic!("expected Error, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_handler_process_icmp_full_ignores_error_for_other_ip() {
        let local_mac = [0x11; 6];
        let local_ip = Ipv4Addr::new(10, 0, 1, 100);
        let other_ip = Ipv4Addr::new(10, 0, 1, 200);
        let handler = IcmpHandler::new(local_mac, local_ip);

        // Error about a datagram originating from other_ip, not us
        let frame = build_test_icmp_error_frame(
            [0xaa; 6], local_mac,
            Ipv4Addr::new(10, 0, 1, 1), local_ip,
            ICMP_TYPE_DEST_UNREACHABLE, ICMP_CODE_PORT_UNREACHABLE, 0,
            other_ip, Ipv4Addr::new(10, 0, 2, 200), 5000, 9000,
        );

        assert!(handler.process_icmp_full(&frame).is_none());
    }
}
