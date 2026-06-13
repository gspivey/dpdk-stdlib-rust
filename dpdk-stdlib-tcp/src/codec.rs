//! TCP codec types: flags, options, parsed segments, and frame parameters.
//! Also provides frame building, parsing, and checksum functions.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use dpdk::mbuf::Mbuf;
use dpdk_stdlib_net::ipv4_checksum;

use crate::error::TcpError;
use crate::seq::SeqNum;

// --- Constants ---

const ETH_HEADER_LEN: usize = 14;
const IPV4_HEADER_LEN: usize = 20;
const TCP_HEADER_LEN: usize = 20;
const MIN_FRAME_LEN: usize = ETH_HEADER_LEN + IPV4_HEADER_LEN + TCP_HEADER_LEN; // 54
const ETH_TYPE_IPV4: u16 = 0x0800;
const IP_PROTO_TCP: u8 = 6;

// --- TCP Flags ---

/// TCP header flags (bitfield).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TcpFlags(pub u8);

impl TcpFlags {
    pub const FIN: TcpFlags = TcpFlags(0x01);
    pub const SYN: TcpFlags = TcpFlags(0x02);
    pub const RST: TcpFlags = TcpFlags(0x04);
    pub const PSH: TcpFlags = TcpFlags(0x08);
    pub const ACK: TcpFlags = TcpFlags(0x10);
    pub const URG: TcpFlags = TcpFlags(0x20);

    #[inline]
    pub fn contains(self, flag: TcpFlags) -> bool {
        (self.0 & flag.0) == flag.0
    }

    #[inline]
    pub fn union(self, other: TcpFlags) -> TcpFlags {
        TcpFlags(self.0 | other.0)
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for TcpFlags {
    type Output = TcpFlags;
    fn bitor(self, rhs: Self) -> Self::Output {
        TcpFlags(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for TcpFlags {
    type Output = TcpFlags;
    fn bitand(self, rhs: Self) -> Self::Output {
        TcpFlags(self.0 & rhs.0)
    }
}

// --- TCP Options ---

/// Parsed TCP options from a segment header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TcpOptions {
    /// Maximum Segment Size (option kind 2).
    pub mss: Option<u16>,
    /// Window Scale shift count (option kind 3).
    pub window_scale: Option<u8>,
    /// SACK Permitted (option kind 4).
    pub sack_permitted: bool,
    /// Timestamps: (TSval, TSecr) (option kind 8).
    pub timestamps: Option<(u32, u32)>,
    /// SACK blocks (option kind 5). Each block is (left_edge, right_edge).
    pub sack_blocks: Vec<(u32, u32)>,
}

// --- Parsed TCP Segment ---

/// A parsed TCP segment extracted from a raw Ethernet frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTcpSegment {
    /// Source address (IP + port).
    pub src: SocketAddr,
    /// Destination address (IP + port).
    pub dst: SocketAddr,
    /// Sequence number.
    pub seq: SeqNum,
    /// Acknowledgment number.
    pub ack: SeqNum,
    /// TCP flags.
    pub flags: TcpFlags,
    /// Window size (raw, before scaling).
    pub window: u16,
    /// Parsed TCP options.
    pub options: TcpOptions,
    /// Payload bytes.
    pub payload: Vec<u8>,
}

// --- TCP Frame Parameters ---

/// Parameters for building a TCP frame (Eth + IPv4 + TCP).
#[derive(Debug, Clone)]
pub struct TcpFrameParams {
    /// Source MAC address.
    pub src_mac: [u8; 6],
    /// Destination MAC address.
    pub dst_mac: [u8; 6],
    /// Source address (IP + port).
    pub src: SocketAddr,
    /// Destination address (IP + port).
    pub dst: SocketAddr,
    /// Sequence number.
    pub seq: SeqNum,
    /// Acknowledgment number.
    pub ack: SeqNum,
    /// TCP flags.
    pub flags: TcpFlags,
    /// Window size (raw, before scaling).
    pub window: u16,
    /// TCP options to include.
    pub options: TcpOptions,
    /// Payload bytes.
    pub payload: Vec<u8>,
    /// TTL (default 64).
    pub ttl: u8,
}

impl Default for TcpFrameParams {
    fn default() -> Self {
        Self {
            src_mac: [0; 6],
            dst_mac: [0; 6],
            src: SocketAddr::from(([0, 0, 0, 0], 0)),
            dst: SocketAddr::from(([0, 0, 0, 0], 0)),
            seq: SeqNum(0),
            ack: SeqNum(0),
            flags: TcpFlags::default(),
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
            ttl: 64,
        }
    }
}

// --- Public API ---

/// Compute TCP checksum with parameterized pseudo-header.
/// `src_ip` and `dst_ip` are 4 bytes for IPv4 (extensible to 16 for IPv6).
pub fn tcp_checksum(src_ip: &[u8], dst_ip: &[u8], tcp_segment: &[u8]) -> u16 {
    let tcp_len = tcp_segment.len() as u32;
    let mut sum: u32 = 0;

    // Pseudo-header: src_ip + dst_ip + reserved(0) + protocol(6) + tcp_length
    for i in (0..src_ip.len()).step_by(2) {
        if i + 1 < src_ip.len() {
            sum = sum.wrapping_add(((src_ip[i] as u32) << 8) | (src_ip[i + 1] as u32));
        } else {
            sum = sum.wrapping_add((src_ip[i] as u32) << 8);
        }
    }
    for i in (0..dst_ip.len()).step_by(2) {
        if i + 1 < dst_ip.len() {
            sum = sum.wrapping_add(((dst_ip[i] as u32) << 8) | (dst_ip[i + 1] as u32));
        } else {
            sum = sum.wrapping_add((dst_ip[i] as u32) << 8);
        }
    }
    sum = sum.wrapping_add(IP_PROTO_TCP as u32);
    sum = sum.wrapping_add(tcp_len);

    // TCP segment (header + payload)
    for i in (0..tcp_segment.len()).step_by(2) {
        let word = if i + 1 < tcp_segment.len() {
            ((tcp_segment[i] as u32) << 8) | (tcp_segment[i + 1] as u32)
        } else {
            (tcp_segment[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    // Fold carry bits
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}

/// Compute MSS from MTU and IP header length.
#[inline]
pub fn compute_mss(mtu: u16, ip_header_len: u16) -> u16 {
    mtu.saturating_sub(ip_header_len).saturating_sub(TCP_HEADER_LEN as u16)
}

/// Build a complete TCP frame (Eth + IPv4 + TCP + payload) as a `Vec<u8>`.
pub fn build_tcp_frame(params: &TcpFrameParams) -> Result<Vec<u8>, TcpError> {
    let (src_ip, src_port) = extract_v4_addr(params.src)?;
    let (dst_ip, dst_port) = extract_v4_addr(params.dst)?;

    let options_bytes = serialize_options(&params.options, &params.flags);
    let tcp_header_len = TCP_HEADER_LEN + options_bytes.len();
    let ip_total_len = (IPV4_HEADER_LEN + tcp_header_len + params.payload.len()) as u16;
    let total_frame_len = ETH_HEADER_LEN + IPV4_HEADER_LEN + tcp_header_len + params.payload.len();

    let mut frame = vec![0u8; total_frame_len];
    write_frame(
        &mut frame,
        &params.src_mac,
        &params.dst_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        params.seq,
        params.ack,
        params.flags,
        params.window,
        &options_bytes,
        &params.payload,
        params.ttl,
        ip_total_len,
        tcp_header_len,
    );

    Ok(frame)
}

/// Build a TCP frame into a DPDK Mbuf (zero-copy path).
/// Produces byte-identical output to `build_tcp_frame`.
pub fn build_tcp_packet(mbuf: &mut Mbuf, params: &TcpFrameParams) -> Result<(), TcpError> {
    let (src_ip, src_port) = extract_v4_addr(params.src)?;
    let (dst_ip, dst_port) = extract_v4_addr(params.dst)?;

    let options_bytes = serialize_options(&params.options, &params.flags);
    let tcp_header_len = TCP_HEADER_LEN + options_bytes.len();
    let ip_total_len = (IPV4_HEADER_LEN + tcp_header_len + params.payload.len()) as u16;
    let total_frame_len = ETH_HEADER_LEN + IPV4_HEADER_LEN + tcp_header_len + params.payload.len();

    // Ensure mbuf has enough room. Set data_len to the available data room
    // so data_mut() returns the full writable buffer (fresh mbufs have data_len=0).
    let available_room = mbuf.buf_len().saturating_sub(mbuf.data_offset());
    if (available_room as usize) < total_frame_len {
        return Err(TcpError::InvalidPacket(format!(
            "mbuf buffer too small: need {} bytes, have {}",
            total_frame_len,
            available_room
        )));
    }
    mbuf.set_data_len(available_room);

    let data = mbuf.data_mut().ok_or_else(|| {
        TcpError::InvalidPacket("mbuf has no data region".to_string())
    })?;

    write_frame(
        &mut data[..total_frame_len],
        &params.src_mac,
        &params.dst_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        params.seq,
        params.ack,
        params.flags,
        params.window,
        &options_bytes,
        &params.payload,
        params.ttl,
        ip_total_len,
        tcp_header_len,
    );

    mbuf.set_data_len(total_frame_len as u16);
    mbuf.set_packet_len(total_frame_len as u32);

    Ok(())
}

/// Parse a raw Ethernet frame into a `ParsedTcpSegment`.
pub fn parse_tcp_packet(frame: &[u8]) -> Result<ParsedTcpSegment, TcpError> {
    if frame.len() < MIN_FRAME_LEN {
        return Err(TcpError::InvalidPacket(format!(
            "frame too short: {} bytes, minimum {}",
            frame.len(),
            MIN_FRAME_LEN
        )));
    }

    // Validate EtherType
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETH_TYPE_IPV4 {
        return Err(TcpError::InvalidPacket(format!(
            "not IPv4: ethertype 0x{:04x}",
            ethertype
        )));
    }

    // Parse IPv4 header
    let ip = ETH_HEADER_LEN;
    let ip_ihl = (frame[ip] & 0x0F) as usize;
    let ip_header_len = ip_ihl * 4;
    if frame[ip + 9] != IP_PROTO_TCP {
        return Err(TcpError::InvalidPacket(format!(
            "not TCP: protocol {}",
            frame[ip + 9]
        )));
    }

    let src_ip = Ipv4Addr::new(frame[ip + 12], frame[ip + 13], frame[ip + 14], frame[ip + 15]);
    let dst_ip = Ipv4Addr::new(frame[ip + 16], frame[ip + 17], frame[ip + 18], frame[ip + 19]);

    // Parse TCP header
    let tcp = ETH_HEADER_LEN + ip_header_len;
    if frame.len() < tcp + TCP_HEADER_LEN {
        return Err(TcpError::InvalidPacket(
            "frame too short for TCP header".to_string(),
        ));
    }

    let src_port = u16::from_be_bytes([frame[tcp], frame[tcp + 1]]);
    let dst_port = u16::from_be_bytes([frame[tcp + 2], frame[tcp + 3]]);
    let seq = SeqNum(u32::from_be_bytes([
        frame[tcp + 4],
        frame[tcp + 5],
        frame[tcp + 6],
        frame[tcp + 7],
    ]));
    let ack = SeqNum(u32::from_be_bytes([
        frame[tcp + 8],
        frame[tcp + 9],
        frame[tcp + 10],
        frame[tcp + 11],
    ]));

    let data_offset = (frame[tcp + 12] >> 4) as usize;
    if data_offset < 5 {
        return Err(TcpError::InvalidPacket(format!(
            "invalid data-offset: {}",
            data_offset
        )));
    }

    let tcp_header_len = data_offset * 4;
    if frame.len() < tcp + tcp_header_len {
        return Err(TcpError::InvalidPacket(format!(
            "frame too short for TCP options: need {} bytes at offset {}, have {}",
            tcp_header_len,
            tcp,
            frame.len() - tcp
        )));
    }

    let flags = TcpFlags(frame[tcp + 13]);
    let window = u16::from_be_bytes([frame[tcp + 14], frame[tcp + 15]]);

    // Parse TCP options
    let options = parse_options(&frame[tcp + TCP_HEADER_LEN..tcp + tcp_header_len]);

    // Payload
    let payload_start = tcp + tcp_header_len;
    let payload = frame[payload_start..].to_vec();

    Ok(ParsedTcpSegment {
        src: SocketAddr::V4(SocketAddrV4::new(src_ip, src_port)),
        dst: SocketAddr::V4(SocketAddrV4::new(dst_ip, dst_port)),
        seq,
        ack,
        flags,
        window,
        options,
        payload,
    })
}

// --- Internal helpers ---

fn extract_v4_addr(addr: SocketAddr) -> Result<(Ipv4Addr, u16), TcpError> {
    match addr {
        SocketAddr::V4(v4) => Ok((*v4.ip(), v4.port())),
        SocketAddr::V6(_) => Err(TcpError::InvalidPacket(
            "IPv6 not supported in TCP codec (use build_tcp6_frame)".to_string(),
        )),
    }
}

/// Serialize TCP options into bytes, padded to 4-byte boundary.
fn serialize_options(options: &TcpOptions, flags: &TcpFlags) -> Vec<u8> {
    let mut buf = Vec::new();

    // For SYN/SYN-ACK: include MSS, WScale, SACK-Perm, Timestamps
    let is_syn = flags.contains(TcpFlags::SYN);

    if let Some(mss) = options.mss {
        buf.push(2); // Kind
        buf.push(4); // Length
        buf.extend_from_slice(&mss.to_be_bytes());
    } else if is_syn {
        // SYN frames must include MSS (use default 1460)
        buf.push(2);
        buf.push(4);
        buf.extend_from_slice(&1460u16.to_be_bytes());
    }

    if let Some(ws) = options.window_scale {
        buf.push(1); // NOP for alignment
        buf.push(3); // Kind
        buf.push(3); // Length
        buf.push(ws);
    } else if is_syn {
        buf.push(1);
        buf.push(3);
        buf.push(3);
        buf.push(7); // default window scale 7
    }

    if options.sack_permitted || is_syn {
        buf.push(1); // NOP
        buf.push(1); // NOP
        buf.push(4); // Kind
        buf.push(2); // Length
    }

    if let Some((tsval, tsecr)) = options.timestamps {
        buf.push(1); // NOP
        buf.push(1); // NOP
        buf.push(8); // Kind
        buf.push(10); // Length
        buf.extend_from_slice(&tsval.to_be_bytes());
        buf.extend_from_slice(&tsecr.to_be_bytes());
    } else if is_syn {
        buf.push(1);
        buf.push(1);
        buf.push(8);
        buf.push(10);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
    }

    // SACK blocks (non-SYN only)
    if !options.sack_blocks.is_empty() {
        buf.push(1); // NOP
        buf.push(1); // NOP
        let block_len = 2 + (options.sack_blocks.len() * 8);
        buf.push(5); // Kind
        buf.push(block_len as u8); // Length
        for &(left, right) in &options.sack_blocks {
            buf.extend_from_slice(&left.to_be_bytes());
            buf.extend_from_slice(&right.to_be_bytes());
        }
    }

    // Pad to 4-byte boundary
    while buf.len() % 4 != 0 {
        buf.push(0); // EOL
    }

    buf
}

/// Parse TCP options from the option bytes.
fn parse_options(data: &[u8]) -> TcpOptions {
    let mut opts = TcpOptions::default();
    let mut i = 0;
    while i < data.len() {
        let kind = data[i];
        match kind {
            0 => break, // EOL
            1 => {
                i += 1; // NOP
            }
            2 => {
                // MSS
                if i + 4 <= data.len() && data[i + 1] == 4 {
                    opts.mss = Some(u16::from_be_bytes([data[i + 2], data[i + 3]]));
                    i += 4;
                } else {
                    break;
                }
            }
            3 => {
                // Window Scale
                if i + 3 <= data.len() && data[i + 1] == 3 {
                    opts.window_scale = Some(data[i + 2]);
                    i += 3;
                } else {
                    break;
                }
            }
            4 => {
                // SACK Permitted
                if i + 2 <= data.len() && data[i + 1] == 2 {
                    opts.sack_permitted = true;
                    i += 2;
                } else {
                    break;
                }
            }
            5 => {
                // SACK blocks
                if i + 2 > data.len() {
                    break;
                }
                let len = data[i + 1] as usize;
                if i + len > data.len() || len < 2 {
                    break;
                }
                let block_count = (len - 2) / 8;
                for b in 0..block_count {
                    let off = i + 2 + b * 8;
                    if off + 8 <= data.len() {
                        let left = u32::from_be_bytes([
                            data[off],
                            data[off + 1],
                            data[off + 2],
                            data[off + 3],
                        ]);
                        let right = u32::from_be_bytes([
                            data[off + 4],
                            data[off + 5],
                            data[off + 6],
                            data[off + 7],
                        ]);
                        opts.sack_blocks.push((left, right));
                    }
                }
                i += len;
            }
            8 => {
                // Timestamps
                if i + 10 <= data.len() && data[i + 1] == 10 {
                    let tsval = u32::from_be_bytes([
                        data[i + 2],
                        data[i + 3],
                        data[i + 4],
                        data[i + 5],
                    ]);
                    let tsecr = u32::from_be_bytes([
                        data[i + 6],
                        data[i + 7],
                        data[i + 8],
                        data[i + 9],
                    ]);
                    opts.timestamps = Some((tsval, tsecr));
                    i += 10;
                } else {
                    break;
                }
            }
            _ => {
                // Unknown option — skip using length field
                if i + 1 >= data.len() {
                    break;
                }
                let len = data[i + 1] as usize;
                if len < 2 || i + len > data.len() {
                    break;
                }
                i += len;
            }
        }
    }
    opts
}

/// Write a complete TCP frame into a mutable buffer slice.
/// Shared between `build_tcp_frame` and `build_tcp_packet`.
#[allow(clippy::too_many_arguments)]
fn write_frame(
    frame: &mut [u8],
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    seq: SeqNum,
    ack: SeqNum,
    flags: TcpFlags,
    window: u16,
    options_bytes: &[u8],
    payload: &[u8],
    ttl: u8,
    ip_total_len: u16,
    tcp_header_len: usize,
) {
    let src_ip_bytes = src_ip.octets();
    let dst_ip_bytes = dst_ip.octets();

    // === Ethernet Header (14 bytes) ===
    frame[0..6].copy_from_slice(dst_mac);
    frame[6..12].copy_from_slice(src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

    // === IPv4 Header (20 bytes) ===
    let ip = ETH_HEADER_LEN;
    frame[ip] = 0x45; // Version(4) + IHL(5)
    frame[ip + 1] = 0x00; // DSCP + ECN
    frame[ip + 2..ip + 4].copy_from_slice(&ip_total_len.to_be_bytes());
    frame[ip + 4..ip + 6].copy_from_slice(&[0x00, 0x00]); // Identification
    frame[ip + 6..ip + 8].copy_from_slice(&[0x40, 0x00]); // Flags (DF) + Fragment Offset
    frame[ip + 8] = ttl;
    frame[ip + 9] = IP_PROTO_TCP;
    frame[ip + 10..ip + 12].copy_from_slice(&[0x00, 0x00]); // Checksum placeholder
    frame[ip + 12..ip + 16].copy_from_slice(&src_ip_bytes);
    frame[ip + 16..ip + 20].copy_from_slice(&dst_ip_bytes);

    // IPv4 checksum
    let ip_cksum = ipv4_checksum(&frame[ip..ip + IPV4_HEADER_LEN]);
    frame[ip + 10..ip + 12].copy_from_slice(&ip_cksum.to_be_bytes());

    // === TCP Header (20+ bytes) ===
    let tcp = ETH_HEADER_LEN + IPV4_HEADER_LEN;
    frame[tcp..tcp + 2].copy_from_slice(&src_port.to_be_bytes());
    frame[tcp + 2..tcp + 4].copy_from_slice(&dst_port.to_be_bytes());
    frame[tcp + 4..tcp + 8].copy_from_slice(&seq.0.to_be_bytes());
    frame[tcp + 8..tcp + 12].copy_from_slice(&ack.0.to_be_bytes());
    let data_offset = (tcp_header_len / 4) as u8;
    frame[tcp + 12] = data_offset << 4; // Data offset + reserved
    frame[tcp + 13] = flags.0;
    frame[tcp + 14..tcp + 16].copy_from_slice(&window.to_be_bytes());
    frame[tcp + 16..tcp + 18].copy_from_slice(&[0x00, 0x00]); // Checksum placeholder
    frame[tcp + 18..tcp + 20].copy_from_slice(&[0x00, 0x00]); // Urgent pointer

    // Options
    if !options_bytes.is_empty() {
        frame[tcp + TCP_HEADER_LEN..tcp + TCP_HEADER_LEN + options_bytes.len()]
            .copy_from_slice(options_bytes);
    }

    // Payload
    let payload_start = tcp + tcp_header_len;
    if !payload.is_empty() {
        frame[payload_start..payload_start + payload.len()].copy_from_slice(payload);
    }

    // TCP checksum (over entire TCP segment: header + options + payload)
    let tcp_segment = &frame[tcp..payload_start + payload.len()];
    let cksum = tcp_checksum(&src_ip_bytes, &dst_ip_bytes, tcp_segment);
    frame[tcp + 16..tcp + 18].copy_from_slice(&cksum.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_operations() {
        let syn_ack = TcpFlags::SYN | TcpFlags::ACK;
        assert!(syn_ack.contains(TcpFlags::SYN));
        assert!(syn_ack.contains(TcpFlags::ACK));
        assert!(!syn_ack.contains(TcpFlags::FIN));
        assert!(!syn_ack.is_empty());
        assert!(TcpFlags::default().is_empty());
    }

    #[test]
    fn flags_union() {
        let a = TcpFlags::SYN;
        let b = TcpFlags::ACK;
        let c = a.union(b);
        assert!(c.contains(TcpFlags::SYN));
        assert!(c.contains(TcpFlags::ACK));
    }

    #[test]
    fn default_options() {
        let opts = TcpOptions::default();
        assert_eq!(opts.mss, None);
        assert_eq!(opts.window_scale, None);
        assert!(!opts.sack_permitted);
        assert_eq!(opts.timestamps, None);
        assert!(opts.sack_blocks.is_empty());
    }

    #[test]
    fn default_frame_params() {
        let params = TcpFrameParams::default();
        assert_eq!(params.ttl, 64);
        assert!(params.flags.is_empty());
        assert!(params.payload.is_empty());
    }

    #[test]
    fn parsed_segment_fields() {
        let seg = ParsedTcpSegment {
            src: SocketAddr::from(([10, 0, 0, 1], 1234)),
            dst: SocketAddr::from(([10, 0, 0, 2], 80)),
            seq: SeqNum(1000),
            ack: SeqNum(2000),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions {
                mss: Some(1460),
                window_scale: Some(7),
                sack_permitted: true,
                timestamps: Some((12345, 67890)),
                sack_blocks: vec![],
            },
            payload: vec![],
        };
        assert!(seg.flags.contains(TcpFlags::SYN));
        assert_eq!(seg.options.mss, Some(1460));
    }

    #[test]
    fn compute_mss_standard() {
        assert_eq!(compute_mss(1500, 20), 1460);
        assert_eq!(compute_mss(1500, 40), 1440); // IPv6-sized header
        assert_eq!(compute_mss(576, 20), 536);
    }

    #[test]
    fn compute_mss_saturates() {
        assert_eq!(compute_mss(20, 20), 0);
        assert_eq!(compute_mss(0, 20), 0);
    }

    #[test]
    fn build_and_parse_roundtrip() {
        let params = TcpFrameParams {
            src_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            dst_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            src: SocketAddr::from(([10, 0, 0, 1], 12345)),
            dst: SocketAddr::from(([10, 0, 0, 2], 80)),
            seq: SeqNum(1000),
            ack: SeqNum(2000),
            flags: TcpFlags::ACK | TcpFlags::PSH,
            window: 32768,
            options: TcpOptions::default(),
            payload: b"Hello, TCP!".to_vec(),
            ttl: 64,
        };

        let frame = build_tcp_frame(&params).unwrap();
        let parsed = parse_tcp_packet(&frame).unwrap();

        assert_eq!(parsed.src, params.src);
        assert_eq!(parsed.dst, params.dst);
        assert_eq!(parsed.seq, params.seq);
        assert_eq!(parsed.ack, params.ack);
        assert_eq!(parsed.flags, params.flags);
        assert_eq!(parsed.window, params.window);
        assert_eq!(parsed.payload, params.payload);
    }

    #[test]
    fn build_syn_includes_required_options() {
        let params = TcpFrameParams {
            src_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            dst_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            src: SocketAddr::from(([10, 0, 0, 1], 12345)),
            dst: SocketAddr::from(([10, 0, 0, 2], 80)),
            seq: SeqNum(100),
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: 65535,
            options: TcpOptions::default(),
            payload: vec![],
            ttl: 64,
        };

        let frame = build_tcp_frame(&params).unwrap();
        let parsed = parse_tcp_packet(&frame).unwrap();

        // SYN must include MSS, WScale, SACK-Perm, Timestamps
        assert!(parsed.options.mss.is_some());
        assert!(parsed.options.window_scale.is_some());
        assert!(parsed.options.sack_permitted);
        assert!(parsed.options.timestamps.is_some());
    }

    #[test]
    fn tcp_checksum_validates() {
        let params = TcpFrameParams {
            src_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            dst_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            src: SocketAddr::from(([192, 168, 1, 1], 5000)),
            dst: SocketAddr::from(([192, 168, 1, 2], 80)),
            seq: SeqNum(42),
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: 65535,
            options: TcpOptions {
                mss: Some(1460),
                window_scale: Some(7),
                sack_permitted: true,
                timestamps: Some((12345, 0)),
                ..Default::default()
            },
            payload: vec![],
            ttl: 64,
        };

        let frame = build_tcp_frame(&params).unwrap();

        // Verify checksum: recomputing over the TCP segment (with checksum field included)
        // should yield 0 (or 0xFFFF depending on convention — standard is that
        // the sum of all 16-bit words including checksum yields 0xFFFF)
        let ip = ETH_HEADER_LEN;
        let src_ip = &frame[ip + 12..ip + 16];
        let dst_ip = &frame[ip + 16..ip + 20];
        let tcp_start = ETH_HEADER_LEN + IPV4_HEADER_LEN;
        let tcp_segment = &frame[tcp_start..];

        let check = tcp_checksum(src_ip, dst_ip, tcp_segment);
        // When including a valid checksum field, result should be 0
        assert_eq!(check, 0);
    }

    #[test]
    fn parse_rejects_short_frame() {
        let frame = [0u8; 53]; // less than 54
        assert!(parse_tcp_packet(&frame).is_err());
    }

    #[test]
    fn parse_rejects_invalid_data_offset() {
        // Build a valid frame then corrupt the data offset
        let params = TcpFrameParams {
            src_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            dst_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            src: SocketAddr::from(([10, 0, 0, 1], 1234)),
            dst: SocketAddr::from(([10, 0, 0, 2], 80)),
            seq: SeqNum(0),
            ack: SeqNum(0),
            flags: TcpFlags::ACK,
            window: 1024,
            options: TcpOptions::default(),
            payload: vec![],
            ttl: 64,
        };
        let mut frame = build_tcp_frame(&params).unwrap();
        // Set data offset to 4 (invalid, must be >= 5)
        let tcp_off = ETH_HEADER_LEN + IPV4_HEADER_LEN;
        frame[tcp_off + 12] = 4 << 4;
        assert!(parse_tcp_packet(&frame).is_err());
    }
}
