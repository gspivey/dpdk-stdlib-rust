//! ICMPv6 (Internet Control Message Protocol for IPv6) implementation
//!
//! Handles ICMPv6 echo request/reply (ping6) and ICMPv6 error processing.
//! Parallels the IPv4 ICMP implementation in `icmp.rs`.
//!
//! ICMPv6 error messages (Destination Unreachable, Packet Too Big, Time Exceeded,
//! Parameter Problem) carry as much of the invoking packet as possible without
//! exceeding the minimum IPv6 MTU. For UDP, the embedded original packet contains
//! the IPv6 header + UDP header, which lets us match errors back to the originating
//! socket and surface them via `take_error()`.
//!
//! ICMPv6 uses the IPv6 pseudo-header in its checksum calculation (RFC 4443 §2.3),
//! unlike IPv4 ICMP which checksums only the ICMP message itself.

use std::io;
use std::net::Ipv6Addr;

use crate::ipv6::{
    ETH_TYPE_IPV6, IPV6_HEADER_LEN, IP_PROTO_ICMPV6, walk_extension_headers,
};
use crate::{ETH_HEADER_LEN, UDP_HEADER_LEN};

// ============================================================================
// Constants
// ============================================================================

/// ICMPv6 type: Echo Request (RFC 4443 §4.1)
pub const ICMPV6_TYPE_ECHO_REQUEST: u8 = 128;

/// ICMPv6 type: Echo Reply (RFC 4443 §4.2)
pub const ICMPV6_TYPE_ECHO_REPLY: u8 = 129;

/// ICMPv6 code for echo messages (always 0)
pub const ICMPV6_CODE_ECHO: u8 = 0;

/// ICMPv6 header size: type(1) + code(1) + checksum(2) + message body(4)
pub const ICMPV6_HEADER_LEN: usize = 8;

/// Minimum ICMPv6 echo packet: Ethernet + IPv6 + ICMPv6 header
pub const MIN_ICMPV6_PACKET_LEN: usize = ETH_HEADER_LEN + IPV6_HEADER_LEN + ICMPV6_HEADER_LEN;

// ICMPv6 error types (RFC 4443 §3)

/// ICMPv6 type: Destination Unreachable (RFC 4443 §3.1)
pub const ICMPV6_TYPE_DEST_UNREACHABLE: u8 = 1;

/// ICMPv6 type: Packet Too Big (RFC 4443 §3.2)
pub const ICMPV6_TYPE_PACKET_TOO_BIG: u8 = 2;

/// ICMPv6 type: Time Exceeded (RFC 4443 §3.3)
pub const ICMPV6_TYPE_TIME_EXCEEDED: u8 = 3;

/// ICMPv6 type: Parameter Problem (RFC 4443 §3.4)
pub const ICMPV6_TYPE_PARAMETER_PROBLEM: u8 = 4;

// Destination Unreachable codes (RFC 4443 §3.1)

/// No route to destination
pub const ICMPV6_CODE_NO_ROUTE: u8 = 0;
/// Communication with destination administratively prohibited
pub const ICMPV6_CODE_ADMIN_PROHIBITED: u8 = 1;
/// Beyond scope of source address
pub const ICMPV6_CODE_BEYOND_SCOPE: u8 = 2;
/// Address unreachable
pub const ICMPV6_CODE_ADDR_UNREACHABLE: u8 = 3;
/// Port unreachable
pub const ICMPV6_CODE_PORT_UNREACHABLE: u8 = 4;
/// Source address failed ingress/egress policy (RFC 5095)
pub const ICMPV6_CODE_POLICY_FAIL: u8 = 5;
/// Reject route to destination (RFC 6550)
pub const ICMPV6_CODE_REJECT_ROUTE: u8 = 6;

// Time Exceeded codes (RFC 4443 §3.3)

/// Hop limit exceeded in transit
pub const ICMPV6_CODE_HOP_LIMIT_EXCEEDED: u8 = 0;
/// Fragment reassembly time exceeded
pub const ICMPV6_CODE_FRAG_REASSEMBLY_EXCEEDED: u8 = 1;

// Parameter Problem codes (RFC 4443 §3.4)

/// Erroneous header field encountered
pub const ICMPV6_CODE_ERRONEOUS_HEADER: u8 = 0;
/// Unrecognized Next Header type encountered
pub const ICMPV6_CODE_UNRECOGNIZED_NEXT_HEADER: u8 = 1;
/// Unrecognized IPv6 option encountered
pub const ICMPV6_CODE_UNRECOGNIZED_OPTION: u8 = 2;

/// IP protocol number for UDP (used when parsing original datagram in ICMPv6 errors)
const IP_PROTO_UDP: u8 = 17;

/// Minimum ICMPv6 error payload: original IPv6 header + UDP ports (first 8 bytes)
pub const MIN_ICMPV6_ERROR_PAYLOAD: usize = IPV6_HEADER_LEN + UDP_HEADER_LEN;

// ============================================================================
// ICMPv6 Packet Structure
// ============================================================================

/// Parsed ICMPv6 echo packet.
#[derive(Debug, Clone)]
pub struct Icmpv6Packet {
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
    pub src_ip: Ipv6Addr,
    pub dst_ip: Ipv6Addr,
    pub hop_limit: u8,
    pub icmp_type: u8,
    pub icmp_code: u8,
    pub checksum: u16,
    pub identifier: u16,
    pub sequence: u16,
    pub payload: Vec<u8>,
}

impl Icmpv6Packet {
    /// Check if this is an echo request (ping6).
    pub fn is_echo_request(&self) -> bool {
        self.icmp_type == ICMPV6_TYPE_ECHO_REQUEST && self.icmp_code == ICMPV6_CODE_ECHO
    }

    /// Check if this is an echo reply.
    pub fn is_echo_reply(&self) -> bool {
        self.icmp_type == ICMPV6_TYPE_ECHO_REPLY && self.icmp_code == ICMPV6_CODE_ECHO
    }

    /// Create an echo reply for this echo request.
    pub fn make_echo_reply(&self, reply_src_mac: [u8; 6]) -> Option<Icmpv6Packet> {
        if !self.is_echo_request() {
            return None;
        }
        Some(Icmpv6Packet {
            src_mac: reply_src_mac,
            dst_mac: self.src_mac,
            src_ip: self.dst_ip,
            dst_ip: self.src_ip,
            hop_limit: 64,
            icmp_type: ICMPV6_TYPE_ECHO_REPLY,
            icmp_code: ICMPV6_CODE_ECHO,
            checksum: 0,
            identifier: self.identifier,
            sequence: self.sequence,
            payload: self.payload.clone(),
        })
    }
}

// ============================================================================
// Checksum
// ============================================================================

/// Compute ICMPv6 checksum using the IPv6 pseudo-header (RFC 4443 §2.3).
///
/// The pseudo-header includes: src addr (16B), dst addr (16B), upper-layer
/// packet length (4B), zeros (3B), next header (1B = 58).
pub fn icmpv6_checksum(src_ip: &Ipv6Addr, dst_ip: &Ipv6Addr, icmp_data: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    // IPv6 pseudo-header
    for chunk in src_ip.octets().chunks(2) {
        sum = sum.wrapping_add(((chunk[0] as u32) << 8) | (chunk[1] as u32));
    }
    for chunk in dst_ip.octets().chunks(2) {
        sum = sum.wrapping_add(((chunk[0] as u32) << 8) | (chunk[1] as u32));
    }

    // Upper-layer packet length (4 bytes, big-endian u32)
    let icmp_len = icmp_data.len() as u32;
    sum = sum.wrapping_add(icmp_len >> 16);
    sum = sum.wrapping_add(icmp_len & 0xFFFF);

    // Next Header = ICMPv6 (58)
    sum = sum.wrapping_add(IP_PROTO_ICMPV6 as u32);

    // ICMPv6 message (skip checksum field at bytes 2-3)
    for i in (0..icmp_data.len()).step_by(2) {
        if i == 2 {
            continue; // skip checksum field
        }
        let word = if i + 1 < icmp_data.len() {
            ((icmp_data[i] as u32) << 8) | (icmp_data[i + 1] as u32)
        } else {
            (icmp_data[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    // Fold to 16 bits
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    let result = !(sum as u16);
    if result == 0 { 0xFFFF } else { result }
}

// ============================================================================
// Parsing
// ============================================================================

/// Parse an ICMPv6 echo packet from a raw Ethernet frame.
///
/// Returns `None` if the frame is not a valid ICMPv6 echo request or reply.
/// Handles extension headers via `walk_extension_headers`.
pub fn parse_icmpv6_packet(frame: &[u8]) -> Option<Icmpv6Packet> {
    let layout = crate::detect_vlan(frame, None)?;
    let l3 = layout.l3_offset;

    if layout.ethertype != ETH_TYPE_IPV6 {
        return None;
    }
    if frame.len() < l3 + IPV6_HEADER_LEN {
        return None;
    }
    if (frame[l3] >> 4) != 6 {
        return None;
    }

    let nh = walk_extension_headers(&frame[l3..])?;
    if nh.protocol != IP_PROTO_ICMPV6 {
        return None;
    }

    let icmp_start = l3 + nh.payload_offset;
    if frame.len() < icmp_start + ICMPV6_HEADER_LEN {
        return None;
    }

    let icmp_type = frame[icmp_start];
    let icmp_code = frame[icmp_start + 1];

    // Only handle echo request/reply
    if icmp_type != ICMPV6_TYPE_ECHO_REQUEST && icmp_type != ICMPV6_TYPE_ECHO_REPLY {
        return None;
    }

    let dst_mac: [u8; 6] = frame[0..6].try_into().ok()?;
    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 8..l3 + 24]).unwrap());
    let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 24..l3 + 40]).unwrap());
    let hop_limit = frame[l3 + 7];

    let checksum = u16::from_be_bytes([frame[icmp_start + 2], frame[icmp_start + 3]]);
    let identifier = u16::from_be_bytes([frame[icmp_start + 4], frame[icmp_start + 5]]);
    let sequence = u16::from_be_bytes([frame[icmp_start + 6], frame[icmp_start + 7]]);

    let payload = if frame.len() > icmp_start + ICMPV6_HEADER_LEN {
        frame[icmp_start + ICMPV6_HEADER_LEN..].to_vec()
    } else {
        Vec::new()
    };

    Some(Icmpv6Packet {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        hop_limit,
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

/// Build an ICMPv6 echo packet into a raw Ethernet frame.
pub fn build_icmpv6_frame(packet: &Icmpv6Packet) -> Vec<u8> {
    let icmp_len = ICMPV6_HEADER_LEN + packet.payload.len();
    let total_len = ETH_HEADER_LEN + IPV6_HEADER_LEN + icmp_len;
    let mut frame = vec![0u8; total_len];

    // Ethernet header
    frame[0..6].copy_from_slice(&packet.dst_mac);
    frame[6..12].copy_from_slice(&packet.src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

    // IPv6 header
    let ip = ETH_HEADER_LEN;
    frame[ip] = 0x60; // version 6
    frame[ip + 1] = 0x00;
    frame[ip + 2] = 0x00;
    frame[ip + 3] = 0x00;
    frame[ip + 4..ip + 6].copy_from_slice(&(icmp_len as u16).to_be_bytes());
    frame[ip + 6] = IP_PROTO_ICMPV6;
    frame[ip + 7] = packet.hop_limit;
    frame[ip + 8..ip + 24].copy_from_slice(&packet.src_ip.octets());
    frame[ip + 24..ip + 40].copy_from_slice(&packet.dst_ip.octets());

    // ICMPv6 header
    let icmp = ETH_HEADER_LEN + IPV6_HEADER_LEN;
    frame[icmp] = packet.icmp_type;
    frame[icmp + 1] = packet.icmp_code;
    // checksum placeholder at [icmp+2..icmp+4]
    frame[icmp + 4..icmp + 6].copy_from_slice(&packet.identifier.to_be_bytes());
    frame[icmp + 6..icmp + 8].copy_from_slice(&packet.sequence.to_be_bytes());

    // Payload
    if !packet.payload.is_empty() {
        frame[icmp + ICMPV6_HEADER_LEN..].copy_from_slice(&packet.payload);
    }

    // Compute checksum over the ICMPv6 message with pseudo-header
    let cksum = icmpv6_checksum(&packet.src_ip, &packet.dst_ip, &frame[icmp..]);
    frame[icmp + 2..icmp + 4].copy_from_slice(&cksum.to_be_bytes());

    frame
}

/// Build an ICMPv6 echo request frame.
pub fn build_echo6_request(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv6Addr,
    dst_ip: Ipv6Addr,
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> Vec<u8> {
    build_icmpv6_frame(&Icmpv6Packet {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        hop_limit: 64,
        icmp_type: ICMPV6_TYPE_ECHO_REQUEST,
        icmp_code: ICMPV6_CODE_ECHO,
        checksum: 0,
        identifier,
        sequence,
        payload: payload.to_vec(),
    })
}

// ============================================================================
// ICMPv6 Error Handling
// ============================================================================

/// Parsed ICMPv6 error with context from the original datagram.
///
/// ICMPv6 error messages (types 1-4) carry as much of the invoking packet as
/// possible. For UDP, the embedded original packet contains the IPv6 header +
/// UDP header (src/dst ports), which lets us match errors to the originating socket.
#[derive(Debug, Clone)]
pub struct Icmpv6ErrorInfo {
    /// ICMPv6 error type (1=Dest Unreachable, 2=Packet Too Big, 3=Time Exceeded, 4=Parameter Problem)
    pub icmp_type: u8,
    /// ICMPv6 error code (sub-type within the error category)
    pub icmp_code: u8,
    /// IPv6 address of the router/host that generated the error
    pub error_source: Ipv6Addr,
    /// Original destination IPv6 from the packet that triggered the error
    pub original_dst_ip: Ipv6Addr,
    /// Original source IPv6 from the packet that triggered the error
    pub original_src_ip: Ipv6Addr,
    /// Original destination port (from the UDP header in the ICMPv6 payload)
    pub original_dst_port: u16,
    /// Original source port (from the UDP header in the ICMPv6 payload)
    pub original_src_port: u16,
    /// MTU value (only valid for Packet Too Big, type 2)
    pub mtu: u32,
}

impl Icmpv6ErrorInfo {
    /// Convert this ICMPv6 error into an `io::Error` matching Linux kernel behavior.
    ///
    /// Linux maps ICMPv6 errors to errno values surfaced via `SO_ERROR` /
    /// `take_error()`. We replicate that mapping here.
    pub fn to_io_error(&self) -> io::Error {
        match (self.icmp_type, self.icmp_code) {
            (ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_NO_ROUTE) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMPv6: no route to destination (from {})", self.error_source),
            ),
            (ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_ADMIN_PROHIBITED) => io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("ICMPv6: administratively prohibited (from {})", self.error_source),
            ),
            (ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_BEYOND_SCOPE) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMPv6: beyond scope of source address (from {})", self.error_source),
            ),
            (ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_ADDR_UNREACHABLE) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMPv6: address unreachable (from {})", self.error_source),
            ),
            (ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_PORT_UNREACHABLE) => io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("ICMPv6: port unreachable (from {})", self.error_source),
            ),
            (ICMPV6_TYPE_DEST_UNREACHABLE, code) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMPv6: destination unreachable code {} (from {})", code, self.error_source),
            ),
            (ICMPV6_TYPE_PACKET_TOO_BIG, _) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMPv6: packet too big, MTU {} (from {})", self.mtu, self.error_source),
            ),
            (ICMPV6_TYPE_TIME_EXCEEDED, ICMPV6_CODE_HOP_LIMIT_EXCEEDED) => io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ICMPv6: hop limit exceeded in transit (from {})", self.error_source),
            ),
            (ICMPV6_TYPE_TIME_EXCEEDED, ICMPV6_CODE_FRAG_REASSEMBLY_EXCEEDED) => io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ICMPv6: fragment reassembly time exceeded (from {})", self.error_source),
            ),
            (ICMPV6_TYPE_PARAMETER_PROBLEM, _) => io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ICMPv6: parameter problem (from {})", self.error_source),
            ),
            (typ, code) => io::Error::new(
                io::ErrorKind::Other,
                format!("ICMPv6 error type {} code {} (from {})", typ, code, self.error_source),
            ),
        }
    }
}

/// Parse an ICMPv6 error message and extract the original datagram context.
///
/// ICMPv6 error messages have this structure:
/// ```text
/// [Ethernet 14B][Outer IPv6 40B][ICMPv6 Header 8B][Original IPv6 Header 40B][Original Transport 8B+]
/// ```
///
/// We extract the original IPv6 src/dst and the original UDP src/dst ports from
/// the embedded datagram, so the socket layer can match the error to the right socket.
///
/// Returns `None` if the frame is not a valid ICMPv6 error for a UDP datagram.
pub fn parse_icmpv6_error(frame: &[u8]) -> Option<Icmpv6ErrorInfo> {
    let layout = crate::detect_vlan(frame, None)?;
    let l3 = layout.l3_offset;

    if layout.ethertype != ETH_TYPE_IPV6 {
        return None;
    }
    if frame.len() < l3 + IPV6_HEADER_LEN {
        return None;
    }
    if (frame[l3] >> 4) != 6 {
        return None;
    }

    // Walk extension headers to find ICMPv6
    let nh = walk_extension_headers(&frame[l3..])?;
    if nh.protocol != IP_PROTO_ICMPV6 {
        return None;
    }

    let icmp_start = l3 + nh.payload_offset;
    if frame.len() < icmp_start + ICMPV6_HEADER_LEN {
        return None;
    }

    let icmp_type = frame[icmp_start];
    let icmp_code = frame[icmp_start + 1];

    // Only process error types (1-4)
    if !matches!(
        icmp_type,
        ICMPV6_TYPE_DEST_UNREACHABLE
            | ICMPV6_TYPE_PACKET_TOO_BIG
            | ICMPV6_TYPE_TIME_EXCEEDED
            | ICMPV6_TYPE_PARAMETER_PROBLEM
    ) {
        return None;
    }

    // For Packet Too Big (type 2), bytes 4-7 of the ICMPv6 header contain the MTU
    let mtu = if icmp_type == ICMPV6_TYPE_PACKET_TOO_BIG {
        u32::from_be_bytes([
            frame[icmp_start + 4],
            frame[icmp_start + 5],
            frame[icmp_start + 6],
            frame[icmp_start + 7],
        ])
    } else {
        0
    };

    // The original packet starts after the ICMPv6 header
    let orig_pkt_start = icmp_start + ICMPV6_HEADER_LEN;
    if frame.len() < orig_pkt_start + IPV6_HEADER_LEN + UDP_HEADER_LEN {
        return None;
    }

    // Parse the original IPv6 header
    let orig_ipv6 = &frame[orig_pkt_start..];
    if (orig_ipv6[0] >> 4) != 6 {
        return None;
    }

    let original_src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&orig_ipv6[8..24]).unwrap());
    let original_dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&orig_ipv6[24..40]).unwrap());

    // Walk extension headers in the original packet to find UDP
    let remaining = &frame[orig_pkt_start..];
    let orig_nh = walk_extension_headers(remaining)?;
    if orig_nh.protocol != IP_PROTO_UDP {
        return None;
    }

    let orig_udp_start = orig_pkt_start + orig_nh.payload_offset;
    if frame.len() < orig_udp_start + 4 {
        return None;
    }

    let original_src_port = u16::from_be_bytes([frame[orig_udp_start], frame[orig_udp_start + 1]]);
    let original_dst_port = u16::from_be_bytes([frame[orig_udp_start + 2], frame[orig_udp_start + 3]]);

    // Extract error source from the outer IPv6 header
    let error_source = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 8..l3 + 24]).unwrap());

    Some(Icmpv6ErrorInfo {
        icmp_type,
        icmp_code,
        error_source,
        original_dst_ip,
        original_src_ip,
        original_dst_port,
        original_src_port,
        mtu,
    })
}

/// Result of processing an ICMPv6 packet: either a reply frame to send, or
/// an error to queue on the matching socket.
pub enum Icmpv6Action {
    /// An echo reply frame that should be transmitted back.
    Reply(Vec<u8>),
    /// An ICMPv6 error that should be queued on the originating socket.
    Error(Icmpv6ErrorInfo),
}

// ============================================================================
// ICMPv6 Handler
// ============================================================================

/// Handles ICMPv6 echo request/reply and error processing for IPv6 sockets.
///
/// Parallels `IcmpHandler` for IPv4.
pub struct Icmpv6Handler {
    pub local_mac: [u8; 6],
    pub local_ips: Vec<Ipv6Addr>,
}

impl Icmpv6Handler {
    /// Create a new ICMPv6 handler.
    pub fn new(local_mac: [u8; 6], local_ip: Ipv6Addr) -> Self {
        Self {
            local_mac,
            local_ips: vec![local_ip],
        }
    }

    /// Add a local IPv6 address.
    pub fn add_local_ip(&mut self, ip: Ipv6Addr) {
        if !self.local_ips.contains(&ip) {
            self.local_ips.push(ip);
        }
    }

    /// Process an incoming ICMPv6 packet (legacy API — echo only).
    ///
    /// Returns an echo reply frame if this was an echo request for one of our
    /// IPv6 addresses, or `None` if no response is needed.
    pub fn process_icmpv6(&self, frame: &[u8]) -> Option<Vec<u8>> {
        let packet = parse_icmpv6_packet(frame)?;
        if packet.is_echo_request() && self.local_ips.contains(&packet.dst_ip) {
            let reply = packet.make_echo_reply(self.local_mac)?;
            return Some(build_icmpv6_frame(&reply));
        }
        None
    }

    /// Process an incoming ICMPv6 packet, handling both echo requests and error messages.
    ///
    /// Returns `Some(Icmpv6Action::Reply(frame))` for echo requests addressed to us,
    /// or `Some(Icmpv6Action::Error(info))` for ICMPv6 errors that reference a UDP
    /// datagram originating from one of our local IPs.
    pub fn process_icmpv6_full(&self, frame: &[u8]) -> Option<Icmpv6Action> {
        // Try echo request first (most common in-bound ICMPv6)
        if let Some(packet) = parse_icmpv6_packet(frame) {
            if packet.is_echo_request() && self.local_ips.contains(&packet.dst_ip) {
                let reply = packet.make_echo_reply(self.local_mac)?;
                return Some(Icmpv6Action::Reply(build_icmpv6_frame(&reply)));
            }
        }

        // Try ICMPv6 error (types 1-4 with embedded original datagram)
        if let Some(error_info) = parse_icmpv6_error(frame) {
            // Only accept errors about datagrams that originated from us
            if self.local_ips.contains(&error_info.original_src_ip) {
                return Some(Icmpv6Action::Error(error_info));
            }
        }

        None
    }
}

/// Build an ICMPv6 error frame for testing purposes.
///
/// Constructs a valid ICMPv6 error message containing the original IPv6+UDP
/// headers as the error payload.
#[cfg(test)]
pub fn build_icmpv6_error_frame(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    error_source: Ipv6Addr,
    error_dst: Ipv6Addr,
    icmp_type: u8,
    icmp_code: u8,
    mtu: u32,
    original_src_ip: Ipv6Addr,
    original_dst_ip: Ipv6Addr,
    original_src_port: u16,
    original_dst_port: u16,
) -> Vec<u8> {
    // Original packet: IPv6 header (40B) + UDP header (8B)
    let orig_pkt_len = IPV6_HEADER_LEN + UDP_HEADER_LEN;
    let icmp_len = ICMPV6_HEADER_LEN + orig_pkt_len;
    let total_len = ETH_HEADER_LEN + IPV6_HEADER_LEN + icmp_len;
    let mut frame = vec![0u8; total_len];

    // Ethernet header
    frame[0..6].copy_from_slice(&dst_mac);
    frame[6..12].copy_from_slice(&src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

    // Outer IPv6 header
    let ip = ETH_HEADER_LEN;
    frame[ip] = 0x60; // version 6
    frame[ip + 4..ip + 6].copy_from_slice(&(icmp_len as u16).to_be_bytes());
    frame[ip + 6] = IP_PROTO_ICMPV6;
    frame[ip + 7] = 64; // hop limit
    frame[ip + 8..ip + 24].copy_from_slice(&error_source.octets());
    frame[ip + 24..ip + 40].copy_from_slice(&error_dst.octets());

    // ICMPv6 header
    let icmp = ETH_HEADER_LEN + IPV6_HEADER_LEN;
    frame[icmp] = icmp_type;
    frame[icmp + 1] = icmp_code;
    // Bytes 4-7: MTU for Packet Too Big, unused (zero) for others
    frame[icmp + 4..icmp + 8].copy_from_slice(&mtu.to_be_bytes());

    // Original IPv6 header (embedded in ICMPv6 payload)
    let orig = icmp + ICMPV6_HEADER_LEN;
    frame[orig] = 0x60; // version 6
    // Payload length = UDP header (8 bytes)
    frame[orig + 4..orig + 6].copy_from_slice(&(UDP_HEADER_LEN as u16).to_be_bytes());
    frame[orig + 6] = IP_PROTO_UDP; // Next Header = UDP
    frame[orig + 7] = 64; // hop limit
    frame[orig + 8..orig + 24].copy_from_slice(&original_src_ip.octets());
    frame[orig + 24..orig + 40].copy_from_slice(&original_dst_ip.octets());

    // Original UDP header (first 8 bytes)
    let udp = orig + IPV6_HEADER_LEN;
    frame[udp..udp + 2].copy_from_slice(&original_src_port.to_be_bytes());
    frame[udp + 2..udp + 4].copy_from_slice(&original_dst_port.to_be_bytes());
    frame[udp + 4..udp + 6].copy_from_slice(&(UDP_HEADER_LEN as u16).to_be_bytes());

    // Compute ICMPv6 checksum
    let cksum = icmpv6_checksum(&error_source, &error_dst, &frame[icmp..]);
    frame[icmp + 2..icmp + 4].copy_from_slice(&cksum.to_be_bytes());

    frame
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SRC_MAC: [u8; 6] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
    const DST_MAC: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];

    fn src_ip() -> Ipv6Addr {
        "2001:db8::1".parse().unwrap()
    }
    fn dst_ip() -> Ipv6Addr {
        "2001:db8::2".parse().unwrap()
    }

    // --- Constants ---

    #[test]
    fn constants_are_correct() {
        assert_eq!(ICMPV6_TYPE_ECHO_REQUEST, 128);
        assert_eq!(ICMPV6_TYPE_ECHO_REPLY, 129);
        assert_eq!(ICMPV6_CODE_ECHO, 0);
        assert_eq!(ICMPV6_HEADER_LEN, 8);
        assert_eq!(MIN_ICMPV6_PACKET_LEN, 14 + 40 + 8); // 62
    }

    // --- Checksum ---

    #[test]
    fn checksum_is_nonzero() {
        let data = [
            ICMPV6_TYPE_ECHO_REQUEST, 0x00, // type, code
            0x00, 0x00, // checksum placeholder
            0x12, 0x34, // identifier
            0x00, 0x01, // sequence
        ];
        let cksum = icmpv6_checksum(&src_ip(), &dst_ip(), &data);
        assert_ne!(cksum, 0);
    }

    #[test]
    fn checksum_verifies_correctly() {
        // Build a frame and verify the stored checksum matches recomputation
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, b"ping");
        let icmp_start = ETH_HEADER_LEN + IPV6_HEADER_LEN;
        let icmp_data = &frame[icmp_start..];

        // Recompute including the stored checksum field (should verify to 0xFFFF or match)
        let stored = u16::from_be_bytes([icmp_data[2], icmp_data[3]]);
        let recomputed = icmpv6_checksum(&src_ip(), &dst_ip(), icmp_data);
        // When checksum field is included in computation, result should equal stored
        // Actually, our function skips the checksum field, so recomputed == stored
        assert_eq!(recomputed, stored);
    }

    #[test]
    fn checksum_detects_corruption() {
        let mut frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, b"data");
        let icmp_start = ETH_HEADER_LEN + IPV6_HEADER_LEN;
        let stored = u16::from_be_bytes([frame[icmp_start + 2], frame[icmp_start + 3]]);

        // Corrupt payload
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;

        let recomputed = icmpv6_checksum(&src_ip(), &dst_ip(), &frame[icmp_start..]);
        assert_ne!(recomputed, stored);
    }

    // --- Build + Parse roundtrip ---

    #[test]
    fn build_and_parse_echo_request() {
        let payload = b"hello ping6";
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 0xABCD, 42, payload);

        let parsed = parse_icmpv6_packet(&frame).unwrap();
        assert!(parsed.is_echo_request());
        assert!(!parsed.is_echo_reply());
        assert_eq!(parsed.src_mac, SRC_MAC);
        assert_eq!(parsed.dst_mac, DST_MAC);
        assert_eq!(parsed.src_ip, src_ip());
        assert_eq!(parsed.dst_ip, dst_ip());
        assert_eq!(parsed.identifier, 0xABCD);
        assert_eq!(parsed.sequence, 42);
        assert_eq!(parsed.payload, payload);
        assert_eq!(parsed.hop_limit, 64);
    }

    #[test]
    fn build_and_parse_echo_reply() {
        let packet = Icmpv6Packet {
            src_mac: SRC_MAC,
            dst_mac: DST_MAC,
            src_ip: src_ip(),
            dst_ip: dst_ip(),
            hop_limit: 64,
            icmp_type: ICMPV6_TYPE_ECHO_REPLY,
            icmp_code: ICMPV6_CODE_ECHO,
            checksum: 0,
            identifier: 0x1234,
            sequence: 7,
            payload: b"pong".to_vec(),
        };
        let frame = build_icmpv6_frame(&packet);
        let parsed = parse_icmpv6_packet(&frame).unwrap();
        assert!(parsed.is_echo_reply());
        assert!(!parsed.is_echo_request());
        assert_eq!(parsed.identifier, 0x1234);
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.payload, b"pong");
    }

    #[test]
    fn empty_payload_roundtrip() {
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, b"");
        assert_eq!(frame.len(), MIN_ICMPV6_PACKET_LEN);
        let parsed = parse_icmpv6_packet(&frame).unwrap();
        assert!(parsed.payload.is_empty());
    }

    // --- make_echo_reply ---

    #[test]
    fn make_echo_reply_swaps_addresses() {
        let request = Icmpv6Packet {
            src_mac: SRC_MAC,
            dst_mac: DST_MAC,
            src_ip: src_ip(),
            dst_ip: dst_ip(),
            hop_limit: 128,
            icmp_type: ICMPV6_TYPE_ECHO_REQUEST,
            icmp_code: ICMPV6_CODE_ECHO,
            checksum: 0,
            identifier: 0x5678,
            sequence: 99,
            payload: b"test data".to_vec(),
        };

        let reply_mac = [0x33; 6];
        let reply = request.make_echo_reply(reply_mac).unwrap();

        assert!(reply.is_echo_reply());
        assert_eq!(reply.src_mac, reply_mac);
        assert_eq!(reply.dst_mac, SRC_MAC);
        assert_eq!(reply.src_ip, dst_ip()); // our IP
        assert_eq!(reply.dst_ip, src_ip()); // requester's IP
        assert_eq!(reply.identifier, 0x5678);
        assert_eq!(reply.sequence, 99);
        assert_eq!(reply.payload, b"test data");
        assert_eq!(reply.hop_limit, 64);
    }

    #[test]
    fn make_echo_reply_from_reply_returns_none() {
        let reply_packet = Icmpv6Packet {
            src_mac: SRC_MAC,
            dst_mac: DST_MAC,
            src_ip: src_ip(),
            dst_ip: dst_ip(),
            hop_limit: 64,
            icmp_type: ICMPV6_TYPE_ECHO_REPLY,
            icmp_code: ICMPV6_CODE_ECHO,
            checksum: 0,
            identifier: 0,
            sequence: 0,
            payload: vec![],
        };
        assert!(reply_packet.make_echo_reply([0; 6]).is_none());
    }

    // --- Parse edge cases ---

    #[test]
    fn parse_rejects_ipv4_frame() {
        let frame = crate::build_udp_frame(
            &SRC_MAC, &DST_MAC,
            "10.0.0.1".parse().unwrap(),
            "10.0.0.2".parse().unwrap(),
            1000, 2000, b"ipv4", 64,
        ).unwrap();
        assert!(parse_icmpv6_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_truncated_frame() {
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, b"x");
        // Truncate before ICMPv6 header completes
        assert!(parse_icmpv6_packet(&frame[..MIN_ICMPV6_PACKET_LEN - 1]).is_none());
    }

    #[test]
    fn parse_rejects_udp_over_ipv6() {
        // Build a UDP/IPv6 frame — should not parse as ICMPv6
        let frame = crate::ipv6::build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1000, 2000, b"udp", 64,
        ).unwrap();
        assert!(parse_icmpv6_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_too_short() {
        assert!(parse_icmpv6_packet(&[0u8; 10]).is_none());
    }

    // --- Handler ---

    #[test]
    fn handler_responds_to_echo_request() {
        let local_mac = DST_MAC;
        let local_ip = dst_ip();
        let handler = Icmpv6Handler::new(local_mac, local_ip);

        let request = build_echo6_request(SRC_MAC, local_mac, src_ip(), local_ip, 0x1234, 1, b"ping6");
        let reply_frame = handler.process_icmpv6(&request).unwrap();

        let reply = parse_icmpv6_packet(&reply_frame).unwrap();
        assert!(reply.is_echo_reply());
        assert_eq!(reply.src_ip, local_ip);
        assert_eq!(reply.dst_ip, src_ip());
        assert_eq!(reply.identifier, 0x1234);
        assert_eq!(reply.sequence, 1);
        assert_eq!(reply.payload, b"ping6");
    }

    #[test]
    fn handler_ignores_other_ips() {
        let local_mac = DST_MAC;
        let local_ip = dst_ip();
        let handler = Icmpv6Handler::new(local_mac, local_ip);

        let other_ip: Ipv6Addr = "2001:db8::99".parse().unwrap();
        let request = build_echo6_request(SRC_MAC, local_mac, src_ip(), other_ip, 1, 1, b"");
        assert!(handler.process_icmpv6(&request).is_none());
    }

    #[test]
    fn handler_multiple_ips() {
        let local_mac = DST_MAC;
        let ip1 = dst_ip();
        let ip2: Ipv6Addr = "fe80::1".parse().unwrap();

        let mut handler = Icmpv6Handler::new(local_mac, ip1);
        handler.add_local_ip(ip2);

        let req1 = build_echo6_request(SRC_MAC, local_mac, src_ip(), ip1, 1, 1, b"");
        assert!(handler.process_icmpv6(&req1).is_some());

        let req2 = build_echo6_request(SRC_MAC, local_mac, src_ip(), ip2, 2, 1, b"");
        assert!(handler.process_icmpv6(&req2).is_some());
    }

    #[test]
    fn handler_ignores_echo_reply() {
        let local_mac = DST_MAC;
        let local_ip = dst_ip();
        let handler = Icmpv6Handler::new(local_mac, local_ip);

        // Build an echo reply (not a request) — handler should ignore it
        let packet = Icmpv6Packet {
            src_mac: SRC_MAC,
            dst_mac: local_mac,
            src_ip: src_ip(),
            dst_ip: local_ip,
            hop_limit: 64,
            icmp_type: ICMPV6_TYPE_ECHO_REPLY,
            icmp_code: ICMPV6_CODE_ECHO,
            checksum: 0,
            identifier: 1,
            sequence: 1,
            payload: vec![],
        };
        let frame = build_icmpv6_frame(&packet);
        assert!(handler.process_icmpv6(&frame).is_none());
    }

    // --- Wire format ---

    #[test]
    fn wire_format_ethertype_is_ipv6() {
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, b"");
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        assert_eq!(ethertype, ETH_TYPE_IPV6);
    }

    #[test]
    fn wire_format_next_header_is_icmpv6() {
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, b"");
        assert_eq!(frame[ETH_HEADER_LEN + 6], IP_PROTO_ICMPV6);
    }

    #[test]
    fn wire_format_ipv6_payload_length() {
        let payload = b"twelve bytes";
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, payload);
        let ip = ETH_HEADER_LEN;
        let payload_len = u16::from_be_bytes([frame[ip + 4], frame[ip + 5]]);
        assert_eq!(payload_len as usize, ICMPV6_HEADER_LEN + payload.len());
    }

    // --- Link-local and special addresses ---

    #[test]
    fn link_local_echo() {
        let ll_src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ll_dst: Ipv6Addr = "fe80::2".parse().unwrap();
        let frame = build_echo6_request(SRC_MAC, DST_MAC, ll_src, ll_dst, 1, 1, b"ll");
        let parsed = parse_icmpv6_packet(&frame).unwrap();
        assert_eq!(parsed.src_ip, ll_src);
        assert_eq!(parsed.dst_ip, ll_dst);
    }

    #[test]
    fn loopback_echo() {
        let lo = Ipv6Addr::LOCALHOST;
        let frame = build_echo6_request(SRC_MAC, DST_MAC, lo, lo, 1, 1, b"lo");
        let parsed = parse_icmpv6_packet(&frame).unwrap();
        assert_eq!(parsed.src_ip, lo);
        assert_eq!(parsed.dst_ip, lo);
    }

    // --- Synthetic performance ---

    #[test]
    fn perf_build_parse_cycle() {
        let payload = vec![0xBB; 64];
        let iterations = 10_000;

        let start = std::time::Instant::now();
        for i in 0..iterations {
            let frame = build_echo6_request(
                SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, i as u16, &payload,
            );
            let _ = parse_icmpv6_packet(&frame).unwrap();
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!(
            "[PERF] ICMPv6 build+parse: {} iterations in {:?} ({} ns/op)",
            iterations, elapsed, ns_per_op
        );
        assert!(ns_per_op < 10_000, "build+parse too slow: {} ns/op", ns_per_op);
    }

    // =========================================================================
    // ICMPv6 Error Handling Tests
    // =========================================================================

    fn router_ip() -> Ipv6Addr {
        "2001:db8::ffff".parse().unwrap()
    }

    // --- Constants ---

    #[test]
    fn error_constants_are_correct() {
        assert_eq!(ICMPV6_TYPE_DEST_UNREACHABLE, 1);
        assert_eq!(ICMPV6_TYPE_PACKET_TOO_BIG, 2);
        assert_eq!(ICMPV6_TYPE_TIME_EXCEEDED, 3);
        assert_eq!(ICMPV6_TYPE_PARAMETER_PROBLEM, 4);
        assert_eq!(ICMPV6_CODE_NO_ROUTE, 0);
        assert_eq!(ICMPV6_CODE_ADMIN_PROHIBITED, 1);
        assert_eq!(ICMPV6_CODE_BEYOND_SCOPE, 2);
        assert_eq!(ICMPV6_CODE_ADDR_UNREACHABLE, 3);
        assert_eq!(ICMPV6_CODE_PORT_UNREACHABLE, 4);
        assert_eq!(ICMPV6_CODE_HOP_LIMIT_EXCEEDED, 0);
        assert_eq!(ICMPV6_CODE_FRAG_REASSEMBLY_EXCEEDED, 1);
        assert_eq!(ICMPV6_CODE_ERRONEOUS_HEADER, 0);
        assert_eq!(ICMPV6_CODE_UNRECOGNIZED_NEXT_HEADER, 1);
        assert_eq!(ICMPV6_CODE_UNRECOGNIZED_OPTION, 2);
        assert_eq!(MIN_ICMPV6_ERROR_PAYLOAD, 40 + 8); // IPv6 + UDP
    }

    // --- parse_icmpv6_error ---

    #[test]
    fn parse_dest_unreachable_port() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_PORT_UNREACHABLE,
            0,
            src_ip(), "2001:db8::99".parse().unwrap(),
            12345, 80,
        );
        let info = parse_icmpv6_error(&frame).unwrap();
        assert_eq!(info.icmp_type, ICMPV6_TYPE_DEST_UNREACHABLE);
        assert_eq!(info.icmp_code, ICMPV6_CODE_PORT_UNREACHABLE);
        assert_eq!(info.error_source, router_ip());
        assert_eq!(info.original_src_ip, src_ip());
        assert_eq!(info.original_dst_ip, "2001:db8::99".parse::<Ipv6Addr>().unwrap());
        assert_eq!(info.original_src_port, 12345);
        assert_eq!(info.original_dst_port, 80);
        assert_eq!(info.mtu, 0);
    }

    #[test]
    fn parse_dest_unreachable_no_route() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_NO_ROUTE,
            0,
            src_ip(), dst_ip(),
            5000, 6000,
        );
        let info = parse_icmpv6_error(&frame).unwrap();
        assert_eq!(info.icmp_type, ICMPV6_TYPE_DEST_UNREACHABLE);
        assert_eq!(info.icmp_code, ICMPV6_CODE_NO_ROUTE);
        assert_eq!(info.original_src_port, 5000);
        assert_eq!(info.original_dst_port, 6000);
    }

    #[test]
    fn parse_dest_unreachable_admin_prohibited() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_ADMIN_PROHIBITED,
            0,
            src_ip(), dst_ip(),
            1000, 2000,
        );
        let info = parse_icmpv6_error(&frame).unwrap();
        assert_eq!(info.icmp_code, ICMPV6_CODE_ADMIN_PROHIBITED);
    }

    #[test]
    fn parse_packet_too_big() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_PACKET_TOO_BIG, 0,
            1280, // MTU
            src_ip(), dst_ip(),
            9000, 9001,
        );
        let info = parse_icmpv6_error(&frame).unwrap();
        assert_eq!(info.icmp_type, ICMPV6_TYPE_PACKET_TOO_BIG);
        assert_eq!(info.icmp_code, 0);
        assert_eq!(info.mtu, 1280);
        assert_eq!(info.original_src_port, 9000);
        assert_eq!(info.original_dst_port, 9001);
    }

    #[test]
    fn parse_time_exceeded_hop_limit() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_TIME_EXCEEDED, ICMPV6_CODE_HOP_LIMIT_EXCEEDED,
            0,
            src_ip(), dst_ip(),
            4000, 5000,
        );
        let info = parse_icmpv6_error(&frame).unwrap();
        assert_eq!(info.icmp_type, ICMPV6_TYPE_TIME_EXCEEDED);
        assert_eq!(info.icmp_code, ICMPV6_CODE_HOP_LIMIT_EXCEEDED);
    }

    #[test]
    fn parse_time_exceeded_frag_reassembly() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_TIME_EXCEEDED, ICMPV6_CODE_FRAG_REASSEMBLY_EXCEEDED,
            0,
            src_ip(), dst_ip(),
            7000, 8000,
        );
        let info = parse_icmpv6_error(&frame).unwrap();
        assert_eq!(info.icmp_type, ICMPV6_TYPE_TIME_EXCEEDED);
        assert_eq!(info.icmp_code, ICMPV6_CODE_FRAG_REASSEMBLY_EXCEEDED);
    }

    #[test]
    fn parse_parameter_problem() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_PARAMETER_PROBLEM, ICMPV6_CODE_ERRONEOUS_HEADER,
            0,
            src_ip(), dst_ip(),
            3000, 4000,
        );
        let info = parse_icmpv6_error(&frame).unwrap();
        assert_eq!(info.icmp_type, ICMPV6_TYPE_PARAMETER_PROBLEM);
        assert_eq!(info.icmp_code, ICMPV6_CODE_ERRONEOUS_HEADER);
    }

    #[test]
    fn parse_rejects_echo_request_as_error() {
        let frame = build_echo6_request(SRC_MAC, DST_MAC, src_ip(), dst_ip(), 1, 1, b"ping");
        assert!(parse_icmpv6_error(&frame).is_none());
    }

    #[test]
    fn parse_rejects_echo_reply_as_error() {
        let packet = Icmpv6Packet {
            src_mac: SRC_MAC,
            dst_mac: DST_MAC,
            src_ip: src_ip(),
            dst_ip: dst_ip(),
            hop_limit: 64,
            icmp_type: ICMPV6_TYPE_ECHO_REPLY,
            icmp_code: ICMPV6_CODE_ECHO,
            checksum: 0,
            identifier: 1,
            sequence: 1,
            payload: vec![],
        };
        let frame = build_icmpv6_frame(&packet);
        assert!(parse_icmpv6_error(&frame).is_none());
    }

    #[test]
    fn parse_rejects_truncated_error() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_PORT_UNREACHABLE,
            0,
            src_ip(), dst_ip(),
            1000, 2000,
        );
        // Truncate before the original UDP ports
        let truncated = &frame[..frame.len() - 5];
        assert!(parse_icmpv6_error(truncated).is_none());
    }

    #[test]
    fn parse_error_rejects_ipv4_frame() {
        let frame = crate::build_udp_frame(
            &SRC_MAC, &DST_MAC,
            "10.0.0.1".parse().unwrap(),
            "10.0.0.2".parse().unwrap(),
            1000, 2000, b"ipv4", 64,
        ).unwrap();
        assert!(parse_icmpv6_error(&frame).is_none());
    }

    #[test]
    fn parse_error_rejects_too_short() {
        assert!(parse_icmpv6_error(&[0u8; 10]).is_none());
    }

    // --- to_io_error mapping ---

    #[test]
    fn error_port_unreachable_maps_to_connection_refused() {
        let info = Icmpv6ErrorInfo {
            icmp_type: ICMPV6_TYPE_DEST_UNREACHABLE,
            icmp_code: ICMPV6_CODE_PORT_UNREACHABLE,
            error_source: router_ip(),
            original_dst_ip: dst_ip(),
            original_src_ip: src_ip(),
            original_dst_port: 80,
            original_src_port: 12345,
            mtu: 0,
        };
        let err = info.to_io_error();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn error_admin_prohibited_maps_to_permission_denied() {
        let info = Icmpv6ErrorInfo {
            icmp_type: ICMPV6_TYPE_DEST_UNREACHABLE,
            icmp_code: ICMPV6_CODE_ADMIN_PROHIBITED,
            error_source: router_ip(),
            original_dst_ip: dst_ip(),
            original_src_ip: src_ip(),
            original_dst_port: 80,
            original_src_port: 12345,
            mtu: 0,
        };
        let err = info.to_io_error();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn error_hop_limit_exceeded_maps_to_timed_out() {
        let info = Icmpv6ErrorInfo {
            icmp_type: ICMPV6_TYPE_TIME_EXCEEDED,
            icmp_code: ICMPV6_CODE_HOP_LIMIT_EXCEEDED,
            error_source: router_ip(),
            original_dst_ip: dst_ip(),
            original_src_ip: src_ip(),
            original_dst_port: 80,
            original_src_port: 12345,
            mtu: 0,
        };
        let err = info.to_io_error();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn error_packet_too_big_includes_mtu() {
        let info = Icmpv6ErrorInfo {
            icmp_type: ICMPV6_TYPE_PACKET_TOO_BIG,
            icmp_code: 0,
            error_source: router_ip(),
            original_dst_ip: dst_ip(),
            original_src_ip: src_ip(),
            original_dst_port: 80,
            original_src_port: 12345,
            mtu: 1280,
        };
        let err = info.to_io_error();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(err.to_string().contains("1280"));
    }

    #[test]
    fn error_parameter_problem_maps_to_invalid_data() {
        let info = Icmpv6ErrorInfo {
            icmp_type: ICMPV6_TYPE_PARAMETER_PROBLEM,
            icmp_code: ICMPV6_CODE_UNRECOGNIZED_NEXT_HEADER,
            error_source: router_ip(),
            original_dst_ip: dst_ip(),
            original_src_ip: src_ip(),
            original_dst_port: 80,
            original_src_port: 12345,
            mtu: 0,
        };
        let err = info.to_io_error();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn error_no_route_maps_to_other() {
        let info = Icmpv6ErrorInfo {
            icmp_type: ICMPV6_TYPE_DEST_UNREACHABLE,
            icmp_code: ICMPV6_CODE_NO_ROUTE,
            error_source: router_ip(),
            original_dst_ip: dst_ip(),
            original_src_ip: src_ip(),
            original_dst_port: 80,
            original_src_port: 12345,
            mtu: 0,
        };
        let err = info.to_io_error();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(err.to_string().contains("no route"));
    }

    // --- Icmpv6Action / process_icmpv6_full ---

    #[test]
    fn handler_full_responds_to_echo_request() {
        let local_mac = DST_MAC;
        let local_ip = dst_ip();
        let handler = Icmpv6Handler::new(local_mac, local_ip);

        let request = build_echo6_request(SRC_MAC, local_mac, src_ip(), local_ip, 0x1234, 1, b"ping6");
        match handler.process_icmpv6_full(&request) {
            Some(Icmpv6Action::Reply(reply_frame)) => {
                let reply = parse_icmpv6_packet(&reply_frame).unwrap();
                assert!(reply.is_echo_reply());
                assert_eq!(reply.identifier, 0x1234);
            }
            _ => panic!("expected Reply action"),
        }
    }

    #[test]
    fn handler_full_returns_error_for_our_ip() {
        let local_mac = DST_MAC;
        let local_ip = src_ip(); // our IP is the original source
        let handler = Icmpv6Handler::new(local_mac, local_ip);

        let frame = build_icmpv6_error_frame(
            SRC_MAC, local_mac,
            router_ip(), local_ip,
            ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_PORT_UNREACHABLE,
            0,
            local_ip, dst_ip(), // original src = our IP
            12345, 80,
        );
        match handler.process_icmpv6_full(&frame) {
            Some(Icmpv6Action::Error(info)) => {
                assert_eq!(info.icmp_type, ICMPV6_TYPE_DEST_UNREACHABLE);
                assert_eq!(info.icmp_code, ICMPV6_CODE_PORT_UNREACHABLE);
                assert_eq!(info.original_src_port, 12345);
                assert_eq!(info.original_dst_port, 80);
            }
            _ => panic!("expected Error action"),
        }
    }

    #[test]
    fn handler_full_ignores_error_for_other_ip() {
        let local_mac = DST_MAC;
        let local_ip = dst_ip();
        let handler = Icmpv6Handler::new(local_mac, local_ip);

        // Error references a different source IP (not ours)
        let other_ip: Ipv6Addr = "2001:db8::99".parse().unwrap();
        let frame = build_icmpv6_error_frame(
            SRC_MAC, local_mac,
            router_ip(), local_ip,
            ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_PORT_UNREACHABLE,
            0,
            other_ip, dst_ip(), // original src = NOT our IP
            12345, 80,
        );
        assert!(handler.process_icmpv6_full(&frame).is_none());
    }

    #[test]
    fn handler_full_ignores_echo_reply() {
        let local_mac = DST_MAC;
        let local_ip = dst_ip();
        let handler = Icmpv6Handler::new(local_mac, local_ip);

        let packet = Icmpv6Packet {
            src_mac: SRC_MAC,
            dst_mac: local_mac,
            src_ip: src_ip(),
            dst_ip: local_ip,
            hop_limit: 64,
            icmp_type: ICMPV6_TYPE_ECHO_REPLY,
            icmp_code: ICMPV6_CODE_ECHO,
            checksum: 0,
            identifier: 1,
            sequence: 1,
            payload: vec![],
        };
        let frame = build_icmpv6_frame(&packet);
        assert!(handler.process_icmpv6_full(&frame).is_none());
    }

    #[test]
    fn handler_full_with_multiple_ips() {
        let local_mac = DST_MAC;
        let ip1 = src_ip();
        let ip2: Ipv6Addr = "fe80::1".parse().unwrap();

        let mut handler = Icmpv6Handler::new(local_mac, ip1);
        handler.add_local_ip(ip2);

        // Error referencing ip2 as original source
        let frame = build_icmpv6_error_frame(
            SRC_MAC, local_mac,
            router_ip(), ip2,
            ICMPV6_TYPE_TIME_EXCEEDED, ICMPV6_CODE_HOP_LIMIT_EXCEEDED,
            0,
            ip2, dst_ip(),
            5000, 6000,
        );
        match handler.process_icmpv6_full(&frame) {
            Some(Icmpv6Action::Error(info)) => {
                assert_eq!(info.original_src_ip, ip2);
                assert_eq!(info.original_src_port, 5000);
            }
            _ => panic!("expected Error action for ip2"),
        }
    }

    // --- Synthetic performance ---

    #[test]
    fn perf_parse_icmpv6_error() {
        let frame = build_icmpv6_error_frame(
            SRC_MAC, DST_MAC,
            router_ip(), dst_ip(),
            ICMPV6_TYPE_DEST_UNREACHABLE, ICMPV6_CODE_PORT_UNREACHABLE,
            0,
            src_ip(), dst_ip(),
            12345, 80,
        );
        let iterations = 10_000;

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            let _ = parse_icmpv6_error(&frame).unwrap();
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!(
            "[PERF] ICMPv6 error parse: {} iterations in {:?} ({} ns/op)",
            iterations, elapsed, ns_per_op
        );
        assert!(ns_per_op < 10_000, "error parse too slow: {} ns/op", ns_per_op);
    }
}
