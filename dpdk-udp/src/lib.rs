//! DPDK-accelerated UDP socket implementation
//!
//! This crate provides a drop-in replacement for `std::net::UdpSocket` that uses
//! DPDK for high-performance packet I/O, bypassing the kernel network stack.
//!
//! ## Features
//!
//! - **UDP Socket API** - Drop-in replacement for `std::net::UdpSocket`
//! - **ARP Protocol** - Automatic address resolution for real network communication
//! - **ICMP Protocol** - Echo reply (ping) support for network diagnostics
//! - **Multiple Backends** - DPDK, AF_PACKET raw sockets, or AF_PACKET with PACKET_MMAP
//! - **Runtime Backend Selection** - Choose backend at runtime based on availability

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicU16, Ordering};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dpdk::{Mbuf, Mempool, Port};
use dpdk::port::{MacAddress, PortConfig};
use dpdk::mbuf::MempoolConfig;

pub use dpdk::port::{RxOffload as HwRxOffload, TxOffload as HwTxOffload};

use thiserror::Error;

// ============================================================================
// Submodules
// ============================================================================

pub mod arp;
pub mod icmp;
pub mod backend;
pub mod backend_dpdk;
pub mod backend_raw;
pub mod ring_buffer;
pub mod ring;
pub mod topology;

pub use arp::{ArpCache, ArpHandler, ArpPacket};
pub use icmp::{IcmpHandler, IcmpPacket};
pub use backend::{PacketBackend, BackendConfig, BackendType};
pub use backend_dpdk::DpdkBackend;
pub use backend_raw::RawSocketBackend;
pub use ring::{SpscRing, MpscRing};
pub use topology::{TopologyConfig, TopologyPlan, TopologySource, MultiCoreTopology, ProcessedPacket, TxFrame};

// ============================================================================
// Error Types
// ============================================================================

#[derive(Error, Debug)]
pub enum UdpError {
    #[error("Invalid packet format")]
    InvalidPacket,
    #[error("Checksum mismatch")]
    ChecksumMismatch,
    #[error("Packet too short: expected at least {expected}, got {actual}")]
    PacketTooShort { expected: usize, actual: usize },
    #[error("Payload too large: max {max}, got {actual}")]
    PayloadTooLarge { max: usize, actual: usize },
    #[error("Port not started")]
    PortNotStarted,
    #[error("No destination address (socket not connected)")]
    NotConnected,
    #[error("IPv6 not supported")]
    Ipv6NotSupported,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("DPDK error: {0}")]
    Dpdk(#[from] dpdk::DpdkError),
}

pub type UdpResult<T> = Result<T, UdpError>;

// ============================================================================
// Constants
// ============================================================================

/// Maximum UDP payload size (MTU 1500 - IP header 20 - UDP header 8)
pub const MAX_UDP_PAYLOAD: usize = 1472;

/// Ethernet header size
pub const ETH_HEADER_LEN: usize = 14;

/// IPv4 header size (no options)
pub const IPV4_HEADER_LEN: usize = 20;

/// UDP header size
pub const UDP_HEADER_LEN: usize = 8;

/// Total header overhead
pub const TOTAL_HEADER_LEN: usize = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;

/// Ethernet type for IPv4
pub const ETH_TYPE_IPV4: u16 = 0x0800;

/// IP protocol number for UDP
pub const IP_PROTO_UDP: u8 = 17;

// ============================================================================
// Packet Building
// ============================================================================

/// Calculate IPv4 header checksum
pub fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    // Sum all 16-bit words
    for i in (0..header.len()).step_by(2) {
        let word = if i + 1 < header.len() {
            ((header[i] as u32) << 8) | (header[i + 1] as u32)
        } else {
            (header[i] as u32) << 8
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

/// Calculate UDP checksum (optional for IPv4, but recommended)
pub fn udp_checksum(
    src_ip: &[u8; 4],
    dst_ip: &[u8; 4],
    udp_header: &[u8],
    payload: &[u8],
) -> u16 {
    let mut sum: u32 = 0;

    // Pseudo-header
    sum = sum.wrapping_add(((src_ip[0] as u32) << 8) | (src_ip[1] as u32));
    sum = sum.wrapping_add(((src_ip[2] as u32) << 8) | (src_ip[3] as u32));
    sum = sum.wrapping_add(((dst_ip[0] as u32) << 8) | (dst_ip[1] as u32));
    sum = sum.wrapping_add(((dst_ip[2] as u32) << 8) | (dst_ip[3] as u32));
    sum = sum.wrapping_add(IP_PROTO_UDP as u32); // Protocol
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u32;
    sum = sum.wrapping_add(udp_len);

    // UDP header (skip checksum field at bytes 6-7)
    for i in (0..udp_header.len()).step_by(2) {
        if i == 6 {
            continue; // Skip checksum field
        }
        let word = if i + 1 < udp_header.len() {
            ((udp_header[i] as u32) << 8) | (udp_header[i + 1] as u32)
        } else {
            (udp_header[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    // Payload
    for i in (0..payload.len()).step_by(2) {
        let word = if i + 1 < payload.len() {
            ((payload[i] as u32) << 8) | (payload[i + 1] as u32)
        } else {
            (payload[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    // Fold and complement
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    let result = !(sum as u16);
    // UDP checksum of 0 means "no checksum", so use 0xFFFF instead
    if result == 0 { 0xFFFF } else { result }
}

/// Build a complete UDP packet in an mbuf
pub fn build_udp_packet(
    mbuf: &mut Mbuf,
    src_mac: &MacAddress,
    dst_mac: &MacAddress,
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    ttl: u8,
) -> UdpResult<()> {
    if payload.len() > MAX_UDP_PAYLOAD {
        return Err(UdpError::PayloadTooLarge {
            max: MAX_UDP_PAYLOAD,
            actual: payload.len(),
        });
    }

    let total_len = TOTAL_HEADER_LEN + payload.len();
    let ip_total_len = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;

    // Get mutable access to the mbuf data
    let data = mbuf.data_mut().ok_or(UdpError::InvalidPacket)?;

    if data.len() < total_len {
        return Err(UdpError::PayloadTooLarge {
            max: data.len() - TOTAL_HEADER_LEN,
            actual: payload.len(),
        });
    }

    let src_ip_bytes = src_ip.octets();
    let dst_ip_bytes = dst_ip.octets();

    // === Ethernet Header (14 bytes) ===
    data[0..6].copy_from_slice(&dst_mac.octets());      // Destination MAC
    data[6..12].copy_from_slice(&src_mac.octets());     // Source MAC
    data[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes()); // EtherType

    // === IPv4 Header (20 bytes) ===
    let ip_header_start = ETH_HEADER_LEN;
    data[ip_header_start] = 0x45;                       // Version (4) + IHL (5)
    data[ip_header_start + 1] = 0x00;                   // DSCP + ECN
    data[ip_header_start + 2..ip_header_start + 4]
        .copy_from_slice(&ip_total_len.to_be_bytes());  // Total Length
    data[ip_header_start + 4..ip_header_start + 6]
        .copy_from_slice(&[0x00, 0x00]);                // Identification
    data[ip_header_start + 6..ip_header_start + 8]
        .copy_from_slice(&[0x40, 0x00]);                // Flags (DF) + Fragment Offset
    data[ip_header_start + 8] = ttl;                    // TTL
    data[ip_header_start + 9] = IP_PROTO_UDP;           // Protocol
    data[ip_header_start + 10..ip_header_start + 12]
        .copy_from_slice(&[0x00, 0x00]);                // Checksum (placeholder)
    data[ip_header_start + 12..ip_header_start + 16]
        .copy_from_slice(&src_ip_bytes);                // Source IP
    data[ip_header_start + 16..ip_header_start + 20]
        .copy_from_slice(&dst_ip_bytes);                // Destination IP

    // Calculate and set IP checksum
    let ip_checksum = ipv4_checksum(&data[ip_header_start..ip_header_start + IPV4_HEADER_LEN]);
    data[ip_header_start + 10..ip_header_start + 12]
        .copy_from_slice(&ip_checksum.to_be_bytes());

    // === UDP Header (8 bytes) ===
    let udp_header_start = ETH_HEADER_LEN + IPV4_HEADER_LEN;
    data[udp_header_start..udp_header_start + 2]
        .copy_from_slice(&src_port.to_be_bytes());      // Source Port
    data[udp_header_start + 2..udp_header_start + 4]
        .copy_from_slice(&dst_port.to_be_bytes());      // Destination Port
    data[udp_header_start + 4..udp_header_start + 6]
        .copy_from_slice(&udp_len.to_be_bytes());       // Length
    data[udp_header_start + 6..udp_header_start + 8]
        .copy_from_slice(&[0x00, 0x00]);                // Checksum (placeholder)

    // === Payload ===
    let payload_start = TOTAL_HEADER_LEN;
    data[payload_start..payload_start + payload.len()].copy_from_slice(payload);

    // Calculate and set UDP checksum
    let udp_header = &data[udp_header_start..udp_header_start + UDP_HEADER_LEN];
    let udp_cksum = udp_checksum(&src_ip_bytes, &dst_ip_bytes, udp_header, payload);
    data[udp_header_start + 6..udp_header_start + 8]
        .copy_from_slice(&udp_cksum.to_be_bytes());

    // Set packet lengths in mbuf metadata
    mbuf.set_data_len(total_len as u16);
    mbuf.set_packet_len(total_len as u32);

    Ok(())
}

/// Build a complete UDP packet as a raw Ethernet frame (backend-agnostic).
///
/// Unlike `build_udp_packet()` which writes into a DPDK mbuf, this function
/// returns a `Vec<u8>` containing the complete Ethernet frame. This can be used
/// with any `PacketBackend` implementation.
///
/// The frame includes: Ethernet header + IPv4 header + UDP header + payload.
/// All checksums (IP and UDP) are calculated.
pub fn build_udp_frame(
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    ttl: u8,
) -> UdpResult<Vec<u8>> {
    if payload.len() > MAX_UDP_PAYLOAD {
        return Err(UdpError::PayloadTooLarge {
            max: MAX_UDP_PAYLOAD,
            actual: payload.len(),
        });
    }

    let total_len = TOTAL_HEADER_LEN + payload.len();
    let ip_total_len = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;

    let mut frame = vec![0u8; total_len];

    let src_ip_bytes = src_ip.octets();
    let dst_ip_bytes = dst_ip.octets();

    // === Ethernet Header (14 bytes) ===
    frame[0..6].copy_from_slice(dst_mac);
    frame[6..12].copy_from_slice(src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

    // === IPv4 Header (20 bytes) ===
    let ip = ETH_HEADER_LEN;
    frame[ip] = 0x45;                                           // Version (4) + IHL (5)
    frame[ip + 1] = 0x00;                                       // DSCP + ECN
    frame[ip + 2..ip + 4].copy_from_slice(&ip_total_len.to_be_bytes()); // Total Length
    frame[ip + 4..ip + 6].copy_from_slice(&[0x00, 0x00]);       // Identification
    frame[ip + 6..ip + 8].copy_from_slice(&[0x40, 0x00]);       // Flags (DF) + Fragment Offset
    frame[ip + 8] = ttl;                                         // TTL
    frame[ip + 9] = IP_PROTO_UDP;                                // Protocol
    frame[ip + 10..ip + 12].copy_from_slice(&[0x00, 0x00]);     // Checksum (placeholder)
    frame[ip + 12..ip + 16].copy_from_slice(&src_ip_bytes);     // Source IP
    frame[ip + 16..ip + 20].copy_from_slice(&dst_ip_bytes);     // Destination IP

    // Calculate and set IP checksum
    let ip_cksum = ipv4_checksum(&frame[ip..ip + IPV4_HEADER_LEN]);
    frame[ip + 10..ip + 12].copy_from_slice(&ip_cksum.to_be_bytes());

    // === UDP Header (8 bytes) ===
    let udp = ETH_HEADER_LEN + IPV4_HEADER_LEN;
    frame[udp..udp + 2].copy_from_slice(&src_port.to_be_bytes());   // Source Port
    frame[udp + 2..udp + 4].copy_from_slice(&dst_port.to_be_bytes()); // Destination Port
    frame[udp + 4..udp + 6].copy_from_slice(&udp_len.to_be_bytes()); // Length
    frame[udp + 6..udp + 8].copy_from_slice(&[0x00, 0x00]);         // Checksum (placeholder)

    // === Payload ===
    frame[TOTAL_HEADER_LEN..].copy_from_slice(payload);

    // Calculate and set UDP checksum
    let udp_cksum = udp_checksum(
        &src_ip_bytes,
        &dst_ip_bytes,
        &frame[udp..udp + UDP_HEADER_LEN],
        payload,
    );
    frame[udp + 6..udp + 8].copy_from_slice(&udp_cksum.to_be_bytes());

    Ok(frame)
}

/// Build a UDP frame into a caller-provided buffer, avoiding per-packet heap allocation.
///
/// The buffer will be resized (via `resize`) to exactly fit the frame.
/// Callers should reuse the same `Vec` across calls so the allocation is amortized.
///
/// Returns the number of bytes written (== total frame length).
pub fn build_udp_frame_into(
    out: &mut Vec<u8>,
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    ttl: u8,
) -> UdpResult<usize> {
    if payload.len() > MAX_UDP_PAYLOAD {
        return Err(UdpError::PayloadTooLarge {
            max: MAX_UDP_PAYLOAD,
            actual: payload.len(),
        });
    }

    let total_len = TOTAL_HEADER_LEN + payload.len();
    let ip_total_len = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;

    // Resize reuses existing capacity — no allocation if capacity >= total_len
    out.resize(total_len, 0);

    let src_ip_bytes = src_ip.octets();
    let dst_ip_bytes = dst_ip.octets();

    // === Ethernet Header (14 bytes) ===
    out[0..6].copy_from_slice(dst_mac);
    out[6..12].copy_from_slice(src_mac);
    out[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

    // === IPv4 Header (20 bytes) ===
    let ip = ETH_HEADER_LEN;
    out[ip] = 0x45;
    out[ip + 1] = 0x00;
    out[ip + 2..ip + 4].copy_from_slice(&ip_total_len.to_be_bytes());
    out[ip + 4..ip + 6].copy_from_slice(&[0x00, 0x00]);
    out[ip + 6..ip + 8].copy_from_slice(&[0x40, 0x00]);
    out[ip + 8] = ttl;
    out[ip + 9] = IP_PROTO_UDP;
    out[ip + 10..ip + 12].copy_from_slice(&[0x00, 0x00]);
    out[ip + 12..ip + 16].copy_from_slice(&src_ip_bytes);
    out[ip + 16..ip + 20].copy_from_slice(&dst_ip_bytes);

    let ip_cksum = ipv4_checksum(&out[ip..ip + IPV4_HEADER_LEN]);
    out[ip + 10..ip + 12].copy_from_slice(&ip_cksum.to_be_bytes());

    // === UDP Header (8 bytes) ===
    let udp_off = ETH_HEADER_LEN + IPV4_HEADER_LEN;
    out[udp_off..udp_off + 2].copy_from_slice(&src_port.to_be_bytes());
    out[udp_off + 2..udp_off + 4].copy_from_slice(&dst_port.to_be_bytes());
    out[udp_off + 4..udp_off + 6].copy_from_slice(&udp_len.to_be_bytes());
    out[udp_off + 6..udp_off + 8].copy_from_slice(&[0x00, 0x00]);

    // === Payload ===
    out[TOTAL_HEADER_LEN..].copy_from_slice(payload);

    // UDP checksum
    let udp_cksum = udp_checksum(
        &src_ip_bytes,
        &dst_ip_bytes,
        &out[udp_off..udp_off + UDP_HEADER_LEN],
        payload,
    );
    out[udp_off + 6..udp_off + 8].copy_from_slice(&udp_cksum.to_be_bytes());

    Ok(total_len)
}

// ============================================================================
// Packet Parsing
// ============================================================================

/// Parsed UDP packet information
#[derive(Debug, Clone)]
pub struct ParsedUdpPacket {
    /// Source MAC address
    pub src_mac: [u8; 6],
    /// Destination MAC address
    pub dst_mac: [u8; 6],
    /// Source IP address
    pub src_ip: Ipv4Addr,
    /// Destination IP address
    pub dst_ip: Ipv4Addr,
    /// Source port
    pub src_port: u16,
    /// Destination port
    pub dst_port: u16,
    /// Payload data
    pub payload: Vec<u8>,
}

/// Zero-copy parsed UDP packet that borrows payload from the frame slice.
///
/// Used on the hot recv path to avoid per-packet heap allocation.
#[derive(Debug)]
pub struct ParsedUdpPacketRef<'a> {
    pub src_mac: [u8; 6],
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    /// Payload borrowed from the original frame — no heap allocation.
    pub payload: &'a [u8],
}

/// Parse a raw Ethernet frame containing a UDP packet
///
/// Returns None if the packet is not a valid UDP/IPv4 packet
pub fn parse_udp_packet(frame: &[u8]) -> Option<ParsedUdpPacket> {
    // Minimum size check
    if frame.len() < TOTAL_HEADER_LEN {
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

    // Check IP version (should be 4)
    let version = (ip_header[0] >> 4) & 0x0F;
    if version != 4 {
        return None;
    }

    // Get IP header length (in 32-bit words)
    let ihl = (ip_header[0] & 0x0F) as usize;
    let ip_header_len = ihl * 4;
    if ip_header_len < 20 {
        return None;
    }

    // Check protocol (should be UDP = 17)
    let protocol = ip_header[9];
    if protocol != IP_PROTO_UDP {
        return None;
    }

    // Extract IP addresses
    let src_ip = Ipv4Addr::new(
        ip_header[12], ip_header[13], ip_header[14], ip_header[15]
    );
    let dst_ip = Ipv4Addr::new(
        ip_header[16], ip_header[17], ip_header[18], ip_header[19]
    );

    // Parse UDP header (starts after IP header)
    let udp_start = ETH_HEADER_LEN + ip_header_len;
    if frame.len() < udp_start + UDP_HEADER_LEN {
        return None;
    }

    let udp_header = &frame[udp_start..];
    let src_port = u16::from_be_bytes([udp_header[0], udp_header[1]]);
    let dst_port = u16::from_be_bytes([udp_header[2], udp_header[3]]);
    let udp_len = u16::from_be_bytes([udp_header[4], udp_header[5]]) as usize;

    // Validate UDP length
    if udp_len < UDP_HEADER_LEN || frame.len() < udp_start + udp_len {
        return None;
    }

    // Extract payload
    let payload_start = udp_start + UDP_HEADER_LEN;
    let payload_len = udp_len - UDP_HEADER_LEN;
    let payload = frame[payload_start..payload_start + payload_len].to_vec();

    Some(ParsedUdpPacket {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        payload,
    })
}

/// Zero-copy UDP packet parser that borrows payload from the frame slice.
///
/// Identical validation to `parse_udp_packet` but returns a reference into the
/// original frame data, eliminating the per-packet `Vec<u8>` heap allocation.
pub fn parse_udp_packet_ref(frame: &[u8]) -> Option<ParsedUdpPacketRef<'_>> {
    if frame.len() < TOTAL_HEADER_LEN {
        return None;
    }

    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETH_TYPE_IPV4 {
        return None;
    }

    let ip_header = &frame[ETH_HEADER_LEN..];
    let version = (ip_header[0] >> 4) & 0x0F;
    if version != 4 {
        return None;
    }
    let ihl = (ip_header[0] & 0x0F) as usize;
    let ip_header_len = ihl * 4;
    if ip_header_len < 20 {
        return None;
    }
    if ip_header[9] != IP_PROTO_UDP {
        return None;
    }

    let src_ip = Ipv4Addr::new(ip_header[12], ip_header[13], ip_header[14], ip_header[15]);
    let dst_ip = Ipv4Addr::new(ip_header[16], ip_header[17], ip_header[18], ip_header[19]);

    let udp_start = ETH_HEADER_LEN + ip_header_len;
    if frame.len() < udp_start + UDP_HEADER_LEN {
        return None;
    }

    let udp_header = &frame[udp_start..];
    let src_port = u16::from_be_bytes([udp_header[0], udp_header[1]]);
    let dst_port = u16::from_be_bytes([udp_header[2], udp_header[3]]);
    let udp_len = u16::from_be_bytes([udp_header[4], udp_header[5]]) as usize;

    if udp_len < UDP_HEADER_LEN || frame.len() < udp_start + udp_len {
        return None;
    }

    let payload_start = udp_start + UDP_HEADER_LEN;
    let payload_len = udp_len - UDP_HEADER_LEN;

    Some(ParsedUdpPacketRef {
        src_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
    })
}

/// Parse UDP packet from an mbuf
pub fn parse_udp_from_mbuf(mbuf: &Mbuf) -> Option<ParsedUdpPacket> {
    let data = mbuf.data()?;
    parse_udp_packet(data)
}

// ============================================================================
// DPDK Resources (shared across sockets)
// ============================================================================

/// Shared DPDK resources for a network interface
struct DpdkResources {
    /// EAL handle — must stay alive for the lifetime of all DPDK resources.
    /// `Eal::drop()` calls `rte_eal_cleanup()`, which tears down the memory subsystem.
    /// If dropped early, subsequent DPDK calls (e.g. `rte_pktmbuf_pool_create`) segfault
    /// because `rte_config->mem_config` becomes NULL.
    _eal: dpdk::Eal,
    port: Port,
    mempool: Mempool,
    src_mac: MacAddress,
    /// Shared ARP cache
    arp_cache: Arc<ArpCache>,
}

/// Global DPDK resources (initialized once per port)
static DPDK_RESOURCES: Mutex<Option<Arc<DpdkResources>>> = Mutex::new(None);

/// Seed an ARP cache from the kernel's `/proc/net/arp` table.
///
/// On Linux, the kernel maintains ARP entries for interfaces it manages (e.g.
/// ens5 in AWS). These entries include the VPC gateway MAC, which DPDK needs
/// for outbound traffic. By reading them at startup we avoid the cold-start
/// problem where DPDK ARP requests may fail before the port is fully up.
///
/// Format of /proc/net/arp:
/// ```text
/// IP address       HW type     Flags       HW address            Mask     Device
/// 10.0.1.1         0x1         0x2         0e:12:ab:cd:ef:01     *        ens5
/// ```
fn seed_arp_cache_from_kernel(cache: &ArpCache) {
    let content = match std::fs::read_to_string("/proc/net/arp") {
        Ok(c) => c,
        Err(_) => return, // Not on Linux or no access — skip silently
    };

    for line in content.lines().skip(1) {
        // Skip header line
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }

        // Flags 0x2 = complete entry (0x0 = incomplete)
        let flags = fields[2];
        if flags == "0x0" {
            continue;
        }

        let ip: Ipv4Addr = match fields[0].parse() {
            Ok(ip) => ip,
            Err(_) => continue,
        };

        let mac_str = fields[3];
        let mac_parts: Vec<u8> = mac_str
            .split(':')
            .filter_map(|s| u8::from_str_radix(s, 16).ok())
            .collect();
        if mac_parts.len() != 6 {
            continue;
        }

        let mac = MacAddress::new([
            mac_parts[0], mac_parts[1], mac_parts[2],
            mac_parts[3], mac_parts[4], mac_parts[5],
        ]);
        cache.insert(ip, mac);
    }
}

fn get_or_init_dpdk(port_id: u16) -> io::Result<Arc<DpdkResources>> {
    let mut guard = DPDK_RESOURCES.lock().unwrap();

    if let Some(ref resources) = *guard {
        return Ok(Arc::clone(resources));
    }

    // Initialize EAL
    // EAL args can be overridden via DPDK_EAL_ARGS env var (space-separated).
    // Default: program name + lcore 0 + 4 memory channels.
    // Note: do NOT include --no-pci — DPDK needs PCI scanning to find vfio-pci devices.
    let eal_args: Vec<String> = if let Ok(args_str) = std::env::var("DPDK_EAL_ARGS") {
        args_str.split_whitespace().map(String::from).collect()
    } else {
        vec![
            "dpdk-app".into(), // argv[0]: program name (rte_eal_init skips this)
            "-l".into(), "0".into(),
            "-n".into(), "4".into(),
        ]
    };
    let eal_args_ref: Vec<&str> = eal_args.iter().map(|s| s.as_str()).collect();

    let eal = dpdk::Eal::init(&eal_args_ref)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("EAL init failed: {}", e)))?;

    // Create mempool
    let mempool = Mempool::create_with_config(
        "udp_pool",
        &MempoolConfig::new()
            .with_size(8192)
            .with_cache_size(256),
    ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Mempool creation failed: {}", e)))?;

    // Initialize port
    let port_config = PortConfig::default();
    let port = Port::init(port_id, port_config, &mempool)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Port init failed: {}", e)))?;

    let src_mac = port.mac_address();

    // Start the port
    let mut port = port;
    port.start()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Port start failed: {}", e)))?;

    // Create shared ARP cache, seeded from kernel's ARP table.
    // On Linux, /proc/net/arp contains entries learned by the kernel stack
    // (e.g. the VPC gateway MAC on the primary ENI). Seeding these into the
    // DPDK ARP cache avoids the cold-start problem where the first DPDK ARP
    // request might time out before the port is fully warmed up.
    let arp_cache = Arc::new(ArpCache::new());
    seed_arp_cache_from_kernel(&arp_cache);

    let resources = Arc::new(DpdkResources {
        _eal: eal,
        port,
        mempool,
        src_mac,
        arp_cache,
    });

    *guard = Some(Arc::clone(&resources));
    Ok(resources)
}

// ============================================================================
// Socket Backend Abstraction
// ============================================================================

/// Internal enum for backend dispatch.
///
/// Supports both the original DPDK-direct path (for backward compatibility)
/// and the generic `PacketBackend` trait path (for raw sockets and other backends).
enum SocketBackend {
    /// Direct DPDK backend (original code path)
    Dpdk(Arc<DpdkResources>),
    /// Generic backend via `PacketBackend` trait
    Generic(Arc<dyn PacketBackend>),
}

impl SocketBackend {
    fn mac_address(&self) -> [u8; 6] {
        match self {
            SocketBackend::Dpdk(res) => res.src_mac.octets(),
            SocketBackend::Generic(b) => b.mac_address(),
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            SocketBackend::Dpdk(_) => "dpdk",
            SocketBackend::Generic(b) => b.backend_name(),
        }
    }

    /// Send a raw Ethernet frame via the backend.
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        match self {
            SocketBackend::Dpdk(res) => {
                let mut mbuf = res.mempool.alloc()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("mbuf alloc failed: {}", e)))?;
                
                // Check if frame fits in the mbuf buffer
                let buf_capacity = mbuf.buf_len() as usize - mbuf.data_offset() as usize;
                if buf_capacity < frame.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, 
                        format!("Frame too large for mbuf: {} bytes needed, {} available", frame.len(), buf_capacity)));
                }
                
                // Set data_len first so data_mut() returns the right size slice
                mbuf.set_data_len(frame.len() as u16);
                mbuf.set_packet_len(frame.len() as u32);
                
                let data = mbuf.data_mut()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Failed to get mbuf data"))?;
                data.copy_from_slice(frame);
                
                let mut packets = vec![mbuf];
                let sent = res.port.tx_burst(0, &mut packets)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("tx_burst failed: {}", e)))?;
                if sent == 0 {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "tx queue full"));
                }
                Ok(frame.len())
            }
            SocketBackend::Generic(b) => b.send_frame(frame),
        }
    }

    /// Receive raw Ethernet frames via the backend.
    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        match self {
            SocketBackend::Dpdk(res) => {
                let packets = res.port.rx_burst(0, max_frames as u16)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rx_burst failed: {}", e)))?;
                let mut frames = Vec::with_capacity(packets.len());
                for mbuf in &packets {
                    if let Some(data) = mbuf.data() {
                        let len = mbuf.data_len() as usize;
                        let actual_len = len.min(data.len());
                        frames.push(data[..actual_len].to_vec());
                    }
                }
                Ok(frames)
            }
            SocketBackend::Generic(b) => b.recv_frames(max_frames),
        }
    }

    fn is_promiscuous(&self) -> bool {
        match self {
            SocketBackend::Dpdk(res) => res.port.is_promiscuous(),
            SocketBackend::Generic(b) => b.is_promiscuous(),
        }
    }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        match self {
            SocketBackend::Dpdk(_) => Err(io::Error::new(
                io::ErrorKind::Other,
                "Promiscuous mode must be set before bind() via PortConfig",
            )),
            SocketBackend::Generic(b) => b.set_promiscuous(enable),
        }
    }

    fn is_allmulticast(&self) -> bool {
        match self {
            SocketBackend::Dpdk(res) => res.port.is_allmulticast(),
            SocketBackend::Generic(b) => b.is_allmulticast(),
        }
    }

    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        match self {
            SocketBackend::Dpdk(res) => {
                res.port.set_allmulticast(enable)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to set allmulticast: {}", e)))
            }
            SocketBackend::Generic(b) => b.set_allmulticast(enable),
        }
    }
}

// ============================================================================
// Runtime Backend Selection
// ============================================================================

/// Create a packet backend based on the given configuration.
///
/// This is the main entry point for runtime backend selection.
///
/// # Backend Selection
///
/// - `BackendType::Dpdk` - Initialize DPDK and use it for packet I/O
/// - `BackendType::RawSocketMmap` - Use AF_PACKET with PACKET_MMAP ring buffers
/// - `BackendType::RawSocket` - Use AF_PACKET with basic send/recv
/// - `BackendType::Auto` - Try DPDK first, fall back to raw socket
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_udp::{create_backend, BackendConfig, BackendType};
///
/// // Use AF_PACKET with mmap on eth0
/// let config = BackendConfig::default()
///     .with_raw_socket_mmap("eth0");
/// let backend = create_backend(&config)?;
/// ```
pub fn create_backend(config: &BackendConfig) -> io::Result<Arc<dyn PacketBackend>> {
    match config.backend_type {
        BackendType::Dpdk => {
            let backend = DpdkBackend::new(config.dpdk_port_id)?;
            Ok(Arc::new(backend))
        }
        BackendType::RawSocketMmap => {
            let iface = config.interface_name.as_deref()
                .ok_or_else(|| io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Interface name required for raw socket backend",
                ))?;
            let ring_config = ring_buffer::RingConfig {
                frame_size: config.ring_frame_size,
                frame_count: config.ring_frame_count,
            };
            let backend = RawSocketBackend::with_mmap(iface, true, &ring_config)?;
            Ok(Arc::new(backend))
        }
        BackendType::RawSocket => {
            let iface = config.interface_name.as_deref()
                .ok_or_else(|| io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Interface name required for raw socket backend",
                ))?;
            let backend = RawSocketBackend::new(iface)?;
            Ok(Arc::new(backend))
        }
        BackendType::Auto => {
            // Try DPDK first
            if let Ok(backend) = DpdkBackend::new(config.dpdk_port_id) {
                return Ok(Arc::new(backend));
            }
            // Fall back to raw socket with mmap if interface is specified
            if let Some(ref iface) = config.interface_name {
                let ring_config = ring_buffer::RingConfig {
                    frame_size: config.ring_frame_size,
                    frame_count: config.ring_frame_count,
                };
                if let Ok(backend) = RawSocketBackend::with_mmap(iface, true, &ring_config) {
                    return Ok(Arc::new(backend));
                }
                // Fall back to basic raw socket
                if let Ok(backend) = RawSocketBackend::new(iface) {
                    return Ok(Arc::new(backend));
                }
            }
            Err(io::Error::new(
                io::ErrorKind::Other,
                "No packet backend available (tried DPDK, AF_PACKET+mmap, AF_PACKET)",
            ))
        }
    }
}

// ============================================================================
// Connection Tracking
// ============================================================================

/// Connection state for connected sockets
#[derive(Debug, Clone)]
pub struct ConnectionState {
    /// Local address
    pub local_addr: SocketAddr,
    /// Remote address
    pub remote_addr: SocketAddr,
    /// Packets received from remote
    pub packets_received: u64,
    /// Packets sent to remote
    pub packets_sent: u64,
    /// Bytes received from remote
    pub bytes_received: u64,
    /// Bytes sent to remote
    pub bytes_sent: u64,
}

impl ConnectionState {
    fn new(local: SocketAddr, remote: SocketAddr) -> Self {
        Self {
            local_addr: local,
            remote_addr: remote,
            packets_received: 0,
            packets_sent: 0,
            bytes_received: 0,
            bytes_sent: 0,
        }
    }

    fn record_send(&mut self, bytes: usize) {
        self.packets_sent += 1;
        self.bytes_sent += bytes as u64;
    }

    fn record_recv(&mut self, bytes: usize) {
        self.packets_received += 1;
        self.bytes_received += bytes as u64;
    }
}

/// Receive queue for buffering packets
struct ReceiveQueue {
    /// Buffered packets: (payload, source_addr)
    packets: VecDeque<(Vec<u8>, SocketAddr)>,
    /// Maximum queue size
    max_size: usize,
}

impl ReceiveQueue {
    fn new(max_size: usize) -> Self {
        Self {
            packets: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    fn push(&mut self, payload: Vec<u8>, src: SocketAddr) -> bool {
        if self.packets.len() >= self.max_size {
            return false; // Queue full
        }
        self.packets.push_back((payload, src));
        true
    }

    fn pop(&mut self) -> Option<(Vec<u8>, SocketAddr)> {
        self.packets.pop_front()
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    fn len(&self) -> usize {
        self.packets.len()
    }
}

// ============================================================================
// UdpSocket Implementation
// ============================================================================

/// Drop-in replacement for std::net::UdpSocket with DPDK acceleration
///
/// Supports multiple packet I/O backends:
/// - **DPDK** (default) - High-performance userspace networking
/// - **AF_PACKET** - Linux raw sockets (fallback when DPDK is unavailable)
/// - **AF_PACKET+MMAP** - Linux raw sockets with zero-copy ring buffers
/// Global ephemeral port counter for DPDK sockets (range: 32768-60999, matching Linux)
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(32768);

/// Allocate an ephemeral port from the Linux-style range (32768-60999).
fn allocate_ephemeral_port() -> u16 {
    loop {
        let port = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
        if port >= 32768 && port <= 60999 {
            return port;
        }
        // Wrapped past 60999; reset to start of range
        NEXT_EPHEMERAL_PORT.store(32769, Ordering::Relaxed);
        return 32768;
    }
}

pub struct UdpSocket {
    local_addr: SocketAddr,
    connected_addr: Mutex<Option<SocketAddr>>,
    /// Backend for packet I/O (DPDK or generic)
    socket_backend: SocketBackend,
    /// Legacy DPDK resources (kept for backward-compatible methods)
    resources: Arc<DpdkResources>,
    ttl: u8,
    /// Destination MAC address (would normally come from ARP)
    dst_mac: MacAddress,
    /// ARP handler for address resolution
    arp_handler: ArpHandler,
    /// ICMP handler for ping responses
    icmp_handler: IcmpHandler,
    /// Connection state tracking (for connected sockets)
    connection_state: RwLock<Option<ConnectionState>>,
    /// Receive queue for buffered packets
    recv_queue: Mutex<ReceiveQueue>,
    /// Whether to automatically respond to ARP requests
    auto_arp: bool,
    /// Whether to automatically respond to ICMP echo requests
    auto_icmp: bool,
    /// Read timeout for recv operations (None = block forever)
    read_timeout: Mutex<Option<Duration>>,
    /// Write timeout for send operations (None = block forever)
    write_timeout: Mutex<Option<Duration>>,
    /// Multi-core pipeline topology (None = run-to-completion, the default).
    /// When active, recv_from() reads from app_ring and send_to() writes to tx_ring.
    topology: Mutex<Option<MultiCoreTopology>>,
    /// Reusable TX frame buffer — avoids per-packet heap allocation in send_to.
    /// Uses Mutex because send_to takes &self (not &mut self) per the std API.
    tx_buf: Mutex<Vec<u8>>,
}

impl UdpSocket {
    /// Creates a UDP socket bound to the given address.
    ///
    /// This initializes DPDK if not already initialized and binds to the specified
    /// local address and port.
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;

        // Only support IPv4 for now
        let local_v4 = match addr {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPv6 not supported"));
            }
        };

        // Allocate an ephemeral port if port 0 was requested
        let local_v4 = if local_v4.port() == 0 {
            let ephemeral = allocate_ephemeral_port();
            SocketAddrV4::new(*local_v4.ip(), ephemeral)
        } else {
            local_v4
        };

        // Get or initialize DPDK resources
        let resources = get_or_init_dpdk(0)?;

        // Create protocol handlers with shared ARP cache
        let local_mac = resources.src_mac.octets();
        let local_ip = *local_v4.ip();

        let arp_handler = ArpHandler::with_cache(
            local_mac,
            local_ip,
            Arc::clone(&resources.arp_cache),
        );

        let icmp_handler = IcmpHandler::new(local_mac, local_ip);

        println!("✅ DPDK UDP socket bound to {} (MAC: {})", SocketAddr::V4(local_v4), resources.src_mac);

        let socket_backend = SocketBackend::Dpdk(Arc::clone(&resources));

        Ok(UdpSocket {
            local_addr: SocketAddr::V4(local_v4),
            connected_addr: Mutex::new(None),
            socket_backend,
            resources,
            ttl: 64,
            dst_mac: MacAddress::broadcast(),
            arp_handler,
            icmp_handler,
            connection_state: RwLock::new(None),
            recv_queue: Mutex::new(ReceiveQueue::new(1024)),
            auto_arp: true,
            auto_icmp: true,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            topology: Mutex::new(None),
            tx_buf: Mutex::new(Vec::with_capacity(TOTAL_HEADER_LEN + MAX_UDP_PAYLOAD)),
        })
    }

    /// Creates a UDP socket bound to the given address using a specific packet backend.
    ///
    /// This allows using alternative backends like AF_PACKET raw sockets
    /// instead of DPDK.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use dpdk_udp::{UdpSocket, BackendConfig};
    ///
    /// // Use AF_PACKET with mmap on eth0
    /// let config = BackendConfig::default().with_raw_socket_mmap("eth0");
    /// let backend = dpdk_udp::create_backend(&config)?;
    /// let socket = UdpSocket::bind_with_backend("0.0.0.0:9000", backend)?;
    /// ```
    pub fn bind_with_backend<A: ToSocketAddrs>(
        addr: A,
        backend: Arc<dyn PacketBackend>,
    ) -> io::Result<UdpSocket> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;

        let local_v4 = match addr {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPv6 not supported"));
            }
        };

        // Allocate an ephemeral port if port 0 was requested
        let local_v4 = if local_v4.port() == 0 {
            let ephemeral = allocate_ephemeral_port();
            SocketAddrV4::new(*local_v4.ip(), ephemeral)
        } else {
            local_v4
        };

        let local_mac = backend.mac_address();
        let local_ip = *local_v4.ip();
        let arp_cache = Arc::new(ArpCache::new());

        let arp_handler = ArpHandler::with_cache(
            local_mac,
            local_ip,
            Arc::clone(&arp_cache),
        );

        let icmp_handler = IcmpHandler::new(local_mac, local_ip);

        let backend_name = backend.backend_name();

        // We still need DpdkResources for backward-compatible methods.
        // Initialize DPDK resources as a fallback (they're lazy-initialized).
        let resources = get_or_init_dpdk(0)?;

        println!("✅ {} UDP socket bound to {} (MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
            backend_name, SocketAddr::V4(local_v4),
            local_mac[0], local_mac[1], local_mac[2],
            local_mac[3], local_mac[4], local_mac[5]);

        Ok(UdpSocket {
            local_addr: SocketAddr::V4(local_v4),
            connected_addr: Mutex::new(None),
            socket_backend: SocketBackend::Generic(backend),
            resources,
            ttl: 64,
            dst_mac: MacAddress::broadcast(),
            arp_handler,
            icmp_handler,
            connection_state: RwLock::new(None),
            recv_queue: Mutex::new(ReceiveQueue::new(1024)),
            auto_arp: true,
            auto_icmp: true,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            topology: Mutex::new(None),
            tx_buf: Mutex::new(Vec::with_capacity(TOTAL_HEADER_LEN + MAX_UDP_PAYLOAD)),
        })
    }

    /// Get the name of the active packet I/O backend.
    pub fn active_backend(&self) -> &'static str {
        self.socket_backend.backend_name()
    }

    /// Returns the active topology plan, if a multi-core pipeline is running.
    ///
    /// Returns `None` when the socket is in run-to-completion mode (default
    /// for `UdpSocket::bind()` and when `workers_per_queue(0)` is configured).
    pub fn topology_plan(&self) -> Option<TopologyPlan> {
        self.topology.lock().unwrap().as_ref().map(|t| t.plan.clone())
    }

    /// Returns `true` if the socket is running in simple run-to-completion mode
    /// (no pipeline threads, lowest latency).
    pub fn is_run_to_completion(&self) -> bool {
        self.topology.lock().unwrap().is_none()
    }

    /// Sets the read timeout for `recv`, `recv_from`, and `peek` operations.
    ///
    /// If `dur` is `None`, reads will block indefinitely. If `dur` is `Some(duration)`,
    /// reads will return `io::ErrorKind::WouldBlock` / `TimedOut` after the duration.
    /// Matches `std::net::UdpSocket::set_read_timeout`.
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        if let Some(d) = dur {
            if d.is_zero() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero duration not supported"));
            }
        }
        *self.read_timeout.lock().unwrap() = dur;
        Ok(())
    }

    /// Returns the read timeout of this socket.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.read_timeout.lock().unwrap())
    }

    /// Sets the write timeout for `send` and `send_to` operations.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        if let Some(d) = dur {
            if d.is_zero() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero duration not supported"));
            }
        }
        *self.write_timeout.lock().unwrap() = dur;
        Ok(())
    }

    /// Returns the write timeout of this socket.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.write_timeout.lock().unwrap())
    }

    /// Sends data on the socket to the given address.
    ///
    /// This builds a complete Ethernet/IPv4/UDP packet and transmits it
    /// using DPDK's tx_burst.
    pub fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;

        self.send_to_addr(buf, addr)
    }

    /// Internal send implementation with resolved address.
    ///
    /// Uses a reusable TX buffer to avoid per-packet heap allocation on the
    /// run-to-completion path. The multi-core topology path still allocates
    /// because the TX ring takes ownership of the frame.
    fn send_to_addr(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        // Extract IPv4 addresses
        let (src_ip, src_port) = match self.local_addr {
            SocketAddr::V4(v4) => (*v4.ip(), v4.port()),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPv6 not supported"));
            }
        };

        let (dst_ip, dst_port) = match addr {
            SocketAddr::V4(v4) => (*v4.ip(), v4.port()),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPv6 not supported"));
            }
        };

        // Resolve destination MAC via ARP (or use configured/broadcast MAC)
        let dst_mac = match self.arp_handler.resolve(&dst_ip) {
            Some(mac) => mac,
            None if self.auto_arp => {
                // Proactively send ARP request and wait for reply
                self.resolve_arp(&dst_ip)?
            }
            None => self.dst_mac.clone(),
        };

        let src_mac = self.socket_backend.mac_address();

        // If multi-core topology is active, enqueue to TX ring (needs owned Vec).
        // Otherwise, use reusable buffer and send directly (zero-alloc steady state).
        let has_topology = self.topology.lock().unwrap().is_some();
        if has_topology {
            // Multi-core path: TX ring takes ownership, so we must allocate
            let frame = build_udp_frame(
                &src_mac,
                &dst_mac.octets(),
                src_ip, dst_ip,
                src_port, dst_port,
                buf, self.ttl,
            ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("packet build failed: {}", e)))?;

            let topo_guard = self.topology.lock().unwrap();
            if let Some(ref topo) = *topo_guard {
                topo.tx_ring.enqueue(topology::TxFrame { frame }).map_err(|_| {
                    io::Error::new(io::ErrorKind::WouldBlock, "TX ring full")
                })?;
            }
        } else {
            // Run-to-completion path: reuse the TX buffer across calls
            let mut tx_buf = self.tx_buf.lock().unwrap();
            build_udp_frame_into(
                &mut tx_buf,
                &src_mac,
                &dst_mac.octets(),
                src_ip, dst_ip,
                src_port, dst_port,
                buf, self.ttl,
            ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("packet build failed: {}", e)))?;
            self.socket_backend.send_frame(&tx_buf)?;
        }

        // Update connection state if connected
        if let Ok(mut guard) = self.connection_state.write() {
            if let Some(ref mut state) = *guard {
                state.record_send(buf.len());
            }
        }

        Ok(buf.len())
    }

    /// Receives a single datagram message on the socket.
    ///
    /// This calls DPDK's rx_burst to receive packets, parses the Ethernet/IPv4/UDP
    /// headers, and copies the payload to the provided buffer.
    ///
    /// Blocks until a packet is received (matching `std::net::UdpSocket` behavior).
    /// While waiting, ARP requests and ICMP pings are handled automatically.
    ///
    /// # Returns
    ///
    /// On success, returns the number of bytes received and the source address.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // Check if multi-core topology is active — if so, dequeue from app_ring.
        let has_topology = self.topology.lock().unwrap().is_some();
        if has_topology {
            return self.recv_from_pipeline(buf);
        }

        // Run-to-completion path (original single-threaded behavior).
        self.recv_from_inline(buf)
    }

    /// Pipeline recv path: dequeue processed packets from the MPSC app_ring.
    fn recv_from_pipeline(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let deadline = self.read_timeout.lock().unwrap().map(|d| Instant::now() + d);

        loop {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"));
                }
            }

            // Check buffered packets first (from connected socket filtering)
            {
                let mut queue = self.recv_queue.lock().unwrap();
                if let Some((payload, src_addr)) = queue.pop() {
                    let copy_len = std::cmp::min(buf.len(), payload.len());
                    buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                    return Ok((copy_len, src_addr));
                }
            }

            // Dequeue from the app_ring (filled by worker threads)
            let packet = {
                let topo_guard = self.topology.lock().unwrap();
                if let Some(ref topo) = *topo_guard {
                    topo.app_ring.dequeue()
                } else {
                    None
                }
            };

            if let Some(packet) = packet {
                // If connected, only accept packets from connected peer
                if let Some(connected) = *self.connected_addr.lock().unwrap() {
                    if packet.src_addr != connected {
                        let mut queue = self.recv_queue.lock().unwrap();
                        queue.push(packet.payload, packet.src_addr);
                        continue;
                    }
                }

                let copy_len = std::cmp::min(buf.len(), packet.payload.len());
                buf[..copy_len].copy_from_slice(&packet.payload[..copy_len]);

                if let Ok(mut guard) = self.connection_state.write() {
                    if let Some(ref mut state) = *guard {
                        state.record_recv(copy_len);
                    }
                }

                return Ok((copy_len, packet.src_addr));
            }

            // No packet available — brief pause
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }

    /// Inline recv path: single-threaded run-to-completion (original behavior).
    fn recv_from_inline(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // Get our local port for filtering (do this once outside the loop)
        let local_port = match self.local_addr {
            SocketAddr::V4(v4) => v4.port(),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPv6 not supported"));
            }
        };

        let deadline = self.read_timeout.lock().unwrap().map(|d| Instant::now() + d);

        // Block until we receive a matching packet (std::net::UdpSocket behavior).
        // DPDK rx_burst is non-blocking, so we poll in a loop with a short sleep
        // to avoid burning 100% CPU while still achieving low latency.
        loop {
            // Check read timeout
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"));
                }
            }
            // First check if we have buffered packets
            {
                let mut queue = self.recv_queue.lock().unwrap();
                if let Some((payload, src_addr)) = queue.pop() {
                    let copy_len = std::cmp::min(buf.len(), payload.len());
                    buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                    return Ok((copy_len, src_addr));
                }
            }

            // Dispatch to the appropriate fast-path based on backend type.
            match &self.socket_backend {
                SocketBackend::Dpdk(res) => {
                    // DPDK fast path: process mbufs inline to avoid per-packet Vec allocation.
                    // rx_burst returns mbufs whose data() borrows from the mbuf buffer —
                    // we parse and copy directly to the user buffer or recv_queue without
                    // any intermediate heap allocation.
                    let packets = res.port.rx_burst(0, 32)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rx_burst failed: {}", e)))?;

                    if packets.is_empty() {
                        std::thread::sleep(std::time::Duration::from_micros(100));
                        continue;
                    }

                    let mut result: Option<(usize, SocketAddr)> = None;

                    for mbuf in &packets {
                        let Some(data) = mbuf.data() else { continue };
                        let len = mbuf.data_len() as usize;
                        let frame_data = &data[..len.min(data.len())];

                        if let Some(r) = self.process_frame_zerocopy(frame_data, local_port, buf, &mut result) {
                            return Ok(r);
                        }
                    }
                    // mbufs are freed here when `packets` drops

                    if let Some(r) = result {
                        return Ok(r);
                    }
                }
                SocketBackend::Generic(backend) => {
                    // Generic backend path: recv_frames returns Vec<Vec<u8>>.
                    // We still use parse_udp_packet_ref to avoid a second copy of the payload.
                    let frames = backend.recv_frames(32)?;

                    if frames.is_empty() {
                        std::thread::sleep(std::time::Duration::from_micros(100));
                        continue;
                    }

                    let mut result: Option<(usize, SocketAddr)> = None;

                    for frame_data in &frames {
                        if let Some(r) = self.process_frame_zerocopy(frame_data, local_port, buf, &mut result) {
                            return Ok(r);
                        }
                    }

                    if let Some(r) = result {
                        return Ok(r);
                    }
                }
            }
            // No matching UDP packets in this batch — continue polling
        }
    }

    /// Process a single frame with zero-copy parsing.
    ///
    /// Handles ARP/ICMP and parses UDP — copies payload directly to user buffer
    /// or recv_queue without intermediate `Vec<u8>` allocation on the primary path.
    ///
    /// Returns `Some((len, addr))` if this is the first matching packet AND
    /// `result` was already `Some` (meaning we had a prior match, so we need to
    /// return immediately). Otherwise returns `None` and sets `result` on first match.
    fn process_frame_zerocopy(
        &self,
        frame_data: &[u8],
        local_port: u16,
        buf: &mut [u8],
        result: &mut Option<(usize, SocketAddr)>,
    ) -> Option<(usize, SocketAddr)> {
        if frame_data.len() < 14 {
            return None;
        }

        let ethertype = u16::from_be_bytes([frame_data[12], frame_data[13]]);

        // Handle ARP
        if ethertype == arp::ETH_TYPE_ARP && self.auto_arp {
            if let Some(reply_frame) = self.arp_handler.process_arp(frame_data) {
                let _ = self.socket_backend.send_frame(&reply_frame);
            }
            return None;
        }

        // Handle ICMP
        if ethertype == ETH_TYPE_IPV4 && frame_data.len() > ETH_HEADER_LEN + 9 {
            let protocol = frame_data[ETH_HEADER_LEN + 9];
            if protocol == icmp::IP_PROTO_ICMP && self.auto_icmp {
                if let Some(reply_frame) = self.icmp_handler.process_icmp(frame_data) {
                    let _ = self.socket_backend.send_frame(&reply_frame);
                }
                return None;
            }
        }

        // Zero-copy UDP parse — payload borrows from frame_data
        let parsed = parse_udp_packet_ref(frame_data)?;

        // Learn source MAC for reply routing
        self.arp_handler.cache.insert(
            parsed.src_ip,
            MacAddress::new(parsed.src_mac),
        );

        if parsed.dst_port != local_port {
            return None;
        }

        let src_addr = SocketAddr::V4(
            SocketAddrV4::new(parsed.src_ip, parsed.src_port)
        );

        // If connected, only accept packets from the connected address
        if let Some(connected) = *self.connected_addr.lock().unwrap() {
            if src_addr != connected {
                let mut queue = self.recv_queue.lock().unwrap();
                // Must allocate here — queued packets outlive the frame/mbuf
                queue.push(parsed.payload.to_vec(), src_addr);
                return None;
            }
        }

        if result.is_none() {
            // First matching packet: copy directly to user buffer (zero intermediate alloc)
            let copy_len = std::cmp::min(buf.len(), parsed.payload.len());
            buf[..copy_len].copy_from_slice(&parsed.payload[..copy_len]);

            if let Ok(mut guard) = self.connection_state.write() {
                if let Some(ref mut state) = *guard {
                    state.record_recv(copy_len);
                }
            }

            *result = Some((copy_len, src_addr));
        } else {
            // Additional matching packets: must allocate for the queue
            let mut queue = self.recv_queue.lock().unwrap();
            queue.push(parsed.payload.to_vec(), src_addr);
        }

        None
    }

    /// Returns the socket address that this socket was created from.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    /// Returns the socket address of the remote peer this socket was connected to.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.connected_addr.lock().unwrap().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "Socket not connected")
        })
    }

    /// Connects this UDP socket to a remote address.
    ///
    /// After connecting, `send()` and `recv()` can be used without specifying addresses.
    /// The socket will also track connection statistics.
    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;

        // Initialize connection tracking
        *self.connection_state.write().unwrap() = Some(ConnectionState::new(
            self.local_addr,
            addr,
        ));

        *self.connected_addr.lock().unwrap() = Some(addr);
        Ok(())
    }

    /// Receives a single datagram message on the socket from the remote address
    /// to which it is connected.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let (len, _addr) = self.recv_from(buf)?;
        Ok(len)
    }

    /// Sends data on the socket to the remote address to which it is connected.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let addr = self.connected_addr.lock().unwrap().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "Socket not connected")
        })?;
        self.send_to_addr(buf, addr)
    }

    /// Sets the TTL (Time To Live) for outgoing packets.
    pub fn set_ttl(&mut self, ttl: u32) -> io::Result<()> {
        self.ttl = ttl as u8;
        Ok(())
    }

    /// Gets the TTL value for outgoing packets.
    pub fn ttl(&self) -> io::Result<u32> {
        Ok(self.ttl as u32)
    }

    /// Sets the destination MAC address for outgoing packets.
    ///
    /// In a full implementation, this would be resolved via ARP.
    /// For testing/direct connections, this can be set manually.
    pub fn set_dst_mac(&mut self, mac: MacAddress) {
        self.dst_mac = mac;
    }

    /// Gets the source MAC address (from the active backend).
    pub fn src_mac(&self) -> MacAddress {
        MacAddress::new(self.socket_backend.mac_address())
    }

    // ========================================================================
    // ARP Configuration
    // ========================================================================

    /// Enable or disable automatic ARP response.
    ///
    /// When enabled (default), the socket will automatically respond to ARP
    /// requests for its IP address.
    pub fn set_auto_arp(&mut self, enable: bool) {
        self.auto_arp = enable;
    }

    /// Check if automatic ARP response is enabled.
    pub fn auto_arp(&self) -> bool {
        self.auto_arp
    }

    /// Get a reference to the ARP cache.
    pub fn arp_cache(&self) -> &Arc<ArpCache> {
        &self.resources.arp_cache
    }

    /// Manually add an ARP cache entry.
    pub fn add_arp_entry(&self, ip: Ipv4Addr, mac: MacAddress) {
        self.resources.arp_cache.insert(ip, mac);
    }

    /// Send an ARP request for the given IP address.
    ///
    /// This is useful for pre-populating the ARP cache before sending data.
    pub fn send_arp_request(&self, target_ip: Ipv4Addr) -> io::Result<()> {
        if let Some(frame) = self.arp_handler.make_request(target_ip) {
            self.socket_backend.send_frame(&frame)?;
        }
        Ok(())
    }

    // ========================================================================
    // ICMP Configuration
    // ========================================================================

    /// Enable or disable automatic ICMP echo reply (ping).
    ///
    /// When enabled (default), the socket will automatically respond to ICMP
    /// echo requests (ping) for its IP address.
    pub fn set_auto_icmp(&mut self, enable: bool) {
        self.auto_icmp = enable;
    }

    /// Check if automatic ICMP echo reply is enabled.
    pub fn auto_icmp(&self) -> bool {
        self.auto_icmp
    }

    // ========================================================================
    // ARP Resolution
    // ========================================================================

    /// Proactively resolve an IP address to a MAC address via ARP.
    ///
    /// Sends ARP requests (with retries) and polls for the reply. Returns the
    /// resolved MAC or an error if resolution fails after all attempts.
    fn resolve_arp(&self, target_ip: &Ipv4Addr) -> io::Result<MacAddress> {
        let arp_frame = match self.arp_handler.make_request(*target_ip) {
            Some(f) => f,
            None => return Ok(self.dst_mac.clone()),
        };

        // Retry ARP up to 3 times (1 second per attempt, 3 seconds total).
        // A single attempt can fail if the port hasn't fully warmed up yet.
        const MAX_ATTEMPTS: u32 = 3;
        const POLLS_PER_ATTEMPT: u32 = 10_000; // 10k * 100us = 1 second

        for attempt in 0..MAX_ATTEMPTS {
            self.socket_backend.send_frame(&arp_frame)?;

            for _ in 0..POLLS_PER_ATTEMPT {
                let frames = self.socket_backend.recv_frames(32)?;
                for frame_data in &frames {
                    if frame_data.len() >= 14 {
                        let ethertype = u16::from_be_bytes([frame_data[12], frame_data[13]]);
                        if ethertype == arp::ETH_TYPE_ARP {
                            if let Some(reply_frame) = self.arp_handler.process_arp(frame_data) {
                                let _ = self.socket_backend.send_frame(&reply_frame);
                            }
                        } else if ethertype == ETH_TYPE_IPV4 {
                            // Queue any UDP packets we receive while waiting
                            if let Some(parsed) = parse_udp_packet(frame_data) {
                                if frame_data.len() >= 12 {
                                    let src_mac: [u8; 6] = frame_data[6..12].try_into().unwrap();
                                    self.arp_handler.cache.insert(parsed.src_ip, MacAddress::new(src_mac));
                                }
                                let src_addr = SocketAddr::V4(
                                    SocketAddrV4::new(parsed.src_ip, parsed.src_port)
                                );
                                let mut queue = self.recv_queue.lock().unwrap();
                                queue.push(parsed.payload, src_addr);
                            }
                        }
                    }
                }

                if let Some(mac) = self.arp_handler.resolve(target_ip) {
                    return Ok(mac);
                }

                std::thread::sleep(std::time::Duration::from_micros(100));
            }

            eprintln!(
                "dpdk-udp: ARP attempt {}/{} for {} timed out",
                attempt + 1, MAX_ATTEMPTS, target_ip
            );
        }

        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("ARP resolution failed for {} after {} attempts", target_ip, MAX_ATTEMPTS),
        ))
    }

    // ========================================================================
    // Connection Tracking
    // ========================================================================

    /// Get connection statistics for a connected socket.
    ///
    /// Returns None if the socket is not connected.
    pub fn connection_stats(&self) -> Option<ConnectionState> {
        self.connection_state.read().ok().and_then(|guard| {
            guard.as_ref().map(|s| s.clone())
        })
    }

    /// Check if this socket is connected.
    pub fn is_connected(&self) -> bool {
        self.connected_addr.lock().unwrap().is_some()
    }

    /// Get the number of packets in the receive queue.
    pub fn recv_queue_len(&self) -> usize {
        self.recv_queue.lock().unwrap().len()
    }

    // ========================================================================
    // Multicast Support
    // ========================================================================

    /// Join a multicast group.
    ///
    /// This adds the multicast MAC address derived from the IPv4 multicast
    /// address to the port's multicast filter.
    ///
    /// # Arguments
    /// * `multicast_addr` - IPv4 multicast address (224.0.0.0 - 239.255.255.255)
    /// * `_interface` - Interface address (ignored, using DPDK port)
    pub fn join_multicast_v4(&mut self, multicast_addr: &Ipv4Addr, _interface: &Ipv4Addr) -> io::Result<()> {
        // Validate multicast address
        if !multicast_addr.is_multicast() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Address is not a multicast address",
            ));
        }

        // Convert IPv4 multicast to MAC multicast (01:00:5e:xx:xx:xx)
        let octets = multicast_addr.octets();
        let mac = MacAddress::new([
            0x01, 0x00, 0x5e,
            octets[1] & 0x7f, // Lower 23 bits of IP mapped to MAC
            octets[2],
            octets[3],
        ]);

        // Add to multicast list
        // Note: In a full implementation, we'd maintain a list and update it
        self.resources.port.set_multicast_addrs(&[mac])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to join multicast: {}", e)))?;

        Ok(())
    }

    /// Leave a multicast group.
    ///
    /// # Arguments
    /// * `multicast_addr` - IPv4 multicast address to leave
    /// * `_interface` - Interface address (ignored)
    pub fn leave_multicast_v4(&mut self, multicast_addr: &Ipv4Addr, _interface: &Ipv4Addr) -> io::Result<()> {
        if !multicast_addr.is_multicast() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Address is not a multicast address",
            ));
        }

        // Clear multicast list (simplified - full implementation would track groups)
        self.resources.port.set_multicast_addrs(&[])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to leave multicast: {}", e)))?;

        Ok(())
    }

    /// Enable or disable reception of all multicast packets.
    ///
    /// When enabled, the port receives all multicast packets regardless
    /// of whether they match the configured multicast addresses.
    pub fn set_multicast_all(&mut self, enable: bool) -> io::Result<()> {
        self.socket_backend.set_allmulticast(enable)
    }

    /// Check if all-multicast mode is enabled.
    pub fn multicast_all(&self) -> bool {
        self.socket_backend.is_allmulticast()
    }

    // ========================================================================
    // Promiscuous Mode
    // ========================================================================

    /// Enable or disable promiscuous mode.
    ///
    /// In promiscuous mode, the port receives all packets regardless of
    /// destination MAC address. This is useful for packet capture and
    /// network monitoring.
    ///
    /// Note: For DPDK backend, promiscuous mode must be set before bind()
    /// via PortConfig. For raw socket backends, it can be set at any time.
    pub fn set_promiscuous(&mut self, enable: bool) -> io::Result<()> {
        self.socket_backend.set_promiscuous(enable)
    }

    /// Check if promiscuous mode is enabled.
    pub fn is_promiscuous(&self) -> bool {
        self.socket_backend.is_promiscuous()
    }

    // ========================================================================
    // Hardware Offload Status
    // ========================================================================

    /// Check if hardware IPv4 checksum offload is enabled for TX.
    ///
    /// When enabled, the NIC calculates IPv4 header checksums in hardware,
    /// reducing CPU overhead.
    pub fn has_tx_ipv4_cksum_offload(&self) -> bool {
        self.resources.port.config().tx_offload.ipv4_cksum
    }

    /// Check if hardware UDP checksum offload is enabled for TX.
    pub fn has_tx_udp_cksum_offload(&self) -> bool {
        self.resources.port.config().tx_offload.udp_cksum
    }

    /// Check if hardware IPv4 checksum offload is enabled for RX.
    pub fn has_rx_ipv4_cksum_offload(&self) -> bool {
        self.resources.port.config().rx_offload.ipv4_cksum
    }

    /// Check if hardware UDP checksum offload is enabled for RX.
    pub fn has_rx_udp_cksum_offload(&self) -> bool {
        self.resources.port.config().rx_offload.udp_cksum
    }
}

// ============================================================================
// SYNTHETIC TESTING UTILITIES
// ============================================================================

pub trait UdpHandler {
    fn on_packet(&self, src_ip: [u8; 4], src_port: u16, dst_ip: [u8; 4], dst_port: u16, payload: &[u8]) -> Option<Vec<u8>>;
}

/// Synthetic packet processor for testing protocol logic without real networking
pub struct SyntheticUdpSocket {
    bind_ip: [u8; 4],
    bind_port: u16,
    handler: Box<dyn UdpHandler>,
}

impl SyntheticUdpSocket {
    pub fn new(bind_ip: [u8; 4], bind_port: u16, handler: Box<dyn UdpHandler>) -> Self {
        Self { bind_ip, bind_port, handler }
    }

    pub fn parse_and_handle(&self, frame: &[u8]) -> UdpResult<Option<Vec<u8>>> {
        if frame.len() < TOTAL_HEADER_LEN {
            return Err(UdpError::PacketTooShort { expected: TOTAL_HEADER_LEN, actual: frame.len() });
        }

        let ip_header = &frame[ETH_HEADER_LEN..ETH_HEADER_LEN + IPV4_HEADER_LEN];
        let udp_header = &frame[ETH_HEADER_LEN + IPV4_HEADER_LEN..TOTAL_HEADER_LEN];
        let payload = &frame[TOTAL_HEADER_LEN..];

        // Check if it's UDP
        if ip_header[9] != IP_PROTO_UDP {
            return Ok(None);
        }

        let src_ip = [ip_header[12], ip_header[13], ip_header[14], ip_header[15]];
        let dst_ip = [ip_header[16], ip_header[17], ip_header[18], ip_header[19]];
        let src_port = u16::from_be_bytes([udp_header[0], udp_header[1]]);
        let dst_port = u16::from_be_bytes([udp_header[2], udp_header[3]]);

        // Check if packet is for us
        if dst_ip != self.bind_ip || dst_port != self.bind_port {
            return Ok(None);
        }

        if let Some(response_payload) = self.handler.on_packet(src_ip, src_port, dst_ip, dst_port, payload) {
            let mut response_frame = vec![0u8; TOTAL_HEADER_LEN + response_payload.len()];

            // Ethernet header (swap src/dst)
            response_frame[0..6].copy_from_slice(&frame[6..12]);
            response_frame[6..12].copy_from_slice(&frame[0..6]);
            response_frame[12..14].copy_from_slice(&frame[12..14]);

            // IP header
            let ip_start = ETH_HEADER_LEN;
            response_frame[ip_start] = 0x45;
            let total_len = (IPV4_HEADER_LEN + UDP_HEADER_LEN + response_payload.len()) as u16;
            response_frame[ip_start + 2..ip_start + 4].copy_from_slice(&total_len.to_be_bytes());
            response_frame[ip_start + 8] = 64; // TTL
            response_frame[ip_start + 9] = IP_PROTO_UDP;
            response_frame[ip_start + 12..ip_start + 16].copy_from_slice(&dst_ip);
            response_frame[ip_start + 16..ip_start + 20].copy_from_slice(&src_ip);

            // Calculate IP checksum
            let ip_cksum = ipv4_checksum(&response_frame[ip_start..ip_start + IPV4_HEADER_LEN]);
            response_frame[ip_start + 10..ip_start + 12].copy_from_slice(&ip_cksum.to_be_bytes());

            // UDP header
            let udp_start = ETH_HEADER_LEN + IPV4_HEADER_LEN;
            response_frame[udp_start..udp_start + 2].copy_from_slice(&dst_port.to_be_bytes());
            response_frame[udp_start + 2..udp_start + 4].copy_from_slice(&src_port.to_be_bytes());
            let udp_len = (UDP_HEADER_LEN + response_payload.len()) as u16;
            response_frame[udp_start + 4..udp_start + 6].copy_from_slice(&udp_len.to_be_bytes());

            // UDP checksum
            let udp_cksum = udp_checksum(&dst_ip, &src_ip, &response_frame[udp_start..udp_start + UDP_HEADER_LEN], &response_payload);
            response_frame[udp_start + 6..udp_start + 8].copy_from_slice(&udp_cksum.to_be_bytes());

            // Payload
            response_frame[TOTAL_HEADER_LEN..].copy_from_slice(&response_payload);

            return Ok(Some(response_frame));
        }

        Ok(None)
    }
}

// ============================================================================
// UdpSocketBuilder — optional builder for topology control (A6)
// ============================================================================

/// Builder for `UdpSocket` with optional multi-core topology configuration.
///
/// Most users should use `UdpSocket::bind()` which auto-detects the best
/// topology. Use the builder when you need explicit control over RSS queue
/// count and worker-per-queue allocation.
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_udp::UdpSocket;
///
/// // Explicit: 4 RSS queues, 2 workers per queue
/// let socket = UdpSocket::builder()
///     .rx_queues(4)
///     .workers_per_queue(2)
///     .bind("0.0.0.0:9000")?;
///
/// // Or just auto-detect (equivalent to UdpSocket::bind()):
/// let socket = UdpSocket::builder()
///     .bind("0.0.0.0:9000")?;
/// ```
pub struct UdpSocketBuilder {
    rx_queues: Option<u16>,
    workers_per_queue: Option<u16>,
    backend_type: Option<BackendType>,
}

impl UdpSocketBuilder {
    /// Create a new builder with all defaults (auto-detect everything).
    pub fn new() -> Self {
        Self {
            rx_queues: None,
            workers_per_queue: None,
            backend_type: None,
        }
    }

    /// Set the number of NIC RSS RX queues.
    ///
    /// Will be clamped to the NIC's maximum supported queue count.
    pub fn rx_queues(mut self, n: u16) -> Self {
        self.rx_queues = Some(n);
        self
    }

    /// Set the number of worker lcores per RX queue.
    ///
    /// Set to 0 for run-to-completion mode (no pipeline).
    pub fn workers_per_queue(mut self, n: u16) -> Self {
        self.workers_per_queue = Some(n);
        self
    }

    /// Force a specific backend type.
    pub fn backend_type(mut self, backend: BackendType) -> Self {
        self.backend_type = Some(backend);
        self
    }

    /// Build the topology configuration from this builder's settings.
    pub fn topology_config(&self) -> TopologyConfig {
        TopologyConfig {
            rx_queues: self.rx_queues,
            workers_per_queue: self.workers_per_queue,
        }
    }

    /// Bind a UDP socket with the configured topology.
    ///
    /// This is equivalent to `UdpSocket::bind()` but uses the builder's
    /// topology configuration instead of pure auto-detection.
    ///
    /// When the topology plan is **not** run-to-completion, pipeline threads
    /// are spawned automatically. Use `.workers_per_queue(0)` to force
    /// run-to-completion mode (no pipeline threads, lowest latency).
    pub fn bind<A: ToSocketAddrs>(self, addr: A) -> io::Result<UdpSocket> {
        let topo_config = self.topology_config();

        // Create the socket using the standard bind path
        let socket = UdpSocket::bind(addr)?;

        // Detect topology from config + runtime environment.
        // Under stubs this always returns run-to-completion.
        let plan = topology::detect_topology(
            &topo_config,
            // Under stubs we report 1 lcore, so the plan will be run-to-completion.
            // With real DPDK we'd query eal_lcore_count().
            if dpdk_sys::is_stub() { 1 } else { 8 },
            // Under stubs NIC max queues = 1.
            if dpdk_sys::is_stub() { 1 } else { 16 },
            0, // NUMA node
        );

        if !plan.is_run_to_completion() {
            // Build pipeline configuration from the socket's state
            let local_port = match socket.local_addr {
                SocketAddr::V4(v4) => v4.port(),
                _ => 0,
            };
            let local_mac = socket.socket_backend.mac_address();
            let local_ip = match socket.local_addr {
                SocketAddr::V4(v4) => *v4.ip(),
                _ => Ipv4Addr::UNSPECIFIED,
            };

            let pipeline_config = topology::PipelineConfig {
                plan: plan.clone(),
                local_port,
                local_mac,
                local_ip,
                arp_cache: Arc::clone(&socket.resources.arp_cache),
            };

            // Create backend closures that capture the socket's backend for the pipeline
            // We need raw function pointers since SocketBackend isn't Clone.
            // Use a shared reference approach via Arc.
            let resources_for_recv = Arc::clone(&socket.resources);
            let resources_for_send = Arc::clone(&socket.resources);

            let recv_fn = move |max_frames: usize| -> io::Result<Vec<Vec<u8>>> {
                let packets = resources_for_recv.port.rx_burst(0, max_frames as u16)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rx_burst failed: {}", e)))?;
                let mut frames = Vec::with_capacity(packets.len());
                for mbuf in &packets {
                    if let Some(data) = mbuf.data() {
                        let len = mbuf.data_len() as usize;
                        let actual_len = len.min(data.len());
                        frames.push(data[..actual_len].to_vec());
                    }
                }
                Ok(frames)
            };

            let send_fn = move |frame: &[u8]| -> io::Result<usize> {
                let mut mbuf = resources_for_send.mempool.alloc()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("mbuf alloc: {}", e)))?;
                mbuf.set_data_len(frame.len() as u16);
                mbuf.set_packet_len(frame.len() as u32);
                let data = mbuf.data_mut()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "mbuf data_mut failed"))?;
                data.copy_from_slice(frame);
                let mut packets = vec![mbuf];
                let sent = resources_for_send.port.tx_burst(0, &mut packets)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("tx_burst: {}", e)))?;
                if sent == 0 {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "tx queue full"));
                }
                Ok(frame.len())
            };

            let topo = topology::start_pipeline(pipeline_config, recv_fn, send_fn);
            *socket.topology.lock().unwrap() = topo;
        }

        Ok(socket)
    }
}

impl Default for UdpSocketBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl UdpSocket {
    /// Returns a builder for configuring multi-core topology before binding.
    ///
    /// Most users should use `UdpSocket::bind()` directly, which auto-detects
    /// the optimal topology. Use the builder when you need explicit control.
    pub fn builder() -> UdpSocketBuilder {
        UdpSocketBuilder::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // API COMPATIBILITY TESTS
    // ========================================================================
    //
    // IMPORTANT: These tests verify that our UdpSocket API matches std::net::UdpSocket.
    // DO NOT modify these tests without ensuring the API change is intentional.
    // These are modeled after std::net::UdpSocket to ensure drop-in compatibility.
    //
    // Reference: https://doc.rust-lang.org/std/net/struct.UdpSocket.html
    // ========================================================================

    /// Test that UdpSocket::bind has the same signature as std::net::UdpSocket::bind
    /// std signature: pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket>
    #[test]
    fn test_api_bind_signature() {
        // Verify bind accepts ToSocketAddrs (string form)
        fn _bind_with_str() -> io::Result<UdpSocket> {
            UdpSocket::bind("127.0.0.1:0")
        }

        // Verify bind accepts ToSocketAddrs (SocketAddr form)
        fn _bind_with_socketaddr() -> io::Result<UdpSocket> {
            let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            UdpSocket::bind(addr)
        }

        // Verify bind returns io::Result<UdpSocket>
        fn _check_return_type(result: io::Result<UdpSocket>) -> io::Result<UdpSocket> {
            result
        }
    }

    /// Test that UdpSocket::send_to has the same signature as std::net::UdpSocket::send_to
    /// std signature: pub fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize>
    #[test]
    fn test_api_send_to_signature() {
        // Verify signature: &self, buf: &[u8], addr: impl ToSocketAddrs -> io::Result<usize>
        fn _send_to_with_str(socket: &UdpSocket) -> io::Result<usize> {
            socket.send_to(b"hello", "127.0.0.1:9000")
        }

        fn _send_to_with_socketaddr(socket: &UdpSocket) -> io::Result<usize> {
            let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();
            socket.send_to(b"hello", addr)
        }
    }

    /// Test that UdpSocket::recv_from has the same signature as std::net::UdpSocket::recv_from
    /// std signature: pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>
    #[test]
    fn test_api_recv_from_signature() {
        // Verify signature: &self, buf: &mut [u8] -> io::Result<(usize, SocketAddr)>
        fn _recv_from(socket: &UdpSocket) -> io::Result<(usize, SocketAddr)> {
            let mut buf = [0u8; 1024];
            socket.recv_from(&mut buf)
        }
    }

    /// Test that UdpSocket::local_addr has the same signature as std::net::UdpSocket::local_addr
    /// std signature: pub fn local_addr(&self) -> io::Result<SocketAddr>
    #[test]
    fn test_api_local_addr_signature() {
        fn _local_addr(socket: &UdpSocket) -> io::Result<SocketAddr> {
            socket.local_addr()
        }
    }

    /// Test that UdpSocket::peer_addr has the same signature as std::net::UdpSocket::peer_addr
    /// std signature: pub fn peer_addr(&self) -> io::Result<SocketAddr>
    #[test]
    fn test_api_peer_addr_signature() {
        fn _peer_addr(socket: &UdpSocket) -> io::Result<SocketAddr> {
            socket.peer_addr()
        }
    }

    /// Test that UdpSocket::connect has the same signature as std::net::UdpSocket::connect
    /// std signature: pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()>
    #[test]
    fn test_api_connect_signature() {
        fn _connect_with_str(socket: &UdpSocket) -> io::Result<()> {
            socket.connect("127.0.0.1:9000")
        }

        fn _connect_with_socketaddr(socket: &UdpSocket) -> io::Result<()> {
            let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();
            socket.connect(addr)
        }
    }

    /// Test that UdpSocket::send has the same signature as std::net::UdpSocket::send
    /// std signature: pub fn send(&self, buf: &[u8]) -> io::Result<usize>
    #[test]
    fn test_api_send_signature() {
        fn _send(socket: &UdpSocket) -> io::Result<usize> {
            socket.send(b"hello")
        }
    }

    /// Test that UdpSocket::recv has the same signature as std::net::UdpSocket::recv
    /// std signature: pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize>
    #[test]
    fn test_api_recv_signature() {
        fn _recv(socket: &UdpSocket) -> io::Result<usize> {
            let mut buf = [0u8; 1024];
            socket.recv(&mut buf)
        }
    }

    /// Test that UdpSocket::set_ttl has the same signature as std::net::UdpSocket::set_ttl
    /// std signature: pub fn set_ttl(&self, ttl: u32) -> io::Result<()>
    #[test]
    fn test_api_set_ttl_signature() {
        // Note: Our set_ttl takes &mut self while std takes &self
        fn _set_ttl(socket: &mut UdpSocket) -> io::Result<()> {
            socket.set_ttl(64)
        }
    }

    /// Test that UdpSocket::ttl has the same signature as std::net::UdpSocket::ttl
    /// std signature: pub fn ttl(&self) -> io::Result<u32>
    #[test]
    fn test_api_ttl_signature() {
        fn _ttl(socket: &UdpSocket) -> io::Result<u32> {
            socket.ttl()
        }
    }

    // ========================================================================
    // CHECKSUM TESTS
    // ========================================================================

    #[test]
    fn test_ipv4_checksum() {
        // Example IP header (without checksum)
        let mut header = [
            0x45, 0x00, // Version, IHL, DSCP, ECN
            0x00, 0x3c, // Total Length
            0x1c, 0x46, // Identification
            0x40, 0x00, // Flags, Fragment Offset
            0x40, 0x06, // TTL, Protocol
            0x00, 0x00, // Checksum (placeholder)
            0xac, 0x10, 0x0a, 0x63, // Source IP (172.16.10.99)
            0xac, 0x10, 0x0a, 0x0c, // Destination IP (172.16.10.12)
        ];

        let checksum = ipv4_checksum(&header);
        header[10..12].copy_from_slice(&checksum.to_be_bytes());

        // Verify checksum is valid (recalculating should give 0)
        let verify = ipv4_checksum(&header);
        assert_eq!(verify, 0);
    }

    #[test]
    fn test_udp_checksum() {
        let src_ip = [192, 168, 1, 1];
        let dst_ip = [192, 168, 1, 2];
        let udp_header = [
            0x30, 0x39, // Source port (12345)
            0x23, 0x28, // Dest port (9000)
            0x00, 0x0c, // Length (12 = 8 header + 4 payload)
            0x00, 0x00, // Checksum (placeholder)
        ];
        let payload = b"test";

        let checksum = udp_checksum(&src_ip, &dst_ip, &udp_header, payload);
        // Just verify it's non-zero and computable
        assert_ne!(checksum, 0);
    }

    #[test]
    fn test_constants() {
        assert_eq!(ETH_HEADER_LEN, 14);
        assert_eq!(IPV4_HEADER_LEN, 20);
        assert_eq!(UDP_HEADER_LEN, 8);
        assert_eq!(TOTAL_HEADER_LEN, 42);
        assert_eq!(MAX_UDP_PAYLOAD, 1472);
    }

    #[test]
    fn test_payload_too_large() {
        let large_payload = vec![0u8; MAX_UDP_PAYLOAD + 1];
        // This would fail in build_udp_packet, but we can test the error type exists
        let err = UdpError::PayloadTooLarge { max: MAX_UDP_PAYLOAD, actual: large_payload.len() };
        assert!(err.to_string().contains("too large"));
    }

    struct EchoHandler;
    impl UdpHandler for EchoHandler {
        fn on_packet(&self, _src_ip: [u8; 4], _src_port: u16, _dst_ip: [u8; 4], _dst_port: u16, payload: &[u8]) -> Option<Vec<u8>> {
            Some(payload.to_vec())
        }
    }

    // ========================================================================
    // PACKET PARSING TESTS
    // ========================================================================

    /// Helper to build a valid UDP packet for testing
    fn build_test_udp_frame(
        src_ip: [u8; 4],
        dst_ip: [u8; 4],
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let total_len = TOTAL_HEADER_LEN + payload.len();
        let mut frame = vec![0u8; total_len];

        // Ethernet header
        frame[0..6].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]); // dst mac
        frame[6..12].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]); // src mac
        frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

        // IP header
        let ip_start = ETH_HEADER_LEN;
        frame[ip_start] = 0x45; // version 4, IHL 5
        frame[ip_start + 1] = 0x00; // DSCP/ECN
        let ip_total_len = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
        frame[ip_start + 2..ip_start + 4].copy_from_slice(&ip_total_len.to_be_bytes());
        frame[ip_start + 8] = 64; // TTL
        frame[ip_start + 9] = IP_PROTO_UDP;
        frame[ip_start + 12..ip_start + 16].copy_from_slice(&src_ip);
        frame[ip_start + 16..ip_start + 20].copy_from_slice(&dst_ip);

        // UDP header
        let udp_start = ETH_HEADER_LEN + IPV4_HEADER_LEN;
        frame[udp_start..udp_start + 2].copy_from_slice(&src_port.to_be_bytes());
        frame[udp_start + 2..udp_start + 4].copy_from_slice(&dst_port.to_be_bytes());
        let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
        frame[udp_start + 4..udp_start + 6].copy_from_slice(&udp_len.to_be_bytes());

        // Payload
        frame[TOTAL_HEADER_LEN..].copy_from_slice(payload);

        frame
    }

    #[test]
    fn test_parse_udp_packet_valid() {
        let frame = build_test_udp_frame(
            [192, 168, 1, 100],
            [192, 168, 1, 1],
            12345,
            9000,
            b"hello world",
        );

        let parsed = parse_udp_packet(&frame);
        assert!(parsed.is_some());

        let p = parsed.unwrap();
        assert_eq!(p.src_ip, Ipv4Addr::new(192, 168, 1, 100));
        assert_eq!(p.dst_ip, Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(p.src_port, 12345);
        assert_eq!(p.dst_port, 9000);
        assert_eq!(p.payload, b"hello world");
    }

    #[test]
    fn test_parse_udp_packet_empty_payload() {
        let frame = build_test_udp_frame(
            [10, 0, 0, 1],
            [10, 0, 0, 2],
            8080,
            80,
            b"",
        );

        let parsed = parse_udp_packet(&frame);
        assert!(parsed.is_some());

        let p = parsed.unwrap();
        assert_eq!(p.src_port, 8080);
        assert_eq!(p.dst_port, 80);
        assert!(p.payload.is_empty());
    }

    #[test]
    fn test_parse_udp_packet_too_short() {
        let frame = vec![0u8; 10]; // Way too short
        let parsed = parse_udp_packet(&frame);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_parse_udp_packet_wrong_ethertype() {
        let mut frame = build_test_udp_frame(
            [192, 168, 1, 1],
            [192, 168, 1, 2],
            1234,
            5678,
            b"test",
        );
        // Change ethertype to ARP
        frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());

        let parsed = parse_udp_packet(&frame);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_parse_udp_packet_not_udp() {
        let mut frame = build_test_udp_frame(
            [192, 168, 1, 1],
            [192, 168, 1, 2],
            1234,
            5678,
            b"test",
        );
        // Change protocol to TCP (6)
        frame[ETH_HEADER_LEN + 9] = 6;

        let parsed = parse_udp_packet(&frame);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_parse_udp_packet_extracts_macs() {
        let frame = build_test_udp_frame(
            [10, 0, 0, 1],
            [10, 0, 0, 2],
            1000,
            2000,
            b"x",
        );

        let parsed = parse_udp_packet(&frame).unwrap();
        assert_eq!(parsed.src_mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(parsed.dst_mac, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn test_parsed_udp_packet_debug() {
        let frame = build_test_udp_frame([1, 2, 3, 4], [5, 6, 7, 8], 100, 200, b"x");
        let parsed = parse_udp_packet(&frame).unwrap();
        // Just ensure Debug is implemented and doesn't panic
        let _ = format!("{:?}", parsed);
    }

    #[test]
    fn test_synthetic_socket_echo() {
        let socket = SyntheticUdpSocket::new([192, 168, 1, 1], 9000, Box::new(EchoHandler));

        // Build a test packet
        let mut frame = vec![0u8; TOTAL_HEADER_LEN + 4];

        // Ethernet
        frame[0..6].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]); // dst mac
        frame[6..12].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]); // src mac
        frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

        // IP header
        let ip_start = ETH_HEADER_LEN;
        frame[ip_start] = 0x45;
        frame[ip_start + 2..ip_start + 4].copy_from_slice(&32u16.to_be_bytes()); // total len
        frame[ip_start + 8] = 64; // TTL
        frame[ip_start + 9] = IP_PROTO_UDP;
        frame[ip_start + 12..ip_start + 16].copy_from_slice(&[10, 0, 0, 1]); // src ip
        frame[ip_start + 16..ip_start + 20].copy_from_slice(&[192, 168, 1, 1]); // dst ip

        // UDP header
        let udp_start = ETH_HEADER_LEN + IPV4_HEADER_LEN;
        frame[udp_start..udp_start + 2].copy_from_slice(&12345u16.to_be_bytes()); // src port
        frame[udp_start + 2..udp_start + 4].copy_from_slice(&9000u16.to_be_bytes()); // dst port
        frame[udp_start + 4..udp_start + 6].copy_from_slice(&12u16.to_be_bytes()); // length

        // Payload
        frame[TOTAL_HEADER_LEN..].copy_from_slice(b"test");

        let result = socket.parse_and_handle(&frame);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.is_some());

        let resp_frame = response.unwrap();
        assert_eq!(&resp_frame[TOTAL_HEADER_LEN..], b"test");
    }

    // ========================================================================
    // BUILD_UDP_FRAME TESTS (backend-agnostic packet building)
    // ========================================================================

    #[test]
    fn test_build_udp_frame_basic() {
        let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let dst_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let src_ip = Ipv4Addr::new(192, 168, 1, 1);
        let dst_ip = Ipv4Addr::new(192, 168, 1, 2);

        let frame = build_udp_frame(
            &src_mac, &dst_mac,
            src_ip, dst_ip,
            12345, 9000,
            b"hello world",
            64,
        );
        assert!(frame.is_ok());

        let frame = frame.unwrap();
        assert_eq!(frame.len(), TOTAL_HEADER_LEN + 11); // 42 + "hello world"

        // Parse it back
        let parsed = parse_udp_packet(&frame);
        assert!(parsed.is_some());

        let p = parsed.unwrap();
        assert_eq!(p.src_mac, src_mac);
        assert_eq!(p.dst_mac, dst_mac);
        assert_eq!(p.src_ip, src_ip);
        assert_eq!(p.dst_ip, dst_ip);
        assert_eq!(p.src_port, 12345);
        assert_eq!(p.dst_port, 9000);
        assert_eq!(p.payload, b"hello world");
    }

    #[test]
    fn test_build_udp_frame_empty_payload() {
        let frame = build_udp_frame(
            &[0; 6], &[0xff; 6],
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            8080, 80,
            b"",
            128,
        ).unwrap();

        assert_eq!(frame.len(), TOTAL_HEADER_LEN);
        let parsed = parse_udp_packet(&frame).unwrap();
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn test_build_udp_frame_payload_too_large() {
        let large_payload = vec![0u8; MAX_UDP_PAYLOAD + 1];
        let result = build_udp_frame(
            &[0; 6], &[0; 6],
            Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED,
            0, 0,
            &large_payload,
            64,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_build_udp_frame_checksums_valid() {
        let frame = build_udp_frame(
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
            &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            Ipv4Addr::new(172, 16, 0, 1),
            Ipv4Addr::new(172, 16, 0, 2),
            5000, 6000,
            b"checksum test data",
            64,
        ).unwrap();

        // Verify IP checksum (should be valid when recalculated)
        let ip_start = ETH_HEADER_LEN;
        let ip_verify = ipv4_checksum(&frame[ip_start..ip_start + IPV4_HEADER_LEN]);
        assert_eq!(ip_verify, 0, "IP checksum should verify to 0");
    }

    #[test]
    fn test_build_udp_frame_roundtrip() {
        // Build and parse various frame sizes
        for payload_size in [0, 1, 100, 500, 1000, MAX_UDP_PAYLOAD] {
            let payload = vec![0xAB; payload_size];
            let frame = build_udp_frame(
                &[1, 2, 3, 4, 5, 6],
                &[7, 8, 9, 10, 11, 12],
                Ipv4Addr::new(1, 2, 3, 4),
                Ipv4Addr::new(5, 6, 7, 8),
                1111, 2222,
                &payload,
                255,
            ).unwrap();

            let parsed = parse_udp_packet(&frame).unwrap();
            assert_eq!(parsed.payload.len(), payload_size);
            assert_eq!(parsed.payload, payload);
        }
    }

    // ========================================================================
    // BACKEND CONFIG TESTS
    // ========================================================================

    #[test]
    fn test_backend_config_default() {
        let config = BackendConfig::default();
        assert_eq!(config.backend_type, BackendType::Auto);
        assert!(config.interface_name.is_none());
        assert_eq!(config.dpdk_port_id, 0);
        assert!(config.use_mmap);
    }

    #[test]
    fn test_backend_config_dpdk() {
        let config = BackendConfig::new().with_dpdk(1);
        assert_eq!(config.backend_type, BackendType::Dpdk);
        assert_eq!(config.dpdk_port_id, 1);
    }

    #[test]
    fn test_backend_config_raw_socket() {
        let config = BackendConfig::new().with_raw_socket("eth0");
        assert_eq!(config.backend_type, BackendType::RawSocket);
        assert_eq!(config.interface_name.as_deref(), Some("eth0"));
        assert!(!config.use_mmap);
    }

    #[test]
    fn test_backend_config_raw_socket_mmap() {
        let config = BackendConfig::new().with_raw_socket_mmap("ens5");
        assert_eq!(config.backend_type, BackendType::RawSocketMmap);
        assert_eq!(config.interface_name.as_deref(), Some("ens5"));
        assert!(config.use_mmap);
    }

    #[test]
    fn test_backend_config_builder_chain() {
        let config = BackendConfig::new()
            .with_raw_socket_mmap("eth0")
            .with_ring_frame_size(4096)
            .with_ring_frame_count(512)
            .with_promiscuous(true);
        assert_eq!(config.ring_frame_size, 4096);
        assert_eq!(config.ring_frame_count, 512);
        assert!(config.promiscuous);
    }

    // ========================================================================
    // BACKEND TYPE TESTS
    // ========================================================================

    #[test]
    fn test_backend_type_equality() {
        assert_eq!(BackendType::Dpdk, BackendType::Dpdk);
        assert_eq!(BackendType::RawSocket, BackendType::RawSocket);
        assert_eq!(BackendType::RawSocketMmap, BackendType::RawSocketMmap);
        assert_eq!(BackendType::Auto, BackendType::Auto);
        assert_ne!(BackendType::Dpdk, BackendType::RawSocket);
    }

    // ========================================================================
    // PHASE B: BUILDER TOPOLOGY CONFIGURATION TESTS
    // ========================================================================

    #[test]
    fn test_builder_default_is_run_to_completion_under_stubs() {
        // Under stubs, builder.bind() should produce a run-to-completion socket
        // (no pipeline threads, no topology overhead)
        let socket = UdpSocket::builder()
            .bind("127.0.0.1:0")
            .expect("builder bind should succeed");
        assert!(socket.is_run_to_completion());
        assert!(socket.topology_plan().is_none());
    }

    #[test]
    fn test_builder_explicit_rtc_override() {
        // Explicitly requesting workers_per_queue(0) forces run-to-completion
        // even if more cores would be available (under stubs this is the default
        // anyway, but the explicit setting is tested for the API contract)
        let socket = UdpSocket::builder()
            .rx_queues(1)
            .workers_per_queue(0)
            .bind("127.0.0.1:0")
            .expect("builder bind should succeed");
        assert!(socket.is_run_to_completion());
    }

    #[test]
    fn test_builder_topology_config() {
        // Test that builder correctly produces TopologyConfig
        let builder = UdpSocketBuilder::new()
            .rx_queues(4)
            .workers_per_queue(2);
        let config = builder.topology_config();
        assert_eq!(config.rx_queues, Some(4));
        assert_eq!(config.workers_per_queue, Some(2));
    }

    #[test]
    fn test_builder_partial_config() {
        // Setting only one parameter should leave the other as auto-detect
        let builder = UdpSocketBuilder::new()
            .workers_per_queue(0);
        let config = builder.topology_config();
        assert_eq!(config.rx_queues, None);
        assert_eq!(config.workers_per_queue, Some(0));
    }

    #[test]
    fn test_socket_is_run_to_completion_by_default() {
        // Standard bind() with no builder should be run-to-completion
        let socket = UdpSocket::bind("127.0.0.1:0")
            .expect("bind should succeed");
        assert!(socket.is_run_to_completion());
    }
}
