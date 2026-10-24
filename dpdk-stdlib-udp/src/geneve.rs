//! GENEVE (RFC 8926) tunnel endpoint.
//!
//! Implements a high-performance GENEVE tunnel endpoint: an inner Ethernet frame
//! is wrapped in an outer UDP/IPv4 frame with a variable-length GENEVE header
//! carrying a 24-bit VNI and optional TLV metadata.
//!
//! Wire format:
//! ```text
//! [Outer Eth 14B][Outer IPv4 20B][Outer UDP 8B][GENEVE 8B+][Inner Eth 14B][Inner IPv4 20B][Inner UDP 8B][Payload]
//! ```
//!
//! The GENEVE base header (8 bytes, no options):
//! ```text
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |Ver|  Opt Len  |O|C|    Rsvd.  |         Protocol Type         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |        Virtual Network Identifier (VNI)       |    Reserved   |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;

use crate::{
    ipv4_checksum, udp_checksum, ETH_HEADER_LEN, ETH_TYPE_IPV4, IPV4_HEADER_LEN,
    IP_PROTO_UDP, UDP_HEADER_LEN,
};
use crate::ipv6::{ETH_TYPE_IPV6, IPV6_HEADER_LEN, udp6_checksum};

/// GENEVE base header size (8 bytes, no options).
pub const GENEVE_BASE_HEADER_LEN: usize = 8;

/// IANA-assigned GENEVE UDP destination port.
pub const GENEVE_DEFAULT_PORT: u16 = 6081;

/// Total encapsulation overhead for GENEVE with no options (outer IPv4 + outer UDP + GENEVE base + inner Ethernet).
/// Does NOT include the outer Ethernet header (always present).
pub const GENEVE_ENCAP_OVERHEAD: usize =
    IPV4_HEADER_LEN + UDP_HEADER_LEN + GENEVE_BASE_HEADER_LEN + ETH_HEADER_LEN;

/// Maximum valid VNI value (24-bit: 0x00FFFFFF).
pub const GENEVE_VNI_MAX: u32 = 0x00FF_FFFF;

/// EtherType for "Transparent Ethernet Bridging" (inner Ethernet payload).
pub const GENEVE_INNER_ETYPE_ETH: u16 = 0x6558;

/// Maximum total options length in bytes (252 = 63 * 4, since Opt Len is 6 bits
/// encoding 4-byte units).
pub const GENEVE_MAX_OPTIONS_LEN: usize = 252;

/// GENEVE protocol version.
pub const GENEVE_VERSION: u8 = 0;

/// Configuration for a GENEVE tunnel endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneveConfig {
    /// Remote tunnel endpoint IP address (VTEP peer).
    pub remote_ip: Ipv4Addr,
    /// GENEVE Virtual Network Identifier (24-bit).
    pub vni: u32,
    /// Outer UDP destination port (default: 6081).
    pub remote_port: u16,
    /// Outer UDP source port (default: 6081).
    pub local_port: u16,
    /// Inner source MAC address for encapsulated frames.
    pub inner_src_mac: [u8; 6],
    /// Inner destination MAC address for encapsulated frames.
    pub inner_dst_mac: [u8; 6],
}

impl GeneveConfig {
    /// Create a new GENEVE config with the given remote VTEP IP and VNI.
    ///
    /// # Panics
    /// Panics if `vni` exceeds 24 bits (> 16,777,215).
    pub fn new(remote_ip: Ipv4Addr, vni: u32) -> Self {
        assert!(vni <= GENEVE_VNI_MAX, "VNI must be 24-bit (max {})", GENEVE_VNI_MAX);
        Self {
            remote_ip,
            vni,
            remote_port: GENEVE_DEFAULT_PORT,
            local_port: GENEVE_DEFAULT_PORT,
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

// ============================================================================
// GENEVE Header
// ============================================================================

/// A single GENEVE TLV option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneveTlvOption {
    /// Option class (16-bit vendor/standards namespace).
    pub class: u16,
    /// Option type (8-bit).
    pub option_type: u8,
    /// Option data (length must be a multiple of 4 bytes).
    pub data: Vec<u8>,
}

/// Parsed GENEVE header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneveHeader {
    /// Protocol version (must be 0).
    pub version: u8,
    /// Options length in bytes (multiple of 4).
    pub options_len: usize,
    /// OAM bit — indicates an OAM frame.
    pub oam: bool,
    /// Critical bit — indicates critical options present.
    pub critical: bool,
    /// Protocol type of the inner payload (0x6558 for Ethernet).
    pub protocol_type: u16,
    /// 24-bit Virtual Network Identifier.
    pub vni: u32,
    /// Parsed TLV options (may be empty).
    pub options: Vec<GeneveTlvOption>,
}

impl GeneveHeader {
    /// Create a new GENEVE header with no options.
    pub fn new(vni: u32) -> Self {
        debug_assert!(vni <= GENEVE_VNI_MAX);
        Self {
            version: GENEVE_VERSION,
            options_len: 0,
            oam: false,
            critical: false,
            protocol_type: GENEVE_INNER_ETYPE_ETH,
            vni,
            options: Vec::new(),
        }
    }

    /// Total header length including options.
    pub fn header_len(&self) -> usize {
        GENEVE_BASE_HEADER_LEN + self.options_len
    }

    /// Encode the GENEVE header into `out`. Returns the number of bytes written.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let total = self.header_len();
        debug_assert!(out.len() >= total);

        let opt_len_words = (self.options_len / 4) as u8;
        // Byte 0: Ver (2 bits) | Opt Len (6 bits)
        out[0] = ((self.version & 0x03) << 6) | (opt_len_words & 0x3F);
        // Byte 1: O (1 bit) | C (1 bit) | Rsvd (6 bits)
        out[1] = if self.oam { 0x80 } else { 0 } | if self.critical { 0x40 } else { 0 };
        // Bytes 2-3: Protocol Type
        out[2..4].copy_from_slice(&self.protocol_type.to_be_bytes());
        // Bytes 4-6: VNI
        out[4] = ((self.vni >> 16) & 0xFF) as u8;
        out[5] = ((self.vni >> 8) & 0xFF) as u8;
        out[6] = (self.vni & 0xFF) as u8;
        // Byte 7: Reserved
        out[7] = 0;

        // Encode TLV options
        let mut off = GENEVE_BASE_HEADER_LEN;
        for opt in &self.options {
            let data_len_words = (opt.data.len() / 4) as u8;
            out[off] = (opt.class >> 8) as u8;
            out[off + 1] = opt.class as u8;
            out[off + 2] = opt.option_type;
            // Length in 4-byte units (low 5 bits), top 3 bits reserved
            out[off + 3] = data_len_words & 0x1F;
            out[off + 4..off + 4 + opt.data.len()].copy_from_slice(&opt.data);
            off += 4 + opt.data.len();
        }

        total
    }

    /// Parse a GENEVE header from `data`.
    ///
    /// Returns `None` if the data is too short, the version is wrong, or the
    /// protocol type is not Transparent Ethernet Bridging (0x6558).
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < GENEVE_BASE_HEADER_LEN {
            return None;
        }
        let version = (data[0] >> 6) & 0x03;
        if version != GENEVE_VERSION {
            return None;
        }
        let opt_len_words = (data[0] & 0x3F) as usize;
        let options_len = opt_len_words * 4;
        let total_len = GENEVE_BASE_HEADER_LEN + options_len;
        if data.len() < total_len {
            return None;
        }
        let oam = (data[1] & 0x80) != 0;
        let critical = (data[1] & 0x40) != 0;
        let protocol_type = u16::from_be_bytes([data[2], data[3]]);
        if protocol_type != GENEVE_INNER_ETYPE_ETH {
            return None;
        }
        let vni = ((data[4] as u32) << 16) | ((data[5] as u32) << 8) | (data[6] as u32);

        // Parse TLV options
        let mut options = Vec::new();
        let mut off = GENEVE_BASE_HEADER_LEN;
        while off + 4 <= total_len {
            let class = u16::from_be_bytes([data[off], data[off + 1]]);
            let option_type = data[off + 2];
            let data_len = (data[off + 3] & 0x1F) as usize * 4;
            if off + 4 + data_len > total_len {
                return None;
            }
            options.push(GeneveTlvOption {
                class,
                option_type,
                data: data[off + 4..off + 4 + data_len].to_vec(),
            });
            off += 4 + data_len;
        }

        Some(Self {
            version,
            options_len,
            oam,
            critical,
            protocol_type,
            vni,
            options,
        })
    }
}

// ============================================================================
// Frame Building
// ============================================================================

/// Build a GENEVE-encapsulated frame into a caller-provided buffer.
///
/// Produces:
/// `[Outer Eth][Outer IPv4][Outer UDP][GENEVE][Inner Eth][Inner IPv4][Inner UDP][Payload]`
///
/// Returns the total frame length written into `out`.
#[allow(clippy::too_many_arguments)]
pub fn build_geneve_frame_into(
    out: &mut Vec<u8>,
    outer_src_mac: &[u8; 6],
    outer_dst_mac: &[u8; 6],
    outer_src_ip: Ipv4Addr,
    outer_dst_ip: Ipv4Addr,
    outer_src_port: u16,
    outer_dst_port: u16,
    geneve_header: &GeneveHeader,
    inner_src_mac: &[u8; 6],
    inner_dst_mac: &[u8; 6],
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    inner_src_port: u16,
    inner_dst_port: u16,
    payload: &[u8],
    ttl: u8,
) -> Result<usize, crate::UdpError> {
    let geneve_len = geneve_header.header_len();

    // Inner frame: Eth(14) + IPv4(20) + UDP(8) + payload
    let inner_udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let inner_ip_total = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let inner_frame_len = ETH_HEADER_LEN + inner_ip_total as usize;

    // Outer: Eth(14) + IPv4(20) + UDP(8) + GENEVE(8+opts) + inner_frame
    let outer_udp_payload_len = geneve_len + inner_frame_len;
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
    out[oudp + 6..oudp + 8].copy_from_slice(&[0x00, 0x00]);

    // === GENEVE Header ===
    let geneve_off = oudp + UDP_HEADER_LEN;
    geneve_header.encode(&mut out[geneve_off..geneve_off + geneve_len]);

    // === Inner Ethernet Header (14 bytes) ===
    let ieth = geneve_off + geneve_len;
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

    // Inner UDP checksum
    let inner_udp_cksum = udp_checksum(
        &inner_src_bytes,
        &inner_dst_bytes,
        &out[iudp..iudp + UDP_HEADER_LEN],
        payload,
    );
    out[iudp + 6..iudp + 8].copy_from_slice(&inner_udp_cksum.to_be_bytes());

    // Outer UDP checksum
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

/// Result of decapsulating a GENEVE frame.
#[derive(Debug)]
pub struct GeneveDecapResult<'a> {
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
    /// The parsed GENEVE header (contains VNI and options).
    pub geneve_header: GeneveHeader,
    /// Inner source MAC address.
    pub inner_src_mac: [u8; 6],
    /// Inner destination MAC address.
    pub inner_dst_mac: [u8; 6],
}

/// Try to decapsulate a GENEVE frame.
///
/// `frame` is the full Ethernet frame. `l3_offset` is where the outer IPv4
/// header starts. `geneve_local_port` is the expected outer UDP destination port.
/// `expected_vni` filters by VNI — pass `None` to accept any VNI.
///
/// Returns `None` if this is not a valid GENEVE packet.
pub fn try_decap_geneve<'a>(
    frame: &'a [u8],
    l3_offset: usize,
    geneve_local_port: u16,
    expected_vni: Option<u32>,
) -> Option<GeneveDecapResult<'a>> {
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
    if outer_dst_port != geneve_local_port {
        return None;
    }

    // GENEVE header
    let geneve_off = oudp_off + UDP_HEADER_LEN;
    let geneve_hdr = GeneveHeader::parse(&frame[geneve_off..])?;

    // VNI filtering
    if let Some(expected) = expected_vni {
        if geneve_hdr.vni != expected {
            return None;
        }
    }

    // Inner Ethernet header
    let ieth = geneve_off + geneve_hdr.header_len();
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

    Some(GeneveDecapResult {
        inner_src_ip,
        inner_dst_ip,
        inner_src_port,
        inner_dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        outer_src_ip,
        geneve_header: geneve_hdr,
        inner_src_mac,
        inner_dst_mac,
    })
}

// ============================================================================
// IPv6 Outer Support
// ============================================================================

/// Encapsulation overhead for GENEVE with IPv6 outer (no options):
/// IPv6(40) + UDP(8) + GENEVE base(8) + inner Eth(14) = 70.
pub const GENEVE_ENCAP_OVERHEAD_V6: usize =
    IPV6_HEADER_LEN + UDP_HEADER_LEN + GENEVE_BASE_HEADER_LEN + ETH_HEADER_LEN;

/// Configuration for a GENEVE tunnel endpoint with IPv6 outer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneveConfig6 {
    /// Remote tunnel endpoint IPv6 address (VTEP peer).
    pub remote_ip: Ipv6Addr,
    /// GENEVE Virtual Network Identifier (24-bit).
    pub vni: u32,
    /// Outer UDP destination port (default: 6081).
    pub remote_port: u16,
    /// Outer UDP source port (default: 6081).
    pub local_port: u16,
    /// Inner source MAC address for encapsulated frames.
    pub inner_src_mac: [u8; 6],
    /// Inner destination MAC address for encapsulated frames.
    pub inner_dst_mac: [u8; 6],
}

impl GeneveConfig6 {
    pub fn new(remote_ip: Ipv6Addr, vni: u32) -> Self {
        assert!(vni <= GENEVE_VNI_MAX, "VNI must be 24-bit (max {})", GENEVE_VNI_MAX);
        Self {
            remote_ip,
            vni,
            remote_port: GENEVE_DEFAULT_PORT,
            local_port: GENEVE_DEFAULT_PORT,
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

/// Build a GENEVE-encapsulated frame with IPv6 outer into a caller-provided buffer.
///
/// Produces:
/// `[Outer Eth][Outer IPv6][Outer UDP][GENEVE][Inner Eth][Inner IPv4][Inner UDP][Payload]`
#[allow(clippy::too_many_arguments)]
pub fn build_geneve_frame_into_v6(
    out: &mut Vec<u8>,
    outer_src_mac: &[u8; 6],
    outer_dst_mac: &[u8; 6],
    outer_src_ip: Ipv6Addr,
    outer_dst_ip: Ipv6Addr,
    outer_src_port: u16,
    outer_dst_port: u16,
    geneve_header: &GeneveHeader,
    inner_src_mac: &[u8; 6],
    inner_dst_mac: &[u8; 6],
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    inner_src_port: u16,
    inner_dst_port: u16,
    payload: &[u8],
    hop_limit: u8,
) -> Result<usize, crate::UdpError> {
    let geneve_len = geneve_header.header_len();

    let inner_udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let inner_ip_total = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let inner_frame_len = ETH_HEADER_LEN + inner_ip_total as usize;

    let outer_udp_payload_len = geneve_len + inner_frame_len;
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

    // === GENEVE Header ===
    let geneve_off = oudp + UDP_HEADER_LEN;
    geneve_header.encode(&mut out[geneve_off..geneve_off + geneve_len]);

    // === Inner Ethernet Header (14 bytes) ===
    let ieth = geneve_off + geneve_len;
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

/// Result of decapsulating a GENEVE frame with IPv6 outer.
#[derive(Debug)]
pub struct GeneveDecapResult6<'a> {
    pub inner_src_ip: Ipv4Addr,
    pub inner_dst_ip: Ipv4Addr,
    pub inner_src_port: u16,
    pub inner_dst_port: u16,
    pub payload: &'a [u8],
    pub outer_src_ip: Ipv6Addr,
    pub geneve_header: GeneveHeader,
    pub inner_src_mac: [u8; 6],
    pub inner_dst_mac: [u8; 6],
}

/// Try to decapsulate a GENEVE frame with IPv6 outer.
pub fn try_decap_geneve_v6<'a>(
    frame: &'a [u8],
    l3_offset: usize,
    geneve_local_port: u16,
    expected_vni: Option<u32>,
) -> Option<GeneveDecapResult6<'a>> {
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
    if outer_dst_port != geneve_local_port {
        return None;
    }

    // GENEVE header
    let geneve_off = oudp_off + UDP_HEADER_LEN;
    let geneve_hdr = GeneveHeader::parse(&frame[geneve_off..])?;

    if let Some(expected) = expected_vni {
        if geneve_hdr.vni != expected {
            return None;
        }
    }

    // Inner Ethernet header
    let ieth = geneve_off + geneve_hdr.header_len();
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

    Some(GeneveDecapResult6 {
        inner_src_ip,
        inner_dst_ip,
        inner_src_port,
        inner_dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        outer_src_ip,
        geneve_header: geneve_hdr,
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
        let hdr = GeneveHeader::new(vni);
        let mut frame = Vec::new();
        build_geneve_frame_into(
            &mut frame,
            &OUTER_SRC_MAC, &OUTER_DST_MAC,
            outer_src_ip(), outer_dst_ip(),
            6081, 6081, &hdr,
            &INNER_SRC_MAC, &INNER_DST_MAC,
            inner_src_ip(), inner_dst_ip(),
            9000, 9001, payload, 64,
        ).unwrap();
        frame
    }

    // --- Constants ---

    #[test]
    fn constants_are_correct() {
        assert_eq!(GENEVE_BASE_HEADER_LEN, 8);
        assert_eq!(GENEVE_DEFAULT_PORT, 6081);
        // overhead = 20 (outer IP) + 8 (outer UDP) + 8 (GENEVE base) + 14 (inner Eth) = 50
        assert_eq!(GENEVE_ENCAP_OVERHEAD, 50);
        assert_eq!(GENEVE_VNI_MAX, 0x00FF_FFFF);
        assert_eq!(GENEVE_INNER_ETYPE_ETH, 0x6558);
        assert_eq!(GENEVE_MAX_OPTIONS_LEN, 252);
    }

    // --- GeneveConfig ---

    #[test]
    fn config_defaults() {
        let cfg = GeneveConfig::new(Ipv4Addr::new(10, 0, 0, 2), 100);
        assert_eq!(cfg.remote_ip, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(cfg.vni, 100);
        assert_eq!(cfg.remote_port, GENEVE_DEFAULT_PORT);
        assert_eq!(cfg.local_port, GENEVE_DEFAULT_PORT);
    }

    #[test]
    fn config_builder() {
        let cfg = GeneveConfig::new(Ipv4Addr::new(10, 0, 0, 2), 200)
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
        GeneveConfig::new(Ipv4Addr::new(10, 0, 0, 1), GENEVE_VNI_MAX + 1);
    }

    #[test]
    fn config_accepts_max_vni() {
        let cfg = GeneveConfig::new(Ipv4Addr::new(10, 0, 0, 1), GENEVE_VNI_MAX);
        assert_eq!(cfg.vni, GENEVE_VNI_MAX);
    }

    // --- GeneveHeader encode/parse ---

    #[test]
    fn header_encode_decode_roundtrip() {
        let hdr = GeneveHeader::new(0x123456);
        let mut buf = [0u8; 8];
        hdr.encode(&mut buf);

        // Byte 0: Ver=0 (top 2 bits), Opt Len=0 (bottom 6 bits)
        assert_eq!(buf[0], 0x00);
        // Byte 1: O=0, C=0, Rsvd=0
        assert_eq!(buf[1], 0x00);
        // Bytes 2-3: Protocol Type = 0x6558
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), GENEVE_INNER_ETYPE_ETH);
        // Bytes 4-6: VNI
        assert_eq!(buf[4], 0x12);
        assert_eq!(buf[5], 0x34);
        assert_eq!(buf[6], 0x56);
        assert_eq!(buf[7], 0x00);

        let parsed = GeneveHeader::parse(&buf).unwrap();
        assert_eq!(parsed.version, 0);
        assert_eq!(parsed.vni, 0x123456);
        assert!(!parsed.oam);
        assert!(!parsed.critical);
        assert_eq!(parsed.protocol_type, GENEVE_INNER_ETYPE_ETH);
        assert!(parsed.options.is_empty());
    }

    #[test]
    fn header_vni_zero() {
        let hdr = GeneveHeader::new(0);
        let mut buf = [0u8; 8];
        hdr.encode(&mut buf);
        let parsed = GeneveHeader::parse(&buf).unwrap();
        assert_eq!(parsed.vni, 0);
    }

    #[test]
    fn header_vni_max() {
        let hdr = GeneveHeader::new(GENEVE_VNI_MAX);
        let mut buf = [0u8; 8];
        hdr.encode(&mut buf);
        let parsed = GeneveHeader::parse(&buf).unwrap();
        assert_eq!(parsed.vni, GENEVE_VNI_MAX);
    }

    #[test]
    fn header_oam_and_critical_flags() {
        let mut hdr = GeneveHeader::new(100);
        hdr.oam = true;
        hdr.critical = true;
        let mut buf = [0u8; 8];
        hdr.encode(&mut buf);
        assert_eq!(buf[1] & 0x80, 0x80); // OAM
        assert_eq!(buf[1] & 0x40, 0x40); // Critical

        let parsed = GeneveHeader::parse(&buf).unwrap();
        assert!(parsed.oam);
        assert!(parsed.critical);
    }

    #[test]
    fn header_with_tlv_option() {
        let mut hdr = GeneveHeader::new(42);
        hdr.options.push(GeneveTlvOption {
            class: 0x0102,
            option_type: 0x03,
            data: vec![0xAA, 0xBB, 0xCC, 0xDD],
        });
        hdr.options_len = 8; // 4-byte TLV header + 4-byte data
        let mut buf = [0u8; 16]; // 8 base + 8 option
        hdr.encode(&mut buf);

        // Opt Len should be 2 (8 bytes / 4)
        assert_eq!(buf[0] & 0x3F, 2);

        let parsed = GeneveHeader::parse(&buf).unwrap();
        assert_eq!(parsed.options.len(), 1);
        assert_eq!(parsed.options[0].class, 0x0102);
        assert_eq!(parsed.options[0].option_type, 0x03);
        assert_eq!(parsed.options[0].data, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn header_rejects_wrong_version() {
        let mut buf = [0u8; 8];
        GeneveHeader::new(1).encode(&mut buf);
        buf[0] = 0x40; // version = 1
        assert!(GeneveHeader::parse(&buf).is_none());
    }

    #[test]
    fn header_rejects_wrong_protocol_type() {
        let mut buf = [0u8; 8];
        GeneveHeader::new(1).encode(&mut buf);
        buf[2] = 0x08; // protocol type = 0x0800 (IPv4, not Ethernet)
        buf[3] = 0x00;
        assert!(GeneveHeader::parse(&buf).is_none());
    }

    #[test]
    fn header_rejects_too_short() {
        let buf = [0u8; 7];
        assert!(GeneveHeader::parse(&buf).is_none());
    }

    #[test]
    fn header_rejects_truncated_options() {
        let mut buf = [0u8; 8];
        GeneveHeader::new(1).encode(&mut buf);
        buf[0] = 0x01; // opt_len = 1 word = 4 bytes, but buf is only 8 bytes total
        assert!(GeneveHeader::parse(&buf).is_none());
    }

    // --- Build + Decap roundtrip ---

    #[test]
    fn build_and_decap_roundtrip() {
        let payload = b"hello GENEVE tunnel";
        let frame = build_test_frame(payload, 100);

        // 14 (outer eth) + 20 (outer ip) + 8 (outer udp) + 8 (geneve)
        // + 14 (inner eth) + 20 (inner ip) + 8 (inner udp) + 19 (payload) = 111
        assert_eq!(frame.len(), 14 + 20 + 8 + 8 + 14 + 20 + 8 + payload.len());

        let decap = try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
        assert_eq!(decap.inner_src_ip, inner_src_ip());
        assert_eq!(decap.inner_dst_ip, inner_dst_ip());
        assert_eq!(decap.inner_src_port, 9000);
        assert_eq!(decap.inner_dst_port, 9001);
        assert_eq!(decap.payload, payload);
        assert_eq!(decap.outer_src_ip, outer_src_ip());
        assert_eq!(decap.geneve_header.vni, 100);
        assert_eq!(decap.inner_src_mac, INNER_SRC_MAC);
        assert_eq!(decap.inner_dst_mac, INNER_DST_MAC);
    }

    #[test]
    fn decap_accepts_any_vni_when_none() {
        let frame = build_test_frame(b"any vni", 999);
        let decap = try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, None).unwrap();
        assert_eq!(decap.geneve_header.vni, 999);
        assert_eq!(decap.payload, b"any vni");
    }

    #[test]
    fn decap_rejects_wrong_vni() {
        let frame = build_test_frame(b"wrong vni", 100);
        assert!(try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(200)).is_none());
    }

    #[test]
    fn decap_rejects_wrong_port() {
        let frame = build_test_frame(b"wrong port", 100);
        assert!(try_decap_geneve(&frame, ETH_HEADER_LEN, 5000, Some(100)).is_none());
    }

    #[test]
    fn build_empty_payload() {
        let frame = build_test_frame(b"", 100);
        // 14 + 20 + 8 + 8 + 14 + 20 + 8 + 0 = 92
        assert_eq!(frame.len(), 92);
        let decap = try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
        assert!(decap.payload.is_empty());
    }

    #[test]
    fn build_large_payload() {
        let payload = vec![0xAB; 1400];
        let frame = build_test_frame(&payload, 100);
        assert_eq!(frame.len(), 92 + 1400);
        let decap = try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
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
        assert_eq!(dst_port, 6081);
    }

    #[test]
    fn wire_format_geneve_version() {
        let frame = build_test_frame(b"x", 100);
        let geneve_off = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        assert_eq!((frame[geneve_off] >> 6) & 0x03, 0);
    }

    #[test]
    fn wire_format_geneve_protocol_type() {
        let frame = build_test_frame(b"x", 100);
        let geneve_off = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        let proto = u16::from_be_bytes([frame[geneve_off + 2], frame[geneve_off + 3]]);
        assert_eq!(proto, GENEVE_INNER_ETYPE_ETH);
    }

    #[test]
    fn wire_format_geneve_vni() {
        let frame = build_test_frame(b"x", 0x0A0B0C);
        let geneve_off = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        assert_eq!(frame[geneve_off + 4], 0x0A);
        assert_eq!(frame[geneve_off + 5], 0x0B);
        assert_eq!(frame[geneve_off + 6], 0x0C);
    }

    #[test]
    fn wire_format_inner_ethernet() {
        let frame = build_test_frame(b"x", 100);
        let ieth = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN + GENEVE_BASE_HEADER_LEN;
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
        assert!(try_decap_geneve(&frame[..40], ETH_HEADER_LEN, 6081, Some(100)).is_none());
    }

    #[test]
    fn decap_rejects_non_udp_outer() {
        let mut frame = build_test_frame(b"data", 100);
        frame[ETH_HEADER_LEN + 9] = 6; // TCP
        assert!(try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(100)).is_none());
    }

    #[test]
    fn decap_rejects_wrong_geneve_version() {
        let mut frame = build_test_frame(b"data", 100);
        let geneve_off = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        frame[geneve_off] = 0x40; // version = 1
        assert!(try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(100)).is_none());
    }

    // --- Custom ports ---

    #[test]
    fn custom_ports() {
        let hdr = GeneveHeader::new(42);
        let mut frame = Vec::new();
        build_geneve_frame_into(
            &mut frame,
            &OUTER_SRC_MAC, &OUTER_DST_MAC,
            outer_src_ip(), outer_dst_ip(),
            5555, 6666, &hdr,
            &INNER_SRC_MAC, &INNER_DST_MAC,
            inner_src_ip(), inner_dst_ip(),
            8000, 8001, b"custom", 128,
        ).unwrap();

        assert!(try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(42)).is_none());
        let decap = try_decap_geneve(&frame, ETH_HEADER_LEN, 6666, Some(42)).unwrap();
        assert_eq!(decap.inner_src_port, 8000);
        assert_eq!(decap.inner_dst_port, 8001);
        assert_eq!(decap.payload, b"custom");
    }

    // --- TLV options roundtrip ---

    #[test]
    fn build_and_decap_with_options() {
        let mut hdr = GeneveHeader::new(500);
        hdr.options.push(GeneveTlvOption {
            class: 0x0100,
            option_type: 0x01,
            data: vec![0x11, 0x22, 0x33, 0x44],
        });
        hdr.options_len = 8;

        let mut frame = Vec::new();
        build_geneve_frame_into(
            &mut frame,
            &OUTER_SRC_MAC, &OUTER_DST_MAC,
            outer_src_ip(), outer_dst_ip(),
            6081, 6081, &hdr,
            &INNER_SRC_MAC, &INNER_DST_MAC,
            inner_src_ip(), inner_dst_ip(),
            9000, 9001, b"with opts", 64,
        ).unwrap();

        // Frame should be 8 bytes longer than no-options frame
        let no_opts_frame = build_test_frame(b"with opts", 500);
        assert_eq!(frame.len(), no_opts_frame.len() + 8);

        let decap = try_decap_geneve(&frame, ETH_HEADER_LEN, 6081, Some(500)).unwrap();
        assert_eq!(decap.payload, b"with opts");
        assert_eq!(decap.geneve_header.options.len(), 1);
        assert_eq!(decap.geneve_header.options[0].class, 0x0100);
        assert_eq!(decap.geneve_header.options[0].data, vec![0x11, 0x22, 0x33, 0x44]);
    }

    // --- Encap overhead constant ---

    #[test]
    fn encap_overhead_matches_frame_size() {
        let frame = build_test_frame(b"", 100);
        let base_frame = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;
        assert_eq!(frame.len(), base_frame + GENEVE_ENCAP_OVERHEAD);
    }

    // --- Synthetic performance benchmark ---

    #[test]
    fn perf_build_decap_cycle() {
        let payload = vec![0xAA; 64];
        let hdr = GeneveHeader::new(100);
        let mut buf = Vec::with_capacity(1500);
        let iterations = 10_000;

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            build_geneve_frame_into(
                &mut buf,
                &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src_ip(), outer_dst_ip(),
                6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src_ip(), inner_dst_ip(),
                12345, 9000, &payload, 64,
            ).unwrap();
            let _ = try_decap_geneve(&buf, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!(
            "[PERF] GENEVE build+decap: {} iterations in {:?} ({} ns/op)",
            iterations, elapsed, ns_per_op
        );
        assert!(ns_per_op < 10_000, "build+decap too slow: {} ns/op", ns_per_op);
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
            let cfg = GeneveConfig6::new(outer_dst(), 100);
            assert_eq!(cfg.remote_ip, outer_dst());
            assert_eq!(cfg.vni, 100);
            assert_eq!(cfg.remote_port, GENEVE_DEFAULT_PORT);
            assert_eq!(cfg.local_port, GENEVE_DEFAULT_PORT);
        }

        #[test]
        fn config6_builder() {
            let cfg = GeneveConfig6::new(outer_dst(), 200)
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
            GeneveConfig6::new(outer_dst(), GENEVE_VNI_MAX + 1);
        }

        #[test]
        fn build_and_decap_roundtrip() {
            let payload = b"hello GENEVE IPv6 tunnel";
            let hdr = GeneveHeader::new(100);
            let mut frame = Vec::new();
            let len = build_geneve_frame_into_v6(
                &mut frame,
                &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(),
                6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(),
                9000, 9001, payload, 64,
            ).unwrap();

            assert_eq!(frame.len(), len);
            // 14 (eth) + 40 (IPv6) + 8 (UDP) + 8 (GENEVE) + 14 (inner eth) + 20 (inner IPv4) + 8 (inner UDP) + payload
            let expected = 14 + 40 + 8 + 8 + 14 + 20 + 8 + payload.len();
            assert_eq!(len, expected);

            let decap = try_decap_geneve_v6(&frame, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
            assert_eq!(decap.inner_src_ip, inner_src());
            assert_eq!(decap.inner_dst_ip, inner_dst());
            assert_eq!(decap.inner_src_port, 9000);
            assert_eq!(decap.inner_dst_port, 9001);
            assert_eq!(decap.payload, payload);
            assert_eq!(decap.outer_src_ip, outer_src());
            assert_eq!(decap.geneve_header.vni, 100);
            assert_eq!(decap.inner_src_mac, INNER_SRC_MAC);
            assert_eq!(decap.inner_dst_mac, INNER_DST_MAC);
        }

        #[test]
        fn build_and_decap_with_options() {
            let options = vec![
                GeneveTlvOption { class: 0x0102, option_type: 1, data: vec![0xAA, 0xBB, 0xCC, 0xDD] },
            ];
            let opt_len = 4 + options[0].data.len(); // 4-byte TLV header + data
            let hdr = GeneveHeader {
                version: GENEVE_VERSION,
                options_len: opt_len,
                oam: false,
                critical: false,
                protocol_type: GENEVE_INNER_ETYPE_ETH,
                vni: 100,
                options: options.clone(),
            };
            let payload = b"with opts";
            let mut frame = Vec::new();
            build_geneve_frame_into_v6(
                &mut frame,
                &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(),
                6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(),
                9000, 9001, payload, 64,
            ).unwrap();

            let decap = try_decap_geneve_v6(&frame, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
            assert_eq!(decap.payload, payload);
            assert_eq!(decap.geneve_header.options.len(), 1);
            assert_eq!(decap.geneve_header.options[0].class, 0x0102);
            assert_eq!(decap.geneve_header.options[0].data, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        }

        #[test]
        fn wire_format_ethertype_is_ipv6() {
            let hdr = GeneveHeader::new(100);
            let mut frame = Vec::new();
            build_geneve_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 1, 2, b"x", 64,
            ).unwrap();
            let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
            assert_eq!(ethertype, ETH_TYPE_IPV6);
        }

        #[test]
        fn wire_format_ipv6_version() {
            let hdr = GeneveHeader::new(100);
            let mut frame = Vec::new();
            build_geneve_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 1, 2, b"x", 64,
            ).unwrap();
            assert_eq!(frame[ETH_HEADER_LEN] >> 4, 6);
        }

        #[test]
        fn wire_format_outer_udp_checksum_valid() {
            let hdr = GeneveHeader::new(100);
            let mut frame = Vec::new();
            build_geneve_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"checksum test", 64,
            ).unwrap();
            assert!(crate::verify_udp6_checksum(&frame));
        }

        #[test]
        fn decap_rejects_wrong_port() {
            let hdr = GeneveHeader::new(100);
            let mut frame = Vec::new();
            build_geneve_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            assert!(try_decap_geneve_v6(&frame, ETH_HEADER_LEN, 7000, Some(100)).is_none());
        }

        #[test]
        fn decap_rejects_wrong_vni() {
            let hdr = GeneveHeader::new(100);
            let mut frame = Vec::new();
            build_geneve_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            assert!(try_decap_geneve_v6(&frame, ETH_HEADER_LEN, 6081, Some(200)).is_none());
        }

        #[test]
        fn decap_accepts_any_vni_when_none() {
            let hdr = GeneveHeader::new(999);
            let mut frame = Vec::new();
            build_geneve_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, b"x", 64,
            ).unwrap();
            let decap = try_decap_geneve_v6(&frame, ETH_HEADER_LEN, 6081, None).unwrap();
            assert_eq!(decap.geneve_header.vni, 999);
        }

        #[test]
        fn build_empty_payload() {
            let hdr = GeneveHeader::new(100);
            let mut frame = Vec::new();
            let len = build_geneve_frame_into_v6(
                &mut frame, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                outer_src(), outer_dst(), 6081, 6081, &hdr,
                &INNER_SRC_MAC, &INNER_DST_MAC,
                inner_src(), inner_dst(), 9000, 9001, &[], 64,
            ).unwrap();
            // 14 + 40 + 8 + 8 + 14 + 20 + 8 = 112
            assert_eq!(len, 112);
            let decap = try_decap_geneve_v6(&frame, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
            assert!(decap.payload.is_empty());
        }

        #[test]
        fn encap_overhead_v6_is_correct() {
            // outer IPv6(40) + outer UDP(8) + GENEVE base(8) + inner Eth(14) = 70
            assert_eq!(GENEVE_ENCAP_OVERHEAD_V6, 70);
        }

        #[test]
        fn perf_build_decap_cycle_v6() {
            let payload = vec![0xAA; 64];
            let hdr = GeneveHeader::new(100);
            let mut buf = Vec::with_capacity(1500);
            let iterations = 10_000;

            let start = std::time::Instant::now();
            for _ in 0..iterations {
                build_geneve_frame_into_v6(
                    &mut buf, &OUTER_SRC_MAC, &OUTER_DST_MAC,
                    outer_src(), outer_dst(), 6081, 6081, &hdr,
                    &INNER_SRC_MAC, &INNER_DST_MAC,
                    inner_src(), inner_dst(), 12345, 9000, &payload, 64,
                ).unwrap();
                let _ = try_decap_geneve_v6(&buf, ETH_HEADER_LEN, 6081, Some(100)).unwrap();
            }
            let elapsed = start.elapsed();
            let ns_per_op = elapsed.as_nanos() / iterations as u128;
            eprintln!(
                "[PERF] GENEVE IPv6-outer build+decap: {} iterations in {:?} ({} ns/op)",
                iterations, elapsed, ns_per_op
            );
            assert!(ns_per_op < 10_000, "build+decap too slow: {} ns/op", ns_per_op);
        }
    }
}
