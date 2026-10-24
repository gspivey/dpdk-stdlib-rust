//! IPv6 header build/parse support.
//!
//! Provides 40-byte fixed IPv6 header construction and parsing, plus
//! extension-header chain walking (Hop-by-Hop, Routing, Fragment,
//! Destination Options) to locate the L4 payload offset.
//!
//! This module parallels the IPv4 build/parse functions in `lib.rs`:
//! - [`build_udp6_frame`] → allocating frame builder (like `build_udp_frame`)
//! - [`build_udp6_frame_into`] → zero-alloc frame builder (like `build_udp_frame_into`)
//! - [`parse_udp6_packet`] → allocating parser (like `parse_udp_packet`)
//! - [`parse_udp6_packet_ref`] → zero-copy parser (like `parse_udp_packet_ref`)

use std::net::Ipv6Addr;

use crate::{
    detect_vlan, ETH_HEADER_LEN, IP_PROTO_UDP, UDP_HEADER_LEN, UdpError, UdpResult,
    VLAN_TAG_LEN,
};

// ============================================================================
// Constants
// ============================================================================

/// IPv6 fixed header size (always 40 bytes, no variable-length options in the
/// fixed header unlike IPv4's IHL field).
pub const IPV6_HEADER_LEN: usize = 40;

/// EtherType for IPv6 (0x86DD).
pub const ETH_TYPE_IPV6: u16 = 0x86DD;

/// Total header overhead for an untagged IPv6/UDP frame.
pub const TOTAL_HEADER_LEN_V6: usize = ETH_HEADER_LEN + IPV6_HEADER_LEN + UDP_HEADER_LEN;

/// Total header overhead for a VLAN-tagged IPv6/UDP frame.
pub const TOTAL_HEADER_LEN_V6_VLAN: usize =
    ETH_HEADER_LEN + VLAN_TAG_LEN + IPV6_HEADER_LEN + UDP_HEADER_LEN;

/// Maximum UDP payload for standard 1500-byte MTU over IPv6.
/// 1500 − 40 (IPv6) − 8 (UDP) = 1452.
pub const MAX_UDP_PAYLOAD_V6: usize = 1452;

/// IPv6 Next Header / IP protocol: Hop-by-Hop Options.
pub const IP_PROTO_HOPOPT: u8 = 0;

/// IPv6 Next Header / IP protocol: Routing.
pub const IP_PROTO_ROUTING: u8 = 43;

/// IPv6 Next Header / IP protocol: Fragment.
pub const IP_PROTO_FRAGMENT: u8 = 44;

/// IPv6 Next Header / IP protocol: ICMPv6.
pub const IP_PROTO_ICMPV6: u8 = 58;

/// IPv6 Next Header / IP protocol: Destination Options.
pub const IP_PROTO_DSTOPTS: u8 = 60;

/// IPv6 Next Header: No Next Header (RFC 2460 §4.7).
pub const IP_PROTO_NONE: u8 = 59;

/// Maximum frame size for jumbo MTU (same as IPv4 — 9001 + 14 Ethernet).
const MAX_FRAME_SIZE_V6: usize = ETH_HEADER_LEN + 9001;

// ============================================================================
// Extension Header Walking
// ============================================================================

/// Result of walking the IPv6 extension header chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv6NextHeader {
    /// The final (upper-layer) protocol number (e.g. 17 for UDP, 58 for ICMPv6).
    pub protocol: u8,
    /// Byte offset from the start of the IPv6 header to the upper-layer payload.
    pub payload_offset: usize,
}

/// Walk the IPv6 extension header chain starting from the fixed header's
/// Next Header field, returning the upper-layer protocol and its byte offset
/// relative to the start of the IPv6 header.
///
/// `data` must start at the first byte of the IPv6 header.
///
/// Recognized extension headers (variable-length TLV with 8-byte granularity):
/// - Hop-by-Hop Options (0)
/// - Routing (43)
/// - Destination Options (60)
///
/// Fragment header (44) is fixed at 8 bytes (no length field).
///
/// Returns `None` if the chain is malformed (truncated or exceeds `data`).
pub fn walk_extension_headers(data: &[u8]) -> Option<Ipv6NextHeader> {
    if data.len() < IPV6_HEADER_LEN {
        return None;
    }

    let mut next_header = data[6]; // Next Header field in fixed header
    let mut offset = IPV6_HEADER_LEN; // past the 40-byte fixed header

    loop {
        match next_header {
            // Variable-length extension headers: Hop-by-Hop, Routing, Dest Options
            IP_PROTO_HOPOPT | IP_PROTO_ROUTING | IP_PROTO_DSTOPTS => {
                if offset + 2 > data.len() {
                    return None;
                }
                let ext_next = data[offset];
                // Length is in 8-octet units, not counting the first 8 octets
                let ext_len = (data[offset + 1] as usize + 1) * 8;
                if offset + ext_len > data.len() {
                    return None;
                }
                next_header = ext_next;
                offset += ext_len;
            }
            // Fragment header: fixed 8 bytes
            IP_PROTO_FRAGMENT => {
                if offset + 8 > data.len() {
                    return None;
                }
                let ext_next = data[offset];
                next_header = ext_next;
                offset += 8;
            }
            // Any other value is the upper-layer protocol (or No Next Header)
            _ => {
                return Some(Ipv6NextHeader {
                    protocol: next_header,
                    payload_offset: offset,
                });
            }
        }
    }
}

// ============================================================================
// Parsed Packet Types
// ============================================================================

/// Parsed UDP-over-IPv6 packet (allocating — owns the payload).
#[derive(Debug, Clone)]
pub struct ParsedUdp6Packet {
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
    pub src_ip: Ipv6Addr,
    pub dst_ip: Ipv6Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: Vec<u8>,
    pub vlan_id: Option<u16>,
}

/// Zero-copy parsed UDP-over-IPv6 packet (borrows payload from the frame).
#[derive(Debug)]
pub struct ParsedUdp6PacketRef<'a> {
    pub src_mac: [u8; 6],
    pub src_ip: Ipv6Addr,
    pub dst_ip: Ipv6Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
    pub vlan_id: Option<u16>,
}

// ============================================================================
// Frame Building
// ============================================================================

/// Write the 40-byte IPv6 fixed header into `buf` starting at `offset`.
///
/// `payload_length` is the number of bytes after the fixed header (extension
/// headers + upper-layer data). `next_header` is the Next Header field value.
fn write_ipv6_header(
    buf: &mut [u8],
    offset: usize,
    src_ip: &Ipv6Addr,
    dst_ip: &Ipv6Addr,
    payload_length: u16,
    next_header: u8,
    hop_limit: u8,
) {
    let ip = offset;
    // Version (6), Traffic Class (0), Flow Label (0)
    buf[ip] = 0x60;
    buf[ip + 1] = 0x00;
    buf[ip + 2] = 0x00;
    buf[ip + 3] = 0x00;
    // Payload Length
    buf[ip + 4..ip + 6].copy_from_slice(&payload_length.to_be_bytes());
    // Next Header
    buf[ip + 6] = next_header;
    // Hop Limit
    buf[ip + 7] = hop_limit;
    // Source Address (16 bytes)
    buf[ip + 8..ip + 24].copy_from_slice(&src_ip.octets());
    // Destination Address (16 bytes)
    buf[ip + 24..ip + 40].copy_from_slice(&dst_ip.octets());
}

/// Write the 8-byte UDP header into `buf` starting at `offset`.
fn write_udp_header(
    buf: &mut [u8],
    offset: usize,
    src_port: u16,
    dst_port: u16,
    udp_len: u16,
) {
    buf[offset..offset + 2].copy_from_slice(&src_port.to_be_bytes());
    buf[offset + 2..offset + 4].copy_from_slice(&dst_port.to_be_bytes());
    buf[offset + 4..offset + 6].copy_from_slice(&udp_len.to_be_bytes());
    buf[offset + 6..offset + 8].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
}

/// Build a complete UDP-over-IPv6 Ethernet frame, returning an owned `Vec<u8>`.
///
/// Parallel to [`crate::build_udp_frame`] but for IPv6. The frame layout is:
/// `[Ethernet 14B][IPv6 40B][UDP 8B][Payload]`.
///
/// The UDP checksum is mandatory for IPv6 (RFC 8200 §8.1) and is always computed.
pub fn build_udp6_frame(
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    src_ip: Ipv6Addr,
    dst_ip: Ipv6Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    hop_limit: u8,
) -> UdpResult<Vec<u8>> {
    let max_payload = MAX_FRAME_SIZE_V6 - TOTAL_HEADER_LEN_V6;
    if payload.len() > max_payload {
        return Err(UdpError::PayloadTooLarge {
            max: max_payload,
            actual: payload.len(),
        });
    }

    let total_len = TOTAL_HEADER_LEN_V6 + payload.len();
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let ipv6_payload_len = udp_len; // no extension headers

    let mut frame = vec![0u8; total_len];

    // Ethernet header
    frame[0..6].copy_from_slice(dst_mac);
    frame[6..12].copy_from_slice(src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

    // IPv6 header
    write_ipv6_header(
        &mut frame,
        ETH_HEADER_LEN,
        &src_ip,
        &dst_ip,
        ipv6_payload_len,
        IP_PROTO_UDP,
        hop_limit,
    );

    // UDP header
    let udp_off = ETH_HEADER_LEN + IPV6_HEADER_LEN;
    write_udp_header(&mut frame, udp_off, src_port, dst_port, udp_len);

    // Payload
    frame[TOTAL_HEADER_LEN_V6..].copy_from_slice(payload);

    // UDP checksum (mandatory for IPv6)
    let cksum = udp6_checksum(&src_ip, &dst_ip, &frame[udp_off..udp_off + UDP_HEADER_LEN], payload);
    frame[udp_off + 6..udp_off + 8].copy_from_slice(&cksum.to_be_bytes());

    Ok(frame)
}

/// Build a UDP-over-IPv6 frame into a caller-provided buffer (zero-alloc hot path).
///
/// Parallel to [`crate::build_udp_frame_into`]. Returns the frame length.
pub fn build_udp6_frame_into(
    out: &mut Vec<u8>,
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    src_ip: Ipv6Addr,
    dst_ip: Ipv6Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    hop_limit: u8,
) -> UdpResult<usize> {
    let max_payload = MAX_FRAME_SIZE_V6 - TOTAL_HEADER_LEN_V6;
    if payload.len() > max_payload {
        return Err(UdpError::PayloadTooLarge {
            max: max_payload,
            actual: payload.len(),
        });
    }

    let total_len = TOTAL_HEADER_LEN_V6 + payload.len();
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let ipv6_payload_len = udp_len;

    out.resize(total_len, 0);

    // Ethernet
    out[0..6].copy_from_slice(dst_mac);
    out[6..12].copy_from_slice(src_mac);
    out[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

    // IPv6
    write_ipv6_header(out, ETH_HEADER_LEN, &src_ip, &dst_ip, ipv6_payload_len, IP_PROTO_UDP, hop_limit);

    // UDP
    let udp_off = ETH_HEADER_LEN + IPV6_HEADER_LEN;
    write_udp_header(out, udp_off, src_port, dst_port, udp_len);

    // Payload
    out[TOTAL_HEADER_LEN_V6..].copy_from_slice(payload);

    // Checksum
    let cksum = udp6_checksum(&src_ip, &dst_ip, &out[udp_off..udp_off + UDP_HEADER_LEN], payload);
    out[udp_off + 6..udp_off + 8].copy_from_slice(&cksum.to_be_bytes());

    Ok(total_len)
}

// ============================================================================
// Checksum
// ============================================================================

/// Fold a 32-bit accumulator to a 16-bit one's-complement sum.
fn fold32(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum as u16
}

/// Add a byte slice to a running checksum accumulator (16-bit word aligned).
fn checksum_add(sum: &mut u32, data: &[u8]) {
    for i in (0..data.len()).step_by(2) {
        let word = if i + 1 < data.len() {
            ((data[i] as u32) << 8) | (data[i + 1] as u32)
        } else {
            (data[i] as u32) << 8
        };
        *sum = sum.wrapping_add(word);
    }
}

/// Compute the UDP checksum over an IPv6 pseudo-header + UDP header + payload.
///
/// Unlike IPv4, the UDP checksum is **mandatory** for IPv6 (RFC 8200 §8.1).
/// A computed value of 0 is transmitted as 0xFFFF.
pub fn udp6_checksum(
    src_ip: &Ipv6Addr,
    dst_ip: &Ipv6Addr,
    udp_header: &[u8],
    payload: &[u8],
) -> u16 {
    let mut sum: u32 = 0;

    // IPv6 pseudo-header (RFC 8200 §8.1):
    //   Source Address (16 bytes)
    //   Destination Address (16 bytes)
    //   Upper-Layer Packet Length (4 bytes, u32)
    //   zero (3 bytes) + Next Header (1 byte)
    checksum_add(&mut sum, &src_ip.octets());
    checksum_add(&mut sum, &dst_ip.octets());

    let udp_len = (UDP_HEADER_LEN + payload.len()) as u32;
    sum = sum.wrapping_add(udp_len >> 16);
    sum = sum.wrapping_add(udp_len & 0xFFFF);
    sum = sum.wrapping_add(IP_PROTO_UDP as u32);

    // UDP header (skip checksum field at bytes 6-7)
    for i in (0..udp_header.len()).step_by(2) {
        if i == 6 { continue; }
        let word = if i + 1 < udp_header.len() {
            ((udp_header[i] as u32) << 8) | (udp_header[i + 1] as u32)
        } else {
            (udp_header[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    // Payload
    checksum_add(&mut sum, payload);

    let result = !fold32(sum);
    if result == 0 { 0xFFFF } else { result }
}

/// Compute the IPv6 UDP pseudo-header checksum for TX hardware offload.
///
/// The NIC adds the UDP header + payload contribution on top of this value.
/// Not one's-complemented — the NIC does that.
///
/// Note: IPv6 pseudo-header uses a 32-bit Upper-Layer Packet Length field
/// (RFC 8200 §8.1), unlike IPv4's 16-bit field.
pub fn udp6_pseudo_header_checksum(src_ip: &Ipv6Addr, dst_ip: &Ipv6Addr, udp_len: u32) -> u16 {
    let mut sum: u32 = 0;
    checksum_add(&mut sum, &src_ip.octets());
    checksum_add(&mut sum, &dst_ip.octets());
    sum = sum.wrapping_add(udp_len >> 16);
    sum = sum.wrapping_add(udp_len & 0xFFFF);
    sum = sum.wrapping_add(IP_PROTO_UDP as u32);
    fold32(sum)
}

/// Verify the UDP checksum of a received IPv6 frame.
///
/// Returns `true` if the checksum is valid. Unlike IPv4, a checksum value of 0
/// is **invalid** for IPv6 (RFC 8200 §8.1) — this function returns `false` for
/// zero checksums.
pub fn verify_udp6_checksum(frame: &[u8]) -> bool {
    let layout = match detect_vlan(frame, None) {
        Some(l) => l,
        None => return false,
    };
    if layout.ethertype != ETH_TYPE_IPV6 {
        return false;
    }
    let l3 = layout.l3_offset;
    if frame.len() < l3 + IPV6_HEADER_LEN {
        return false;
    }

    let nh = match walk_extension_headers(&frame[l3..]) {
        Some(nh) if nh.protocol == IP_PROTO_UDP => nh,
        _ => return false,
    };

    let udp_start = l3 + nh.payload_offset;
    if frame.len() < udp_start + UDP_HEADER_LEN {
        return false;
    }

    // Zero checksum is invalid for IPv6
    let stored_cksum = u16::from_be_bytes([frame[udp_start + 6], frame[udp_start + 7]]);
    if stored_cksum == 0 {
        return false;
    }

    let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 8..l3 + 24]).unwrap());
    let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 24..l3 + 40]).unwrap());

    let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;
    if udp_len < UDP_HEADER_LEN || frame.len() < udp_start + udp_len {
        return false;
    }

    let udp_header = &frame[udp_start..udp_start + UDP_HEADER_LEN];
    let payload = &frame[udp_start + UDP_HEADER_LEN..udp_start + udp_len];

    let computed = udp6_checksum(&src_ip, &dst_ip, udp_header, payload);
    // When verifying, the stored checksum is included in the computation.
    // Re-compute from scratch (skipping the checksum field) and compare.
    computed == stored_cksum
}

// ============================================================================
// Packet Parsing
// ============================================================================

/// Parse a raw Ethernet frame containing a UDP-over-IPv6 packet.
///
/// Handles both untagged and 802.1Q VLAN-tagged frames. Walks extension
/// headers to locate the UDP payload. Returns `None` if the frame is not
/// a valid UDP/IPv6 packet.
pub fn parse_udp6_packet(frame: &[u8]) -> Option<ParsedUdp6Packet> {
    let layout = detect_vlan(frame, None)?;
    let l3 = layout.l3_offset;

    if layout.ethertype != ETH_TYPE_IPV6 {
        return None;
    }
    if frame.len() < l3 + IPV6_HEADER_LEN {
        return None;
    }

    // Verify version == 6
    if (frame[l3] >> 4) != 6 {
        return None;
    }

    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    let dst_mac: [u8; 6] = frame[0..6].try_into().ok()?;

    let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 8..l3 + 24]).unwrap());
    let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 24..l3 + 40]).unwrap());

    // Walk extension headers to find UDP
    let nh = walk_extension_headers(&frame[l3..])?;
    if nh.protocol != IP_PROTO_UDP {
        return None;
    }

    let udp_start = l3 + nh.payload_offset;
    if frame.len() < udp_start + UDP_HEADER_LEN {
        return None;
    }

    let src_port = u16::from_be_bytes([frame[udp_start], frame[udp_start + 1]]);
    let dst_port = u16::from_be_bytes([frame[udp_start + 2], frame[udp_start + 3]]);
    let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;

    if udp_len < UDP_HEADER_LEN || frame.len() < udp_start + udp_len {
        return None;
    }

    let payload_start = udp_start + UDP_HEADER_LEN;
    let payload_len = udp_len - UDP_HEADER_LEN;
    let payload = frame[payload_start..payload_start + payload_len].to_vec();

    let vlan_id = layout.vlan_tci.map(|tci| tci & 0x0FFF);

    Some(ParsedUdp6Packet {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        payload,
        vlan_id,
    })
}

/// Zero-copy UDP-over-IPv6 parser that borrows payload from the frame slice.
///
/// Identical validation to [`parse_udp6_packet`] but avoids heap allocation.
pub fn parse_udp6_packet_ref(frame: &[u8]) -> Option<ParsedUdp6PacketRef<'_>> {
    let layout = detect_vlan(frame, None)?;
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

    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 8..l3 + 24]).unwrap());
    let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 24..l3 + 40]).unwrap());

    let nh = walk_extension_headers(&frame[l3..])?;
    if nh.protocol != IP_PROTO_UDP {
        return None;
    }

    let udp_start = l3 + nh.payload_offset;
    if frame.len() < udp_start + UDP_HEADER_LEN {
        return None;
    }

    let src_port = u16::from_be_bytes([frame[udp_start], frame[udp_start + 1]]);
    let dst_port = u16::from_be_bytes([frame[udp_start + 2], frame[udp_start + 3]]);
    let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;

    if udp_len < UDP_HEADER_LEN || frame.len() < udp_start + udp_len {
        return None;
    }

    let payload_start = udp_start + UDP_HEADER_LEN;
    let payload_len = udp_len - UDP_HEADER_LEN;
    let vlan_id = layout.vlan_tci.map(|tci| tci & 0x0FFF);

    Some(ParsedUdp6PacketRef {
        src_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        vlan_id,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SRC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    const DST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

    fn src_ip() -> Ipv6Addr {
        "2001:db8::1".parse().unwrap()
    }
    fn dst_ip() -> Ipv6Addr {
        "2001:db8::2".parse().unwrap()
    }

    // --- Constants ---

    #[test]
    fn constants_are_correct() {
        assert_eq!(IPV6_HEADER_LEN, 40);
        assert_eq!(ETH_TYPE_IPV6, 0x86DD);
        assert_eq!(TOTAL_HEADER_LEN_V6, 14 + 40 + 8); // 62
        assert_eq!(TOTAL_HEADER_LEN_V6_VLAN, 14 + 4 + 40 + 8); // 66
        assert_eq!(MAX_UDP_PAYLOAD_V6, 1500 - 40 - 8); // 1452
    }

    // --- Extension header walking ---

    #[test]
    fn walk_no_extension_headers() {
        // Minimal IPv6 header: next_header = UDP (17), no extensions
        let mut data = vec![0u8; IPV6_HEADER_LEN + 8]; // + UDP header
        data[0] = 0x60; // version 6
        data[6] = IP_PROTO_UDP; // next header
        let nh = walk_extension_headers(&data).unwrap();
        assert_eq!(nh.protocol, IP_PROTO_UDP);
        assert_eq!(nh.payload_offset, IPV6_HEADER_LEN);
    }

    #[test]
    fn walk_hop_by_hop_then_udp() {
        // IPv6 fixed header → Hop-by-Hop (8 bytes) → UDP
        let mut data = vec![0u8; IPV6_HEADER_LEN + 8 + 8];
        data[0] = 0x60;
        data[6] = IP_PROTO_HOPOPT; // next header = Hop-by-Hop
        // Hop-by-Hop ext header at offset 40
        data[40] = IP_PROTO_UDP; // next header
        data[41] = 0; // length = (0+1)*8 = 8 bytes
        let nh = walk_extension_headers(&data).unwrap();
        assert_eq!(nh.protocol, IP_PROTO_UDP);
        assert_eq!(nh.payload_offset, IPV6_HEADER_LEN + 8);
    }

    #[test]
    fn walk_routing_then_dstopts_then_udp() {
        // IPv6 → Routing (16 bytes) → Dest Options (8 bytes) → UDP
        let mut data = vec![0u8; IPV6_HEADER_LEN + 16 + 8 + 8];
        data[0] = 0x60;
        data[6] = IP_PROTO_ROUTING;
        // Routing header at offset 40: 16 bytes
        data[40] = IP_PROTO_DSTOPTS; // next = Dest Options
        data[41] = 1; // length = (1+1)*8 = 16 bytes
        // Dest Options at offset 56: 8 bytes
        data[56] = IP_PROTO_UDP;
        data[57] = 0; // length = (0+1)*8 = 8 bytes
        let nh = walk_extension_headers(&data).unwrap();
        assert_eq!(nh.protocol, IP_PROTO_UDP);
        assert_eq!(nh.payload_offset, IPV6_HEADER_LEN + 16 + 8);
    }

    #[test]
    fn walk_fragment_header() {
        // IPv6 → Fragment (always 8 bytes) → UDP
        let mut data = vec![0u8; IPV6_HEADER_LEN + 8 + 8];
        data[0] = 0x60;
        data[6] = IP_PROTO_FRAGMENT;
        data[40] = IP_PROTO_UDP; // next header in fragment header
        let nh = walk_extension_headers(&data).unwrap();
        assert_eq!(nh.protocol, IP_PROTO_UDP);
        assert_eq!(nh.payload_offset, IPV6_HEADER_LEN + 8);
    }

    #[test]
    fn walk_truncated_returns_none() {
        // Too short for even the fixed header
        let data = vec![0u8; 20];
        assert!(walk_extension_headers(&data).is_none());
    }

    #[test]
    fn walk_truncated_extension_returns_none() {
        // Fixed header says Hop-by-Hop, but data is too short
        let mut data = vec![0u8; IPV6_HEADER_LEN + 1]; // only 1 byte of ext
        data[0] = 0x60;
        data[6] = IP_PROTO_HOPOPT;
        assert!(walk_extension_headers(&data).is_none());
    }

    #[test]
    fn walk_no_next_header() {
        let mut data = vec![0u8; IPV6_HEADER_LEN];
        data[0] = 0x60;
        data[6] = IP_PROTO_NONE; // No Next Header
        let nh = walk_extension_headers(&data).unwrap();
        assert_eq!(nh.protocol, IP_PROTO_NONE);
        assert_eq!(nh.payload_offset, IPV6_HEADER_LEN);
    }

    #[test]
    fn walk_icmpv6() {
        let mut data = vec![0u8; IPV6_HEADER_LEN + 8];
        data[0] = 0x60;
        data[6] = IP_PROTO_ICMPV6;
        let nh = walk_extension_headers(&data).unwrap();
        assert_eq!(nh.protocol, IP_PROTO_ICMPV6);
        assert_eq!(nh.payload_offset, IPV6_HEADER_LEN);
    }

    // --- Build + Parse roundtrip ---

    #[test]
    fn build_and_parse_roundtrip() {
        let payload = b"hello ipv6";
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 12345, 9000, payload, 64,
        )
        .unwrap();

        assert_eq!(frame.len(), TOTAL_HEADER_LEN_V6 + payload.len());

        let parsed = parse_udp6_packet(&frame).unwrap();
        assert_eq!(parsed.src_mac, SRC_MAC);
        assert_eq!(parsed.dst_mac, DST_MAC);
        assert_eq!(parsed.src_ip, src_ip());
        assert_eq!(parsed.dst_ip, dst_ip());
        assert_eq!(parsed.src_port, 12345);
        assert_eq!(parsed.dst_port, 9000);
        assert_eq!(parsed.payload, payload);
        assert_eq!(parsed.vlan_id, None);
    }

    #[test]
    fn build_and_parse_ref_roundtrip() {
        let payload = b"zero-copy ipv6";
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 5000, 6000, payload, 128,
        )
        .unwrap();

        let parsed = parse_udp6_packet_ref(&frame).unwrap();
        assert_eq!(parsed.src_mac, SRC_MAC);
        assert_eq!(parsed.src_ip, src_ip());
        assert_eq!(parsed.dst_ip, dst_ip());
        assert_eq!(parsed.src_port, 5000);
        assert_eq!(parsed.dst_port, 6000);
        assert_eq!(parsed.payload, payload);
        assert_eq!(parsed.vlan_id, None);
    }

    #[test]
    fn build_into_and_parse_roundtrip() {
        let payload = b"frame_into test";
        let mut buf = Vec::new();
        let len = build_udp6_frame_into(
            &mut buf, &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1111, 2222, payload, 255,
        )
        .unwrap();

        assert_eq!(len, TOTAL_HEADER_LEN_V6 + payload.len());
        assert_eq!(buf.len(), len);

        let parsed = parse_udp6_packet(&buf).unwrap();
        assert_eq!(parsed.src_port, 1111);
        assert_eq!(parsed.dst_port, 2222);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn build_into_reuses_buffer() {
        let mut buf = Vec::with_capacity(1500);
        let ptr1 = buf.as_ptr();
        build_udp6_frame_into(
            &mut buf, &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"a", 64,
        )
        .unwrap();
        let ptr2 = buf.as_ptr();
        // Same allocation reused (no realloc for small payloads)
        assert_eq!(ptr1, ptr2);
    }

    #[test]
    fn empty_payload() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 100, 200, b"", 64,
        )
        .unwrap();
        assert_eq!(frame.len(), TOTAL_HEADER_LEN_V6);

        let parsed = parse_udp6_packet(&frame).unwrap();
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn max_standard_payload() {
        let payload = vec![0xAB; MAX_UDP_PAYLOAD_V6];
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 100, 200, &payload, 64,
        )
        .unwrap();
        let parsed = parse_udp6_packet(&frame).unwrap();
        assert_eq!(parsed.payload.len(), MAX_UDP_PAYLOAD_V6);
    }

    #[test]
    fn oversized_payload_rejected() {
        let max = MAX_FRAME_SIZE_V6 - TOTAL_HEADER_LEN_V6;
        let payload = vec![0u8; max + 1];
        let err = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, &payload, 64,
        )
        .unwrap_err();
        match err {
            UdpError::PayloadTooLarge { .. } => {}
            other => panic!("expected PayloadTooLarge, got {:?}", other),
        }
    }

    // --- Checksum ---

    #[test]
    fn udp6_checksum_is_valid_on_built_frame() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 4000, 5000, b"checksum test", 64,
        )
        .unwrap();
        assert!(verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_detects_corruption() {
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 4000, 5000, b"corrupt me", 64,
        )
        .unwrap();
        // Flip a payload byte
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(!verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_zero_is_invalid() {
        // Build a valid frame, then zero out the checksum field
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"x", 64,
        )
        .unwrap();
        let udp_off = ETH_HEADER_LEN + IPV6_HEADER_LEN;
        frame[udp_off + 6] = 0;
        frame[udp_off + 7] = 0;
        // IPv6 mandates non-zero UDP checksum
        assert!(!verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_pseudo_header_checksum_basic() {
        let phc = udp6_pseudo_header_checksum(&src_ip(), &dst_ip(), 16);
        // Just verify it's non-zero and deterministic
        assert_ne!(phc, 0);
        assert_eq!(phc, udp6_pseudo_header_checksum(&src_ip(), &dst_ip(), 16));
    }

    // --- Wire format ---

    #[test]
    fn wire_format_ethertype() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"", 64,
        )
        .unwrap();
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        assert_eq!(ethertype, ETH_TYPE_IPV6);
    }

    #[test]
    fn wire_format_ipv6_version() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"", 64,
        )
        .unwrap();
        let version = frame[ETH_HEADER_LEN] >> 4;
        assert_eq!(version, 6);
    }

    #[test]
    fn wire_format_hop_limit() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"", 42,
        )
        .unwrap();
        assert_eq!(frame[ETH_HEADER_LEN + 7], 42);
    }

    #[test]
    fn wire_format_next_header_is_udp() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"", 64,
        )
        .unwrap();
        assert_eq!(frame[ETH_HEADER_LEN + 6], IP_PROTO_UDP);
    }

    #[test]
    fn wire_format_payload_length() {
        let payload = b"twelve bytes";
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, payload, 64,
        )
        .unwrap();
        let ip = ETH_HEADER_LEN;
        let payload_len = u16::from_be_bytes([frame[ip + 4], frame[ip + 5]]);
        assert_eq!(payload_len as usize, UDP_HEADER_LEN + payload.len());
    }

    #[test]
    fn wire_format_addresses() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"", 64,
        )
        .unwrap();
        let ip = ETH_HEADER_LEN;
        let src: [u8; 16] = frame[ip + 8..ip + 24].try_into().unwrap();
        let dst: [u8; 16] = frame[ip + 24..ip + 40].try_into().unwrap();
        assert_eq!(Ipv6Addr::from(src), src_ip());
        assert_eq!(Ipv6Addr::from(dst), dst_ip());
    }

    // --- Parse edge cases ---

    #[test]
    fn parse_rejects_ipv4_frame() {
        // Build an IPv4 frame and try to parse as IPv6
        let frame = crate::build_udp_frame(
            &SRC_MAC, &DST_MAC,
            "10.0.0.1".parse().unwrap(),
            "10.0.0.2".parse().unwrap(),
            1000, 2000, b"ipv4", 64,
        )
        .unwrap();
        assert!(parse_udp6_packet(&frame).is_none());
        assert!(parse_udp6_packet_ref(&frame).is_none());
    }

    #[test]
    fn parse_rejects_truncated_frame() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"data", 64,
        )
        .unwrap();
        // Truncate to just the Ethernet + partial IPv6 header
        assert!(parse_udp6_packet(&frame[..30]).is_none());
    }

    #[test]
    fn parse_rejects_wrong_version() {
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"x", 64,
        )
        .unwrap();
        // Change version nibble from 6 to 4
        frame[ETH_HEADER_LEN] = (frame[ETH_HEADER_LEN] & 0x0F) | 0x40;
        assert!(parse_udp6_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_non_udp_protocol() {
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"x", 64,
        )
        .unwrap();
        // Change next header from UDP (17) to ICMPv6 (58)
        frame[ETH_HEADER_LEN + 6] = IP_PROTO_ICMPV6;
        assert!(parse_udp6_packet(&frame).is_none());
    }

    // --- VLAN ---

    #[test]
    fn parse_vlan_tagged_frame() {
        // Manually construct a VLAN-tagged IPv6/UDP frame
        let payload = b"vlan6";
        let inner_len = IPV6_HEADER_LEN + UDP_HEADER_LEN + payload.len();
        let total = ETH_HEADER_LEN + crate::VLAN_TAG_LEN + inner_len;
        let mut frame = vec![0u8; total];

        // Ethernet
        frame[0..6].copy_from_slice(&DST_MAC);
        frame[6..12].copy_from_slice(&SRC_MAC);
        frame[12..14].copy_from_slice(&crate::ETH_TYPE_VLAN.to_be_bytes());
        // VLAN TCI: VID=100
        frame[14..16].copy_from_slice(&100u16.to_be_bytes());
        frame[16..18].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

        let l3 = ETH_HEADER_LEN + crate::VLAN_TAG_LEN;
        // IPv6 header
        frame[l3] = 0x60;
        let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
        frame[l3 + 4..l3 + 6].copy_from_slice(&udp_len.to_be_bytes());
        frame[l3 + 6] = IP_PROTO_UDP;
        frame[l3 + 7] = 64;
        frame[l3 + 8..l3 + 24].copy_from_slice(&src_ip().octets());
        frame[l3 + 24..l3 + 40].copy_from_slice(&dst_ip().octets());

        // UDP header
        let udp_off = l3 + IPV6_HEADER_LEN;
        frame[udp_off..udp_off + 2].copy_from_slice(&3000u16.to_be_bytes());
        frame[udp_off + 2..udp_off + 4].copy_from_slice(&4000u16.to_be_bytes());
        frame[udp_off + 4..udp_off + 6].copy_from_slice(&udp_len.to_be_bytes());
        // Payload
        frame[udp_off + UDP_HEADER_LEN..].copy_from_slice(payload);
        // Checksum
        let cksum = udp6_checksum(
            &src_ip(), &dst_ip(),
            &frame[udp_off..udp_off + UDP_HEADER_LEN],
            payload,
        );
        frame[udp_off + 6..udp_off + 8].copy_from_slice(&cksum.to_be_bytes());

        let parsed = parse_udp6_packet(&frame).unwrap();
        assert_eq!(parsed.vlan_id, Some(100));
        assert_eq!(parsed.src_port, 3000);
        assert_eq!(parsed.dst_port, 4000);
        assert_eq!(parsed.payload, payload);
    }

    // --- Loopback and special addresses ---

    #[test]
    fn loopback_address() {
        let lo = Ipv6Addr::LOCALHOST;
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, lo, lo, 1, 2, b"lo", 64,
        )
        .unwrap();
        let parsed = parse_udp6_packet(&frame).unwrap();
        assert_eq!(parsed.src_ip, lo);
        assert_eq!(parsed.dst_ip, lo);
    }

    #[test]
    fn link_local_address() {
        let ll: Ipv6Addr = "fe80::1".parse().unwrap();
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, ll, ll, 1, 2, b"ll", 64,
        )
        .unwrap();
        let parsed = parse_udp6_packet(&frame).unwrap();
        assert_eq!(parsed.src_ip, ll);
    }

    // --- UDP6 checksum: VLAN-tagged frames ---

    #[test]
    fn udp6_checksum_valid_on_vlan_tagged_frame() {
        // Manually construct a VLAN-tagged IPv6/UDP frame with valid checksum
        let payload = b"vlan-cksum";
        let inner_len = IPV6_HEADER_LEN + UDP_HEADER_LEN + payload.len();
        let total = ETH_HEADER_LEN + crate::VLAN_TAG_LEN + inner_len;
        let mut frame = vec![0u8; total];

        frame[0..6].copy_from_slice(&DST_MAC);
        frame[6..12].copy_from_slice(&SRC_MAC);
        frame[12..14].copy_from_slice(&crate::ETH_TYPE_VLAN.to_be_bytes());
        frame[14..16].copy_from_slice(&200u16.to_be_bytes());
        frame[16..18].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

        let l3 = ETH_HEADER_LEN + crate::VLAN_TAG_LEN;
        frame[l3] = 0x60;
        let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
        frame[l3 + 4..l3 + 6].copy_from_slice(&udp_len.to_be_bytes());
        frame[l3 + 6] = IP_PROTO_UDP;
        frame[l3 + 7] = 64;
        frame[l3 + 8..l3 + 24].copy_from_slice(&src_ip().octets());
        frame[l3 + 24..l3 + 40].copy_from_slice(&dst_ip().octets());

        let udp_off = l3 + IPV6_HEADER_LEN;
        frame[udp_off..udp_off + 2].copy_from_slice(&5000u16.to_be_bytes());
        frame[udp_off + 2..udp_off + 4].copy_from_slice(&6000u16.to_be_bytes());
        frame[udp_off + 4..udp_off + 6].copy_from_slice(&udp_len.to_be_bytes());
        frame[udp_off + UDP_HEADER_LEN..].copy_from_slice(payload);

        let cksum = udp6_checksum(
            &src_ip(), &dst_ip(),
            &frame[udp_off..udp_off + UDP_HEADER_LEN],
            payload,
        );
        frame[udp_off + 6..udp_off + 8].copy_from_slice(&cksum.to_be_bytes());

        assert!(verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_detects_corruption_in_vlan_frame() {
        let payload = b"vlan-corrupt";
        let inner_len = IPV6_HEADER_LEN + UDP_HEADER_LEN + payload.len();
        let total = ETH_HEADER_LEN + crate::VLAN_TAG_LEN + inner_len;
        let mut frame = vec![0u8; total];

        frame[0..6].copy_from_slice(&DST_MAC);
        frame[6..12].copy_from_slice(&SRC_MAC);
        frame[12..14].copy_from_slice(&crate::ETH_TYPE_VLAN.to_be_bytes());
        frame[14..16].copy_from_slice(&100u16.to_be_bytes());
        frame[16..18].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

        let l3 = ETH_HEADER_LEN + crate::VLAN_TAG_LEN;
        frame[l3] = 0x60;
        let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
        frame[l3 + 4..l3 + 6].copy_from_slice(&udp_len.to_be_bytes());
        frame[l3 + 6] = IP_PROTO_UDP;
        frame[l3 + 7] = 64;
        frame[l3 + 8..l3 + 24].copy_from_slice(&src_ip().octets());
        frame[l3 + 24..l3 + 40].copy_from_slice(&dst_ip().octets());

        let udp_off = l3 + IPV6_HEADER_LEN;
        frame[udp_off..udp_off + 2].copy_from_slice(&5000u16.to_be_bytes());
        frame[udp_off + 2..udp_off + 4].copy_from_slice(&6000u16.to_be_bytes());
        frame[udp_off + 4..udp_off + 6].copy_from_slice(&udp_len.to_be_bytes());
        frame[udp_off + UDP_HEADER_LEN..].copy_from_slice(payload);

        let cksum = udp6_checksum(
            &src_ip(), &dst_ip(),
            &frame[udp_off..udp_off + UDP_HEADER_LEN],
            payload,
        );
        frame[udp_off + 6..udp_off + 8].copy_from_slice(&cksum.to_be_bytes());

        // Corrupt a payload byte
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(!verify_udp6_checksum(&frame));
    }

    // --- UDP6 checksum: extension headers ---

    #[test]
    fn udp6_checksum_with_hop_by_hop_extension() {
        // Build frame with Hop-by-Hop extension header before UDP
        let payload = b"ext-hdr";
        let ext_len = 8; // minimum extension header size
        let inner_len = IPV6_HEADER_LEN + ext_len + UDP_HEADER_LEN + payload.len();
        let total = ETH_HEADER_LEN + inner_len;
        let mut frame = vec![0u8; total];

        frame[0..6].copy_from_slice(&DST_MAC);
        frame[6..12].copy_from_slice(&SRC_MAC);
        frame[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

        let l3 = ETH_HEADER_LEN;
        frame[l3] = 0x60;
        let ipv6_payload_len = (ext_len + UDP_HEADER_LEN + payload.len()) as u16;
        frame[l3 + 4..l3 + 6].copy_from_slice(&ipv6_payload_len.to_be_bytes());
        frame[l3 + 6] = IP_PROTO_HOPOPT; // Next Header = Hop-by-Hop
        frame[l3 + 7] = 64;
        frame[l3 + 8..l3 + 24].copy_from_slice(&src_ip().octets());
        frame[l3 + 24..l3 + 40].copy_from_slice(&dst_ip().octets());

        // Hop-by-Hop extension header (8 bytes)
        let ext_off = l3 + IPV6_HEADER_LEN;
        frame[ext_off] = IP_PROTO_UDP; // Next Header = UDP
        frame[ext_off + 1] = 0; // Length = (0+1)*8 = 8 bytes

        // UDP header
        let udp_off = ext_off + ext_len;
        let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
        frame[udp_off..udp_off + 2].copy_from_slice(&7000u16.to_be_bytes());
        frame[udp_off + 2..udp_off + 4].copy_from_slice(&8000u16.to_be_bytes());
        frame[udp_off + 4..udp_off + 6].copy_from_slice(&udp_len.to_be_bytes());
        frame[udp_off + UDP_HEADER_LEN..].copy_from_slice(payload);

        // Compute checksum (pseudo-header uses the IPv6 addresses, not extension headers)
        let cksum = udp6_checksum(
            &src_ip(), &dst_ip(),
            &frame[udp_off..udp_off + UDP_HEADER_LEN],
            payload,
        );
        frame[udp_off + 6..udp_off + 8].copy_from_slice(&cksum.to_be_bytes());

        assert!(verify_udp6_checksum(&frame));
    }

    // --- UDP6 checksum: various payload sizes ---

    #[test]
    fn udp6_checksum_empty_payload() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"", 64,
        ).unwrap();
        assert!(verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_single_byte_payload() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"x", 64,
        ).unwrap();
        assert!(verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_odd_length_payload() {
        // Odd-length payload exercises the padding logic in checksum_add
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"odd", 64,
        ).unwrap();
        assert!(verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_max_standard_payload() {
        let payload = vec![0xAB; MAX_UDP_PAYLOAD_V6];
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, &payload, 64,
        ).unwrap();
        assert!(verify_udp6_checksum(&frame));
    }

    // --- UDP6 checksum: corruption in different fields ---

    #[test]
    fn udp6_checksum_detects_src_ip_corruption() {
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"data", 64,
        ).unwrap();
        // Corrupt source IP (byte 8 of IPv6 header)
        frame[ETH_HEADER_LEN + 8] ^= 0x01;
        assert!(!verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_detects_dst_ip_corruption() {
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"data", 64,
        ).unwrap();
        // Corrupt destination IP (byte 24 of IPv6 header)
        frame[ETH_HEADER_LEN + 24] ^= 0x01;
        assert!(!verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_detects_src_port_corruption() {
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1000, 2000, b"data", 64,
        ).unwrap();
        let udp_off = ETH_HEADER_LEN + IPV6_HEADER_LEN;
        frame[udp_off] ^= 0x01; // corrupt src port high byte
        assert!(!verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_detects_dst_port_corruption() {
        let mut frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1000, 2000, b"data", 64,
        ).unwrap();
        let udp_off = ETH_HEADER_LEN + IPV6_HEADER_LEN;
        frame[udp_off + 2] ^= 0x01; // corrupt dst port high byte
        assert!(!verify_udp6_checksum(&frame));
    }

    // --- UDP6 checksum: edge cases ---

    #[test]
    fn udp6_checksum_rejects_truncated_frame() {
        let frame = build_udp6_frame(
            &SRC_MAC, &DST_MAC, src_ip(), dst_ip(), 1, 2, b"data", 64,
        ).unwrap();
        // Truncate to just past the UDP header (missing payload)
        let truncated = &frame[..ETH_HEADER_LEN + IPV6_HEADER_LEN + UDP_HEADER_LEN];
        // The UDP length field says there's payload, but it's missing
        assert!(!verify_udp6_checksum(truncated));
    }

    #[test]
    fn udp6_checksum_rejects_non_ipv6_frame() {
        // Build an IPv4 frame
        let frame = crate::build_udp_frame(
            &SRC_MAC, &DST_MAC,
            "10.0.0.1".parse().unwrap(),
            "10.0.0.2".parse().unwrap(),
            1000, 2000, b"ipv4", 64,
        ).unwrap();
        assert!(!verify_udp6_checksum(&frame));
    }

    #[test]
    fn udp6_checksum_rejects_too_short_frame() {
        // Frame too short to contain even Ethernet + IPv6 header
        let frame = vec![0u8; 20];
        assert!(!verify_udp6_checksum(&frame));
    }

    // --- UDP6 pseudo-header checksum properties ---

    #[test]
    fn udp6_pseudo_header_checksum_different_lengths() {
        let a = udp6_pseudo_header_checksum(&src_ip(), &dst_ip(), 8);
        let b = udp6_pseudo_header_checksum(&src_ip(), &dst_ip(), 100);
        assert_ne!(a, b);
    }

    #[test]
    fn udp6_pseudo_header_checksum_different_src() {
        let other_src: Ipv6Addr = "2001:db8::ff".parse().unwrap();
        let a = udp6_pseudo_header_checksum(&src_ip(), &dst_ip(), 16);
        let b = udp6_pseudo_header_checksum(&other_src, &dst_ip(), 16);
        assert_ne!(a, b);
    }

    #[test]
    fn udp6_pseudo_header_checksum_different_dst() {
        let other_dst: Ipv6Addr = "2001:db8::ff".parse().unwrap();
        let a = udp6_pseudo_header_checksum(&src_ip(), &dst_ip(), 16);
        let b = udp6_pseudo_header_checksum(&src_ip(), &other_dst, 16);
        assert_ne!(a, b);
    }

    #[test]
    fn udp6_pseudo_header_checksum_large_length() {
        // Test with a length > 65535 (uses the 32-bit upper-layer length field)
        let phc = udp6_pseudo_header_checksum(&src_ip(), &dst_ip(), 70000);
        assert_ne!(phc, 0);
    }

    // --- UDP6 checksum: computed value of 0 becomes 0xFFFF ---

    #[test]
    fn udp6_checksum_zero_becomes_ffff() {
        // RFC 8200 §8.1: if the computed checksum is zero, it is transmitted as 0xFFFF
        // We can't easily construct a payload that produces exactly zero, but we can
        // verify the function's contract: the return value is never 0.
        // Test with many different payloads to increase confidence.
        for i in 0..256u16 {
            let payload = [i as u8; 1];
            let cksum = udp6_checksum(
                &src_ip(), &dst_ip(),
                &[0x00, 0x01, 0x00, 0x02, 0x00, 0x09, 0x00, 0x00], // ports 1,2 len 9
                &payload,
            );
            assert_ne!(cksum, 0, "checksum must never be 0 for IPv6");
        }
    }

    // --- Synthetic performance benchmark ---

    #[test]
    fn perf_build_parse_cycle() {
        let payload = vec![0xAA; 64];
        let mut buf = Vec::with_capacity(1500);
        let iterations = 10_000;

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            build_udp6_frame_into(
                &mut buf, &SRC_MAC, &DST_MAC, src_ip(), dst_ip(),
                12345, 9000, &payload, 64,
            )
            .unwrap();
            let _ = parse_udp6_packet_ref(&buf).unwrap();
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        // Just print — no hard assertion, but sanity check it's under 10µs
        eprintln!(
            "[PERF] IPv6 build+parse: {} iterations in {:?} ({} ns/op)",
            iterations, elapsed, ns_per_op
        );
        assert!(ns_per_op < 10_000, "build+parse too slow: {} ns/op", ns_per_op);
    }
}
