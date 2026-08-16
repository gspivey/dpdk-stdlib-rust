//! GUE (Generic UDP Encapsulation) tunnel endpoint.
//!
//! Implements RFC 8470-style L3-over-UDP encapsulation: an inner IPv4 packet
//! is wrapped in an outer UDP/IPv4 frame with a 4-byte GUE header identifying
//! the inner protocol.
//!
//! Wire format (no extensions):
//! ```text
//! [Outer Eth 14B][Outer IPv4 20B][Outer UDP 8B][GUE 4B][Inner IPv4 20B][Inner UDP 8B][Payload]
//! ```
//!
//! The GUE header (4 bytes, no extensions):
//! ```text
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |Ver|C|  Hlen   |  Proto/Ctype  |           Flags               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;

use crate::{
    ipv4_checksum, udp_checksum, ETH_HEADER_LEN, ETH_TYPE_IPV4, IPV4_HEADER_LEN,
    IP_PROTO_UDP, UDP_HEADER_LEN,
};
use crate::ipv6::{ETH_TYPE_IPV6, IPV6_HEADER_LEN, udp6_checksum};

pub const GUE_HEADER_LEN: usize = 4;

pub const GUE_DEFAULT_PORT: u16 = 6080;

pub const GUE_VERSION: u8 = 0;

pub const GUE_PROTO_IPV4: u8 = 4;

pub const GUE_ENCAP_OVERHEAD: usize = IPV4_HEADER_LEN + UDP_HEADER_LEN + GUE_HEADER_LEN;

/// Configuration for a GUE tunnel endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GueConfig {
    /// Remote tunnel endpoint IP address.
    pub remote_ip: Ipv4Addr,
    /// Outer UDP destination port (default: 6080).
    pub remote_port: u16,
    /// Outer UDP source port (default: 6080).
    pub local_port: u16,
}

impl GueConfig {
    pub fn new(remote_ip: Ipv4Addr) -> Self {
        Self {
            remote_ip,
            remote_port: GUE_DEFAULT_PORT,
            local_port: GUE_DEFAULT_PORT,
        }
    }

    pub fn with_remote_port(mut self, port: u16) -> Self {
        self.remote_port = port;
        self
    }

    pub fn with_local_port(mut self, port: u16) -> Self {
        self.local_port = port;
        self
    }
}

/// Parsed GUE header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GueHeader {
    pub version: u8,
    pub c_flag: bool,
    pub hlen: u8,
    pub proto: u8,
    pub flags: u16,
}

impl GueHeader {
    pub fn new_data(proto: u8) -> Self {
        Self {
            version: GUE_VERSION,
            c_flag: false,
            hlen: 0,
            proto,
            flags: 0,
        }
    }

    pub fn total_len(&self) -> usize {
        GUE_HEADER_LEN + (self.hlen as usize) * 4
    }

    pub fn encode(&self, out: &mut [u8]) {
        debug_assert!(out.len() >= GUE_HEADER_LEN);
        out[0] = (self.version << 6)
            | (if self.c_flag { 1 << 5 } else { 0 })
            | (self.hlen & 0x1F);
        out[1] = self.proto;
        out[2..4].copy_from_slice(&self.flags.to_be_bytes());
    }

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < GUE_HEADER_LEN {
            return None;
        }
        let version = (data[0] >> 6) & 0x03;
        let c_flag = (data[0] >> 5) & 0x01 != 0;
        let hlen = data[0] & 0x1F;
        let proto = data[1];
        let flags = u16::from_be_bytes([data[2], data[3]]);

        if version != GUE_VERSION {
            return None;
        }

        let total = GUE_HEADER_LEN + (hlen as usize) * 4;
        if data.len() < total {
            return None;
        }

        Some(Self {
            version,
            c_flag,
            hlen,
            proto,
            flags,
        })
    }
}

/// Build a GUE-encapsulated frame into a caller-provided buffer.
///
/// Produces: `[Outer Eth][Outer IPv4][Outer UDP][GUE][Inner IPv4][Inner UDP][Payload]`
///
/// The outer Ethernet/IPv4 uses `outer_src_mac/outer_dst_mac/outer_src_ip/outer_dst_ip`.
/// The inner IPv4/UDP uses `inner_src_ip/inner_dst_ip/inner_src_port/inner_dst_port`.
///
/// Returns the total frame length written into `out`.
pub fn build_gue_frame_into(
    out: &mut Vec<u8>,
    outer_src_mac: &[u8; 6],
    outer_dst_mac: &[u8; 6],
    outer_src_ip: Ipv4Addr,
    outer_dst_ip: Ipv4Addr,
    outer_src_port: u16,
    outer_dst_port: u16,
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    inner_src_port: u16,
    inner_dst_port: u16,
    payload: &[u8],
    ttl: u8,
) -> Result<usize, crate::UdpError> {
    let inner_udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let inner_ip_total = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let inner_pkt_len = IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();

    let outer_udp_payload = GUE_HEADER_LEN + inner_pkt_len;
    let outer_udp_len = (UDP_HEADER_LEN + outer_udp_payload) as u16;
    let outer_ip_total = (IPV4_HEADER_LEN + UDP_HEADER_LEN + outer_udp_payload) as u16;

    let total_len = ETH_HEADER_LEN + outer_ip_total as usize;
    out.resize(total_len, 0);

    let outer_src_bytes = outer_src_ip.octets();
    let outer_dst_bytes = outer_dst_ip.octets();
    let inner_src_bytes = inner_src_ip.octets();
    let inner_dst_bytes = inner_dst_ip.octets();

    // === Outer Ethernet Header (14 bytes) ===
    out[0..6].copy_from_slice(outer_dst_mac);
    out[6..12].copy_from_slice(outer_src_mac);
    out[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

    // === Outer IPv4 Header (20 bytes) ===
    let oip = ETH_HEADER_LEN;
    out[oip] = 0x45;
    out[oip + 1] = 0x00;
    out[oip + 2..oip + 4].copy_from_slice(&outer_ip_total.to_be_bytes());
    out[oip + 4..oip + 6].copy_from_slice(&[0x00, 0x00]);
    out[oip + 6..oip + 8].copy_from_slice(&[0x40, 0x00]); // DF
    out[oip + 8] = ttl;
    out[oip + 9] = IP_PROTO_UDP;
    out[oip + 10..oip + 12].copy_from_slice(&[0x00, 0x00]);
    out[oip + 12..oip + 16].copy_from_slice(&outer_src_bytes);
    out[oip + 16..oip + 20].copy_from_slice(&outer_dst_bytes);

    let oip_cksum = ipv4_checksum(&out[oip..oip + IPV4_HEADER_LEN]);
    out[oip + 10..oip + 12].copy_from_slice(&oip_cksum.to_be_bytes());

    // === Outer UDP Header (8 bytes) ===
    let oudp = oip + IPV4_HEADER_LEN;
    out[oudp..oudp + 2].copy_from_slice(&outer_src_port.to_be_bytes());
    out[oudp + 2..oudp + 4].copy_from_slice(&outer_dst_port.to_be_bytes());
    out[oudp + 4..oudp + 6].copy_from_slice(&outer_udp_len.to_be_bytes());
    out[oudp + 6..oudp + 8].copy_from_slice(&[0x00, 0x00]); // checksum placeholder

    // === GUE Header (4 bytes) ===
    let gue_off = oudp + UDP_HEADER_LEN;
    let gue_hdr = GueHeader::new_data(GUE_PROTO_IPV4);
    gue_hdr.encode(&mut out[gue_off..gue_off + GUE_HEADER_LEN]);

    // === Inner IPv4 Header (20 bytes) ===
    let iip = gue_off + GUE_HEADER_LEN;
    out[iip] = 0x45;
    out[iip + 1] = 0x00;
    out[iip + 2..iip + 4].copy_from_slice(&inner_ip_total.to_be_bytes());
    out[iip + 4..iip + 6].copy_from_slice(&[0x00, 0x00]);
    out[iip + 6..iip + 8].copy_from_slice(&[0x40, 0x00]); // DF
    out[iip + 8] = 64; // inner TTL
    out[iip + 9] = IP_PROTO_UDP;
    out[iip + 10..iip + 12].copy_from_slice(&[0x00, 0x00]);
    out[iip + 12..iip + 16].copy_from_slice(&inner_src_bytes);
    out[iip + 16..iip + 20].copy_from_slice(&inner_dst_bytes);

    let iip_cksum = ipv4_checksum(&out[iip..iip + IPV4_HEADER_LEN]);
    out[iip + 10..iip + 12].copy_from_slice(&iip_cksum.to_be_bytes());

    // === Inner UDP Header (8 bytes) ===
    let iudp = iip + IPV4_HEADER_LEN;
    out[iudp..iudp + 2].copy_from_slice(&inner_src_port.to_be_bytes());
    out[iudp + 2..iudp + 4].copy_from_slice(&inner_dst_port.to_be_bytes());
    out[iudp + 4..iudp + 6].copy_from_slice(&inner_udp_len.to_be_bytes());
    out[iudp + 6..iudp + 8].copy_from_slice(&[0x00, 0x00]);

    // === Payload ===
    let poff = iudp + UDP_HEADER_LEN;
    out[poff..poff + payload.len()].copy_from_slice(payload);

    // Inner UDP checksum
    let inner_udp_cksum = udp_checksum(
        &inner_src_bytes,
        &inner_dst_bytes,
        &out[iudp..iudp + UDP_HEADER_LEN],
        payload,
    );
    out[iudp + 6..iudp + 8].copy_from_slice(&inner_udp_cksum.to_be_bytes());

    // Outer UDP checksum (over outer UDP header + GUE + inner packet)
    let outer_udp_payload_bytes = &out[oudp + UDP_HEADER_LEN..total_len];
    let outer_udp_cksum = udp_checksum(
        &outer_src_bytes,
        &outer_dst_bytes,
        &out[oudp..oudp + UDP_HEADER_LEN],
        outer_udp_payload_bytes,
    );
    out[oudp + 6..oudp + 8].copy_from_slice(&outer_udp_cksum.to_be_bytes());

    Ok(total_len)
}

/// Result of decapsulating a GUE frame.
#[derive(Debug)]
pub struct GueDecapResult<'a> {
    pub inner_src_ip: Ipv4Addr,
    pub inner_dst_ip: Ipv4Addr,
    pub inner_src_port: u16,
    pub inner_dst_port: u16,
    pub payload: &'a [u8],
    pub outer_src_ip: Ipv4Addr,
    pub gue_header: GueHeader,
}

/// Try to decapsulate a GUE frame from an already-parsed outer frame.
///
/// `frame` is the full Ethernet frame. `l3_offset` is where the outer IPv4
/// header starts (from `detect_vlan`). The caller has already verified the
/// outer ethertype is IPv4.
///
/// Returns `None` if this is not a valid GUE packet (wrong protocol, bad header, etc.).
pub fn try_decap_gue<'a>(
    frame: &'a [u8],
    l3_offset: usize,
    gue_local_port: u16,
) -> Option<GueDecapResult<'a>> {
    // Outer IPv4 header
    if frame.len() < l3_offset + IPV4_HEADER_LEN {
        return None;
    }
    let oip = &frame[l3_offset..];
    if oip[9] != IP_PROTO_UDP {
        return None;
    }
    let outer_src_ip = Ipv4Addr::new(oip[12], oip[13], oip[14], oip[15]);

    let oip_ihl = (oip[0] & 0x0F) as usize * 4;
    if oip_ihl < 20 {
        return None;
    }

    // Outer UDP header
    let oudp_off = l3_offset + oip_ihl;
    if frame.len() < oudp_off + UDP_HEADER_LEN {
        return None;
    }
    let outer_dst_port = u16::from_be_bytes([frame[oudp_off + 2], frame[oudp_off + 3]]);
    if outer_dst_port != gue_local_port {
        return None;
    }

    // GUE header
    let gue_off = oudp_off + UDP_HEADER_LEN;
    let gue_hdr = GueHeader::parse(&frame[gue_off..])?;
    if gue_hdr.c_flag {
        return None; // control messages not supported
    }
    if gue_hdr.proto != GUE_PROTO_IPV4 {
        return None; // only IPv4 inner supported
    }

    // Inner IPv4 header
    let inner_off = gue_off + gue_hdr.total_len();
    if frame.len() < inner_off + IPV4_HEADER_LEN {
        return None;
    }
    let iip = &frame[inner_off..];
    let version = (iip[0] >> 4) & 0x0F;
    if version != 4 {
        return None;
    }
    let iip_ihl = (iip[0] & 0x0F) as usize * 4;
    if iip_ihl < 20 || frame.len() < inner_off + iip_ihl {
        return None;
    }
    if iip[9] != IP_PROTO_UDP {
        return None;
    }

    let inner_src_ip = Ipv4Addr::new(iip[12], iip[13], iip[14], iip[15]);
    let inner_dst_ip = Ipv4Addr::new(iip[16], iip[17], iip[18], iip[19]);

    // Inner UDP header
    let iudp_off = inner_off + iip_ihl;
    if frame.len() < iudp_off + UDP_HEADER_LEN {
        return None;
    }
    let inner_src_port = u16::from_be_bytes([frame[iudp_off], frame[iudp_off + 1]]);
    let inner_dst_port = u16::from_be_bytes([frame[iudp_off + 2], frame[iudp_off + 3]]);
    let inner_udp_len = u16::from_be_bytes([frame[iudp_off + 4], frame[iudp_off + 5]]) as usize;

    if inner_udp_len < UDP_HEADER_LEN || frame.len() < iudp_off + inner_udp_len {
        return None;
    }

    let payload_start = iudp_off + UDP_HEADER_LEN;
    let payload_len = inner_udp_len - UDP_HEADER_LEN;

    Some(GueDecapResult {
        inner_src_ip,
        inner_dst_ip,
        inner_src_port,
        inner_dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        outer_src_ip,
        gue_header: gue_hdr,
    })
}

// ============================================================================
// IPv6 Outer Support
// ============================================================================

/// Encapsulation overhead for GUE with IPv6 outer: IPv6(40) + UDP(8) + GUE(4) = 52.
pub const GUE_ENCAP_OVERHEAD_V6: usize = IPV6_HEADER_LEN + UDP_HEADER_LEN + GUE_HEADER_LEN;

/// Configuration for a GUE tunnel endpoint with IPv6 outer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GueConfig6 {
    /// Remote tunnel endpoint IPv6 address.
    pub remote_ip: Ipv6Addr,
    /// Outer UDP destination port (default: 6080).
    pub remote_port: u16,
    /// Outer UDP source port (default: 6080).
    pub local_port: u16,
}

impl GueConfig6 {
    pub fn new(remote_ip: Ipv6Addr) -> Self {
        Self {
            remote_ip,
            remote_port: GUE_DEFAULT_PORT,
            local_port: GUE_DEFAULT_PORT,
        }
    }

    pub fn with_remote_port(mut self, port: u16) -> Self {
        self.remote_port = port;
        self
    }

    pub fn with_local_port(mut self, port: u16) -> Self {
        self.local_port = port;
        self
    }
}

/// Build a GUE-encapsulated frame with IPv6 outer into a caller-provided buffer.
///
/// Produces: `[Outer Eth][Outer IPv6][Outer UDP][GUE][Inner IPv4][Inner UDP][Payload]`
#[allow(clippy::too_many_arguments)]
pub fn build_gue_frame_into_v6(
    out: &mut Vec<u8>,
    outer_src_mac: &[u8; 6],
    outer_dst_mac: &[u8; 6],
    outer_src_ip: Ipv6Addr,
    outer_dst_ip: Ipv6Addr,
    outer_src_port: u16,
    outer_dst_port: u16,
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    inner_src_port: u16,
    inner_dst_port: u16,
    payload: &[u8],
    hop_limit: u8,
) -> Result<usize, crate::UdpError> {
    let inner_udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let inner_ip_total = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let inner_pkt_len = IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();

    let outer_udp_payload = GUE_HEADER_LEN + inner_pkt_len;
    let outer_udp_len = (UDP_HEADER_LEN + outer_udp_payload) as u16;
    let ipv6_payload_len = outer_udp_len;

    let total_len = ETH_HEADER_LEN + IPV6_HEADER_LEN + outer_udp_len as usize;
    out.resize(total_len, 0);

    let inner_src_bytes = inner_src_ip.octets();
    let inner_dst_bytes = inner_dst_ip.octets();

    // === Outer Ethernet Header (14 bytes) ===
    out[0..6].copy_from_slice(outer_dst_mac);
    out[6..12].copy_from_slice(outer_src_mac);
    out[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

    // === Outer IPv6 Header (40 bytes) ===
    let oip = ETH_HEADER_LEN;
    out[oip] = 0x60; // version 6, traffic class 0
    out[oip + 1] = 0x00;
    out[oip + 2] = 0x00;
    out[oip + 3] = 0x00; // flow label 0
    out[oip + 4..oip + 6].copy_from_slice(&ipv6_payload_len.to_be_bytes());
    out[oip + 6] = IP_PROTO_UDP; // next header
    out[oip + 7] = hop_limit;
    out[oip + 8..oip + 24].copy_from_slice(&outer_src_ip.octets());
    out[oip + 24..oip + 40].copy_from_slice(&outer_dst_ip.octets());

    // === Outer UDP Header (8 bytes) ===
    let oudp = oip + IPV6_HEADER_LEN;
    out[oudp..oudp + 2].copy_from_slice(&outer_src_port.to_be_bytes());
    out[oudp + 2..oudp + 4].copy_from_slice(&outer_dst_port.to_be_bytes());
    out[oudp + 4..oudp + 6].copy_from_slice(&outer_udp_len.to_be_bytes());
    out[oudp + 6..oudp + 8].copy_from_slice(&[0x00, 0x00]); // checksum placeholder

    // === GUE Header (4 bytes) ===
    let gue_off = oudp + UDP_HEADER_LEN;
    let gue_hdr = GueHeader::new_data(GUE_PROTO_IPV4);
    gue_hdr.encode(&mut out[gue_off..gue_off + GUE_HEADER_LEN]);

    // === Inner IPv4 Header (20 bytes) ===
    let iip = gue_off + GUE_HEADER_LEN;
    out[iip] = 0x45;
    out[iip + 1] = 0x00;
    out[iip + 2..iip + 4].copy_from_slice(&inner_ip_total.to_be_bytes());
    out[iip + 4..iip + 6].copy_from_slice(&[0x00, 0x00]);
    out[iip + 6..iip + 8].copy_from_slice(&[0x40, 0x00]); // DF
    out[iip + 8] = 64; // inner TTL
    out[iip + 9] = IP_PROTO_UDP;
    out[iip + 10..iip + 12].copy_from_slice(&[0x00, 0x00]);
    out[iip + 12..iip + 16].copy_from_slice(&inner_src_bytes);
    out[iip + 16..iip + 20].copy_from_slice(&inner_dst_bytes);
    let iip_cksum = ipv4_checksum(&out[iip..iip + IPV4_HEADER_LEN]);
    out[iip + 10..iip + 12].copy_from_slice(&iip_cksum.to_be_bytes());

    // === Inner UDP Header (8 bytes) ===
    let iudp = iip + IPV4_HEADER_LEN;
    out[iudp..iudp + 2].copy_from_slice(&inner_src_port.to_be_bytes());
    out[iudp + 2..iudp + 4].copy_from_slice(&inner_dst_port.to_be_bytes());
    out[iudp + 4..iudp + 6].copy_from_slice(&inner_udp_len.to_be_bytes());
    out[iudp + 6..iudp + 8].copy_from_slice(&[0x00, 0x00]);

    // === Payload ===
    let poff = iudp + UDP_HEADER_LEN;
    out[poff..poff + payload.len()].copy_from_slice(payload);

    // Inner UDP checksum (IPv4)
    let inner_udp_cksum = udp_checksum(
        &inner_src_bytes,
        &inner_dst_bytes,
        &out[iudp..iudp + UDP_HEADER_LEN],
        payload,
    );
    out[iudp + 6..iudp + 8].copy_from_slice(&inner_udp_cksum.to_be_bytes());

    // Outer UDP checksum (IPv6 — mandatory)
    let outer_udp_cksum = udp6_checksum(
        &outer_src_ip,
        &outer_dst_ip,
        &out[oudp..oudp + UDP_HEADER_LEN],
        &out[oudp + UDP_HEADER_LEN..total_len],
    );
    out[oudp + 6..oudp + 8].copy_from_slice(&outer_udp_cksum.to_be_bytes());

    Ok(total_len)
}

/// Result of decapsulating a GUE frame with IPv6 outer.
#[derive(Debug)]
pub struct GueDecapResult6<'a> {
    pub inner_src_ip: Ipv4Addr,
    pub inner_dst_ip: Ipv4Addr,
    pub inner_src_port: u16,
    pub inner_dst_port: u16,
    pub payload: &'a [u8],
    pub outer_src_ip: Ipv6Addr,
    pub gue_header: GueHeader,
}

/// Try to decapsulate a GUE frame with IPv6 outer.
///
/// `frame` is the full Ethernet frame. `l3_offset` is where the outer IPv6
/// header starts. The caller has already verified the outer ethertype is IPv6.
pub fn try_decap_gue_v6<'a>(
    frame: &'a [u8],
    l3_offset: usize,
    gue_local_port: u16,
) -> Option<GueDecapResult6<'a>> {
    if frame.len() < l3_offset + IPV6_HEADER_LEN {
        return None;
    }
    let oip = &frame[l3_offset..];
    // Verify IPv6 version
    if (oip[0] >> 4) != 6 {
        return None;
    }
    // Next header must be UDP
    if oip[6] != IP_PROTO_UDP {
        return None;
    }
    let outer_src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&oip[8..24]).unwrap());

    // Outer UDP header
    let oudp_off = l3_offset + IPV6_HEADER_LEN;
    if frame.len() < oudp_off + UDP_HEADER_LEN {
        return None;
    }
    let outer_dst_port = u16::from_be_bytes([frame[oudp_off + 2], frame[oudp_off + 3]]);
    if outer_dst_port != gue_local_port {
        return None;
    }

    // GUE header
    let gue_off = oudp_off + UDP_HEADER_LEN;
    let gue_hdr = GueHeader::parse(&frame[gue_off..])?;
    if gue_hdr.c_flag {
        return None;
    }
    if gue_hdr.proto != GUE_PROTO_IPV4 {
        return None;
    }

    // Inner IPv4 header
    let inner_off = gue_off + gue_hdr.total_len();
    if frame.len() < inner_off + IPV4_HEADER_LEN {
        return None;
    }
    let iip = &frame[inner_off..];
    if (iip[0] >> 4) != 4 {
        return None;
    }
    let iip_ihl = (iip[0] & 0x0F) as usize * 4;
    if iip_ihl < 20 || frame.len() < inner_off + iip_ihl {
        return None;
    }
    if iip[9] != IP_PROTO_UDP {
        return None;
    }
    let inner_src_ip = Ipv4Addr::new(iip[12], iip[13], iip[14], iip[15]);
    let inner_dst_ip = Ipv4Addr::new(iip[16], iip[17], iip[18], iip[19]);

    // Inner UDP header
    let iudp_off = inner_off + iip_ihl;
    if frame.len() < iudp_off + UDP_HEADER_LEN {
        return None;
    }
    let inner_src_port = u16::from_be_bytes([frame[iudp_off], frame[iudp_off + 1]]);
    let inner_dst_port = u16::from_be_bytes([frame[iudp_off + 2], frame[iudp_off + 3]]);
    let inner_udp_len = u16::from_be_bytes([frame[iudp_off + 4], frame[iudp_off + 5]]) as usize;

    if inner_udp_len < UDP_HEADER_LEN || frame.len() < iudp_off + inner_udp_len {
        return None;
    }

    let payload_start = iudp_off + UDP_HEADER_LEN;
    let payload_len = inner_udp_len - UDP_HEADER_LEN;

    Some(GueDecapResult6 {
        inner_src_ip,
        inner_dst_ip,
        inner_src_port,
        inner_dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        outer_src_ip,
        gue_header: gue_hdr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gue_header_encode_decode_roundtrip() {
        let hdr = GueHeader::new_data(GUE_PROTO_IPV4);
        let mut buf = [0u8; 4];
        hdr.encode(&mut buf);

        assert_eq!(buf[0], 0x00); // version=0, C=0, hlen=0
        assert_eq!(buf[1], 0x04); // proto=4 (IPv4)
        assert_eq!(buf[2], 0x00);
        assert_eq!(buf[3], 0x00);

        let parsed = GueHeader::parse(&buf).unwrap();
        assert_eq!(parsed.version, 0);
        assert!(!parsed.c_flag);
        assert_eq!(parsed.hlen, 0);
        assert_eq!(parsed.proto, GUE_PROTO_IPV4);
        assert_eq!(parsed.flags, 0);
    }

    #[test]
    fn gue_header_rejects_wrong_version() {
        let buf = [0x40, 0x04, 0x00, 0x00]; // version=1
        assert!(GueHeader::parse(&buf).is_none());
    }

    #[test]
    fn gue_header_rejects_too_short() {
        let buf = [0x00, 0x04, 0x00]; // only 3 bytes
        assert!(GueHeader::parse(&buf).is_none());
    }

    #[test]
    fn gue_header_with_extensions() {
        // hlen=2 means 2*4=8 bytes of extensions after the base header
        let mut buf = vec![0u8; 12];
        buf[0] = 0x02; // version=0, C=0, hlen=2
        buf[1] = 0x04;
        buf[2] = 0x00;
        buf[3] = 0x00;
        // 8 bytes of extension data
        let parsed = GueHeader::parse(&buf).unwrap();
        assert_eq!(parsed.hlen, 2);
        assert_eq!(parsed.total_len(), 12);
    }

    #[test]
    fn gue_header_extensions_too_short() {
        // hlen=2 requires 12 bytes total, but only 8 available
        let buf = [0x02, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(GueHeader::parse(&buf).is_none());
    }

    #[test]
    fn build_gue_frame_roundtrip() {
        let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let dst_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let outer_src = Ipv4Addr::new(10, 0, 0, 1);
        let outer_dst = Ipv4Addr::new(10, 0, 0, 2);
        let inner_src = Ipv4Addr::new(192, 168, 1, 10);
        let inner_dst = Ipv4Addr::new(192, 168, 1, 20);
        let payload = b"hello GUE tunnel";

        let mut frame = Vec::new();
        let len = build_gue_frame_into(
            &mut frame,
            &src_mac, &dst_mac,
            outer_src, outer_dst,
            6080, 6080,
            inner_src, inner_dst,
            9000, 9001,
            payload, 64,
        ).unwrap();

        assert_eq!(frame.len(), len);
        // Expected: 14 (eth) + 20 (outer ip) + 8 (outer udp) + 4 (gue) + 20 (inner ip) + 8 (inner udp) + 16 (payload) = 90
        assert_eq!(len, 14 + 20 + 8 + 4 + 20 + 8 + payload.len());

        // Verify outer Ethernet
        assert_eq!(&frame[0..6], &dst_mac);
        assert_eq!(&frame[6..12], &src_mac);
        assert_eq!(u16::from_be_bytes([frame[12], frame[13]]), ETH_TYPE_IPV4);

        // Verify outer IPv4
        assert_eq!(frame[14] >> 4, 4); // version
        assert_eq!(frame[23], IP_PROTO_UDP); // protocol

        // Verify outer UDP port
        assert_eq!(u16::from_be_bytes([frame[34], frame[35]]), 6080);
        assert_eq!(u16::from_be_bytes([frame[36], frame[37]]), 6080);

        // Verify GUE header
        assert_eq!(frame[42], 0x00); // version=0, C=0, hlen=0
        assert_eq!(frame[43], 0x04); // proto=IPv4

        // Decapsulate and verify inner payload
        let decap = try_decap_gue(&frame, ETH_HEADER_LEN, 6080).unwrap();
        assert_eq!(decap.inner_src_ip, inner_src);
        assert_eq!(decap.inner_dst_ip, inner_dst);
        assert_eq!(decap.inner_src_port, 9000);
        assert_eq!(decap.inner_dst_port, 9001);
        assert_eq!(decap.payload, payload);
        assert_eq!(decap.outer_src_ip, outer_src);
    }

    #[test]
    fn decap_rejects_non_gue_port() {
        let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let dst_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let mut frame = Vec::new();
        build_gue_frame_into(
            &mut frame,
            &src_mac, &dst_mac,
            Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2),
            6080, 6080,
            Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(192, 168, 1, 20),
            9000, 9001,
            b"test", 64,
        ).unwrap();

        // Wrong port => None
        assert!(try_decap_gue(&frame, ETH_HEADER_LEN, 7000).is_none());
    }

    #[test]
    fn decap_rejects_control_message() {
        let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let dst_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let mut frame = Vec::new();
        build_gue_frame_into(
            &mut frame,
            &src_mac, &dst_mac,
            Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2),
            6080, 6080,
            Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(192, 168, 1, 20),
            9000, 9001,
            b"test", 64,
        ).unwrap();

        // Set C flag in GUE header
        frame[42] = 0x20; // C=1
        assert!(try_decap_gue(&frame, ETH_HEADER_LEN, 6080).is_none());
    }

    #[test]
    fn build_gue_frame_verifies_checksums() {
        let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let dst_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let mut frame = Vec::new();
        build_gue_frame_into(
            &mut frame,
            &src_mac, &dst_mac,
            Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2),
            6080, 6080,
            Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(192, 168, 1, 20),
            9000, 9001,
            b"checksum test payload", 64,
        ).unwrap();

        // Verify outer IPv4 checksum
        assert!(crate::verify_ipv4_checksum(&frame));
    }

    #[test]
    fn gue_config_defaults() {
        let cfg = GueConfig::new(Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(cfg.remote_ip, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(cfg.remote_port, GUE_DEFAULT_PORT);
        assert_eq!(cfg.local_port, GUE_DEFAULT_PORT);
    }

    #[test]
    fn gue_config_builder() {
        let cfg = GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))
            .with_remote_port(7000)
            .with_local_port(7001);
        assert_eq!(cfg.remote_port, 7000);
        assert_eq!(cfg.local_port, 7001);
    }

    #[test]
    fn gue_encap_overhead_is_correct() {
        assert_eq!(GUE_ENCAP_OVERHEAD, 20 + 8 + 4); // outer IP + outer UDP + GUE = 32
    }

    #[test]
    fn build_gue_frame_empty_payload() {
        let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let dst_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let mut frame = Vec::new();
        let len = build_gue_frame_into(
            &mut frame,
            &src_mac, &dst_mac,
            Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2),
            6080, 6080,
            Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(192, 168, 1, 20),
            9000, 9001,
            &[], 64,
        ).unwrap();

        // 14 + 20 + 8 + 4 + 20 + 8 + 0 = 74
        assert_eq!(len, 74);

        let decap = try_decap_gue(&frame, ETH_HEADER_LEN, 6080).unwrap();
        assert!(decap.payload.is_empty());
    }

    #[test]
    fn build_gue_frame_large_payload() {
        let src_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let dst_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let payload = vec![0xAB; 1400];
        let mut frame = Vec::new();
        let len = build_gue_frame_into(
            &mut frame,
            &src_mac, &dst_mac,
            Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2),
            6080, 6080,
            Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(192, 168, 1, 20),
            9000, 9001,
            &payload, 64,
        ).unwrap();

        assert_eq!(len, 74 + 1400);

        let decap = try_decap_gue(&frame, ETH_HEADER_LEN, 6080).unwrap();
        assert_eq!(decap.payload.len(), 1400);
        assert!(decap.payload.iter().all(|&b| b == 0xAB));
    }

    // =========================================================================
    // IPv6 outer tests
    // =========================================================================

    mod ipv6_outer {
        use super::*;
        use std::net::Ipv6Addr;
        use crate::ipv6::{ETH_TYPE_IPV6, IPV6_HEADER_LEN};

        const SRC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        const DST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

        fn outer_src() -> Ipv6Addr { "2001:db8::1".parse().unwrap() }
        fn outer_dst() -> Ipv6Addr { "2001:db8::2".parse().unwrap() }
        fn inner_src() -> Ipv4Addr { Ipv4Addr::new(192, 168, 1, 10) }
        fn inner_dst() -> Ipv4Addr { Ipv4Addr::new(192, 168, 1, 20) }

        #[test]
        fn config6_defaults() {
            let cfg = GueConfig6::new(outer_dst());
            assert_eq!(cfg.remote_ip, outer_dst());
            assert_eq!(cfg.remote_port, GUE_DEFAULT_PORT);
            assert_eq!(cfg.local_port, GUE_DEFAULT_PORT);
        }

        #[test]
        fn config6_builder() {
            let cfg = GueConfig6::new(outer_dst())
                .with_remote_port(7000)
                .with_local_port(7001);
            assert_eq!(cfg.remote_port, 7000);
            assert_eq!(cfg.local_port, 7001);
        }

        #[test]
        fn build_and_decap_roundtrip() {
            let payload = b"hello GUE IPv6 tunnel";
            let mut frame = Vec::new();
            let len = build_gue_frame_into_v6(
                &mut frame,
                &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(),
                6080, 6080,
                inner_src(), inner_dst(),
                9000, 9001,
                payload, 64,
            ).unwrap();

            assert_eq!(frame.len(), len);
            // 14 (eth) + 40 (outer IPv6) + 8 (outer UDP) + 4 (GUE) + 20 (inner IPv4) + 8 (inner UDP) + payload
            let expected = 14 + 40 + 8 + 4 + 20 + 8 + payload.len();
            assert_eq!(len, expected);

            let decap = try_decap_gue_v6(&frame, ETH_HEADER_LEN, 6080).unwrap();
            assert_eq!(decap.inner_src_ip, inner_src());
            assert_eq!(decap.inner_dst_ip, inner_dst());
            assert_eq!(decap.inner_src_port, 9000);
            assert_eq!(decap.inner_dst_port, 9001);
            assert_eq!(decap.payload, payload);
            assert_eq!(decap.outer_src_ip, outer_src());
        }

        #[test]
        fn wire_format_ethertype_is_ipv6() {
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 1, 2, b"x", 64,
            ).unwrap();
            let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
            assert_eq!(ethertype, ETH_TYPE_IPV6);
        }

        #[test]
        fn wire_format_ipv6_version() {
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 1, 2, b"x", 64,
            ).unwrap();
            assert_eq!(frame[ETH_HEADER_LEN] >> 4, 6);
        }

        #[test]
        fn wire_format_hop_limit() {
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 1, 2, b"x", 42,
            ).unwrap();
            assert_eq!(frame[ETH_HEADER_LEN + 7], 42);
        }

        #[test]
        fn wire_format_next_header_is_udp() {
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 1, 2, b"x", 64,
            ).unwrap();
            assert_eq!(frame[ETH_HEADER_LEN + 6], crate::IP_PROTO_UDP);
        }

        #[test]
        fn wire_format_outer_udp_checksum_valid() {
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 9000, 9001,
                b"checksum test", 64,
            ).unwrap();
            assert!(crate::verify_udp6_checksum(&frame));
        }

        #[test]
        fn decap_rejects_wrong_port() {
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            assert!(try_decap_gue_v6(&frame, ETH_HEADER_LEN, 7000).is_none());
        }

        #[test]
        fn decap_rejects_control_message() {
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            // GUE header is at: ETH(14) + IPv6(40) + UDP(8) = offset 62
            frame[62] = 0x20; // Set C flag
            assert!(try_decap_gue_v6(&frame, ETH_HEADER_LEN, 6080).is_none());
        }

        #[test]
        fn build_empty_payload() {
            let mut frame = Vec::new();
            let len = build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 9000, 9001, &[], 64,
            ).unwrap();
            // 14 + 40 + 8 + 4 + 20 + 8 = 94
            assert_eq!(len, 94);
            let decap = try_decap_gue_v6(&frame, ETH_HEADER_LEN, 6080).unwrap();
            assert!(decap.payload.is_empty());
        }

        #[test]
        fn build_large_payload() {
            let payload = vec![0xAB; 1400];
            let mut frame = Vec::new();
            build_gue_frame_into_v6(
                &mut frame, &SRC_MAC, &DST_MAC,
                outer_src(), outer_dst(), 6080, 6080,
                inner_src(), inner_dst(), 9000, 9001, &payload, 64,
            ).unwrap();
            let decap = try_decap_gue_v6(&frame, ETH_HEADER_LEN, 6080).unwrap();
            assert_eq!(decap.payload.len(), 1400);
            assert!(decap.payload.iter().all(|&b| b == 0xAB));
        }

        #[test]
        fn encap_overhead_v6_is_correct() {
            // outer IPv6(40) + outer UDP(8) + GUE(4) = 52
            assert_eq!(GUE_ENCAP_OVERHEAD_V6, 52);
        }

        #[test]
        fn perf_build_decap_cycle_v6() {
            let payload = vec![0xAA; 64];
            let mut buf = Vec::with_capacity(1500);
            let iterations = 10_000;

            let start = std::time::Instant::now();
            for _ in 0..iterations {
                build_gue_frame_into_v6(
                    &mut buf, &SRC_MAC, &DST_MAC,
                    outer_src(), outer_dst(), 6080, 6080,
                    inner_src(), inner_dst(), 9000, 9001, &payload, 64,
                ).unwrap();
                let _ = try_decap_gue_v6(&buf, ETH_HEADER_LEN, 6080).unwrap();
            }
            let elapsed = start.elapsed();
            let ns_per_op = elapsed.as_nanos() / iterations as u128;
            eprintln!(
                "[PERF] GUE IPv6-outer build+decap: {} iterations in {:?} ({} ns/op)",
                iterations, elapsed, ns_per_op
            );
            assert!(ns_per_op < 10_000, "build+decap too slow: {} ns/op", ns_per_op);
        }
    }
}
