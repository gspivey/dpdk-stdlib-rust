//! VXLAN (RFC 7348) tunnel endpoint.
//!
//! Implements a high-performance VXLAN tunnel endpoint: an inner Ethernet frame
//! is wrapped in an outer UDP/IPv4 frame with an 8-byte VXLAN header carrying
//! a 24-bit VNI (Virtual Network Identifier).
//!
//! Wire format:
//! ```text
//! [Outer Eth 14B][Outer IPv4 20B][Outer UDP 8B][VXLAN 8B][Inner Eth 14B][Inner IPv4 20B][Inner UDP 8B][Payload]
//! ```
//!
//! The VXLAN header (8 bytes):
//! ```text
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |R|R|R|R|I|R|R|R|            Reserved                           |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                VXLAN Network Identifier (VNI) |   Reserved    |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;

use crate::{
    ipv4_checksum, udp_checksum, ETH_HEADER_LEN, ETH_TYPE_IPV4, IPV4_HEADER_LEN,
    IP_PROTO_UDP, UDP_HEADER_LEN,
};
use crate::ipv6::{ETH_TYPE_IPV6, IPV6_HEADER_LEN, udp6_checksum};

/// VXLAN header size (always 8 bytes).
pub const VXLAN_HEADER_LEN: usize = 8;

/// IANA-assigned VXLAN UDP destination port.
pub const VXLAN_DEFAULT_PORT: u16 = 4789;

/// Total encapsulation overhead added by VXLAN (outer IPv4 + outer UDP + VXLAN header + inner Ethernet).
/// Does NOT include the outer Ethernet header (that's always present).
pub const VXLAN_ENCAP_OVERHEAD: usize =
    IPV4_HEADER_LEN + UDP_HEADER_LEN + VXLAN_HEADER_LEN + ETH_HEADER_LEN;

/// VXLAN flags byte with the I (VNI valid) bit set (bit 3 = 0x08).
const VXLAN_FLAGS_I: u8 = 0x08;

/// Maximum valid VNI value (24-bit: 0x00FFFFFF).
pub const VXLAN_VNI_MAX: u32 = 0x00FF_FFFF;

/// Configuration for a VXLAN tunnel endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VxlanConfig {
    /// Remote tunnel endpoint IP address (VTEP peer).
    pub remote_ip: Ipv4Addr,
    /// VXLAN Network Identifier (24-bit).
    pub vni: u32,
    /// Outer UDP destination port (default: 4789).
    pub remote_port: u16,
    /// Outer UDP source port (default: 4789).
    pub local_port: u16,
    /// Inner source MAC address for encapsulated frames.
    pub inner_src_mac: [u8; 6],
    /// Inner destination MAC address for encapsulated frames.
    pub inner_dst_mac: [u8; 6],
}

impl VxlanConfig {
    /// Create a new VXLAN config with the given remote VTEP IP and VNI.
    ///
    /// # Panics
    /// Panics if `vni` exceeds 24 bits (> 16,777,215).
    pub fn new(remote_ip: Ipv4Addr, vni: u32) -> Self {
        assert!(vni <= VXLAN_VNI_MAX, "VNI must be 24-bit (max {})", VXLAN_VNI_MAX);
        Self {
            remote_ip,
            vni,
            remote_port: VXLAN_DEFAULT_PORT,
            local_port: VXLAN_DEFAULT_PORT,
            inner_src_mac: [0; 6],
            inner_dst_mac: [0xFF; 6], // broadcast by default
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

    pub fn with_inner_src_mac(mut self, mac: [u8; 6]) -> Self {
        self.inner_src_mac = mac;
        self
    }

    pub fn with_inner_dst_mac(mut self, mac: [u8; 6]) -> Self {
        self.inner_dst_mac = mac;
        self
    }
}

// ============================================================================
// VXLAN Header
// ============================================================================

/// Parsed VXLAN header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VxlanHeader {
    /// The I flag (bit 3 of flags byte). Must be 1 for a valid VNI.
    pub i_flag: bool,
    /// 24-bit VXLAN Network Identifier.
    pub vni: u32,
}

impl VxlanHeader {
    /// Create a new VXLAN header with the I flag set and the given VNI.
    pub fn new(vni: u32) -> Self {
        debug_assert!(vni <= VXLAN_VNI_MAX);
        Self { i_flag: true, vni }
    }

    /// Encode the 8-byte VXLAN header into `out`.
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert!(out.len() >= VXLAN_HEADER_LEN);
        out[0] = if self.i_flag { VXLAN_FLAGS_I } else { 0 };
        out[1] = 0; // reserved
        out[2] = 0; // reserved
        out[3] = 0; // reserved
        out[4] = ((self.vni >> 16) & 0xFF) as u8;
        out[5] = ((self.vni >> 8) & 0xFF) as u8;
        out[6] = (self.vni & 0xFF) as u8;
        out[7] = 0; // reserved
    }

    /// Parse an 8-byte VXLAN header from `data`.
    ///
    /// Returns `None` if the data is too short or the I flag is not set.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < VXLAN_HEADER_LEN {
            return None;
        }
        let i_flag = (data[0] & VXLAN_FLAGS_I) != 0;
        if !i_flag {
            return None;
        }
        let vni = ((data[4] as u32) << 16) | ((data[5] as u32) << 8) | (data[6] as u32);
        Some(Self { i_flag, vni })
    }
}

// ============================================================================
// Frame Building
// ============================================================================

/// Build a VXLAN-encapsulated frame into a caller-provided buffer.
///
/// Produces:
/// `[Outer Eth][Outer IPv4][Outer UDP][VXLAN][Inner Eth][Inner IPv4][Inner UDP][Payload]`
///
/// Returns the total frame length written into `out`.
#[allow(clippy::too_many_arguments)]
pub fn build_vxlan_frame_into(
    out: &mut Vec<u8>,
    outer_src_mac: &[u8; 6],
    outer_dst_mac: &[u8; 6],
    outer_src_ip: Ipv4Addr,
    outer_dst_ip: Ipv4Addr,
    outer_src_port: u16,
    outer_dst_port: u16,
    vni: u32,
    inner_src_mac: &[u8; 6],
    inner_dst_mac: &[u8; 6],
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    inner_src_port: u16,
    inner_dst_port: u16,
    payload: &[u8],
    ttl: u8,
) -> Result<usize, crate::UdpError> {
    // Inner frame: Eth(14) + IPv4(20) + UDP(8) + payload
    let inner_udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let inner_ip_total = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let inner_frame_len = ETH_HEADER_LEN + inner_ip_total as usize;

    // Outer: Eth(14) + IPv4(20) + UDP(8) + VXLAN(8) + inner_frame
    let outer_udp_payload_len = VXLAN_HEADER_LEN + inner_frame_len;
    let outer_udp_len = (UDP_HEADER_LEN + outer_udp_payload_len) as u16;
    let outer_ip_total = (IPV4_HEADER_LEN + outer_udp_len as usize) as u16;
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
    out[oip + 4..oip + 6].copy_from_slice(&[0x00, 0x00]); // identification
    out[oip + 6..oip + 8].copy_from_slice(&[0x40, 0x00]); // DF
    out[oip + 8] = ttl;
    out[oip + 9] = IP_PROTO_UDP;
    out[oip + 10..oip + 12].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
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

    // === VXLAN Header (8 bytes) ===
    let vxlan_off = oudp + UDP_HEADER_LEN;
    VxlanHeader::new(vni).encode(&mut out[vxlan_off..vxlan_off + VXLAN_HEADER_LEN]);

    // === Inner Ethernet Header (14 bytes) ===
    let ieth = vxlan_off + VXLAN_HEADER_LEN;
    out[ieth..ieth + 6].copy_from_slice(inner_dst_mac);
    out[ieth + 6..ieth + 12].copy_from_slice(inner_src_mac);
    out[ieth + 12..ieth + 14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

    // === Inner IPv4 Header (20 bytes) ===
    let iip = ieth + ETH_HEADER_LEN;
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

    // Outer UDP checksum (over outer UDP header + VXLAN + inner frame)
    let outer_payload = &out[oudp + UDP_HEADER_LEN..total_len];
    let outer_udp_cksum = udp_checksum(
        &outer_src_bytes,
        &outer_dst_bytes,
        &out[oudp..oudp + UDP_HEADER_LEN],
        outer_payload,
    );
    out[oudp + 6..oudp + 8].copy_from_slice(&outer_udp_cksum.to_be_bytes());

    Ok(total_len)
}

// ============================================================================
// Decapsulation
// ============================================================================

/// Result of decapsulating a VXLAN frame.
#[derive(Debug)]
pub struct VxlanDecapResult<'a> {
    /// Inner source IP from the encapsulated packet.
    pub inner_src_ip: Ipv4Addr,
    /// Inner destination IP from the encapsulated packet.
    pub inner_dst_ip: Ipv4Addr,
    /// Inner source port.
    pub inner_src_port: u16,
    /// Inner destination port.
    pub inner_dst_port: u16,
    /// Inner payload (application data).
    pub payload: &'a [u8],
    /// Outer source IP (the remote VTEP).
    pub outer_src_ip: Ipv4Addr,
    /// The parsed VXLAN header (contains VNI).
    pub vxlan_header: VxlanHeader,
    /// Inner source MAC address.
    pub inner_src_mac: [u8; 6],
    /// Inner destination MAC address.
    pub inner_dst_mac: [u8; 6],
}

/// Try to decapsulate a VXLAN frame.
///
/// `frame` is the full Ethernet frame. `l3_offset` is where the outer IPv4
/// header starts. `vxlan_local_port` is the expected outer UDP destination port.
/// `expected_vni` filters by VNI — pass `None` to accept any VNI.
///
/// Returns `None` if this is not a valid VXLAN packet.
pub fn try_decap_vxlan<'a>(
    frame: &'a [u8],
    l3_offset: usize,
    vxlan_local_port: u16,
    expected_vni: Option<u32>,
) -> Option<VxlanDecapResult<'a>> {
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
    if outer_dst_port != vxlan_local_port {
        return None;
    }

    // VXLAN header
    let vxlan_off = oudp_off + UDP_HEADER_LEN;
    let vxlan_hdr = VxlanHeader::parse(&frame[vxlan_off..])?;

    // VNI filtering
    if let Some(expected) = expected_vni {
        if vxlan_hdr.vni != expected {
            return None;
        }
    }

    // Inner Ethernet header
    let ieth = vxlan_off + VXLAN_HEADER_LEN;
    if frame.len() < ieth + ETH_HEADER_LEN {
        return None;
    }
    let inner_dst_mac: [u8; 6] = frame[ieth..ieth + 6].try_into().ok()?;
    let inner_src_mac: [u8; 6] = frame[ieth + 6..ieth + 12].try_into().ok()?;
    let inner_ethertype = u16::from_be_bytes([frame[ieth + 12], frame[ieth + 13]]);
    if inner_ethertype != ETH_TYPE_IPV4 {
        return None; // only IPv4 inner supported for now
    }

    // Inner IPv4 header
    let iip_off = ieth + ETH_HEADER_LEN;
    if frame.len() < iip_off + IPV4_HEADER_LEN {
        return None;
    }
    let iip = &frame[iip_off..];
    if (iip[0] >> 4) != 4 {
        return None;
    }
    let iip_ihl = (iip[0] & 0x0F) as usize * 4;
    if iip_ihl < 20 || frame.len() < iip_off + iip_ihl {
        return None;
    }
    if iip[9] != IP_PROTO_UDP {
        return None;
    }
    let inner_src_ip = Ipv4Addr::new(iip[12], iip[13], iip[14], iip[15]);
    let inner_dst_ip = Ipv4Addr::new(iip[16], iip[17], iip[18], iip[19]);

    // Inner UDP header
    let iudp_off = iip_off + iip_ihl;
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

    Some(VxlanDecapResult {
        inner_src_ip,
        inner_dst_ip,
        inner_src_port,
        inner_dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        outer_src_ip,
        vxlan_header: vxlan_hdr,
        inner_src_mac,
        inner_dst_mac,
    })
}

// ============================================================================
// IPv6 Outer Support
// ============================================================================

/// Encapsulation overhead for VXLAN with IPv6 outer: IPv6(40) + UDP(8) + VXLAN(8) + inner Eth(14) = 70.
pub const VXLAN_ENCAP_OVERHEAD_V6: usize =
    IPV6_HEADER_LEN + UDP_HEADER_LEN + VXLAN_HEADER_LEN + ETH_HEADER_LEN;

/// Configuration for a VXLAN tunnel endpoint with IPv6 outer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VxlanConfig6 {
    /// Remote tunnel endpoint IPv6 address (VTEP peer).
    pub remote_ip: Ipv6Addr,
    /// VXLAN Network Identifier (24-bit).
    pub vni: u32,
    /// Outer UDP destination port (default: 4789).
    pub remote_port: u16,
    /// Outer UDP source port (default: 4789).
    pub local_port: u16,
    /// Inner source MAC address for encapsulated frames.
    pub inner_src_mac: [u8; 6],
    /// Inner destination MAC address for encapsulated frames.
    pub inner_dst_mac: [u8; 6],
}

impl VxlanConfig6 {
    pub fn new(remote_ip: Ipv6Addr, vni: u32) -> Self {
        assert!(vni <= VXLAN_VNI_MAX, "VNI must be 24-bit (max {})", VXLAN_VNI_MAX);
        Self {
            remote_ip,
            vni,
            remote_port: VXLAN_DEFAULT_PORT,
            local_port: VXLAN_DEFAULT_PORT,
            inner_src_mac: [0; 6],
            inner_dst_mac: [0xFF; 6],
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

    pub fn with_inner_src_mac(mut self, mac: [u8; 6]) -> Self {
        self.inner_src_mac = mac;
        self
    }

    pub fn with_inner_dst_mac(mut self, mac: [u8; 6]) -> Self {
        self.inner_dst_mac = mac;
        self
    }
}

/// Build a VXLAN-encapsulated frame with IPv6 outer into a caller-provided buffer.
///
/// Produces:
/// `[Outer Eth][Outer IPv6][Outer UDP][VXLAN][Inner Eth][Inner IPv4][Inner UDP][Payload]`
#[allow(clippy::too_many_arguments)]
pub fn build_vxlan_frame_into_v6(
    out: &mut Vec<u8>,
    outer_src_mac: &[u8; 6],
    outer_dst_mac: &[u8; 6],
    outer_src_ip: Ipv6Addr,
    outer_dst_ip: Ipv6Addr,
    outer_src_port: u16,
    outer_dst_port: u16,
    vni: u32,
    inner_src_mac: &[u8; 6],
    inner_dst_mac: &[u8; 6],
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    inner_src_port: u16,
    inner_dst_port: u16,
    payload: &[u8],
    hop_limit: u8,
) -> Result<usize, crate::UdpError> {
    let inner_udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let inner_ip_total = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let inner_frame_len = ETH_HEADER_LEN + inner_ip_total as usize;

    let outer_udp_payload_len = VXLAN_HEADER_LEN + inner_frame_len;
    let outer_udp_len = (UDP_HEADER_LEN + outer_udp_payload_len) as u16;
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
    out[oip] = 0x60;
    out[oip + 1] = 0x00;
    out[oip + 2] = 0x00;
    out[oip + 3] = 0x00;
    out[oip + 4..oip + 6].copy_from_slice(&ipv6_payload_len.to_be_bytes());
    out[oip + 6] = IP_PROTO_UDP;
    out[oip + 7] = hop_limit;
    out[oip + 8..oip + 24].copy_from_slice(&outer_src_ip.octets());
    out[oip + 24..oip + 40].copy_from_slice(&outer_dst_ip.octets());

    // === Outer UDP Header (8 bytes) ===
    let oudp = oip + IPV6_HEADER_LEN;
    out[oudp..oudp + 2].copy_from_slice(&outer_src_port.to_be_bytes());
    out[oudp + 2..oudp + 4].copy_from_slice(&outer_dst_port.to_be_bytes());
    out[oudp + 4..oudp + 6].copy_from_slice(&outer_udp_len.to_be_bytes());
    out[oudp + 6..oudp + 8].copy_from_slice(&[0x00, 0x00]);

    // === VXLAN Header (8 bytes) ===
    let vxlan_off = oudp + UDP_HEADER_LEN;
    VxlanHeader::new(vni).encode(&mut out[vxlan_off..vxlan_off + VXLAN_HEADER_LEN]);

    // === Inner Ethernet Header (14 bytes) ===
    let ieth = vxlan_off + VXLAN_HEADER_LEN;
    out[ieth..ieth + 6].copy_from_slice(inner_dst_mac);
    out[ieth + 6..ieth + 12].copy_from_slice(inner_src_mac);
    out[ieth + 12..ieth + 14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

    // === Inner IPv4 Header (20 bytes) ===
    let iip = ieth + ETH_HEADER_LEN;
    out[iip] = 0x45;
    out[iip + 1] = 0x00;
    out[iip + 2..iip + 4].copy_from_slice(&inner_ip_total.to_be_bytes());
    out[iip + 4..iip + 6].copy_from_slice(&[0x00, 0x00]);
    out[iip + 6..iip + 8].copy_from_slice(&[0x40, 0x00]);
    out[iip + 8] = 64;
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

/// Result of decapsulating a VXLAN frame with IPv6 outer.
#[derive(Debug)]
pub struct VxlanDecapResult6<'a> {
    pub inner_src_ip: Ipv4Addr,
    pub inner_dst_ip: Ipv4Addr,
    pub inner_src_port: u16,
    pub inner_dst_port: u16,
    pub payload: &'a [u8],
    pub outer_src_ip: Ipv6Addr,
    pub vxlan_header: VxlanHeader,
    pub inner_src_mac: [u8; 6],
    pub inner_dst_mac: [u8; 6],
}

/// Try to decapsulate a VXLAN frame with IPv6 outer.
pub fn try_decap_vxlan_v6<'a>(
    frame: &'a [u8],
    l3_offset: usize,
    vxlan_local_port: u16,
    expected_vni: Option<u32>,
) -> Option<VxlanDecapResult6<'a>> {
    if frame.len() < l3_offset + IPV6_HEADER_LEN {
        return None;
    }
    let oip = &frame[l3_offset..];
    if (oip[0] >> 4) != 6 {
        return None;
    }
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
    if outer_dst_port != vxlan_local_port {
        return None;
    }

    // VXLAN header
    let vxlan_off = oudp_off + UDP_HEADER_LEN;
    let vxlan_hdr = VxlanHeader::parse(&frame[vxlan_off..])?;

    if let Some(expected) = expected_vni {
        if vxlan_hdr.vni != expected {
            return None;
        }
    }

    // Inner Ethernet header
    let ieth = vxlan_off + VXLAN_HEADER_LEN;
    if frame.len() < ieth + ETH_HEADER_LEN {
        return None;
    }
    let inner_dst_mac: [u8; 6] = frame[ieth..ieth + 6].try_into().ok()?;
    let inner_src_mac: [u8; 6] = frame[ieth + 6..ieth + 12].try_into().ok()?;
    let inner_ethertype = u16::from_be_bytes([frame[ieth + 12], frame[ieth + 13]]);
    if inner_ethertype != ETH_TYPE_IPV4 {
        return None;
    }

    // Inner IPv4 header
    let iip_off = ieth + ETH_HEADER_LEN;
    if frame.len() < iip_off + IPV4_HEADER_LEN {
        return None;
    }
    let iip = &frame[iip_off..];
    if (iip[0] >> 4) != 4 {
        return None;
    }
    let iip_ihl = (iip[0] & 0x0F) as usize * 4;
    if iip_ihl < 20 || frame.len() < iip_off + iip_ihl {
        return None;
    }
    if iip[9] != IP_PROTO_UDP {
        return None;
    }
    let inner_src_ip = Ipv4Addr::new(iip[12], iip[13], iip[14], iip[15]);
    let inner_dst_ip = Ipv4Addr::new(iip[16], iip[17], iip[18], iip[19]);

    // Inner UDP header
    let iudp_off = iip_off + iip_ihl;
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

    Some(VxlanDecapResult6 {
        inner_src_ip,
        inner_dst_ip,
        inner_src_port,
        inner_dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        outer_src_ip,
        vxlan_header: vxlan_hdr,
        inner_src_mac,
        inner_dst_mac,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const OUTER_SRC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    const OUTER_DST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
    const INNER_SRC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x01, 0x01];
    const INNER_DST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x01, 0x02];

    fn outer_src_ip() -> Ipv4Addr { Ipv4Addr::new(10, 0, 0, 1) }
    fn outer_dst_ip() -> Ipv4Addr { Ipv4Addr::new(10, 0, 0, 2) }
    fn inner_src_ip() -> Ipv4Addr { Ipv4Addr::new(192, 168, 1, 10) }
    fn inner_dst_ip() -> Ipv4Addr { Ipv4Addr::new(192, 168, 1, 20) }

    fn build_test_frame(payload: &[u8], vni: u32) -> Vec<u8> {
        let mut frame = Vec::new();
        build_vxlan_frame_into(
            &mut frame,
            &OUTER_SRC_MAC, &OUTER_DST_MAC,
            outer_src_ip(), outer_dst_ip(),
            4789, 4789, vni,
            &INNER_SRC_MAC, &INNER_DST_MAC,
            inner_src_ip(), inner_dst_ip(),
            9000, 9001, payload, 64,
        ).unwrap();
        frame
    }

    // --- Constants ---

    #[test]
    fn constants_are_correct() {
        assert_eq!(VXLAN_HEADER_LEN, 8);
        assert_eq!(VXLAN_DEFAULT_PORT, 4789);
        // overhead = 20 (outer IP) + 8 (outer UDP) + 8 (VXLAN) + 14 (inner Eth) = 50
        assert_eq!(VXLAN_ENCAP_OVERHEAD, 50);
        assert_eq!(VXLAN_VNI_MAX, 0x00FF_FFFF);
    }

    // --- VxlanConfig ---

    #[test]
    fn config_defaults() {
        let cfg = VxlanConfig::new(Ipv4Addr::new(10, 0, 0, 2), 100);
        assert_eq!(cfg.remote_ip, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(cfg.vni, 100);
        assert_eq!(cfg.remote_port, VXLAN_DEFAULT_PORT);
        assert_eq!(cfg.local_port, VXLAN_DEFAULT_PORT);
    }

    #[test]
    fn config_builder() {
        let cfg = VxlanConfig::new(Ipv4Addr::new(10, 0, 0, 2), 200)
            .with_remote_port(5000)
            .with_local_port(5001)
            .with_inner_src_mac([0xAA; 6])
            .with_inner_dst_mac([0xBB; 6]);
        assert_eq!(cfg.remote_port, 5000);
        assert_eq!(cfg.local_port, 5001);
        assert_eq!(cfg.inner_src_mac, [0xAA; 6]);
        assert_eq!(cfg.inner_dst_mac, [0xBB; 6]);
    }

    #[test]
    #[should_panic(expected = "VNI must be 24-bit")]
    fn config_rejects_oversized_vni() {
        VxlanConfig::new(Ipv4Addr::new(10, 0, 0, 1), VXLAN_VNI_MAX + 1);
    }

    #[test]
    fn config_accepts_max_vni() {
        let cfg = VxlanConfig::new(Ipv4Addr::new(10, 0, 0, 1), VXLAN_VNI_MAX);
        assert_eq!(cfg.vni, VXLAN_VNI_MAX);
    }

    // --- VxlanHeader encode/parse ---

    #[test]
    fn header_encode_decode_roundtrip() {
        let hdr = VxlanHeader::new(0x123456);
        let mut buf = [0u8; 8];
        hdr.encode(&mut buf);

        assert_eq!(buf[0], VXLAN_FLAGS_I); // I flag set
        assert_eq!(buf[1], 0); // reserved
        assert_eq!(buf[2], 0); // reserved
        assert_eq!(buf[3], 0); // reserved
        assert_eq!(buf[4], 0x12); // VNI high byte
        assert_eq!(buf[5], 0x34); // VNI mid byte
        assert_eq!(buf[6], 0x56); // VNI low byte
        assert_eq!(buf[7], 0); // reserved

        let parsed = VxlanHeader::parse(&buf).unwrap();
        assert!(parsed.i_flag);
        assert_eq!(parsed.vni, 0x123456);
    }

    #[test]
    fn header_vni_zero() {
        let hdr = VxlanHeader::new(0);
        let mut buf = [0u8; 8];
        hdr.encode(&mut buf);
        let parsed = VxlanHeader::parse(&buf).unwrap();
        assert_eq!(parsed.vni, 0);
    }

    #[test]
    fn header_vni_max() {
        let hdr = VxlanHeader::new(VXLAN_VNI_MAX);
        let mut buf = [0u8; 8];
        hdr.encode(&mut buf);
        let parsed = VxlanHeader::parse(&buf).unwrap();
        assert_eq!(parsed.vni, VXLAN_VNI_MAX);
    }

    #[test]
    fn header_rejects_no_i_flag() {
        let buf = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00];
        assert!(VxlanHeader::parse(&buf).is_none());
    }

    #[test]
    fn header_rejects_too_short() {
        let buf = [VXLAN_FLAGS_I, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00];
        assert!(VxlanHeader::parse(&buf).is_none());
    }

    // --- Build + Decap roundtrip ---

    #[test]
    fn build_and_decap_roundtrip() {
        let payload = b"hello VXLAN tunnel";
        let frame = build_test_frame(payload, 100);

        // Expected: 14 (outer eth) + 20 (outer ip) + 8 (outer udp) + 8 (vxlan)
        //         + 14 (inner eth) + 20 (inner ip) + 8 (inner udp) + 18 (payload) = 110
        assert_eq!(frame.len(), 14 + 20 + 8 + 8 + 14 + 20 + 8 + payload.len());

        let decap = try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, Some(100)).unwrap();
        assert_eq!(decap.inner_src_ip, inner_src_ip());
        assert_eq!(decap.inner_dst_ip, inner_dst_ip());
        assert_eq!(decap.inner_src_port, 9000);
        assert_eq!(decap.inner_dst_port, 9001);
        assert_eq!(decap.payload, payload);
        assert_eq!(decap.outer_src_ip, outer_src_ip());
        assert_eq!(decap.vxlan_header.vni, 100);
        assert_eq!(decap.inner_src_mac, INNER_SRC_MAC);
        assert_eq!(decap.inner_dst_mac, INNER_DST_MAC);
    }

    #[test]
    fn decap_accepts_any_vni_when_none() {
        let frame = build_test_frame(b"any vni", 999);
        let decap = try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, None).unwrap();
        assert_eq!(decap.vxlan_header.vni, 999);
        assert_eq!(decap.payload, b"any vni");
    }

    #[test]
    fn decap_rejects_wrong_vni() {
        let frame = build_test_frame(b"wrong vni", 100);
        assert!(try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, Some(200)).is_none());
    }

    #[test]
    fn decap_rejects_wrong_port() {
        let frame = build_test_frame(b"wrong port", 100);
        assert!(try_decap_vxlan(&frame, ETH_HEADER_LEN, 5000, Some(100)).is_none());
    }

    #[test]
    fn build_empty_payload() {
        let frame = build_test_frame(b"", 100);
        // 14 + 20 + 8 + 8 + 14 + 20 + 8 + 0 = 92
        assert_eq!(frame.len(), 92);
        let decap = try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, Some(100)).unwrap();
        assert!(decap.payload.is_empty());
    }

    #[test]
    fn build_large_payload() {
        let payload = vec![0xAB; 1400];
        let frame = build_test_frame(&payload, 100);
        assert_eq!(frame.len(), 92 + 1400);
        let decap = try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, Some(100)).unwrap();
        assert_eq!(decap.payload.len(), 1400);
        assert!(decap.payload.iter().all(|&b| b == 0xAB));
    }

    // --- Wire format verification ---

    #[test]
    fn wire_format_outer_ethertype() {
        let frame = build_test_frame(b"x", 100);
        assert_eq!(u16::from_be_bytes([frame[12], frame[13]]), ETH_TYPE_IPV4);
    }

    #[test]
    fn wire_format_outer_ipv4_version() {
        let frame = build_test_frame(b"x", 100);
        assert_eq!(frame[ETH_HEADER_LEN] >> 4, 4);
    }

    #[test]
    fn wire_format_outer_protocol_is_udp() {
        let frame = build_test_frame(b"x", 100);
        assert_eq!(frame[ETH_HEADER_LEN + 9], IP_PROTO_UDP);
    }

    #[test]
    fn wire_format_outer_udp_port() {
        let frame = build_test_frame(b"x", 100);
        let oudp = ETH_HEADER_LEN + IPV4_HEADER_LEN;
        let dst_port = u16::from_be_bytes([frame[oudp + 2], frame[oudp + 3]]);
        assert_eq!(dst_port, 4789);
    }

    #[test]
    fn wire_format_vxlan_i_flag() {
        let frame = build_test_frame(b"x", 100);
        let vxlan_off = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        assert_eq!(frame[vxlan_off] & VXLAN_FLAGS_I, VXLAN_FLAGS_I);
    }

    #[test]
    fn wire_format_vxlan_vni() {
        let frame = build_test_frame(b"x", 0x0A0B0C);
        let vxlan_off = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        assert_eq!(frame[vxlan_off + 4], 0x0A);
        assert_eq!(frame[vxlan_off + 5], 0x0B);
        assert_eq!(frame[vxlan_off + 6], 0x0C);
    }

    #[test]
    fn wire_format_inner_ethernet() {
        let frame = build_test_frame(b"x", 100);
        let ieth = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN + VXLAN_HEADER_LEN;
        assert_eq!(&frame[ieth..ieth + 6], &INNER_DST_MAC);
        assert_eq!(&frame[ieth + 6..ieth + 12], &INNER_SRC_MAC);
        assert_eq!(u16::from_be_bytes([frame[ieth + 12], frame[ieth + 13]]), ETH_TYPE_IPV4);
    }

    #[test]
    fn wire_format_checksums_valid() {
        let frame = build_test_frame(b"checksum test", 100);
        assert!(crate::verify_ipv4_checksum(&frame));
    }

    // --- Decap edge cases ---

    #[test]
    fn decap_rejects_truncated_frame() {
        let frame = build_test_frame(b"data", 100);
        // Truncate to just outer headers
        assert!(try_decap_vxlan(&frame[..40], ETH_HEADER_LEN, 4789, Some(100)).is_none());
    }

    #[test]
    fn decap_rejects_non_udp_outer() {
        let mut frame = build_test_frame(b"data", 100);
        // Change outer protocol from UDP to TCP
        frame[ETH_HEADER_LEN + 9] = 6; // TCP
        assert!(try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, Some(100)).is_none());
    }

    #[test]
    fn decap_rejects_no_i_flag() {
        let mut frame = build_test_frame(b"data", 100);
        let vxlan_off = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        frame[vxlan_off] = 0x00; // clear I flag
        assert!(try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, Some(100)).is_none());
    }

    // --- Custom ports ---

    #[test]
    fn custom_ports() {
        let mut frame = Vec::new();
        build_vxlan_frame_into(
            &mut frame,
            &OUTER_SRC_MAC, &OUTER_DST_MAC,
            outer_src_ip(), outer_dst_ip(),
            5555, 6666, 42,
            &INNER_SRC_MAC, &INNER_DST_MAC,
            inner_src_ip(), inner_dst_ip(),
            8000, 8001, b"custom", 128,
        ).unwrap();

        // Must match on the custom port
        assert!(try_decap_vxlan(&frame, ETH_HEADER_LEN, 4789, Some(42)).is_none());
        let decap = try_decap_vxlan(&frame, ETH_HEADER_LEN, 6666, Some(42)).unwrap();
        assert_eq!(decap.inner_src_port, 8000);
        assert_eq!(decap.inner_dst_port, 8001);
        assert_eq!(decap.payload, b"custom");
    }

    // --- Synthetic performance benchmark ---

    #[test]
    fn perf_build_decap_cycle() {
        let payload = vec![0xAA; 64];
        let mut buf = Vec::with_capacity(1500);
        let iterations = 10_000;

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            build_vxlan_frame_into(
                &mut buf,
                &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src_ip(), outer_dst_ip(),
                4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src_ip(), inner_dst_ip(),
                12345, 9000, &payload, 64,
            ).unwrap();
            let _ = try_decap_vxlan(&buf, ETH_HEADER_LEN, 4789, Some(100)).unwrap();
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!(
            "[PERF] VXLAN build+decap: {} iterations in {:?} ({} ns/op)",
            iterations, elapsed, ns_per_op
        );
        assert!(ns_per_op < 10_000, "build+decap too slow: {} ns/op", ns_per_op);
    }

    // --- Encap overhead constant ---

    #[test]
    fn encap_overhead_matches_frame_size() {
        // A frame with empty payload should be exactly:
        // outer_eth(14) + outer_ip(20) + outer_udp(8) + vxlan(8) + inner_eth(14) + inner_ip(20) + inner_udp(8) = 92
        // VXLAN_ENCAP_OVERHEAD = 50 (everything except outer eth and inner ip+udp headers)
        // The overhead constant covers: outer_ip + outer_udp + vxlan + inner_eth = 20+8+8+14 = 50
        let frame = build_test_frame(b"", 100);
        let base_frame = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN; // plain UDP frame = 42
        assert_eq!(frame.len(), base_frame + VXLAN_ENCAP_OVERHEAD);
    }

    // =========================================================================
    // IPv6 outer tests
    // =========================================================================

    mod ipv6_outer {
        use super::*;
        use std::net::Ipv6Addr;
        use crate::ipv6::{ETH_TYPE_IPV6, IPV6_HEADER_LEN};

        const OUTER_SRC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        const OUTER_DST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        const INNER_SRC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x01, 0x01];
        const INNER_DST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x01, 0x02];

        fn outer_src() -> Ipv6Addr { "2001:db8::1".parse().unwrap() }
        fn outer_dst() -> Ipv6Addr { "2001:db8::2".parse().unwrap() }
        fn inner_src() -> Ipv4Addr { Ipv4Addr::new(192, 168, 1, 10) }
        fn inner_dst() -> Ipv4Addr { Ipv4Addr::new(192, 168, 1, 20) }

        #[test]
        fn config6_defaults() {
            let cfg = VxlanConfig6::new(outer_dst(), 100);
            assert_eq!(cfg.remote_ip, outer_dst());
            assert_eq!(cfg.vni, 100);
            assert_eq!(cfg.remote_port, VXLAN_DEFAULT_PORT);
            assert_eq!(cfg.local_port, VXLAN_DEFAULT_PORT);
        }

        #[test]
        fn config6_builder() {
            let cfg = VxlanConfig6::new(outer_dst(), 200)
                .with_remote_port(5000)
                .with_local_port(5001)
                .with_inner_src_mac([0xAA; 6])
                .with_inner_dst_mac([0xBB; 6]);
            assert_eq!(cfg.remote_port, 5000);
            assert_eq!(cfg.local_port, 5001);
            assert_eq!(cfg.inner_src_mac, [0xAA; 6]);
            assert_eq!(cfg.inner_dst_mac, [0xBB; 6]);
        }

        #[test]
        #[should_panic(expected = "VNI must be 24-bit")]
        fn config6_rejects_oversized_vni() {
            VxlanConfig6::new(outer_dst(), VXLAN_VNI_MAX + 1);
        }

        #[test]
        fn build_and_decap_roundtrip() {
            let payload = b"hello VXLAN IPv6 tunnel";
            let mut frame = Vec::new();
            let len = build_vxlan_frame_into_v6(
                &mut frame,
                &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(),
                4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(),
                9000, 9001, payload, 64,
            ).unwrap();

            assert_eq!(frame.len(), len);
            // 14 (eth) + 40 (IPv6) + 8 (UDP) + 8 (VXLAN) + 14 (inner eth) + 20 (inner IPv4) + 8 (inner UDP) + payload
            let expected = 14 + 40 + 8 + 8 + 14 + 20 + 8 + payload.len();
            assert_eq!(len, expected);

            let decap = try_decap_vxlan_v6(&frame, ETH_HEADER_LEN, 4789, Some(100)).unwrap();
            assert_eq!(decap.inner_src_ip, inner_src());
            assert_eq!(decap.inner_dst_ip, inner_dst());
            assert_eq!(decap.inner_src_port, 9000);
            assert_eq!(decap.inner_dst_port, 9001);
            assert_eq!(decap.payload, payload);
            assert_eq!(decap.outer_src_ip, outer_src());
            assert_eq!(decap.vxlan_header.vni, 100);
            assert_eq!(decap.inner_src_mac, INNER_SRC_MAC);
            assert_eq!(decap.inner_dst_mac, INNER_DST_MAC);
        }

        #[test]
        fn wire_format_ethertype_is_ipv6() {
            let mut frame = Vec::new();
            build_vxlan_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 1, 2, b"x", 64,
            ).unwrap();
            let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
            assert_eq!(ethertype, ETH_TYPE_IPV6);
        }

        #[test]
        fn wire_format_ipv6_version() {
            let mut frame = Vec::new();
            build_vxlan_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 1, 2, b"x", 64,
            ).unwrap();
            assert_eq!(frame[ETH_HEADER_LEN] >> 4, 6);
        }

        #[test]
        fn wire_format_outer_udp_checksum_valid() {
            let mut frame = Vec::new();
            build_vxlan_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"checksum test", 64,
            ).unwrap();
            assert!(crate::verify_udp6_checksum(&frame));
        }

        #[test]
        fn decap_rejects_wrong_port() {
            let mut frame = Vec::new();
            build_vxlan_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            assert!(try_decap_vxlan_v6(&frame, ETH_HEADER_LEN, 5000, Some(100)).is_none());
        }

        #[test]
        fn decap_rejects_wrong_vni() {
            let mut frame = Vec::new();
            build_vxlan_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            assert!(try_decap_vxlan_v6(&frame, ETH_HEADER_LEN, 4789, Some(200)).is_none());
        }

        #[test]
        fn decap_accepts_any_vni_when_none() {
            let mut frame = Vec::new();
            build_vxlan_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 4789, 4789, 999,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            let decap = try_decap_vxlan_v6(&frame, ETH_HEADER_LEN, 4789, None).unwrap();
            assert_eq!(decap.vxlan_header.vni, 999);
        }

        #[test]
        fn build_empty_payload() {
            let mut frame = Vec::new();
            let len = build_vxlan_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 4789, 4789, 100,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, &[], 64,
            ).unwrap();
            // 14 + 40 + 8 + 8 + 14 + 20 + 8 = 112
            assert_eq!(len, 112);
            let decap = try_decap_vxlan_v6(&frame, ETH_HEADER_LEN, 4789, Some(100)).unwrap();
            assert!(decap.payload.is_empty());
        }

        #[test]
        fn encap_overhead_v6_is_correct() {
            // outer IPv6(40) + outer UDP(8) + VXLAN(8) + inner Eth(14) = 70
            assert_eq!(VXLAN_ENCAP_OVERHEAD_V6, 70);
        }

        #[test]
        fn perf_build_decap_cycle_v6() {
            let payload = vec![0xAA; 64];
            let mut buf = Vec::with_capacity(1500);
            let iterations = 10_000;

            let start = std::time::Instant::now();
            for _ in 0..iterations {
                build_vxlan_frame_into_v6(
                    &mut buf, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                    outer_src(), outer_dst(), 4789, 4789, 100,
                    &INNER_SRC_MAC, &INNER_DST_MAC,
                    inner_src(), inner_dst(), 12345, 9000, &payload, 64,
                ).unwrap();
                let _ = try_decap_vxlan_v6(&buf, ETH_HEADER_LEN, 4789, Some(100)).unwrap();
            }
            let elapsed = start.elapsed();
            let ns_per_op = elapsed.as_nanos() / iterations as u128;
            eprintln!(
                "[PERF] VXLAN IPv6-outer build+decap: {} iterations in {:?} ({} ns/op)",
                iterations, elapsed, ns_per_op
            );
            assert!(ns_per_op < 10_000, "build+decap too slow: {} ns/op", ns_per_op);
        }
    }
}
