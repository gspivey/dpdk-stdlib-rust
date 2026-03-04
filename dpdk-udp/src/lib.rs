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
use std::collections::VecDeque;

use dpdk::{Mbuf, Mempool, Port};
use dpdk::port::PortConfig;
use dpdk::mbuf::MempoolConfig;

pub use dpdk::port::{MacAddress, RxOffload as HwRxOffload, TxOffload as HwTxOffload};

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

pub use arp::{ArpCache, ArpHandler, ArpPacket};
pub use icmp::{IcmpHandler, IcmpPacket};
pub use backend::{PacketBackend, BackendConfig, BackendType};
pub use backend_dpdk::DpdkBackend;
pub use backend_raw::RawSocketBackend;

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

    // Create shared ARP cache
    let arp_cache = Arc::new(ArpCache::new());

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

        println!("✅ DPDK UDP socket bound to {} (MAC: {})", addr, resources.src_mac);

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
            backend_name, addr,
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
        })
    }

    /// Get the name of the active packet I/O backend.
    pub fn active_backend(&self) -> &'static str {
        self.socket_backend.backend_name()
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
    /// Uses `build_udp_frame()` to construct the packet and sends it via the
    /// active backend (DPDK or generic PacketBackend).
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

        // Build the frame using backend-agnostic builder
        let frame = build_udp_frame(
            &src_mac,
            &dst_mac.octets(),
            src_ip,
            dst_ip,
            src_port,
            dst_port,
            buf,
            self.ttl,
        ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("packet build failed: {}", e)))?;

        // Send via the active backend
        self.socket_backend.send_frame(&frame)?;

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
        // Get our local port for filtering (do this once outside the loop)
        let local_port = match self.local_addr {
            SocketAddr::V4(v4) => v4.port(),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPv6 not supported"));
            }
        };

        // Block until we receive a matching packet (std::net::UdpSocket behavior).
        // DPDK rx_burst is non-blocking, so we poll in a loop with a short sleep
        // to avoid burning 100% CPU while still achieving low latency.
        loop {
            // First check if we have buffered packets
            {
                let mut queue = self.recv_queue.lock().unwrap();
                if let Some((payload, src_addr)) = queue.pop() {
                    let copy_len = std::cmp::min(buf.len(), payload.len());
                    buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                    return Ok((copy_len, src_addr));
                }
            }

            // Receive raw frames via the active backend
            let frames = self.socket_backend.recv_frames(32)?;

            if frames.is_empty() {
                // No packets available — sleep briefly and retry (blocking behavior)
                std::thread::sleep(std::time::Duration::from_micros(100));
                continue;
            }

            // Process frames, handling ARP/ICMP and looking for UDP packets for our port
            let mut result: Option<(usize, SocketAddr)> = None;

            for frame_data in &frames {
                // Check ethertype
                if frame_data.len() >= 14 {
                    let ethertype = u16::from_be_bytes([frame_data[12], frame_data[13]]);

                    // Handle ARP packets (reuse existing handler)
                    if ethertype == arp::ETH_TYPE_ARP && self.auto_arp {
                        if let Some(reply_frame) = self.arp_handler.process_arp(frame_data) {
                            // Send ARP reply via backend
                            let _ = self.socket_backend.send_frame(&reply_frame);
                        }
                        continue;
                    }

                    // Handle ICMP packets (reuse existing handler)
                    if ethertype == ETH_TYPE_IPV4 && frame_data.len() > ETH_HEADER_LEN + 9 {
                        let protocol = frame_data[ETH_HEADER_LEN + 9];
                        if protocol == icmp::IP_PROTO_ICMP && self.auto_icmp {
                            if let Some(reply_frame) = self.icmp_handler.process_icmp(frame_data) {
                                // Send ICMP reply via backend
                                let _ = self.socket_backend.send_frame(&reply_frame);
                            }
                            continue;
                        }
                    }
                }

                // Try to parse as UDP (reuse existing parser)
                if let Some(parsed) = parse_udp_packet(frame_data) {
                    // Learn source MAC from incoming packets for reply routing.
                    // This ensures send_to() uses the correct destination MAC
                    // instead of broadcast, which is important for DPDK backends
                    // where the NIC doesn't handle ARP automatically.
                    if frame_data.len() >= 12 {
                        let src_mac: [u8; 6] = frame_data[6..12].try_into().unwrap();
                        self.arp_handler.cache.insert(
                            parsed.src_ip,
                            MacAddress::new(src_mac),
                        );
                    }

                    // Check if this packet is for us
                    if parsed.dst_port == local_port {
                        let src_addr = SocketAddr::V4(
                            SocketAddrV4::new(parsed.src_ip, parsed.src_port)
                        );

                        // If connected, only accept packets from the connected address
                        if let Some(connected) = *self.connected_addr.lock().unwrap() {
                            if src_addr != connected {
                                // Queue for later if not from connected peer
                                let mut queue = self.recv_queue.lock().unwrap();
                                queue.push(parsed.payload, src_addr);
                                continue;
                            }
                        }

                        // If we haven't found a result yet, use this one
                        if result.is_none() {
                            let copy_len = std::cmp::min(buf.len(), parsed.payload.len());
                            buf[..copy_len].copy_from_slice(&parsed.payload[..copy_len]);

                            // Update connection state
                            if let Ok(mut guard) = self.connection_state.write() {
                                if let Some(ref mut state) = *guard {
                                    state.record_recv(copy_len);
                                }
                            }

                            result = Some((copy_len, src_addr));
                        } else {
                            // Queue additional packets
                            let mut queue = self.recv_queue.lock().unwrap();
                            queue.push(parsed.payload, src_addr);
                        }
                    }
                }
                // Packet not for us or not valid UDP - dropped
            }

            if let Some(r) = result {
                return Ok(r);
            }
            // No matching UDP packets in this batch — continue polling
        }
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
    /// Sends an ARP request and polls for the reply. Returns the resolved MAC
    /// or falls back to broadcast MAC if no reply is received.
    fn resolve_arp(&self, target_ip: &Ipv4Addr) -> io::Result<MacAddress> {
        // Build and send ARP request
        if let Some(arp_frame) = self.arp_handler.make_request(*target_ip) {
            self.socket_backend.send_frame(&arp_frame)?;
        } else {
            return Ok(self.dst_mac.clone());
        }

        // Poll for ARP reply (up to 3 seconds with 100us intervals)
        for _ in 0..30_000 {
            let frames = self.socket_backend.recv_frames(32)?;
            for frame_data in &frames {
                if frame_data.len() >= 14 {
                    let ethertype = u16::from_be_bytes([frame_data[12], frame_data[13]]);
                    if ethertype == arp::ETH_TYPE_ARP {
                        // Process ARP (learns from replies, responds to requests)
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

            // Check if ARP resolved
            if let Some(mac) = self.arp_handler.resolve(target_ip) {
                return Ok(mac);
            }

            std::thread::sleep(std::time::Duration::from_micros(100));
        }

        // Fallback to broadcast if ARP didn't resolve
        Ok(self.dst_mac.clone())
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
}
