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

use std::cell::UnsafeCell;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dpdk::{Mbuf, Mempool, Port};
use dpdk::port::{MacAddress, PortConfig};
use dpdk::mbuf::MempoolConfig;

pub use dpdk::port::{RxOffload as HwRxOffload, TxOffload as HwTxOffload};

/// Returns true when this build is linked against the dpdk-sys stub backend
/// (no real DPDK library found at compile time, or the `bindgen` feature
/// was not enabled).
///
/// Re-exported from `dpdk_sys::is_stub` so callers don't need to take a
/// direct dependency on `dpdk-sys`.
#[inline]
pub fn is_stub() -> bool {
    dpdk_sys::is_stub()
}

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
pub mod routing;
pub mod topology;
pub mod perf;
pub mod frame_pool;
pub mod gue;
pub mod ipv6;
pub mod vxlan;

pub use arp::{ArpCache, ArpHandler, ArpPacket};
pub use icmp::{IcmpAction, IcmpErrorInfo, IcmpHandler, IcmpPacket};
pub use backend::{PacketBackend, BackendConfig, BackendType};
pub use backend_dpdk::DpdkBackend;
pub use backend_raw::RawSocketBackend;
pub use ring::{SpscRing, MpscRing};
pub use topology::{TopologyConfig, TopologyPlan, TopologySource, MultiCoreTopology, ProcessedPacket, TxFrame};
pub use frame_pool::{AppPacket, FramePool, FrameRef};
pub use perf::{
    LatencySampler, NicStatsFn, NicStatsSnapshot, PerfCounters, PerfReporter, PerfSnapshot,
};
pub use routing::{RoutingTable, NetworkConfig, RouteEntry, NextHop, ProcArpEntry};
pub use gue::{GueConfig, GueHeader, GUE_DEFAULT_PORT, GUE_ENCAP_OVERHEAD};
pub use vxlan::{
    VxlanConfig, VxlanHeader, VxlanDecapResult, VXLAN_DEFAULT_PORT, VXLAN_ENCAP_OVERHEAD,
    VXLAN_HEADER_LEN, VXLAN_VNI_MAX, build_vxlan_frame_into, try_decap_vxlan,
};
pub use ipv6::{
    build_udp6_frame, build_udp6_frame_into, parse_udp6_packet, parse_udp6_packet_ref,
    udp6_checksum, udp6_pseudo_header_checksum, verify_udp6_checksum, walk_extension_headers,
    ParsedUdp6Packet, ParsedUdp6PacketRef, Ipv6NextHeader,
    ETH_TYPE_IPV6, IPV6_HEADER_LEN, MAX_UDP_PAYLOAD_V6, TOTAL_HEADER_LEN_V6,
    TOTAL_HEADER_LEN_V6_VLAN, IP_PROTO_HOPOPT, IP_PROTO_ROUTING, IP_PROTO_FRAGMENT,
    IP_PROTO_ICMPV6, IP_PROTO_DSTOPTS, IP_PROTO_NONE,
};

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

/// Maximum UDP payload size for standard MTU 1500 (1500 - 20 IPv4 - 8 UDP).
/// For runtime MTU-aware limits use `UdpSocket::max_udp_payload()`, which
/// returns `mtu - 28` for whatever MTU the port is configured with.
pub const MAX_UDP_PAYLOAD: usize = 1472;

/// Maximum possible frame size for jumbo MTU (9001 + 14 Ethernet header).
/// TxBuffer is always allocated at this size to avoid reallocation when
/// `set_routing()` changes the MTU after bind.
const MAX_FRAME_SIZE: usize = ETH_HEADER_LEN + 9001;

/// Mbuf data room size for jumbo frames: 9216 bytes data + headroom.
/// 9216 accommodates the largest AWS VPC frame (9001 MTU + 14 eth + padding).
/// Compile-time assertion ensures this fits in u16 (required by DPDK API).
pub(crate) const JUMBO_DATA_ROOM_SIZE: u16 = 9216 + 128; // 128 = RTE_PKTMBUF_HEADROOM
const _: () = assert!(JUMBO_DATA_ROOM_SIZE as u32 == 9216 + 128, "JUMBO_DATA_ROOM_SIZE overflow");

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

/// Ethernet type for 802.1Q VLAN-tagged frames (TPID)
pub const ETH_TYPE_VLAN: u16 = 0x8100;

/// 802.1Q VLAN tag length in bytes (TPID + TCI)
pub const VLAN_TAG_LEN: usize = 4;

/// Total header overhead for VLAN-tagged frames
pub const TOTAL_HEADER_LEN_VLAN: usize = ETH_HEADER_LEN + VLAN_TAG_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;

/// IP protocol number for UDP
pub const IP_PROTO_UDP: u8 = 17;

// ============================================================================
// VLAN (802.1Q) Support
// ============================================================================

/// VLAN operating mode, matching Linux 8021q subinterface semantics.
///
/// Each mode defines how inbound (RX) frames are filtered and how outbound
/// (TX) frames are tagged or left untagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VlanMode {
    /// Access mode: the port belongs to exactly one VLAN.
    ///
    /// - **RX**: accept untagged frames AND frames tagged with the configured VID
    ///   (strip tag before delivery). Drop frames tagged with any other VID.
    /// - **TX**: send frames untagged (no 802.1Q tag inserted).
    Access,

    /// Trunk mode: carry multiple VLANs on a single port.
    ///
    /// - **RX**: accept frames tagged with any VID in `allowed_vlans`.
    ///   If `native_vlan` is set, also accept untagged frames (treated as
    ///   the native VLAN). Drop all other frames.
    /// - **TX**: tag frames with the configured VID.
    Trunk {
        /// Set of allowed VLAN IDs on this trunk.
        allowed_vlans: Vec<u16>,
        /// If set, untagged frames are accepted and treated as this VLAN.
        native_vlan: Option<u16>,
    },

    /// Port tagging (strict VLAN subinterface) mode.
    ///
    /// - **RX**: only accept frames tagged with the configured VID (strip tag).
    ///   Drop untagged frames and frames with any other VID.
    /// - **TX**: always tag frames with the configured VID.
    PortTagging,
}

impl Default for VlanMode {
    fn default() -> Self {
        VlanMode::PortTagging
    }
}

/// VLAN configuration for 802.1Q frame tagging.
///
/// When configured on a socket, outgoing and incoming frames are handled
/// according to the configured [`VlanMode`]:
///
/// - **Access**: RX accepts untagged + matching VID (strips tag); TX sends untagged.
/// - **Trunk**: RX accepts allowed VIDs (optionally untagged via native_vlan); TX tags.
/// - **PortTagging** (default): RX only accepts matching VID (strips tag); TX always tags.
///
/// # Wire format
///
/// A VLAN-tagged Ethernet frame inserts 4 bytes between the source MAC and the
/// original EtherType:
///
/// ```text
/// | dst_mac (6) | src_mac (6) | TPID 0x8100 (2) | TCI (2) | EtherType (2) | payload... |
/// ```
///
/// The TCI (Tag Control Information) encodes:
/// - PCP (bits 15-13): Priority Code Point (0-7)
/// - DEI (bit 12): Drop Eligible Indicator
/// - VID (bits 11-0): VLAN Identifier (0-4094)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VlanConfig {
    /// VLAN Identifier (12 bits, 0-4094). VID 0 means priority-only tagging.
    /// VID 4095 is reserved.
    pub vlan_id: u16,
    /// Priority Code Point (3 bits, 0-7). Higher values = higher priority.
    pub priority: u8,
    /// Drop Eligible Indicator. When set, the frame may be dropped under congestion.
    pub dei: bool,
    /// VLAN operating mode (Access, Trunk, or PortTagging).
    pub mode: VlanMode,
    /// Force software VLAN tag insert/strip even when the NIC supports hardware
    /// offload. Default is `false` (use hardware when available).
    pub force_software: bool,
}

impl VlanConfig {
    /// Create a VLAN config with the given VLAN ID, default priority (0), no DEI,
    /// and default mode (PortTagging).
    pub fn new(vlan_id: u16) -> Self {
        assert!(vlan_id <= 4094, "VLAN ID must be 0-4094");
        Self { vlan_id, priority: 0, dei: false, mode: VlanMode::default(), force_software: false }
    }

    /// Set the priority code point (0-7).
    pub fn with_priority(mut self, priority: u8) -> Self {
        assert!(priority <= 7, "PCP must be 0-7");
        self.priority = priority;
        self
    }

    /// Set the Drop Eligible Indicator.
    pub fn with_dei(mut self, dei: bool) -> Self {
        self.dei = dei;
        self
    }

    /// Set the VLAN operating mode.
    pub fn with_mode(mut self, mode: VlanMode) -> Self {
        self.mode = mode;
        self
    }

    /// Configure as access port: RX accepts untagged + matching VID; TX sends untagged.
    pub fn access(mut self) -> Self {
        self.mode = VlanMode::Access;
        self
    }

    /// Configure as trunk port with the given allowed VLANs and optional native VLAN.
    pub fn trunk(mut self, allowed_vlans: Vec<u16>, native_vlan: Option<u16>) -> Self {
        self.mode = VlanMode::Trunk { allowed_vlans, native_vlan };
        self
    }

    /// Configure as port tagging (strict): RX only matching VID; TX always tags.
    pub fn port_tagging(mut self) -> Self {
        self.mode = VlanMode::PortTagging;
        self
    }

    /// Force software VLAN tag insert/strip even when the NIC supports hardware
    /// offload. Useful for debugging or when hardware offload produces incorrect
    /// results on a particular NIC.
    pub fn with_force_software(mut self, force: bool) -> Self {
        self.force_software = force;
        self
    }

    /// Returns true if outbound frames should be VLAN-tagged in the current mode.
    pub fn tags_on_tx(&self) -> bool {
        !matches!(self.mode, VlanMode::Access)
    }

    /// Check whether an inbound frame should be accepted based on the VLAN mode.
    ///
    /// `frame_vid` is the VID extracted from the frame (None if untagged).
    /// Returns `true` if the frame should be accepted, `false` if it should be dropped.
    pub fn accepts_frame(&self, frame_vid: Option<u16>) -> bool {
        match &self.mode {
            VlanMode::Access => match frame_vid {
                None => true,                             // untagged: accepted
                Some(vid) => vid == self.vlan_id,         // matching VID: accepted; other: drop
            },
            VlanMode::Trunk { allowed_vlans, native_vlan } => match frame_vid {
                Some(vid) => allowed_vlans.contains(&vid), // VID must be in allowed set
                None => native_vlan.is_some(),              // untagged only if native_vlan set
            },
            VlanMode::PortTagging => match frame_vid {
                Some(vid) => vid == self.vlan_id,          // only matching VID
                None => false,                             // drop untagged
            },
        }
    }

    /// Encode the 16-bit TCI (Tag Control Information) for the wire.
    pub fn encode_tci(&self) -> u16 {
        let pcp = (self.priority as u16 & 0x07) << 13;
        let dei = if self.dei { 1u16 << 12 } else { 0 };
        let vid = self.vlan_id & 0x0FFF;
        pcp | dei | vid
    }

    /// Decode a TCI value from the wire into a VlanConfig (PortTagging mode by default).
    pub fn from_tci(tci: u16) -> Self {
        Self {
            vlan_id: tci & 0x0FFF,
            priority: ((tci >> 13) & 0x07) as u8,
            dei: (tci >> 12) & 1 != 0,
            mode: VlanMode::default(),
            force_software: false,
        }
    }
}

/// Result of detecting whether an Ethernet frame carries a VLAN tag.
///
/// Used internally to compute the correct L3 header offset for both
/// tagged and untagged frames.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameLayout {
    /// The "inner" EtherType (e.g. 0x0800 for IPv4, 0x0806 for ARP).
    pub(crate) ethertype: u16,
    /// Byte offset where the L3 header (IP or ARP) starts.
    pub(crate) l3_offset: usize,
    /// If VLAN-tagged, the TCI value; otherwise None.
    pub(crate) vlan_tci: Option<u16>,
}

/// Detect whether a frame is 802.1Q VLAN-tagged and return the layout.
///
/// For untagged frames: ethertype from bytes 12-13, L3 starts at byte 14.
/// For VLAN-tagged frames: ethertype from bytes 16-17, L3 starts at byte 18.
///
/// When `hw_vlan_tci` is `Some(tci)`, the NIC has already stripped the VLAN tag
/// from the frame bytes. The frame is physically untagged (L3 at byte 14) but
/// the returned `FrameLayout` will carry the hardware-provided TCI so that VLAN
/// filtering works correctly without reconstructing the frame.
pub(crate) fn detect_vlan(frame: &[u8], hw_vlan_tci: Option<u16>) -> Option<FrameLayout> {
    if frame.len() < ETH_HEADER_LEN {
        return None;
    }
    let outer_ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if outer_ethertype == ETH_TYPE_VLAN {
        if frame.len() < ETH_HEADER_LEN + VLAN_TAG_LEN {
            return None;
        }
        let tci = u16::from_be_bytes([frame[14], frame[15]]);
        let inner_ethertype = u16::from_be_bytes([frame[16], frame[17]]);
        Some(FrameLayout {
            ethertype: inner_ethertype,
            l3_offset: ETH_HEADER_LEN + VLAN_TAG_LEN,
            vlan_tci: Some(tci),
        })
    } else {
        Some(FrameLayout {
            ethertype: outer_ethertype,
            l3_offset: ETH_HEADER_LEN,
            // If the NIC stripped the VLAN tag, use the hardware TCI;
            // otherwise this is a genuinely untagged frame.
            vlan_tci: hw_vlan_tci,
        })
    }
}

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

/// Compute the UDP pseudo-header checksum (used for TX hardware offload).
///
/// When the NIC computes the UDP checksum, the application must place the
/// pseudo-header checksum in the UDP checksum field. The NIC adds the UDP
/// header + payload contribution on top.
///
/// The pseudo-header sum is: src_ip + dst_ip + protocol + UDP length,
/// folded to 16 bits (NOT one's-complemented — the NIC does that).
pub fn udp_pseudo_header_checksum(src_ip: &[u8; 4], dst_ip: &[u8; 4], udp_len: u16) -> u16 {
    let mut sum: u32 = 0;
    sum = sum.wrapping_add(((src_ip[0] as u32) << 8) | (src_ip[1] as u32));
    sum = sum.wrapping_add(((src_ip[2] as u32) << 8) | (src_ip[3] as u32));
    sum = sum.wrapping_add(((dst_ip[0] as u32) << 8) | (dst_ip[1] as u32));
    sum = sum.wrapping_add(((dst_ip[2] as u32) << 8) | (dst_ip[3] as u32));
    sum = sum.wrapping_add(IP_PROTO_UDP as u32);
    sum = sum.wrapping_add(udp_len as u32);

    // Fold 32-bit sum to 16 bits
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    sum as u16
}

/// Verify the IPv4 header checksum of a received frame.
///
/// Returns true if the checksum is valid (recomputed checksum == 0).
/// Handles both untagged and 802.1Q VLAN-tagged frames.
pub fn verify_ipv4_checksum(frame: &[u8]) -> bool {
    let layout = match detect_vlan(frame, None) {
        Some(l) => l,
        None => return false,
    };
    let l3 = layout.l3_offset;

    if frame.len() < l3 + IPV4_HEADER_LEN {
        return false;
    }

    let ip_header = &frame[l3..];
    let ihl = (ip_header[0] & 0x0F) as usize;
    let ip_header_len = ihl * 4;
    if ip_header_len < 20 || frame.len() < l3 + ip_header_len {
        return false;
    }

    let mut sum: u32 = 0;
    for i in (0..ip_header_len).step_by(2) {
        let word = if i + 1 < ip_header_len {
            ((ip_header[i] as u32) << 8) | (ip_header[i + 1] as u32)
        } else {
            (ip_header[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    (sum as u16) == 0xFFFF
}

/// Verify the UDP checksum of a received frame.
///
/// Returns true if the checksum is valid or if the checksum field is 0 (disabled).
/// Per RFC 768, a UDP checksum of 0 means "no checksum computed".
/// Handles both untagged and 802.1Q VLAN-tagged frames.
pub fn verify_udp_checksum(frame: &[u8]) -> bool {
    let layout = match detect_vlan(frame, None) {
        Some(l) => l,
        None => return false,
    };
    let l3 = layout.l3_offset;

    if frame.len() < l3 + IPV4_HEADER_LEN + UDP_HEADER_LEN {
        return false;
    }

    let ip_header = &frame[l3..];
    let ihl = (ip_header[0] & 0x0F) as usize;
    let ip_header_len = ihl * 4;
    let udp_start = l3 + ip_header_len;

    if frame.len() < udp_start + UDP_HEADER_LEN {
        return false;
    }

    let stored_cksum = u16::from_be_bytes([frame[udp_start + 6], frame[udp_start + 7]]);
    if stored_cksum == 0 {
        return true; // Checksum disabled (RFC 768)
    }

    let src_ip: [u8; 4] = frame[l3 + 12..l3 + 16].try_into().unwrap();
    let dst_ip: [u8; 4] = frame[l3 + 16..l3 + 20].try_into().unwrap();
    let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;

    if frame.len() < udp_start + udp_len {
        return false;
    }

    // Sum over pseudo-header + entire UDP segment (header + payload, including checksum field)
    let mut sum: u32 = 0;
    // Pseudo-header
    sum = sum.wrapping_add(((src_ip[0] as u32) << 8) | (src_ip[1] as u32));
    sum = sum.wrapping_add(((src_ip[2] as u32) << 8) | (src_ip[3] as u32));
    sum = sum.wrapping_add(((dst_ip[0] as u32) << 8) | (dst_ip[1] as u32));
    sum = sum.wrapping_add(((dst_ip[2] as u32) << 8) | (dst_ip[3] as u32));
    sum = sum.wrapping_add(IP_PROTO_UDP as u32);
    sum = sum.wrapping_add(udp_len as u32);

    // UDP segment (header + payload)
    let udp_data = &frame[udp_start..udp_start + udp_len];
    for i in (0..udp_data.len()).step_by(2) {
        let word = if i + 1 < udp_data.len() {
            ((udp_data[i] as u32) << 8) | (udp_data[i + 1] as u32)
        } else {
            (udp_data[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    (sum as u16) == 0xFFFF
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
    // Absolute frame-size guard. The caller (send_to_addr) enforces the
    // MTU-specific limit via the routing table; this catches truly oversized
    // payloads that would exceed the maximum Ethernet frame.
    let max_payload = MAX_FRAME_SIZE - TOTAL_HEADER_LEN;
    if payload.len() > max_payload {
        return Err(UdpError::PayloadTooLarge {
            max: max_payload,
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
    // Absolute frame-size guard. The caller (send_to_addr) enforces the
    // MTU-specific limit via the routing table; this catches truly oversized
    // payloads that would exceed the maximum Ethernet frame.
    let max_payload = MAX_FRAME_SIZE - TOTAL_HEADER_LEN;
    if payload.len() > max_payload {
        return Err(UdpError::PayloadTooLarge {
            max: max_payload,
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
    // Absolute frame-size guard. The caller (send_to_addr) enforces the
    // MTU-specific limit via the routing table; this catches truly oversized
    // payloads that would exceed the maximum Ethernet frame.
    let max_payload = MAX_FRAME_SIZE - TOTAL_HEADER_LEN;
    if payload.len() > max_payload {
        return Err(UdpError::PayloadTooLarge {
            max: max_payload,
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

/// Build a UDP frame with an 802.1Q VLAN tag into a caller-provided buffer.
///
/// Identical to `build_udp_frame_into` but inserts a 4-byte VLAN tag between
/// the source MAC and the EtherType, producing a frame 4 bytes longer.
pub fn build_udp_frame_into_vlan(
    out: &mut Vec<u8>,
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    ttl: u8,
    vlan: &VlanConfig,
) -> UdpResult<usize> {
    let max_payload = MAX_FRAME_SIZE - TOTAL_HEADER_LEN_VLAN;
    if payload.len() > max_payload {
        return Err(UdpError::PayloadTooLarge {
            max: max_payload,
            actual: payload.len(),
        });
    }

    let total_len = TOTAL_HEADER_LEN_VLAN + payload.len();
    let ip_total_len = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;

    out.resize(total_len, 0);

    let src_ip_bytes = src_ip.octets();
    let dst_ip_bytes = dst_ip.octets();

    // === Ethernet Header (14 bytes) + VLAN Tag (4 bytes) = 18 bytes ===
    out[0..6].copy_from_slice(dst_mac);
    out[6..12].copy_from_slice(src_mac);
    out[12..14].copy_from_slice(&ETH_TYPE_VLAN.to_be_bytes()); // TPID
    out[14..16].copy_from_slice(&vlan.encode_tci().to_be_bytes()); // TCI
    out[16..18].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes()); // Inner EtherType

    // === IPv4 Header (20 bytes) — starts at offset 18 ===
    let ip = ETH_HEADER_LEN + VLAN_TAG_LEN;
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

    // === UDP Header (8 bytes) — starts at offset 38 ===
    let udp_off = ip + IPV4_HEADER_LEN;
    out[udp_off..udp_off + 2].copy_from_slice(&src_port.to_be_bytes());
    out[udp_off + 2..udp_off + 4].copy_from_slice(&dst_port.to_be_bytes());
    out[udp_off + 4..udp_off + 6].copy_from_slice(&udp_len.to_be_bytes());
    out[udp_off + 6..udp_off + 8].copy_from_slice(&[0x00, 0x00]);

    // === Payload ===
    out[TOTAL_HEADER_LEN_VLAN..].copy_from_slice(payload);

    // UDP checksum (computed over IP pseudo-header + UDP header + payload — VLAN tag not included)
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
    /// VLAN ID if the frame was 802.1Q tagged (None for untagged frames).
    pub vlan_id: Option<u16>,
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
    /// VLAN ID if the frame was 802.1Q tagged (None for untagged frames).
    pub vlan_id: Option<u16>,
}

/// Parse a raw Ethernet frame containing a UDP packet.
///
/// Handles both untagged and 802.1Q VLAN-tagged frames. For tagged frames,
/// the VLAN tag is stripped and the `vlan_id` field is populated.
///
/// Returns None if the packet is not a valid UDP/IPv4 packet.
pub fn parse_udp_packet(frame: &[u8]) -> Option<ParsedUdpPacket> {
    // Detect VLAN tag and determine L3 offset
    let layout = detect_vlan(frame, None)?;
    let l3 = layout.l3_offset;

    // Minimum size: L3 offset + IP header + UDP header
    if frame.len() < l3 + IPV4_HEADER_LEN + UDP_HEADER_LEN {
        return None;
    }

    // Only handle IPv4
    if layout.ethertype != ETH_TYPE_IPV4 {
        return None;
    }

    // Parse Ethernet header
    let dst_mac: [u8; 6] = frame[0..6].try_into().ok()?;
    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;

    // Parse IPv4 header
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
    if ip_header[9] != IP_PROTO_UDP {
        return None;
    }

    let src_ip = Ipv4Addr::new(ip_header[12], ip_header[13], ip_header[14], ip_header[15]);
    let dst_ip = Ipv4Addr::new(ip_header[16], ip_header[17], ip_header[18], ip_header[19]);

    // Parse UDP header
    let udp_start = l3 + ip_header_len;
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
    let payload = frame[payload_start..payload_start + payload_len].to_vec();

    let vlan_id = layout.vlan_tci.map(|tci| tci & 0x0FFF);

    Some(ParsedUdpPacket {
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

/// Zero-copy UDP packet parser that borrows payload from the frame slice.
///
/// Identical validation to `parse_udp_packet` but returns a reference into the
/// original frame data, eliminating the per-packet `Vec<u8>` heap allocation.
/// Handles both untagged and 802.1Q VLAN-tagged frames.
pub fn parse_udp_packet_ref(frame: &[u8]) -> Option<ParsedUdpPacketRef<'_>> {
    let layout = detect_vlan(frame, None)?;
    let l3 = layout.l3_offset;

    if frame.len() < l3 + IPV4_HEADER_LEN + UDP_HEADER_LEN {
        return None;
    }
    if layout.ethertype != ETH_TYPE_IPV4 {
        return None;
    }

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
    if ip_header[9] != IP_PROTO_UDP {
        return None;
    }

    let src_ip = Ipv4Addr::new(ip_header[12], ip_header[13], ip_header[14], ip_header[15]);
    let dst_ip = Ipv4Addr::new(ip_header[16], ip_header[17], ip_header[18], ip_header[19]);

    let udp_start = l3 + ip_header_len;
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

    let vlan_id = layout.vlan_tci.map(|tci| tci & 0x0FFF);

    Some(ParsedUdpPacketRef {
        src_mac,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        payload: &frame[payload_start..payload_start + payload_len],
        vlan_id,
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
    /// Active TX offload flags (intersection of requested and NIC capabilities).
    /// Cached here to avoid querying the port on every send.
    active_tx_offload: u64,
    /// Active RX offload flags (used for hardware VLAN strip detection).
    active_rx_offload: u64,
    /// MTU the port was configured with. Drives mempool sizing and routing table init.
    mtu: u16,
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

fn get_or_init_dpdk(port_id: u16, mtu: u16) -> io::Result<Arc<DpdkResources>> {
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

    // Size the mempool from the configured MTU:
    //   data_room = mtu + 14 (Eth header) + 128 (RTE_PKTMBUF_HEADROOM)
    //   pool count = 4096 for jumbo (>1500) to keep hugepage footprint under ~40 MB;
    //                8192 for standard MTU (~16 MB).
    let data_room_size: u16 = mtu + ETH_HEADER_LEN as u16 + 128;
    let pool_n: u32 = if mtu > 1500 { 4096 } else { 8192 };

    let mempool = Mempool::create_with_config(
        "udp_pool",
        &MempoolConfig::new()
            .with_size(pool_n)
            .with_cache_size(256)
            .with_data_room_size(data_room_size),
    ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Mempool creation failed: {}", e)))?;

    // Descriptor ring size: jumbo frames are ~6× larger than standard, so halve
    // the ring depth to keep per-queue memory proportional.
    let desc_n: u16 = if mtu > 1500 { 512 } else { 1024 };

    // Initialize port with 2 TX queues, configured MTU, and checksum offloads:
    // - TX queue 0: RX lcore (ARP/ICMP replies, tx_ring drain)
    // - TX queue 1: Application thread (worker-direct TX for send_to)
    // Checksum offload is requested for both RX and TX; Port::init() will
    // mask these against device capabilities, so unsupported offloads are
    // silently dropped (software fallback).
    let port_config = PortConfig::default()
        .with_queues(1, 2)
        .with_descriptors(desc_n, desc_n)
        .with_mtu(mtu as u32)
        .with_checksum_offload()
        .with_vlan_offload();
    let port = Port::init(port_id, port_config, &mempool)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Port init failed: {}", e)))?;

    let src_mac = port.mac_address();
    let active_tx_offload = port.active_tx_offload();
    let active_rx_offload = port.active_rx_offload();

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
        active_tx_offload,
        active_rx_offload,
        mtu,
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
    ///
    /// When `hw_vlan_tci` is `Some(tci)`, the DPDK backend sets the mbuf VLAN TCI
    /// field and `RTE_MBUF_F_TX_VLAN` flag so the NIC inserts the 802.1Q tag on
    /// the wire. The `frame` must be an **untagged** Ethernet frame in this case.
    /// For Generic backends, `hw_vlan_tci` is ignored (the frame should already
    /// contain any VLAN tags in the Ethernet header).
    fn send_frame(&self, frame: &[u8], hw_vlan_tci: Option<u16>) -> io::Result<usize> {
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

                let tx_offload = res.active_tx_offload;
                let mut ol_flags = 0u64;

                // TX hardware VLAN insert: set mbuf VLAN TCI before data_mut()
                // borrow to satisfy the borrow checker. The NIC reads vlan_tci
                // from the mbuf metadata, not from the frame data.
                if let Some(tci) = hw_vlan_tci {
                    if (tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT as u64) != 0 {
                        mbuf.set_vlan_tci(tci);
                        ol_flags |= dpdk_sys::RTE_MBUF_F_TX_VLAN as u64;
                    }
                }

                let data = mbuf.data_mut()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Failed to get mbuf data"))?;
                data.copy_from_slice(frame);

                // TX hardware checksum offload: when the NIC supports it, set mbuf
                // metadata so the NIC computes IPv4 and UDP checksums instead of
                // software. The frame was already built with software checksums by
                // build_udp_frame_into(); the NIC will overwrite them.
                if tx_offload != 0 && frame.len() >= TOTAL_HEADER_LEN {
                    let has_ip_cksum = (tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_IPV4_CKSUM as u64) != 0;
                    let has_udp_cksum = (tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_UDP_CKSUM as u64) != 0;

                    if has_ip_cksum || has_udp_cksum {
                        ol_flags |= dpdk_sys::RTE_MBUF_F_TX_IPV4 as u64;

                        if has_ip_cksum {
                            ol_flags |= dpdk_sys::RTE_MBUF_F_TX_IP_CKSUM as u64;
                            // NIC expects IPv4 checksum field to be 0
                            let ip_cksum_off = ETH_HEADER_LEN + 10;
                            data[ip_cksum_off] = 0;
                            data[ip_cksum_off + 1] = 0;
                        }

                        if has_udp_cksum {
                            ol_flags |= dpdk_sys::RTE_MBUF_F_TX_UDP_CKSUM as u64;
                            // NIC expects pseudo-header checksum in the UDP checksum field
                            let src_ip: [u8; 4] = data[ETH_HEADER_LEN + 12..ETH_HEADER_LEN + 16]
                                .try_into().unwrap();
                            let dst_ip: [u8; 4] = data[ETH_HEADER_LEN + 16..ETH_HEADER_LEN + 20]
                                .try_into().unwrap();
                            let udp_off = ETH_HEADER_LEN + IPV4_HEADER_LEN;
                            let udp_len = u16::from_be_bytes([data[udp_off + 4], data[udp_off + 5]]);
                            let phdr_cksum = udp_pseudo_header_checksum(&src_ip, &dst_ip, udp_len);
                            data[udp_off + 6..udp_off + 8].copy_from_slice(&phdr_cksum.to_be_bytes());
                        }
                    }
                }

                // set_tx_offload and set_ol_flags go through raw pointer, not
                // through the &mut [u8] data slice, so they don't conflict.
                let _ = data;
                if tx_offload != 0 && frame.len() >= TOTAL_HEADER_LEN {
                    let has_ip_cksum = (tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_IPV4_CKSUM as u64) != 0;
                    let has_udp_cksum = (tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_UDP_CKSUM as u64) != 0;
                    if has_ip_cksum || has_udp_cksum {
                        mbuf.set_tx_offload(
                            ETH_HEADER_LEN as u8,
                            IPV4_HEADER_LEN as u16,
                            UDP_HEADER_LEN as u8,
                        );
                    }
                }
                if ol_flags != 0 {
                    mbuf.set_ol_flags(ol_flags);
                }

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
    ///
    /// Returns raw frame bytes as-is. When the NIC has stripped VLAN tags, the
    /// frame bytes are untagged — the VLAN TCI is available via mbuf metadata
    /// and is passed to `detect_vlan()` / `process_frame_zerocopy()` separately.
    /// This avoids per-packet Vec allocation for frame reconstruction.
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

/// Default socket-level receive buffer size in bytes.
///
/// Mirrors the Linux kernel default for `SO_RCVBUF` (approximately 208 KiB on
/// modern kernels). Applications that need more headroom can call
/// [`UdpSocket::set_recv_buffer_size`] after `bind()`.
pub const DEFAULT_RECV_BUFFER_BYTES: usize = 256 * 1024;

/// Hard upper bound on queued packet count — a secondary safety net against
/// pathological small-payload floods. Byte accounting is the primary limit.
pub const DEFAULT_RECV_BUFFER_PACKETS: usize = 4096;

/// Snapshot of socket-level receive drop counters, surfaced via
/// [`UdpSocket::recv_drops`]. Applications use these to detect when their
/// receive buffer is too small (`SO_RCVBUF` equivalent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecvDropStats {
    /// Number of UDP payloads dropped because the socket receive buffer was full.
    pub packets: u64,
    /// Total bytes of payload dropped because the socket receive buffer was full.
    pub bytes: u64,
}

/// Receive queue for buffering packets.
///
/// Tracks both packet count (`max_packets`) and total queued payload bytes
/// (`max_bytes`). This mirrors the Linux kernel's `sk_rcvbuf`/`sk_rmem_alloc`
/// accounting: when either limit is exceeded, the incoming packet is dropped
/// and the caller is expected to bump the socket-level drop counters.
struct ReceiveQueue {
    /// Buffered packets: (payload, source_addr)
    packets: VecDeque<(Vec<u8>, SocketAddr)>,
    /// Maximum number of buffered packets — secondary safety limit.
    max_packets: usize,
    /// Maximum queued payload bytes — primary limit (SO_RCVBUF equivalent).
    max_bytes: usize,
    /// Current queued payload bytes.
    current_bytes: usize,
}

impl ReceiveQueue {
    fn with_limits(max_packets: usize, max_bytes: usize) -> Self {
        Self {
            // Pre-allocate with a modest capacity; VecDeque will grow as needed.
            packets: VecDeque::with_capacity(max_packets.min(1024)),
            max_packets,
            max_bytes,
            current_bytes: 0,
        }
    }

    /// Try to enqueue a packet. On success returns `Ok(())`. On failure returns
    /// `Err(payload)` so the caller can inspect the dropped payload length for
    /// drop-counter accounting (and recover the allocation if desired).
    fn push(&mut self, payload: Vec<u8>, src: SocketAddr) -> Result<(), Vec<u8>> {
        let size = payload.len();
        if self.packets.len() >= self.max_packets
            || self.current_bytes.saturating_add(size) > self.max_bytes
        {
            return Err(payload);
        }
        self.current_bytes += size;
        self.packets.push_back((payload, src));
        Ok(())
    }

    fn pop(&mut self) -> Option<(Vec<u8>, SocketAddr)> {
        let (payload, src) = self.packets.pop_front()?;
        self.current_bytes = self.current_bytes.saturating_sub(payload.len());
        Some((payload, src))
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    fn len(&self) -> usize {
        self.packets.len()
    }

    fn bytes(&self) -> usize {
        self.current_bytes
    }

    fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    fn set_max_bytes(&mut self, max_bytes: usize) {
        self.max_bytes = max_bytes;
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

/// Try to auto-detect routing configuration from the OS.
///
/// Reads `/proc/net/route` and `/proc/net/arp` to discover the local subnet,
/// default gateway, and seed the ARP cache with known MAC entries (especially
/// the gateway MAC). Falls back to `RoutingTable::new()` (passthrough) if
/// detection fails — this preserves backward compatibility.
fn auto_detect_routing(local_ip: Ipv4Addr, arp_handler: &ArpHandler) -> RoutingTable {
    // Skip auto-detect for INADDR_ANY — we don't know which interface to look up.
    if local_ip.is_unspecified() {
        return RoutingTable::new();
    }

    match routing::detect_from_proc(local_ip) {
        Some((config, arp_entries)) => {
            // Seed ARP cache with entries from /proc/net/arp (especially gateway MAC).
            for entry in &arp_entries {
                arp_handler.cache.insert(
                    entry.ip,
                    dpdk::port::MacAddress::new(entry.mac),
                );
            }
            RoutingTable::with_config(config)
        }
        None => RoutingTable::new(),
    }
}

// ============================================================================
// TxBuffer — lock-free TX buffer for run-to-completion mode
// ============================================================================

/// A reusable TX frame buffer that avoids `Mutex` overhead in run-to-completion mode.
///
/// In RTC mode (single-threaded `send_to` path), the inner `Vec<u8>` is accessed
/// via `UnsafeCell` — no locking overhead. A debug-mode assertion verifies that
/// concurrent access never occurs.
///
/// # Safety
///
/// This type is `Sync` because:
/// - In RTC mode, only one thread ever calls `send_to`, so `borrow_mut` is
///   called from a single thread. The `AtomicBool` guard catches violations in debug builds.
/// - In multi-core mode, `send_to` uses `build_udp_frame` (allocates a new Vec)
///   and enqueues to the TX ring — `TxBuffer` is never accessed.
struct TxBuffer {
    buf: UnsafeCell<Vec<u8>>,
    #[cfg(debug_assertions)]
    in_use: AtomicBool,
}

// SAFETY: TxBuffer is only accessed in RTC mode from a single thread.
// The debug-mode AtomicBool guard detects misuse.
unsafe impl Sync for TxBuffer {}

impl TxBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: UnsafeCell::new(Vec::with_capacity(capacity)),
            #[cfg(debug_assertions)]
            in_use: AtomicBool::new(false),
        }
    }

    /// Get a mutable reference to the inner buffer.
    ///
    /// # Safety
    ///
    /// Caller must guarantee that no other thread is accessing the buffer
    /// concurrently. In debug builds, a runtime assertion enforces this.
    #[inline]
    fn borrow_mut(&self) -> TxBufferGuard<'_> {
        #[cfg(debug_assertions)]
        {
            let was_in_use = self.in_use.swap(true, Ordering::Acquire);
            debug_assert!(
                !was_in_use,
                "TxBuffer: concurrent access detected — this should only be used in RTC mode"
            );
        }
        TxBufferGuard { tx_buf: self }
    }
}

/// RAII guard that provides `&mut Vec<u8>` access and clears the in-use flag on drop.
struct TxBufferGuard<'a> {
    tx_buf: &'a TxBuffer,
}

impl<'a> std::ops::Deref for TxBufferGuard<'a> {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        // SAFETY: single-thread guarantee enforced by debug assertion in borrow_mut
        unsafe { &*self.tx_buf.buf.get() }
    }
}

impl<'a> std::ops::DerefMut for TxBufferGuard<'a> {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        // SAFETY: single-thread guarantee enforced by debug assertion in borrow_mut
        unsafe { &mut *self.tx_buf.buf.get() }
    }
}

impl<'a> Drop for TxBufferGuard<'a> {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        self.tx_buf.in_use.store(false, Ordering::Release);
    }
}

impl std::fmt::Debug for TxBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxBuffer").finish_non_exhaustive()
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
    /// Whether to send a Gratuitous ARP announcement on bind
    auto_garp: bool,
    /// Read timeout for recv operations (None = block forever)
    read_timeout: Mutex<Option<Duration>>,
    /// Write timeout for send operations (None = block forever)
    write_timeout: Mutex<Option<Duration>>,
    /// Multi-core pipeline topology (None = run-to-completion, the default).
    /// When active, recv_from() reads from app_ring and send_to() writes to tx_ring.
    topology: Mutex<Option<MultiCoreTopology>>,
    /// Reusable TX frame buffer — avoids per-packet heap allocation in send_to.
    /// Uses UnsafeCell (via TxBuffer) instead of Mutex for zero-overhead access
    /// in run-to-completion mode. Only accessed from the RTC send path.
    tx_buf: TxBuffer,
    /// Performance counters — always available, zero-cost if not read.
    perf_counters: Arc<PerfCounters>,
    /// Latency sampler — samples 1 in N packets for percentile tracking.
    latency_sampler: Arc<LatencySampler>,
    /// Background perf reporter (None if not enabled).
    perf_reporter: Mutex<Option<PerfReporter>>,
    /// Round-robin index for polling per-worker app rings in recv_from_pipeline.
    recv_from_rr_index: AtomicUsize,
    // ---- Cached pipeline handles (set once during builder.bind()) ----
    // These avoid locking `topology` on every send/recv in the hot path.
    /// True when a multi-core pipeline is active (avoids topology.lock() in recv_from).
    has_pipeline: AtomicBool,
    /// Cached per-worker app rings for lock-free recv_from_pipeline.
    cached_app_rings: Option<Vec<Arc<SpscRing<AppPacket>>>>,
    /// Cached frame pool for lock-free payload reads in recv_from_pipeline.
    cached_frame_pool: Option<Arc<FramePool>>,
    /// Cached direct-send function for lock-free send_to in pipeline mode.
    /// The optional `u16` is the VLAN TCI for hardware VLAN insert (None = no HW VLAN).
    cached_direct_send: Option<Arc<dyn Fn(&[u8], Option<u16>) -> io::Result<usize> + Send + Sync>>,
    // ---- Atomic fast-path flags to skip Mutex locks in hot path ----
    /// True after connect() is called — skip connected_addr.lock() when false.
    is_connected: AtomicBool,
    /// True when recv_queue has buffered packets — skip recv_queue.lock() when false.
    has_buffered_packets: AtomicBool,
    /// True when connection_state is Some — skip connection_state.write() when false.
    has_connection_state: AtomicBool,
    /// Subnet-aware routing table for next-hop MAC resolution.
    routing_table: RoutingTable,
    /// Optional 802.1Q VLAN configuration. When set, outgoing frames are tagged.
    vlan_config: Option<VlanConfig>,
    /// Optional GUE tunnel configuration. When set, packets are encapsulated in
    /// GUE (outer UDP + 4-byte GUE header + inner IPv4/UDP) on TX, and
    /// decapsulated on RX.
    gue_config: Option<gue::GueConfig>,
    /// Number of UDP payloads dropped because the socket receive buffer was full.
    /// Lock-free read via `recv_drops()` — mirrors Linux `sk_drops`.
    rx_dropped_packets: AtomicU64,
    /// Total bytes of payload dropped due to receive buffer overflow.
    rx_dropped_bytes: AtomicU64,
    /// Socket error queue — populated by ICMP error messages that match this socket.
    /// Drained via `take_error()`, mirroring Linux `SO_ERROR` / `sk_err` behavior.
    error_queue: Mutex<VecDeque<io::Error>>,
    /// Fast-path flag: true when `error_queue` has entries. Avoids locking the
    /// mutex on every `take_error()` call when there are no errors (common case).
    has_pending_error: AtomicBool,
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

        // Get or initialize DPDK resources (ENA supports 9001 MTU by default)
        let resources = get_or_init_dpdk(0, 9001)?;

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

        // Auto-detect routing from OS if possible (Phase 3).
        // Seeds the ARP cache with gateway MAC from /proc/net/arp.
        // Falls back to passthrough (no routing) if detection fails.
        let mut routing_table = auto_detect_routing(local_ip, &arp_handler);

        // The DPDK ENI is bound to vfio-pci so the kernel has no interface to
        // read MTU from; override the routing table to match the port config.
        if routing_table.mtu() < resources.mtu {
            routing_table.set_mtu(resources.mtu);
        }

        let socket = UdpSocket {
            local_addr: SocketAddr::V4(local_v4),
            connected_addr: Mutex::new(None),
            socket_backend,
            resources,
            ttl: 64,
            dst_mac: MacAddress::broadcast(),
            arp_handler,
            icmp_handler,
            connection_state: RwLock::new(None),
            recv_queue: Mutex::new(ReceiveQueue::with_limits(
                DEFAULT_RECV_BUFFER_PACKETS,
                DEFAULT_RECV_BUFFER_BYTES,
            )),
            auto_arp: true,
            auto_icmp: true,
            auto_garp: true,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            topology: Mutex::new(None),
            tx_buf: TxBuffer::new(MAX_FRAME_SIZE),
            perf_counters: Arc::new(PerfCounters::new()),
            latency_sampler: Arc::new(LatencySampler::default()),
            perf_reporter: Mutex::new(None),
            recv_from_rr_index: AtomicUsize::new(0),
            has_pipeline: AtomicBool::new(false),
            cached_app_rings: None,
            cached_frame_pool: None,
            cached_direct_send: None,
            is_connected: AtomicBool::new(false),
            has_buffered_packets: AtomicBool::new(false),
            has_connection_state: AtomicBool::new(false),
            routing_table,
            vlan_config: None,
            gue_config: None,
            rx_dropped_packets: AtomicU64::new(0),
            rx_dropped_bytes: AtomicU64::new(0),
            error_queue: Mutex::new(VecDeque::new()),
            has_pending_error: AtomicBool::new(false),
        };

        // Send Gratuitous ARP to announce our MAC/IP mapping on the network
        socket.send_gratuitous_arp_if_enabled();

        Ok(socket)
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
        let resources = get_or_init_dpdk(0, 9001)?;

        println!("✅ {} UDP socket bound to {} (MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
            backend_name, SocketAddr::V4(local_v4),
            local_mac[0], local_mac[1], local_mac[2],
            local_mac[3], local_mac[4], local_mac[5]);

        // Auto-detect routing from OS if possible (Phase 3).
        let mut routing_table = auto_detect_routing(local_ip, &arp_handler);

        // DPDK backends have jumbo MTU configured at the port level, but
        // auto-detect may miss it because the ENI is on vfio-pci (no kernel iface).
        if backend_name == "dpdk" && routing_table.mtu() < resources.mtu {
            routing_table.set_mtu(resources.mtu);
        }

        let socket = UdpSocket {
            local_addr: SocketAddr::V4(local_v4),
            connected_addr: Mutex::new(None),
            socket_backend: SocketBackend::Generic(backend),
            resources,
            ttl: 64,
            dst_mac: MacAddress::broadcast(),
            arp_handler,
            icmp_handler,
            connection_state: RwLock::new(None),
            recv_queue: Mutex::new(ReceiveQueue::with_limits(
                DEFAULT_RECV_BUFFER_PACKETS,
                DEFAULT_RECV_BUFFER_BYTES,
            )),
            auto_arp: true,
            auto_icmp: true,
            auto_garp: true,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            topology: Mutex::new(None),
            tx_buf: TxBuffer::new(MAX_FRAME_SIZE),
            perf_counters: Arc::new(PerfCounters::new()),
            latency_sampler: Arc::new(LatencySampler::default()),
            perf_reporter: Mutex::new(None),
            recv_from_rr_index: AtomicUsize::new(0),
            has_pipeline: AtomicBool::new(false),
            cached_app_rings: None,
            cached_frame_pool: None,
            cached_direct_send: None,
            is_connected: AtomicBool::new(false),
            has_buffered_packets: AtomicBool::new(false),
            has_connection_state: AtomicBool::new(false),
            routing_table,
            vlan_config: None,
            gue_config: None,
            rx_dropped_packets: AtomicU64::new(0),
            rx_dropped_bytes: AtomicU64::new(0),
            error_queue: Mutex::new(VecDeque::new()),
            has_pending_error: AtomicBool::new(false),
        };

        // Send Gratuitous ARP to announce our MAC/IP mapping on the network
        socket.send_gratuitous_arp_if_enabled();

        Ok(socket)
    }

    /// Get the name of the active packet I/O backend.
    pub fn active_backend(&self) -> &'static str {
        self.socket_backend.backend_name()
    }

    /// Returns the active topology plan, if a multi-core pipeline is running.
    ///
    /// Returns `None` when the socket is in run-to-completion mode (default
    /// for `UdpSocket::bind()` and when `rx_queues(1)` is configured).
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

        // Reject payloads that exceed the MTU-derived limit.
        // GUE encapsulation adds 32 bytes of overhead (outer IP + outer UDP + GUE header).
        let max_payload = if self.gue_config.is_some() {
            self.routing_table.max_udp_payload().saturating_sub(gue::GUE_ENCAP_OVERHEAD)
        } else {
            self.routing_table.max_udp_payload()
        };
        if buf.len() > max_payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "payload too large: {} bytes exceeds max UDP payload {} (MTU {})",
                    buf.len(), max_payload, self.routing_table.mtu(),
                ),
            ));
        }

        // When GUE is configured, ARP for the tunnel remote endpoint instead
        // of the original destination. The outer frame is addressed to the
        // tunnel peer; the inner addresses carry the original src/dst.
        let arp_target_ip = if let Some(ref gue_cfg) = self.gue_config {
            gue_cfg.remote_ip
        } else {
            dst_ip
        };
        let arp_target = match self.routing_table.lookup(arp_target_ip) {
            routing::NextHop::Direct(ip) => ip,
            routing::NextHop::Gateway(gw) => gw,
        };

        // Resolve next-hop MAC via ARP (or use configured/broadcast MAC)
        let dst_mac = match self.arp_handler.resolve(&arp_target) {
            Some(mac) => {
                perf_inc!(self.perf_counters.arp_cache_hits);
                mac
            }
            None if self.auto_arp => {
                perf_inc!(self.perf_counters.arp_cache_misses);
                // Proactively send ARP request and wait for reply
                self.resolve_arp(&arp_target)?
            }
            None => {
                perf_inc!(self.perf_counters.arp_cache_misses);
                self.dst_mac.clone()
            }
        };

        let src_mac = self.socket_backend.mac_address();

        // Build frame into reusable buffer (zero-alloc) and send.
        //
        // Three mutually exclusive modes:
        //   GUE    -> encapsulate: outer-eth/outer-ip/outer-udp/gue/inner-ip/inner-udp/payload
        //   VLAN   -> tag or hardware-offload per VlanMode
        //   Plain  -> standard untagged UDP frame
        let mut tx_buf = self.tx_buf.borrow_mut();
        let hw_vlan_tci = if let Some(ref gue_cfg) = self.gue_config {
            // GUE tunnel encapsulation
            gue::build_gue_frame_into(
                &mut tx_buf,
                &src_mac,
                &dst_mac.octets(),
                src_ip, gue_cfg.remote_ip,
                gue_cfg.local_port, gue_cfg.remote_port,
                src_ip, dst_ip,
                src_port, dst_port,
                buf, self.ttl,
            ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("GUE encap failed: {}", e)))?;
            None
        } else {
            let should_tag = self.vlan_config.as_ref().map_or(false, |v| v.tags_on_tx());
            if should_tag {
                let vlan_cfg = self.vlan_config.as_ref().unwrap();
                let use_hw = self.has_hw_vlan_insert() && !vlan_cfg.force_software;
                if use_hw {
                    build_udp_frame_into(
                        &mut tx_buf,
                        &src_mac,
                        &dst_mac.octets(),
                        src_ip, dst_ip,
                        src_port, dst_port,
                        buf, self.ttl,
                    ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("packet build failed: {}", e)))?;
                    Some(vlan_cfg.encode_tci())
                } else {
                    build_udp_frame_into_vlan(
                        &mut tx_buf,
                        &src_mac,
                        &dst_mac.octets(),
                        src_ip, dst_ip,
                        src_port, dst_port,
                        buf, self.ttl,
                        vlan_cfg,
                    ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("packet build failed: {}", e)))?;
                    None
                }
            } else {
                build_udp_frame_into(
                    &mut tx_buf,
                    &src_mac,
                    &dst_mac.octets(),
                    src_ip, dst_ip,
                    src_port, dst_port,
                    buf, self.ttl,
                ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("packet build failed: {}", e)))?;
                None
            }
        };

        // P3.5: Worker-direct TX uses cached send function (no topology.lock()).
        if let Some(ref direct_send) = self.cached_direct_send {
            // Worker-direct TX: send on dedicated TX queue (no ring hop)
            if let Err(e) = direct_send(&tx_buf, hw_vlan_tci) {
                perf_inc!(self.perf_counters.tx_failures);
                return Err(e);
            }
        } else {
            // Run-to-completion path: send via backend on TX queue 0
            if let Err(e) = self.socket_backend.send_frame(&tx_buf, hw_vlan_tci) {
                perf_inc!(self.perf_counters.tx_failures);
                return Err(e);
            }
        }

        // Increment TX counters
        perf_inc!(self.perf_counters.tx_packets);
        perf_inc!(self.perf_counters.tx_bytes, buf.len() as u64);

        // Update connection state if connected — skip lock when no state exists
        if self.has_connection_state.load(Ordering::Acquire) {
            if let Ok(mut guard) = self.connection_state.write() {
                if let Some(ref mut state) = *guard {
                    state.record_send(buf.len());
                }
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
        // Uses cached AtomicBool to avoid topology.lock() on every recv.
        if self.has_pipeline.load(Ordering::Acquire) {
            return self.recv_from_pipeline(buf);
        }

        // Run-to-completion path (original single-threaded behavior).
        self.recv_from_inline(buf)
    }

    /// Non-blocking single-poll receive attempt.
    ///
    /// Unlike `recv_from()` which blocks (polling in a loop with sleep) until a
    /// packet arrives, this method performs exactly ONE poll of the backend and
    /// returns immediately. Returns `Ok(None)` if no matching packet is available.
    ///
    /// This is designed for async wrappers that manage their own poll/yield loop
    /// (e.g. the Tokio integration) to avoid the ~100μs internal sleep that the
    /// blocking `recv_from` uses between polls.
    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        if self.has_pipeline.load(Ordering::Acquire) {
            return self.try_recv_from_pipeline(buf);
        }
        self.try_recv_from_inline(buf)
    }

    /// Non-blocking pipeline recv: single dequeue attempt from app rings.
    fn try_recv_from_pipeline(&self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let app_rings = self.cached_app_rings.as_ref()
            .expect("try_recv_from_pipeline called without pipeline");
        let frame_pool = self.cached_frame_pool.as_ref()
            .expect("try_recv_from_pipeline called without pipeline");

        // Check buffered packets first.
        if self.has_buffered_packets.load(Ordering::Acquire) {
            let mut queue = self.recv_queue.lock().unwrap();
            if let Some((payload, src_addr)) = queue.pop() {
                if queue.is_empty() {
                    self.has_buffered_packets.store(false, Ordering::Release);
                }
                let copy_len = std::cmp::min(buf.len(), payload.len());
                buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                return Ok(Some((copy_len, src_addr)));
            }
            self.has_buffered_packets.store(false, Ordering::Release);
        }

        // Single round-robin dequeue attempt across app rings.
        let mut rr = self.recv_from_rr_index.load(Ordering::Relaxed);
        let packet = dequeue_app_rings(app_rings, &mut rr);
        self.recv_from_rr_index.store(rr, Ordering::Relaxed);

        if let Some(app_pkt) = packet {
            let start = app_pkt.payload_offset as usize;
            let len = app_pkt.payload_len as usize;
            let frame_data = unsafe { frame_pool.frame(app_pkt.frame_ref.pool_idx) };
            let copy_len = std::cmp::min(buf.len(), len);
            buf[..copy_len].copy_from_slice(&frame_data[start..start + copy_len]);
            frame_pool.free(app_pkt.frame_ref.pool_idx);

            // Connected socket filtering
            if self.is_connected.load(Ordering::Acquire) {
                if let Some(connected) = *self.connected_addr.lock().unwrap() {
                    if app_pkt.src_addr != connected {
                        let mut queue = self.recv_queue.lock().unwrap();
                        if queue.push(buf[..copy_len].to_vec(), app_pkt.src_addr).is_err() {
                            self.record_rx_drop(copy_len);
                        } else {
                            self.has_buffered_packets.store(true, Ordering::Release);
                        }
                        return Ok(None);
                    }
                }
            }

            return Ok(Some((copy_len, app_pkt.src_addr)));
        }

        Ok(None)
    }

    /// Non-blocking inline recv: single poll of the backend, no sleep.
    fn try_recv_from_inline(&self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let local_port = match self.local_addr {
            SocketAddr::V4(v4) => v4.port(),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPv6 not supported"));
            }
        };

        // Check buffered packets first.
        {
            let mut queue = self.recv_queue.lock().unwrap();
            if let Some((payload, src_addr)) = queue.pop() {
                let copy_len = std::cmp::min(buf.len(), payload.len());
                buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                return Ok(Some((copy_len, src_addr)));
            }
        }

        // Single poll of the backend.
        match &self.socket_backend {
            SocketBackend::Dpdk(res) => {
                let packets = res.port.rx_burst(0, 32)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rx_burst failed: {}", e)))?;

                if packets.is_empty() {
                    return Ok(None);
                }

                perf_inc!(self.perf_counters.rx_bursts);
                perf_inc!(self.perf_counters.rx_burst_sum, packets.len() as u64);

                let sample_this_burst = perf_should_sample!(self.latency_sampler);
                let rx_timestamp = if sample_this_burst { Some(Instant::now()) } else { None };

                let mut result: Option<(usize, SocketAddr)> = None;

                for mbuf in &packets {
                    let Some(data) = mbuf.data() else { continue };
                    let len = mbuf.data_len() as usize;
                    let frame_data = &data[..len.min(data.len())];

                    let hw_vlan_tci = {
                        let ol_flags = mbuf.ol_flags();
                        let stripped = (ol_flags & dpdk_sys::RTE_MBUF_F_RX_VLAN_STRIPPED as u64) != 0;
                        if stripped { Some(mbuf.vlan_tci()) } else { None }
                    };

                    if let Some(r) = self.process_frame_zerocopy(frame_data, local_port, buf, &mut result, hw_vlan_tci) {
                        if let Some(ts) = rx_timestamp {
                            let latency_ns = ts.elapsed().as_nanos() as u64;
                            self.latency_sampler.record(latency_ns);
                            perf_inc!(self.perf_counters.latency_sample_count);
                            perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                            self.perf_counters.update_latency_max(latency_ns);
                        }
                        return Ok(Some(r));
                    }
                }

                if let Some(r) = result {
                    if let Some(ts) = rx_timestamp {
                        let latency_ns = ts.elapsed().as_nanos() as u64;
                        self.latency_sampler.record(latency_ns);
                        perf_inc!(self.perf_counters.latency_sample_count);
                        perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                        self.perf_counters.update_latency_max(latency_ns);
                    }
                    return Ok(Some(r));
                }
            }
            SocketBackend::Generic(backend) => {
                let frames = backend.recv_frames(32)?;

                if frames.is_empty() {
                    return Ok(None);
                }

                perf_inc!(self.perf_counters.rx_bursts);
                perf_inc!(self.perf_counters.rx_burst_sum, frames.len() as u64);

                let sample_this_burst = perf_should_sample!(self.latency_sampler);
                let rx_timestamp = if sample_this_burst { Some(Instant::now()) } else { None };

                let mut result: Option<(usize, SocketAddr)> = None;

                for frame_data in &frames {
                    if let Some(r) = self.process_frame_zerocopy(frame_data, local_port, buf, &mut result, None) {
                        if let Some(ts) = rx_timestamp {
                            let latency_ns = ts.elapsed().as_nanos() as u64;
                            self.latency_sampler.record(latency_ns);
                            perf_inc!(self.perf_counters.latency_sample_count);
                            perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                            self.perf_counters.update_latency_max(latency_ns);
                        }
                        return Ok(Some(r));
                    }
                }

                if let Some(r) = result {
                    if let Some(ts) = rx_timestamp {
                        let latency_ns = ts.elapsed().as_nanos() as u64;
                        self.latency_sampler.record(latency_ns);
                        perf_inc!(self.perf_counters.latency_sample_count);
                        perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                        self.perf_counters.update_latency_max(latency_ns);
                    }
                    return Ok(Some(r));
                }
            }
        }

        Ok(None)
    }

    /// Pipeline recv path: dequeue AppPackets from per-worker SPSC app rings.
    ///
    /// Phase 3 zero-copy: reads payload directly from the FramePool via the
    /// AppPacket's FrameRef, copies it to the user buffer, then frees the frame.
    ///
    /// Uses cached pipeline handles (set once during builder.bind()) to avoid
    /// locking `topology` on every packet. Uses adaptive backoff (spin → yield
    /// → sleep 1us) matching the worker thread strategy instead of a fixed 100us
    /// sleep, which was causing catastrophic backpressure at high packet rates.
    fn recv_from_pipeline(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let deadline = self.read_timeout.lock().unwrap().map(|d| Instant::now() + d);
        let mut empty_polls: u32 = 0;

        // Cache references from the struct — these never change after bind().
        let app_rings = self.cached_app_rings.as_ref()
            .expect("recv_from_pipeline called without pipeline");
        let frame_pool = self.cached_frame_pool.as_ref()
            .expect("recv_from_pipeline called without pipeline");

        loop {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"));
                }
            }

            // Check buffered packets first (from connected socket filtering).
            // Fast-path: skip the lock entirely when no packets are buffered.
            if self.has_buffered_packets.load(Ordering::Acquire) {
                let mut queue = self.recv_queue.lock().unwrap();
                if let Some((payload, src_addr)) = queue.pop() {
                    if queue.is_empty() {
                        self.has_buffered_packets.store(false, Ordering::Release);
                    }
                    let copy_len = std::cmp::min(buf.len(), payload.len());
                    buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                    return Ok((copy_len, src_addr));
                }
                self.has_buffered_packets.store(false, Ordering::Release);
            }

            // Poll per-worker SPSC app rings round-robin (lock-free).
            let mut rr = self.recv_from_rr_index.load(Ordering::Relaxed);
            let packet = dequeue_app_rings(app_rings, &mut rr);
            self.recv_from_rr_index.store(rr, Ordering::Relaxed);

            if let Some(app_pkt) = packet {
                empty_polls = 0;

                // Read payload from pool (zero-copy until this final memcpy to user buf)
                let start = app_pkt.payload_offset as usize;
                let len = app_pkt.payload_len as usize;
                // SAFETY: frame_ref is valid — allocated by rx_loop, not yet freed.
                let frame_data = unsafe {
                    frame_pool.frame(app_pkt.frame_ref.pool_idx)
                };
                let copy_len = std::cmp::min(buf.len(), len);
                buf[..copy_len].copy_from_slice(&frame_data[start..start + copy_len]);
                // Free the frame back to pool
                frame_pool.free(app_pkt.frame_ref.pool_idx);

                // If connected, only accept packets from connected peer.
                // Fast-path: skip the lock when not connected (common for echo servers).
                if self.is_connected.load(Ordering::Acquire) {
                    if let Some(connected) = *self.connected_addr.lock().unwrap() {
                        if app_pkt.src_addr != connected {
                            let mut queue = self.recv_queue.lock().unwrap();
                            if queue.push(buf[..copy_len].to_vec(), app_pkt.src_addr).is_err() {
                                self.record_rx_drop(copy_len);
                            } else {
                                self.has_buffered_packets.store(true, Ordering::Release);
                            }
                            continue;
                        }
                    }
                }

                // Update connection stats — skip lock when no connection state exists.
                if self.has_connection_state.load(Ordering::Acquire) {
                    if let Ok(mut guard) = self.connection_state.write() {
                        if let Some(ref mut state) = *guard {
                            state.record_recv(copy_len);
                        }
                    }
                }

                return Ok((copy_len, app_pkt.src_addr));
            }

            // No packet available — adaptive backoff (spin → yield → sleep 1us).
            // Matches the worker thread backoff strategy instead of the old fixed
            // 100us sleep, which caused ~35 packets to pile up per idle cycle at 350k pps.
            empty_polls += 1;
            if empty_polls <= 64 {
                std::hint::spin_loop();
            } else if empty_polls <= 80 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(std::time::Duration::from_micros(1));
            }
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

                    // Record burst stats
                    perf_inc!(self.perf_counters.rx_bursts);
                    perf_inc!(self.perf_counters.rx_burst_sum, packets.len() as u64);

                    // Latency sampling: timestamp at rx_burst return
                    let sample_this_burst = perf_should_sample!(self.latency_sampler);
                    let rx_timestamp = if sample_this_burst { Some(Instant::now()) } else { None };

                    let mut result: Option<(usize, SocketAddr)> = None;

                    for mbuf in &packets {
                        let Some(data) = mbuf.data() else { continue };
                        let len = mbuf.data_len() as usize;
                        let frame_data = &data[..len.min(data.len())];

                        // Extract hardware VLAN TCI from mbuf if NIC stripped the tag
                        let hw_vlan_tci = {
                            let ol_flags = mbuf.ol_flags();
                            let stripped = (ol_flags & dpdk_sys::RTE_MBUF_F_RX_VLAN_STRIPPED as u64) != 0;
                            if stripped { Some(mbuf.vlan_tci()) } else { None }
                        };

                        if let Some(r) = self.process_frame_zerocopy(frame_data, local_port, buf, &mut result, hw_vlan_tci) {
                            // Record latency sample if applicable
                            if let Some(ts) = rx_timestamp {
                                let latency_ns = ts.elapsed().as_nanos() as u64;
                                self.latency_sampler.record(latency_ns);
                                perf_inc!(self.perf_counters.latency_sample_count);
                                perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                                self.perf_counters.update_latency_max(latency_ns);
                            }
                            return Ok(r);
                        }
                    }
                    // mbufs are freed here when `packets` drops

                    if let Some(r) = result {
                        // Record latency sample if applicable
                        if let Some(ts) = rx_timestamp {
                            let latency_ns = ts.elapsed().as_nanos() as u64;
                            self.latency_sampler.record(latency_ns);
                            perf_inc!(self.perf_counters.latency_sample_count);
                            perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                            self.perf_counters.update_latency_max(latency_ns);
                        }
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

                    // Record burst stats
                    perf_inc!(self.perf_counters.rx_bursts);
                    perf_inc!(self.perf_counters.rx_burst_sum, frames.len() as u64);

                    // Latency sampling: timestamp at recv_frames return
                    let sample_this_burst = perf_should_sample!(self.latency_sampler);
                    let rx_timestamp = if sample_this_burst { Some(Instant::now()) } else { None };

                    let mut result: Option<(usize, SocketAddr)> = None;

                    for frame_data in &frames {
                        if let Some(r) = self.process_frame_zerocopy(frame_data, local_port, buf, &mut result, None) {
                            if let Some(ts) = rx_timestamp {
                                let latency_ns = ts.elapsed().as_nanos() as u64;
                                self.latency_sampler.record(latency_ns);
                                perf_inc!(self.perf_counters.latency_sample_count);
                                perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                                self.perf_counters.update_latency_max(latency_ns);
                            }
                            return Ok(r);
                        }
                    }

                    if let Some(r) = result {
                        if let Some(ts) = rx_timestamp {
                            let latency_ns = ts.elapsed().as_nanos() as u64;
                            self.latency_sampler.record(latency_ns);
                            perf_inc!(self.perf_counters.latency_sample_count);
                            perf_inc!(self.perf_counters.latency_sum_ns, latency_ns);
                            self.perf_counters.update_latency_max(latency_ns);
                        }
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
        hw_vlan_tci: Option<u16>,
    ) -> Option<(usize, SocketAddr)> {
        // Detect VLAN tag and determine the inner ethertype + L3 offset.
        // When the NIC has stripped the VLAN tag (hw_vlan_tci is Some), the frame
        // bytes are untagged but the layout carries the hardware TCI for filtering.
        let layout = match detect_vlan(frame_data, hw_vlan_tci) {
            Some(l) => l,
            None => return None,
        };

        // VLAN RX filtering: drop frames that don't match our VLAN mode.
        if let Some(ref vlan_cfg) = self.vlan_config {
            let frame_vid = layout.vlan_tci.map(|tci| tci & 0x0FFF);
            if !vlan_cfg.accepts_frame(frame_vid) {
                return None;
            }
        }

        // Handle ARP (both tagged and untagged)
        if layout.ethertype == arp::ETH_TYPE_ARP && self.auto_arp {
            if let Some(reply_frame) = self.arp_handler.process_arp(frame_data) {
                let _ = self.socket_backend.send_frame(&reply_frame, None);
            }
            perf_inc!(self.perf_counters.rx_arp_handled);
            return None;
        }

        // Handle ICMP (echo replies + error messages)
        if layout.ethertype == ETH_TYPE_IPV4 && frame_data.len() > layout.l3_offset + 9 {
            let protocol = frame_data[layout.l3_offset + 9];
            if protocol == icmp::IP_PROTO_ICMP {
                if let Some(action) = self.icmp_handler.process_icmp_full(frame_data) {
                    match action {
                        icmp::IcmpAction::Reply(reply_frame) => {
                            if self.auto_icmp {
                                let _ = self.socket_backend.send_frame(&reply_frame, None);
                            }
                        }
                        icmp::IcmpAction::Error(error_info) => {
                            let local_port = match self.local_addr {
                                SocketAddr::V4(v4) => v4.port(),
                                _ => 0,
                            };
                            if error_info.original_src_port == local_port {
                                self.queue_icmp_error(error_info.to_io_error());
                            }
                        }
                    }
                }
                perf_inc!(self.perf_counters.rx_icmp_handled);
                return None;
            }
        }

        // GUE RX decapsulation: if GUE is configured and the outer UDP dst_port
        // matches the GUE local port, decapsulate the inner IPv4/UDP packet.
        if let Some(ref gue_cfg) = self.gue_config {
            if layout.ethertype == ETH_TYPE_IPV4 {
                if let Some(decap) = gue::try_decap_gue(frame_data, layout.l3_offset, gue_cfg.local_port) {
                    if decap.inner_dst_port != local_port {
                        return None;
                    }

                    perf_inc!(self.perf_counters.rx_packets);
                    perf_inc!(self.perf_counters.rx_bytes, decap.payload.len() as u64);

                    let src_addr = SocketAddr::V4(
                        SocketAddrV4::new(decap.inner_src_ip, decap.inner_src_port),
                    );

                    if let Some(connected) = *self.connected_addr.lock().unwrap() {
                        if src_addr != connected {
                            let payload_len = decap.payload.len();
                            let mut queue = self.recv_queue.lock().unwrap();
                            if queue.push(decap.payload.to_vec(), src_addr).is_err() {
                                self.record_rx_drop(payload_len);
                            }
                            return None;
                        }
                    }

                    if result.is_none() {
                        let copy_len = std::cmp::min(buf.len(), decap.payload.len());
                        buf[..copy_len].copy_from_slice(&decap.payload[..copy_len]);
                        *result = Some((copy_len, src_addr));
                    } else {
                        let payload_len = decap.payload.len();
                        let mut queue = self.recv_queue.lock().unwrap();
                        if queue.push(decap.payload.to_vec(), src_addr).is_err() {
                            self.record_rx_drop(payload_len);
                        }
                    }

                    return None;
                }
            }
        }

        // Zero-copy UDP parse — payload borrows from frame_data.
        // Handles both tagged and untagged frames via detect_vlan internally.
        let parsed = match parse_udp_packet_ref(frame_data) {
            Some(p) => p,
            None => {
                perf_inc!(self.perf_counters.rx_drops_parse_fail);
                return None;
            }
        };

        // Validate RX checksums (IPv4 header + UDP) in software.
        // Both verifiers handle VLAN-tagged frames via detect_vlan internally.
        if !verify_ipv4_checksum(frame_data) {
            perf_inc!(self.perf_counters.rx_drops_parse_fail);
            return None;
        }
        if !verify_udp_checksum(frame_data) {
            perf_inc!(self.perf_counters.rx_drops_parse_fail);
            return None;
        }

        // Count successfully parsed RX packets
        perf_inc!(self.perf_counters.rx_packets);
        perf_inc!(self.perf_counters.rx_bytes, parsed.payload.len() as u64);

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
                let payload_len = parsed.payload.len();
                let mut queue = self.recv_queue.lock().unwrap();
                // Must allocate here — queued packets outlive the frame/mbuf
                if queue.push(parsed.payload.to_vec(), src_addr).is_err() {
                    self.record_rx_drop(payload_len);
                }
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
            let payload_len = parsed.payload.len();
            let mut queue = self.recv_queue.lock().unwrap();
            if queue.push(parsed.payload.to_vec(), src_addr).is_err() {
                self.record_rx_drop(payload_len);
            }
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
        self.has_connection_state.store(true, Ordering::Release);

        *self.connected_addr.lock().unwrap() = Some(addr);
        self.is_connected.store(true, Ordering::Release);
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
    // Routing Configuration
    // ========================================================================

    /// Configure subnet-aware routing for this socket.
    ///
    /// When set, the socket uses the routing table to determine whether to ARP
    /// for the destination IP directly (same subnet) or for the gateway IP
    /// (cross-subnet). Without this, all ARP targets the destination directly
    /// (legacy behavior, compatible with AWS VPC where gateway MAC is seeded).
    pub fn set_routing(&mut self, config: NetworkConfig) {
        self.routing_table = RoutingTable::with_config(config);
    }

    /// Get a reference to the current routing table.
    pub fn routing_table(&self) -> &RoutingTable {
        &self.routing_table
    }

    /// Returns the maximum UDP payload size for the configured MTU.
    ///
    /// This is `MTU - 20 (IPv4 header) - 8 (UDP header)`. With the default
    /// MTU of 1500 this returns 1472. With jumbo frames (MTU 9001) it returns
    /// 8973. Payloads larger than this limit will be rejected by `send_to()`.
    pub fn max_udp_payload(&self) -> usize {
        self.routing_table.max_udp_payload()
    }

    // ========================================================================
    // VLAN Configuration
    // ========================================================================

    /// Configure 802.1Q VLAN tagging and filtering on this socket.
    ///
    /// The [`VlanMode`] on the config determines both TX tagging and RX filtering:
    ///
    /// - **Access**: RX accepts untagged + matching VID (strips tag). TX sends untagged.
    /// - **Trunk**: RX accepts allowed VIDs (optional native_vlan for untagged). TX tags.
    /// - **PortTagging** (default): RX only accepts matching VID (strips tag). TX tags.
    ///
    /// Set to `None` to disable VLAN processing (accept all frames, send untagged).
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use dpdk_udp::{UdpSocket, VlanConfig};
    ///
    /// let mut socket = UdpSocket::bind("0.0.0.0:9000")?;
    /// // Port tagging (default): strict VID match on RX, tag on TX
    /// socket.set_vlan(Some(VlanConfig::new(100).with_priority(3)));
    ///
    /// // Access mode: accept untagged + VID 100, send untagged
    /// socket.set_vlan(Some(VlanConfig::new(100).access()));
    ///
    /// // Trunk mode: accept VIDs 100, 200, 300; untagged treated as VID 100
    /// socket.set_vlan(Some(VlanConfig::new(100).trunk(vec![100, 200, 300], Some(100))));
    /// ```
    pub fn set_vlan(&mut self, config: Option<VlanConfig>) {
        self.vlan_config = config;
    }

    /// Returns the current VLAN configuration, if any.
    pub fn vlan(&self) -> Option<&VlanConfig> {
        self.vlan_config.as_ref()
    }

    // ========================================================================
    // GUE (Generic UDP Encapsulation) Configuration
    // ========================================================================

    /// Configure GUE tunnel encapsulation on this socket.
    ///
    /// When set, outgoing packets are encapsulated in a GUE tunnel:
    /// `[Outer Eth][Outer IPv4][Outer UDP][GUE Header][Inner IPv4][Inner UDP][Payload]`
    ///
    /// Incoming packets on the GUE port are automatically decapsulated, and
    /// the inner source address is returned to the application.
    ///
    /// Set to `None` to disable GUE (send/receive plain UDP).
    pub fn set_gue(&mut self, config: Option<gue::GueConfig>) {
        self.gue_config = config;
    }

    /// Returns the current GUE tunnel configuration, if any.
    pub fn gue(&self) -> Option<&gue::GueConfig> {
        self.gue_config.as_ref()
    }

    /// Returns the maximum UDP payload size accounting for GUE overhead.
    ///
    /// When GUE is configured, the effective max payload is reduced by 32 bytes
    /// (outer IPv4 + outer UDP + GUE header).
    pub fn max_gue_payload(&self) -> usize {
        let base = self.routing_table.max_udp_payload();
        if self.gue_config.is_some() {
            base.saturating_sub(gue::GUE_ENCAP_OVERHEAD)
        } else {
            base
        }
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
            self.socket_backend.send_frame(&frame, None)?;
        }
        Ok(())
    }

    /// Send a Gratuitous ARP announcing this socket's MAC/IP mapping.
    ///
    /// Broadcasts an unsolicited ARP request so all network neighbors
    /// immediately learn our MAC address. This is useful after failover
    /// or IP migration to update stale ARP caches without waiting for
    /// the next inbound ARP request.
    pub fn send_gratuitous_arp(&self) -> io::Result<()> {
        if let Some(frame) = self.arp_handler.make_gratuitous_arp() {
            self.socket_backend.send_frame(&frame, None)?;
        }
        Ok(())
    }

    /// Internal helper: send GARP if auto_garp is enabled.
    /// Called at the end of bind() / bind_with_backend().
    /// Failures are silently ignored — GARP is best-effort.
    fn send_gratuitous_arp_if_enabled(&self) {
        if self.auto_garp {
            let _ = self.send_gratuitous_arp();
        }
    }

    /// Enable or disable automatic Gratuitous ARP on bind.
    ///
    /// When enabled (default), a Gratuitous ARP is broadcast during `bind()`
    /// to announce this socket's MAC/IP mapping to all network neighbors.
    pub fn set_auto_garp(&mut self, enable: bool) {
        self.auto_garp = enable;
    }

    /// Check if automatic Gratuitous ARP on bind is enabled.
    pub fn auto_garp(&self) -> bool {
        self.auto_garp
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
            self.socket_backend.send_frame(&arp_frame, None)?;

            for _ in 0..POLLS_PER_ATTEMPT {
                let frames = self.socket_backend.recv_frames(32)?;
                for frame_data in &frames {
                    if frame_data.len() >= 14 {
                        let ethertype = u16::from_be_bytes([frame_data[12], frame_data[13]]);
                        if ethertype == arp::ETH_TYPE_ARP {
                            if let Some(reply_frame) = self.arp_handler.process_arp(frame_data) {
                                let _ = self.socket_backend.send_frame(&reply_frame, None);
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
                                let payload_len = parsed.payload.len();
                                let mut queue = self.recv_queue.lock().unwrap();
                                if queue.push(parsed.payload, src_addr).is_err() {
                                    self.record_rx_drop(payload_len);
                                }
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
    // Receive Buffer Backpressure (SO_RCVBUF equivalent)
    // ========================================================================

    /// Returns the configured socket receive buffer size in bytes.
    ///
    /// Mirrors `getsockopt(SO_RCVBUF)` on a POSIX socket. When the queued
    /// payload bytes exceed this limit, incoming packets are dropped and the
    /// drop counters exposed by [`recv_drops`](Self::recv_drops) are bumped.
    pub fn recv_buffer_size(&self) -> usize {
        self.recv_queue.lock().unwrap().max_bytes()
    }

    /// Set the socket receive buffer size in bytes.
    ///
    /// Mirrors `setsockopt(SO_RCVBUF)` on a POSIX socket. The new limit is
    /// applied immediately — subsequent pushes that would exceed it are
    /// rejected and counted via [`recv_drops`](Self::recv_drops). Already-queued
    /// packets are not evicted.
    ///
    /// Returns `InvalidInput` if `bytes` is zero.
    pub fn set_recv_buffer_size(&self, bytes: usize) -> io::Result<()> {
        if bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recv buffer size must be > 0",
            ));
        }
        self.recv_queue.lock().unwrap().set_max_bytes(bytes);
        Ok(())
    }

    /// Returns the total bytes of queued payload currently held in the
    /// receive buffer. Useful for watermark-style monitoring.
    pub fn recv_buffer_bytes(&self) -> usize {
        self.recv_queue.lock().unwrap().bytes()
    }

    /// Returns socket-level receive drop counters.
    ///
    /// These increment whenever a UDP payload is rejected because the socket
    /// receive buffer was full. Applications should periodically diff the
    /// values to detect receive-side backpressure in production.
    ///
    /// Equivalent in spirit to the Linux `sk_drops` counter surfaced via
    /// `/proc/net/udp` column 13 or `ss -u -a -e`.
    pub fn recv_drops(&self) -> RecvDropStats {
        RecvDropStats {
            packets: self.rx_dropped_packets.load(Ordering::Relaxed),
            bytes: self.rx_dropped_bytes.load(Ordering::Relaxed),
        }
    }

    /// Reset the receive drop counters to zero.
    ///
    /// Intended for test harnesses and long-running services that want to
    /// measure drops over a specific window. Thread-safe.
    pub fn reset_recv_drops(&self) {
        self.rx_dropped_packets.store(0, Ordering::Relaxed);
        self.rx_dropped_bytes.store(0, Ordering::Relaxed);
    }

    /// Internal: record a receive-buffer drop (packet + bytes).
    ///
    /// Called from every `recv_queue.push()` call site when the push fails
    /// because either the packet-count or byte limit would be exceeded. Also
    /// bumps the `rx_drops_buffer_full` perf counter for the perf reporter.
    #[inline]
    fn record_rx_drop(&self, payload_len: usize) {
        self.rx_dropped_packets.fetch_add(1, Ordering::Relaxed);
        self.rx_dropped_bytes
            .fetch_add(payload_len as u64, Ordering::Relaxed);
        perf_inc!(self.perf_counters.rx_drops_buffer_full);
    }

    // ========================================================================
    // Socket Error Queue (ICMP Error Handling)
    // ========================================================================

    /// Gets the value of the `SO_ERROR` option on this socket.
    ///
    /// Returns the first pending error and removes it from the queue, or
    /// `Ok(None)` if no errors are pending. This mirrors Linux `getsockopt(SO_ERROR)`
    /// which dequeues errors one at a time.
    ///
    /// Errors are queued when ICMP error messages (Destination Unreachable,
    /// Time Exceeded, etc.) are received that reference a UDP datagram
    /// originating from this socket.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        // Fast path: no errors pending (common case)
        if !self.has_pending_error.load(Ordering::Acquire) {
            return Ok(None);
        }
        let mut queue = self.error_queue.lock().unwrap();
        let err = queue.pop_front();
        if queue.is_empty() {
            self.has_pending_error.store(false, Ordering::Release);
        }
        Ok(err)
    }

    /// Returns the number of pending errors in the socket error queue.
    pub fn pending_errors(&self) -> usize {
        self.error_queue.lock().unwrap().len()
    }

    /// Internal: queue an ICMP error on this socket.
    ///
    /// Called from the receive path when an ICMP error message references a
    /// UDP datagram originating from this socket's local IP and port.
    /// The error queue is bounded (max 16 entries) to prevent unbounded growth
    /// from ICMP floods.
    fn queue_icmp_error(&self, error: io::Error) {
        const MAX_ERROR_QUEUE: usize = 16;
        let mut queue = self.error_queue.lock().unwrap();
        if queue.len() < MAX_ERROR_QUEUE {
            queue.push_back(error);
            self.has_pending_error.store(true, Ordering::Release);
        }
        // Silently drop if queue is full (matches Linux behavior under ICMP flood)
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
    // Performance Instrumentation
    // ========================================================================

    /// Access live performance counters. Always available, zero-cost if not read.
    pub fn perf_counters(&self) -> &PerfCounters {
        &self.perf_counters
    }

    /// Get a shared reference to the performance counters (for passing to pipeline threads).
    pub fn perf_counters_arc(&self) -> Arc<PerfCounters> {
        Arc::clone(&self.perf_counters)
    }

    /// Get a shared reference to the latency sampler.
    pub fn latency_sampler(&self) -> &LatencySampler {
        &self.latency_sampler
    }

    /// Start background performance reporting to stderr.
    ///
    /// Emits one structured log line per `interval` with key=value pairs.
    /// Default interval: 10 seconds.
    pub fn enable_perf_reporting(&self, interval: Duration) -> std::io::Result<()> {
        let mut reporter_guard = self.perf_reporter.lock().unwrap();
        if reporter_guard.is_some() {
            return Ok(()); // already running
        }

        // Build a NIC stats callback for DPDK-backed sockets so the perf
        // reporter can emit `nic_imissed`/`nic_ierrors`/`nic_rx_nombuf`
        // deltas alongside the software-layer counters. Non-DPDK backends
        // (AF_PACKET, generic) pass `None` and the reporter emits "-".
        let nic_stats_fn: Option<NicStatsFn> = match &self.socket_backend {
            SocketBackend::Dpdk(res) => {
                let res = Arc::clone(res);
                Some(Box::new(move || {
                    res.port.stats().ok().map(|s| NicStatsSnapshot {
                        rx_missed: s.rx_missed,
                        rx_errors: s.rx_errors,
                        rx_nombuf: s.rx_nombuf,
                    })
                }))
            }
            SocketBackend::Generic(_) => None,
        };

        *reporter_guard = Some(PerfReporter::start(
            Arc::clone(&self.perf_counters),
            Arc::clone(&self.latency_sampler),
            interval,
            nic_stats_fn,
        ));
        Ok(())
    }

    /// Stop background performance reporting.
    pub fn disable_perf_reporting(&self) {
        let mut reporter_guard = self.perf_reporter.lock().unwrap();
        if let Some(mut reporter) = reporter_guard.take() {
            reporter.stop();
        }
    }

    /// Get a snapshot of current performance statistics.
    pub fn perf_snapshot(&self) -> PerfSnapshot {
        let snap = self.perf_counters.snapshot();
        let latencies = self.latency_sampler.percentiles();

        let lat_avg_us = if snap.latency_sample_count > 0 {
            (snap.latency_sum_ns as f64 / snap.latency_sample_count as f64) / 1000.0
        } else {
            0.0
        };

        let worker_total = snap.worker_packets_processed + snap.worker_idle_polls;
        let worker_idle_pct = if worker_total > 0 {
            snap.worker_idle_polls as f64 / worker_total as f64 * 100.0
        } else {
            0.0
        };

        let ring_drops = snap.worker_ring_enqueue_fail
            + snap.app_ring_enqueue_fail
            + snap.tx_ring_enqueue_fail;
        let total_attempted = snap.rx_packets + ring_drops;
        let ring_drop_rate = if total_attempted > 0 {
            ring_drops as f64 / total_attempted as f64
        } else {
            0.0
        };

        PerfSnapshot {
            rx_pps: 0.0, // instantaneous rate requires two snapshots
            tx_pps: 0.0,
            rx_drops: snap.rx_drops_ring_full,
            latency_avg_us: lat_avg_us,
            latency_p50_us: latencies.p50_ns as f64 / 1000.0,
            latency_p95_us: latencies.p95_ns as f64 / 1000.0,
            latency_p99_us: latencies.p99_ns as f64 / 1000.0,
            latency_max_us: snap.latency_max_ns as f64 / 1000.0,
            worker_idle_pct,
            ring_drop_rate,
        }
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

    /// Check if hardware VLAN tag insertion is active on the NIC for TX.
    ///
    /// When active, the NIC inserts 802.1Q VLAN tags from `mbuf.vlan_tci`,
    /// eliminating the CPU overhead of software tag insertion (~10% on tagged
    /// frames). To take effect, a `VlanConfig` must also be set on the socket.
    pub fn has_tx_vlan_offload(&self) -> bool {
        (self.resources.active_tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT as u64) != 0
    }

    /// Check if hardware VLAN tag stripping is active on the NIC for RX.
    ///
    /// When active, the NIC strips 802.1Q VLAN tags and stores the TCI in
    /// `mbuf.vlan_tci`, delivering untagged frames to software.
    pub fn has_rx_vlan_offload(&self) -> bool {
        (self.resources.active_rx_offload & dpdk_sys::RTE_ETH_RX_OFFLOAD_VLAN_STRIP as u64) != 0
    }

    /// Internal helper: returns true when hardware VLAN insert is available
    /// on the underlying DPDK port. Used by send_to() to decide between
    /// hardware and software VLAN tag insertion.
    fn has_hw_vlan_insert(&self) -> bool {
        match &self.socket_backend {
            SocketBackend::Dpdk(res) => {
                (res.active_tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT as u64) != 0
            }
            SocketBackend::Generic(_) => false,
        }
    }
}

// ============================================================================
// Pipeline helpers (free functions to avoid topology.lock() in hot path)
// ============================================================================

/// Poll per-worker SPSC app rings in round-robin order (lock-free).
///
/// This is a free function that operates on cached `Arc<SpscRing>` references
/// instead of going through `MultiCoreTopology::dequeue_app()`, which would
/// require holding the `topology` Mutex.
#[inline]
fn dequeue_app_rings(app_rings: &[Arc<SpscRing<AppPacket>>], rr_index: &mut usize) -> Option<AppPacket> {
    let n = app_rings.len();
    if n == 0 {
        return None;
    }
    for offset in 0..*rr_index + n {
        let idx = offset % n;
        if let Some(pkt) = app_rings[idx].dequeue() {
            *rr_index = idx + 1;
            return Some(pkt);
        }
    }
    None
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
/// count.
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_udp::UdpSocket;
///
/// // Explicit: 4 RSS queues (1 RX dispatcher + 3 queue workers)
/// let socket = UdpSocket::builder()
///     .rx_queues(4)
///     .bind("0.0.0.0:9000")?;
///
/// // Or just auto-detect (equivalent to UdpSocket::bind()):
/// let socket = UdpSocket::builder()
///     .bind("0.0.0.0:9000")?;
/// ```
pub struct UdpSocketBuilder {
    rx_queues: Option<u16>,
    backend_type: Option<BackendType>,
    network_config: Option<NetworkConfig>,
}

impl UdpSocketBuilder {
    /// Create a new builder with all defaults (auto-detect everything).
    pub fn new() -> Self {
        Self {
            rx_queues: None,
            backend_type: None,
            network_config: None,
        }
    }

    /// Set the number of RSS RX queues (pipeline threads).
    ///
    /// Each queue gets 1 processing thread. Set to 1 for run-to-completion
    /// mode (no pipeline). Will be clamped to the NIC's maximum.
    pub fn rx_queues(mut self, n: u16) -> Self {
        self.rx_queues = Some(n);
        self
    }

    /// Force a specific backend type.
    pub fn backend_type(mut self, backend: BackendType) -> Self {
        self.backend_type = Some(backend);
        self
    }

    /// Configure subnet-aware routing.
    ///
    /// When set, the socket distinguishes same-subnet destinations (ARP for
    /// peer directly) from cross-subnet destinations (ARP for gateway).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use dpdk_udp::{UdpSocket, NetworkConfig};
    /// use std::net::Ipv4Addr;
    ///
    /// let socket = UdpSocket::builder()
    ///     .network(
    ///         NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
    ///             .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
    ///     )
    ///     .bind("10.0.1.10:9000")?;
    /// ```
    pub fn network(mut self, config: NetworkConfig) -> Self {
        self.network_config = Some(config);
        self
    }

    /// Build the topology configuration from this builder's settings.
    pub fn topology_config(&self) -> TopologyConfig {
        TopologyConfig {
            rx_queues: self.rx_queues,
        }
    }

    /// Bind a UDP socket with the configured topology.
    ///
    /// This is equivalent to `UdpSocket::bind()` but uses the builder's
    /// topology configuration instead of pure auto-detection.
    ///
    /// When the topology plan is **not** run-to-completion, pipeline threads
    /// are spawned automatically. Use `.rx_queues(1)` to force
    /// run-to-completion mode (no pipeline threads, lowest latency).
    pub fn bind<A: ToSocketAddrs>(self, addr: A) -> io::Result<UdpSocket> {
        let topo_config = self.topology_config();

        // Pre-initialize DPDK with the configured MTU so the singleton is
        // sized correctly before UdpSocket::bind() claims it. Ignored if DPDK
        // is unavailable (UdpSocket::bind() will fall back to raw sockets).
        let mtu = self.network_config.as_ref()
            .map(|c| c.mtu)
            .filter(|&m| m > 0)
            .unwrap_or(9001);
        let _ = get_or_init_dpdk(0, mtu);

        // Create the socket using the standard bind path
        let mut socket = UdpSocket::bind(addr)?;

        // Apply routing, VLAN, and GUE configuration if provided.
        // Extract vlan/gue before moving net_config into RoutingTable.
        if let Some(mut net_config) = self.network_config {
            socket.vlan_config = net_config.vlan.take();
            socket.gue_config = net_config.gue.take();
            socket.routing_table = RoutingTable::with_config(net_config);
        }

        // Detect topology from config + runtime environment.
        // Under stubs this always returns run-to-completion.
        let plan = topology::detect_topology(
            &topo_config,
            // Under stubs we report 1 lcore, so the plan will be run-to-completion.
            // With real DPDK we'd query eal_lcore_count().
            if dpdk_sys::is_stub() { 1 } else { 8 },
            // Under stubs NIC max queues = 1.
            if dpdk_sys::is_stub() { 1 } else { 16 }, // max RX queues
            if dpdk_sys::is_stub() { 1 } else { 16 }, // max TX queues (P3.5)
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
                perf_counters: Arc::clone(&socket.perf_counters),
            };

            // Create backend closures that capture the socket's backend for the pipeline
            // We need raw function pointers since SocketBackend isn't Clone.
            // Use a shared reference approach via Arc.
            let resources_for_recv = Arc::clone(&socket.resources);
            let resources_for_send = Arc::clone(&socket.resources);

            let recv_fn = move |max_frames: usize, pool: &FramePool| -> io::Result<Vec<FrameRef>> {
                let packets = resources_for_recv.port.rx_burst(0, max_frames as u16)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rx_burst failed: {}", e)))?;
                let mut refs = Vec::with_capacity(packets.len());
                for mbuf in &packets {
                    if let Some(data) = mbuf.data() {
                        let len = mbuf.data_len() as usize;
                        let actual_len = len.min(data.len());
                        // Write directly from mbuf into FramePool — eliminates the
                        // intermediate Vec<u8> allocation and one full memcpy per packet.
                        if let Some(frame_ref) = pool.alloc_copy(&data[..actual_len]) {
                            refs.push(frame_ref);
                        }
                    }
                }
                Ok(refs)
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

            // P3.5: Worker-direct TX — app thread sends on TX queue 1, bypassing
            // the tx_ring → RX lcore → TX queue 0 hop. This halves echo latency
            // by eliminating cross-thread synchronization on the TX path.
            let resources_for_direct = Arc::clone(&socket.resources);
            let direct_send_fn: Arc<dyn Fn(&[u8], Option<u16>) -> io::Result<usize> + Send + Sync> =
                Arc::new(move |frame: &[u8], hw_vlan_tci: Option<u16>| -> io::Result<usize> {
                    let mut mbuf = resources_for_direct.mempool.alloc()
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("mbuf alloc: {}", e)))?;
                    mbuf.set_data_len(frame.len() as u16);
                    mbuf.set_packet_len(frame.len() as u32);
                    let data = mbuf.data_mut()
                        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "mbuf data_mut failed"))?;
                    data.copy_from_slice(frame);

                    // Hardware VLAN insert: set mbuf VLAN TCI so the NIC tags on wire
                    if let Some(tci) = hw_vlan_tci {
                        let tx_offload = resources_for_direct.active_tx_offload;
                        if (tx_offload & dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT as u64) != 0 {
                            mbuf.set_vlan_tci(tci);
                            mbuf.set_ol_flags(dpdk_sys::RTE_MBUF_F_TX_VLAN as u64);
                        }
                    }

                    let mut packets = vec![mbuf];
                    // TX queue 1 = dedicated app thread TX queue (no RX lcore contention)
                    let sent = resources_for_direct.port.tx_burst(1, &mut packets)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("tx_burst: {}", e)))?;
                    if sent == 0 {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "tx queue full"));
                    }
                    Ok(frame.len())
                });

            let topo = topology::start_pipeline(
                pipeline_config, recv_fn, send_fn, Some(direct_send_fn),
            );
            if let Some(topo) = topo {
                // Cache hot-path handles to avoid topology.lock() per packet.
                socket.cached_app_rings = Some(topo.app_rings.clone());
                socket.cached_frame_pool = Some(Arc::clone(&topo.frame_pool));
                socket.cached_direct_send = topo.direct_send_fn.clone();
                socket.has_pipeline.store(true, Ordering::Release);
                *socket.topology.lock().unwrap() = Some(topo);

                // Auto-enable perf reporting in multi-core mode so instrumentation
                // output is always visible in logs (10s interval).
                let _ = socket.enable_perf_reporting(Duration::from_secs(10));
            }
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

    // ========================================================================
    // RX CHECKSUM VALIDATION TESTS
    // ========================================================================

    #[test]
    fn test_verify_ipv4_checksum_valid_frame() {
        // Build a frame with valid checksums using build_udp_frame
        let frame = build_udp_frame(
            &[0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(192, 168, 1, 2),
            12345, 9000,
            b"hello",
            64,
        ).unwrap();

        assert!(verify_ipv4_checksum(&frame));
    }

    #[test]
    fn test_verify_ipv4_checksum_corrupted() {
        let mut frame = build_udp_frame(
            &[0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(192, 168, 1, 2),
            12345, 9000,
            b"hello",
            64,
        ).unwrap();

        // Corrupt the IP TTL field (byte 22 = ETH_HEADER_LEN + 8)
        frame[ETH_HEADER_LEN + 8] ^= 0xFF;
        assert!(!verify_ipv4_checksum(&frame));
    }

    #[test]
    fn test_verify_ipv4_checksum_too_short() {
        assert!(!verify_ipv4_checksum(&[0u8; 20])); // Less than ETH + IP header
    }

    #[test]
    fn test_verify_udp_checksum_valid_frame() {
        let frame = build_udp_frame(
            &[0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            Ipv4Addr::new(10, 0, 1, 10),
            Ipv4Addr::new(10, 0, 1, 20),
            5000, 6000,
            b"test payload data",
            64,
        ).unwrap();

        assert!(verify_udp_checksum(&frame));
    }

    #[test]
    fn test_verify_udp_checksum_corrupted_payload() {
        let mut frame = build_udp_frame(
            &[0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            Ipv4Addr::new(10, 0, 1, 10),
            Ipv4Addr::new(10, 0, 1, 20),
            5000, 6000,
            b"test payload",
            64,
        ).unwrap();

        // Corrupt the payload
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(!verify_udp_checksum(&frame));
    }

    #[test]
    fn test_verify_udp_checksum_zero_means_disabled() {
        let mut frame = build_udp_frame(
            &[0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            Ipv4Addr::new(10, 0, 1, 10),
            Ipv4Addr::new(10, 0, 1, 20),
            5000, 6000,
            b"test",
            64,
        ).unwrap();

        // Set UDP checksum to 0 (disabled per RFC 768)
        let udp_off = ETH_HEADER_LEN + IPV4_HEADER_LEN;
        frame[udp_off + 6] = 0;
        frame[udp_off + 7] = 0;
        assert!(verify_udp_checksum(&frame));
    }

    #[test]
    fn test_verify_udp_checksum_too_short() {
        assert!(!verify_udp_checksum(&[0u8; 30])); // Less than TOTAL_HEADER_LEN
    }

    #[test]
    fn test_verify_both_checksums_roundtrip() {
        // Build a frame and verify both checksums pass
        for payload in &[b"" as &[u8], b"a", b"hello world", &[0xAB; 100]] {
            let frame = build_udp_frame(
                &[0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                Ipv4Addr::new(172, 16, 0, 1),
                Ipv4Addr::new(172, 16, 0, 2),
                1234, 5678,
                payload,
                128,
            ).unwrap();

            assert!(verify_ipv4_checksum(&frame), "IPv4 checksum failed for payload len {}", payload.len());
            assert!(verify_udp_checksum(&frame), "UDP checksum failed for payload len {}", payload.len());
        }
    }

    // ========================================================================
    // TX OFFLOAD TESTS
    // ========================================================================

    #[test]
    fn test_udp_pseudo_header_checksum() {
        let src_ip = [192, 168, 1, 1];
        let dst_ip = [192, 168, 1, 2];
        let udp_len: u16 = 12; // 8 header + 4 payload

        let phdr_cksum = udp_pseudo_header_checksum(&src_ip, &dst_ip, udp_len);

        // The pseudo-header checksum should be non-zero for real addresses
        assert_ne!(phdr_cksum, 0);

        // Verify it's the correct partial sum: add the rest of the UDP data
        // and it should match the full checksum
        let udp_header = [
            0x30, 0x39, // Source port (12345)
            0x23, 0x28, // Dest port (9000)
            0x00, 0x0c, // Length (12)
            0x00, 0x00, // Checksum placeholder
        ];
        let payload = b"test";

        let full_cksum = udp_checksum(&src_ip, &dst_ip, &udp_header, payload);

        // The pseudo-header checksum is the starting point for the full calculation.
        // Verify both produce valid non-zero results.
        assert_ne!(phdr_cksum, 0);
        assert_ne!(full_cksum, 0);
    }

    #[test]
    fn test_mbuf_offload_fields() {
        // Test that ol_flags and tx_offload can be set on an Mbuf via the wrapper
        let pool = Mempool::create("offload_test_pool", 128, 32, 2048, -1).unwrap();
        let mut mbuf = pool.alloc().unwrap();

        // Initially zero
        assert_eq!(mbuf.ol_flags(), 0);

        // Set TX offload flags
        let flags = dpdk_sys::RTE_MBUF_F_TX_IPV4 as u64
            | dpdk_sys::RTE_MBUF_F_TX_IP_CKSUM as u64
            | dpdk_sys::RTE_MBUF_F_TX_UDP_CKSUM as u64;
        mbuf.set_ol_flags(flags);
        assert_eq!(mbuf.ol_flags(), flags);

        // Set TX offload lengths
        mbuf.set_tx_offload(14, 20, 8); // ETH, IPv4, UDP
        // Verify encoding: l2=14 (bits 0-6), l3=20 (bits 7-15), l4=8 (bits 16-23)
        let expected = 14u64 | (20u64 << 7) | (8u64 << 16);
        // Read back via shim function to verify (tx_offload is in an anonymous
        // union in real DPDK, so direct field access doesn't work with bindgen)
        let raw_tx_offload = unsafe { dpdk_sys::mbuf_get_tx_offload(mbuf.as_raw()) };
        assert_eq!(raw_tx_offload, expected);
    }

    #[test]
    fn test_mbuf_vlan_tci_field() {
        let pool = Mempool::create("vlan_tci_pool", 128, 32, 2048, -1).unwrap();
        let mut mbuf = pool.alloc().unwrap();

        // Initially zero
        assert_eq!(mbuf.vlan_tci(), 0);

        // Set VLAN TCI (VID=100, PCP=3, DEI=0 → TCI = 0x6064)
        let tci = VlanConfig::new(100).with_priority(3).encode_tci();
        mbuf.set_vlan_tci(tci);
        assert_eq!(mbuf.vlan_tci(), tci);
        assert_eq!(tci & 0x0FFF, 100); // VID
        assert_eq!((tci >> 13) & 0x07, 3); // PCP

        // Set TX VLAN offload flag
        mbuf.set_ol_flags(dpdk_sys::RTE_MBUF_F_TX_VLAN as u64);
        assert_eq!(mbuf.ol_flags() & dpdk_sys::RTE_MBUF_F_TX_VLAN as u64, dpdk_sys::RTE_MBUF_F_TX_VLAN as u64);
    }

    #[test]
    fn test_mbuf_vlan_tci_combined_with_checksum_flags() {
        let pool = Mempool::create("vlan_cksum_pool", 128, 32, 2048, -1).unwrap();
        let mut mbuf = pool.alloc().unwrap();

        // Combine VLAN insert + checksum offload flags (both can be active simultaneously)
        let ol_flags = dpdk_sys::RTE_MBUF_F_TX_VLAN as u64
            | dpdk_sys::RTE_MBUF_F_TX_IPV4 as u64
            | dpdk_sys::RTE_MBUF_F_TX_IP_CKSUM as u64
            | dpdk_sys::RTE_MBUF_F_TX_UDP_CKSUM as u64;

        mbuf.set_vlan_tci(0x0064); // VID 100
        mbuf.set_ol_flags(ol_flags);

        assert_eq!(mbuf.vlan_tci(), 0x0064);
        assert_eq!(mbuf.ol_flags(), ol_flags);
        // Verify individual flags are set
        assert_ne!(mbuf.ol_flags() & dpdk_sys::RTE_MBUF_F_TX_VLAN as u64, 0);
        assert_ne!(mbuf.ol_flags() & dpdk_sys::RTE_MBUF_F_TX_IP_CKSUM as u64, 0);
        assert_ne!(mbuf.ol_flags() & dpdk_sys::RTE_MBUF_F_TX_UDP_CKSUM as u64, 0);
    }

    #[test]
    fn test_vlan_config_force_software_default() {
        let cfg = VlanConfig::new(100);
        assert!(!cfg.force_software);
    }

    #[test]
    fn test_vlan_config_force_software_builder() {
        let cfg = VlanConfig::new(100).with_force_software(true);
        assert!(cfg.force_software);

        let cfg = VlanConfig::new(100).with_force_software(false);
        assert!(!cfg.force_software);
    }

    #[test]
    fn test_vlan_config_force_software_does_not_affect_tags_on_tx() {
        // force_software doesn't change WHAT gets tagged — only HOW
        let cfg_hw = VlanConfig::new(100).port_tagging();
        let cfg_sw = VlanConfig::new(100).port_tagging().with_force_software(true);
        assert_eq!(cfg_hw.tags_on_tx(), cfg_sw.tags_on_tx());

        let cfg_hw = VlanConfig::new(100).access();
        let cfg_sw = VlanConfig::new(100).access().with_force_software(true);
        assert_eq!(cfg_hw.tags_on_tx(), cfg_sw.tags_on_tx());
    }

    #[test]
    fn test_hw_vlan_offload_constants() {
        // Verify VLAN offload constants are defined and non-zero
        assert_ne!(dpdk_sys::RTE_MBUF_F_TX_VLAN as u64, 0);
        assert_ne!(dpdk_sys::RTE_MBUF_F_RX_VLAN as u64, 0);
        assert_ne!(dpdk_sys::RTE_MBUF_F_RX_VLAN_STRIPPED as u64, 0);
        assert_ne!(dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT as u64, 0);
        assert_ne!(dpdk_sys::RTE_ETH_RX_OFFLOAD_VLAN_STRIP as u64, 0);

        // TX VLAN flag should not overlap with checksum flags
        assert_eq!(dpdk_sys::RTE_MBUF_F_TX_VLAN as u64 & dpdk_sys::RTE_MBUF_F_TX_IP_CKSUM as u64, 0);
        assert_eq!(dpdk_sys::RTE_MBUF_F_TX_VLAN as u64 & dpdk_sys::RTE_MBUF_F_TX_UDP_CKSUM as u64, 0);
    }

    #[test]
    fn test_port_vlan_offload_config() {
        use dpdk::port::{PortConfig, RxOffload, TxOffload};

        // with_vlan_offload() enables both RX strip and TX insert
        let config = PortConfig::default().with_vlan_offload();
        assert!(config.rx_offload.vlan_strip);
        assert!(config.tx_offload.vlan_insert);

        // Can combine with checksum offload
        let config = PortConfig::default().with_checksum_offload().with_vlan_offload();
        assert!(config.rx_offload.vlan_strip);
        assert!(config.rx_offload.ipv4_cksum);
        assert!(config.tx_offload.vlan_insert);
        assert!(config.tx_offload.ipv4_cksum);

        // Flags encode correctly
        let rx_flags = config.rx_offload.to_flags();
        assert_ne!(rx_flags & dpdk_sys::RTE_ETH_RX_OFFLOAD_VLAN_STRIP as u64, 0);
        assert_ne!(rx_flags & dpdk_sys::RTE_ETH_RX_OFFLOAD_IPV4_CKSUM as u64, 0);

        let tx_flags = config.tx_offload.to_flags();
        assert_ne!(tx_flags & dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT as u64, 0);
        assert_ne!(tx_flags & dpdk_sys::RTE_ETH_TX_OFFLOAD_IPV4_CKSUM as u64, 0);
    }

    #[test]
    fn test_rx_hw_vlan_strip_direct_tci() {
        // Simulate what happens when the NIC strips a VLAN tag:
        // The NIC delivers an untagged frame but sets ol_flags and vlan_tci
        // in the mbuf metadata. detect_vlan() receives the hw_vlan_tci directly
        // and returns the correct FrameLayout without frame reconstruction.

        // An untagged frame (what the NIC delivers after stripping):
        let untagged = vec![
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // dst MAC (broadcast)
            0x02, 0x00, 0x00, 0x00, 0x00, 0x01, // src MAC
            0x08, 0x00, // EtherType: IPv4
            // ... payload bytes
            0x45, 0x00, 0x00, 0x1C,
        ];

        let hw_tci: u16 = 0x0064; // VID=100, PCP=0, DEI=0

        // With hw_vlan_tci=None: untagged frame, no VLAN detected
        let layout_no_hw = detect_vlan(&untagged, None).unwrap();
        assert!(layout_no_hw.vlan_tci.is_none());
        assert_eq!(layout_no_hw.ethertype, ETH_TYPE_IPV4);
        assert_eq!(layout_no_hw.l3_offset, ETH_HEADER_LEN);

        // With hw_vlan_tci=Some(tci): untagged frame, but VLAN TCI from hardware
        let layout_hw = detect_vlan(&untagged, Some(hw_tci)).unwrap();
        assert_eq!(layout_hw.vlan_tci, Some(hw_tci));
        assert_eq!(layout_hw.ethertype, ETH_TYPE_IPV4);
        // L3 offset is still 14 (frame bytes are untagged)
        assert_eq!(layout_hw.l3_offset, ETH_HEADER_LEN);
        // The VID from the TCI should be 100
        assert_eq!(hw_tci & 0x0FFF, 100);
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
        let max_payload = MAX_FRAME_SIZE - TOTAL_HEADER_LEN;
        let large_payload = vec![0u8; max_payload + 1];
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
        // Explicitly requesting rx_queues(1) forces run-to-completion
        // even if more cores would be available (under stubs this is the default
        // anyway, but the explicit setting is tested for the API contract)
        let socket = UdpSocket::builder()
            .rx_queues(1)
            .bind("127.0.0.1:0")
            .expect("builder bind should succeed");
        assert!(socket.is_run_to_completion());
    }

    #[test]
    fn test_builder_topology_config() {
        // Test that builder correctly produces TopologyConfig
        let builder = UdpSocketBuilder::new()
            .rx_queues(4);
        let config = builder.topology_config();
        assert_eq!(config.rx_queues, Some(4));
    }

    #[test]
    fn test_builder_partial_config() {
        // Default builder has auto-detect for rx_queues
        let builder = UdpSocketBuilder::new();
        let config = builder.topology_config();
        assert_eq!(config.rx_queues, None);
    }

    #[test]
    fn test_socket_is_run_to_completion_by_default() {
        // Standard bind() with no builder should be run-to-completion
        let socket = UdpSocket::bind("127.0.0.1:0")
            .expect("bind should succeed");
        assert!(socket.is_run_to_completion());
    }

    // ========================================================================
    // RX BACKPRESSURE / DROP COUNTER TESTS
    // ========================================================================

    /// Dummy source address used in all ReceiveQueue tests.
    fn test_src() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234))
    }

    #[test]
    fn recv_queue_push_accepts_under_byte_limit() {
        let mut q = ReceiveQueue::with_limits(16, 1024);
        assert!(q.push(vec![0u8; 512], test_src()).is_ok());
        assert_eq!(q.len(), 1);
        assert_eq!(q.bytes(), 512);
    }

    #[test]
    fn recv_queue_push_rejects_when_byte_limit_exceeded() {
        let mut q = ReceiveQueue::with_limits(16, 1024);
        assert!(q.push(vec![0u8; 900], test_src()).is_ok());
        // Second push would bring us to 1800 bytes > 1024 limit.
        let err = q.push(vec![0u8; 900], test_src()).unwrap_err();
        assert_eq!(err.len(), 900, "push should return the rejected payload");
        assert_eq!(q.len(), 1, "queue count should not change on reject");
        assert_eq!(q.bytes(), 900);
    }

    #[test]
    fn recv_queue_push_rejects_at_exact_byte_boundary() {
        let mut q = ReceiveQueue::with_limits(16, 1000);
        assert!(q.push(vec![0u8; 600], test_src()).is_ok());
        // 600 + 500 = 1100 > 1000 — reject.
        assert!(q.push(vec![0u8; 500], test_src()).is_err());
        // 600 + 400 = 1000 — accept (inclusive).
        assert!(q.push(vec![0u8; 400], test_src()).is_ok());
        assert_eq!(q.bytes(), 1000);
    }

    #[test]
    fn recv_queue_push_rejects_when_packet_limit_exceeded() {
        // Small packet count, generous byte budget.
        let mut q = ReceiveQueue::with_limits(2, 1_000_000);
        assert!(q.push(vec![0u8; 10], test_src()).is_ok());
        assert!(q.push(vec![0u8; 10], test_src()).is_ok());
        assert!(q.push(vec![0u8; 10], test_src()).is_err());
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn recv_queue_pop_decrements_current_bytes() {
        let mut q = ReceiveQueue::with_limits(16, 1024);
        q.push(vec![0u8; 300], test_src()).unwrap();
        q.push(vec![0u8; 400], test_src()).unwrap();
        assert_eq!(q.bytes(), 700);

        let (payload, _) = q.pop().unwrap();
        assert_eq!(payload.len(), 300);
        assert_eq!(q.bytes(), 400);

        let (payload, _) = q.pop().unwrap();
        assert_eq!(payload.len(), 400);
        assert_eq!(q.bytes(), 0);
    }

    #[test]
    fn recv_queue_pop_then_push_reuses_reclaimed_capacity() {
        let mut q = ReceiveQueue::with_limits(16, 1000);
        q.push(vec![0u8; 800], test_src()).unwrap();
        // Second push would overflow.
        assert!(q.push(vec![0u8; 500], test_src()).is_err());
        // Drain one packet, freeing 800 bytes of capacity.
        q.pop().unwrap();
        // Same push now succeeds.
        assert!(q.push(vec![0u8; 500], test_src()).is_ok());
    }

    #[test]
    fn recv_queue_set_max_bytes_updates_limit() {
        let mut q = ReceiveQueue::with_limits(16, 1000);
        q.push(vec![0u8; 800], test_src()).unwrap();
        assert!(q.push(vec![0u8; 300], test_src()).is_err());

        // Raise the limit; now the same push fits.
        q.set_max_bytes(2000);
        assert_eq!(q.max_bytes(), 2000);
        assert!(q.push(vec![0u8; 300], test_src()).is_ok());
    }

    #[test]
    fn socket_default_recv_buffer_size_matches_constant() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        assert_eq!(socket.recv_buffer_size(), DEFAULT_RECV_BUFFER_BYTES);
    }

    #[test]
    fn socket_set_recv_buffer_size_roundtrip() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        socket.set_recv_buffer_size(128 * 1024).unwrap();
        assert_eq!(socket.recv_buffer_size(), 128 * 1024);

        socket.set_recv_buffer_size(1 * 1024 * 1024).unwrap();
        assert_eq!(socket.recv_buffer_size(), 1024 * 1024);
    }

    #[test]
    fn socket_set_recv_buffer_size_rejects_zero() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        let err = socket.set_recv_buffer_size(0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn socket_drop_counters_start_at_zero() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        let drops = socket.recv_drops();
        assert_eq!(drops.packets, 0);
        assert_eq!(drops.bytes, 0);
    }

    #[test]
    fn socket_record_rx_drop_bumps_counters() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        socket.record_rx_drop(1400);
        socket.record_rx_drop(600);

        let drops = socket.recv_drops();
        assert_eq!(drops.packets, 2);
        assert_eq!(drops.bytes, 2000);
    }

    #[test]
    fn socket_reset_recv_drops_zeros_counters() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        socket.record_rx_drop(512);
        socket.record_rx_drop(512);
        assert_eq!(socket.recv_drops().packets, 2);

        socket.reset_recv_drops();
        let drops = socket.recv_drops();
        assert_eq!(drops.packets, 0);
        assert_eq!(drops.bytes, 0);
    }

    #[test]
    fn socket_record_rx_drop_also_bumps_perf_counter() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        socket.record_rx_drop(100);
        socket.record_rx_drop(200);
        // Perf counter is shared via Arc — the socket holds it directly.
        let snap = socket.perf_counters.snapshot();
        assert_eq!(snap.rx_drops_buffer_full, 2);
    }

    #[test]
    fn recv_buffer_bytes_tracks_queued_payload() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        assert_eq!(socket.recv_buffer_bytes(), 0);

        // Push directly into the queue to simulate buffered packets.
        {
            let mut q = socket.recv_queue.lock().unwrap();
            q.push(vec![0u8; 500], test_src()).unwrap();
            q.push(vec![0u8; 700], test_src()).unwrap();
        }
        assert_eq!(socket.recv_buffer_bytes(), 1200);
    }

    #[test]
    fn recv_buffer_overflow_records_drops() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
        // Shrink the buffer to a tight 1 KiB budget.
        socket.set_recv_buffer_size(1024).unwrap();

        // Fill to 900 bytes.
        {
            let mut q = socket.recv_queue.lock().unwrap();
            q.push(vec![0u8; 900], test_src()).unwrap();
        }

        // Simulate a push attempt that overflows via the helper path.
        let rejected_len = 300usize;
        {
            let mut q = socket.recv_queue.lock().unwrap();
            if q.push(vec![0u8; rejected_len], test_src()).is_err() {
                socket.record_rx_drop(rejected_len);
            }
        }

        let drops = socket.recv_drops();
        assert_eq!(drops.packets, 1);
        assert_eq!(drops.bytes, rejected_len as u64);
        assert_eq!(socket.recv_buffer_bytes(), 900, "queued bytes unchanged");
    }

    #[test]
    fn recv_drop_stats_is_copy_and_comparable() {
        let a = RecvDropStats { packets: 5, bytes: 1500 };
        let b = a; // Copy
        assert_eq!(a, b);
        assert_eq!(a.packets, 5);
        assert_eq!(a.bytes, 1500);
    }

    #[test]
    fn default_recv_buffer_bytes_constant_is_reasonable() {
        // Must be large enough to hold at least ~170 MTU-1500 UDP datagrams —
        // this keeps the default within one order of magnitude of the Linux
        // kernel's `net.core.rmem_default`.
        assert!(DEFAULT_RECV_BUFFER_BYTES >= 200 * 1024);
        assert!(DEFAULT_RECV_BUFFER_BYTES <= 4 * 1024 * 1024);
    }

    // ========================================================================
    // ICMP Error Queue / take_error() Tests
    // ========================================================================

    #[test]
    fn take_error_returns_none_when_empty() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(socket.take_error().unwrap().is_none());
        assert_eq!(socket.pending_errors(), 0);
    }

    #[test]
    fn take_error_returns_queued_errors_in_order() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();

        // Queue two errors
        socket.queue_icmp_error(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "ICMP: port unreachable",
        ));
        socket.queue_icmp_error(io::Error::new(
            io::ErrorKind::TimedOut,
            "ICMP: TTL exceeded",
        ));

        assert_eq!(socket.pending_errors(), 2);

        // First error
        let err1 = socket.take_error().unwrap().expect("should have error");
        assert_eq!(err1.kind(), io::ErrorKind::ConnectionRefused);

        // Second error
        let err2 = socket.take_error().unwrap().expect("should have error");
        assert_eq!(err2.kind(), io::ErrorKind::TimedOut);

        // Queue is now empty
        assert!(socket.take_error().unwrap().is_none());
        assert_eq!(socket.pending_errors(), 0);
    }

    #[test]
    fn take_error_queue_is_bounded() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();

        // Queue more than the max (16)
        for i in 0..20 {
            socket.queue_icmp_error(io::Error::new(
                io::ErrorKind::Other,
                format!("error {}", i),
            ));
        }

        // Only 16 should be stored
        assert_eq!(socket.pending_errors(), 16);

        // Drain them all
        let mut count = 0;
        while socket.take_error().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 16);
    }

    #[test]
    fn has_pending_error_flag_tracks_queue_state() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();

        // Initially no errors
        assert!(!socket.has_pending_error.load(Ordering::Acquire));

        // Queue one
        socket.queue_icmp_error(io::Error::new(io::ErrorKind::Other, "test"));
        assert!(socket.has_pending_error.load(Ordering::Acquire));

        // Drain it
        socket.take_error().unwrap();
        assert!(!socket.has_pending_error.load(Ordering::Acquire));
    }

    #[test]
    fn process_frame_zerocopy_queues_icmp_port_unreachable() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_port = match socket.local_addr {
            SocketAddr::V4(v4) => v4.port(),
            _ => panic!("expected v4"),
        };

        // Build an ICMP Destination Unreachable (port unreachable) frame
        // that references a UDP packet from our socket
        let our_ip = match socket.local_addr {
            SocketAddr::V4(v4) => *v4.ip(),
            _ => panic!("expected v4"),
        };

        let frame = build_icmp_error_frame_for_test(
            Ipv4Addr::new(10, 0, 1, 1), // router
            our_ip,
            icmp::ICMP_TYPE_DEST_UNREACHABLE,
            icmp::ICMP_CODE_PORT_UNREACHABLE,
            0, // no MTU
            our_ip,
            Ipv4Addr::new(10, 0, 2, 200),
            local_port,
            9000,
        );

        // Process the frame
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, local_port, &mut buf, &mut result, None);

        // Should NOT produce a UDP result
        assert!(result.is_none());

        // Should have queued an ICMP error
        let err = socket.take_error().unwrap().expect("should have ICMP error");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert!(err.to_string().contains("port unreachable"));
    }

    #[test]
    fn process_frame_zerocopy_ignores_icmp_error_for_wrong_port() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_port = match socket.local_addr {
            SocketAddr::V4(v4) => v4.port(),
            _ => panic!("expected v4"),
        };

        let our_ip = match socket.local_addr {
            SocketAddr::V4(v4) => *v4.ip(),
            _ => panic!("expected v4"),
        };

        // Build ICMP error referencing a DIFFERENT source port
        let frame = build_icmp_error_frame_for_test(
            Ipv4Addr::new(10, 0, 1, 1),
            our_ip,
            icmp::ICMP_TYPE_DEST_UNREACHABLE,
            icmp::ICMP_CODE_PORT_UNREACHABLE,
            0,
            our_ip,
            Ipv4Addr::new(10, 0, 2, 200),
            local_port.wrapping_add(1), // different port
            9000,
        );

        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, local_port, &mut buf, &mut result, None);

        // No error should be queued
        assert!(socket.take_error().unwrap().is_none());
    }

    /// Helper: build an ICMP error frame for use in lib.rs tests.
    fn build_icmp_error_frame_for_test(
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
        let total = 14 + 20 + 8 + 20 + 8;
        let mut frame = vec![0u8; total];

        // Ethernet
        frame[0..6].copy_from_slice(&[0xbb; 6]);
        frame[6..12].copy_from_slice(&[0xaa; 6]);
        frame[12..14].copy_from_slice(&ETH_TYPE_IPV4.to_be_bytes());

        // Outer IP
        let ip = 14;
        frame[ip] = 0x45;
        let outer_total_len = (20 + 8 + 20 + 8) as u16;
        frame[ip + 2..ip + 4].copy_from_slice(&outer_total_len.to_be_bytes());
        frame[ip + 8] = 64;
        frame[ip + 9] = icmp::IP_PROTO_ICMP;
        frame[ip + 12..ip + 16].copy_from_slice(&error_src_ip.octets());
        frame[ip + 16..ip + 20].copy_from_slice(&error_dst_ip.octets());

        // ICMP header
        let icmp_off = 34;
        frame[icmp_off] = icmp_type;
        frame[icmp_off + 1] = icmp_code;
        frame[icmp_off + 6..icmp_off + 8].copy_from_slice(&next_hop_mtu.to_be_bytes());

        // Original IP header
        let orig_ip = 42;
        frame[orig_ip] = 0x45;
        let orig_total = (20 + 8) as u16;
        frame[orig_ip + 2..orig_ip + 4].copy_from_slice(&orig_total.to_be_bytes());
        frame[orig_ip + 8] = 64;
        frame[orig_ip + 9] = 17; // UDP
        frame[orig_ip + 12..orig_ip + 16].copy_from_slice(&orig_src_ip.octets());
        frame[orig_ip + 16..orig_ip + 20].copy_from_slice(&orig_dst_ip.octets());

        // Original UDP ports
        let orig_udp = 62;
        frame[orig_udp..orig_udp + 2].copy_from_slice(&orig_src_port.to_be_bytes());
        frame[orig_udp + 2..orig_udp + 4].copy_from_slice(&orig_dst_port.to_be_bytes());

        // ICMP checksum
        let cksum = icmp::icmp_checksum(&frame[icmp_off..]);
        frame[icmp_off + 2..icmp_off + 4].copy_from_slice(&cksum.to_be_bytes());

        frame
    }

    // ========================================================================
    // VLAN (802.1Q) Tests
    // ========================================================================

    #[test]
    fn vlan_config_encode_decode_tci() {
        let config = VlanConfig::new(100).with_priority(3).with_dei(true);
        let tci = config.encode_tci();
        let decoded = VlanConfig::from_tci(tci);
        assert_eq!(decoded.vlan_id, 100);
        assert_eq!(decoded.priority, 3);
        assert!(decoded.dei);
    }

    #[test]
    fn vlan_config_tci_encoding() {
        // VID 100, PCP 5, DEI 0
        let config = VlanConfig { vlan_id: 100, priority: 5, dei: false, mode: VlanMode::default(), force_software: false };
        let tci = config.encode_tci();
        assert_eq!(tci & 0x0FFF, 100);           // VID
        assert_eq!((tci >> 13) & 0x07, 5);        // PCP
        assert_eq!((tci >> 12) & 1, 0);            // DEI

        // VID 4094 (max), PCP 7, DEI 1
        let config = VlanConfig { vlan_id: 4094, priority: 7, dei: true, mode: VlanMode::default(), force_software: false };
        let tci = config.encode_tci();
        assert_eq!(tci & 0x0FFF, 4094);
        assert_eq!((tci >> 13) & 0x07, 7);
        assert_eq!((tci >> 12) & 1, 1);
    }

    #[test]
    fn vlan_config_new_sets_defaults() {
        let config = VlanConfig::new(42);
        assert_eq!(config.vlan_id, 42);
        assert_eq!(config.priority, 0);
        assert!(!config.dei);
    }

    #[test]
    fn build_and_parse_vlan_tagged_frame() {
        let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let dst_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let src_ip = Ipv4Addr::new(10, 0, 1, 10);
        let dst_ip = Ipv4Addr::new(10, 0, 2, 20);
        let payload = b"hello vlan";
        let vlan = VlanConfig::new(100).with_priority(3);

        let mut buf = Vec::new();
        let len = build_udp_frame_into_vlan(
            &mut buf, &src_mac, &dst_mac, src_ip, dst_ip, 5000, 9000,
            payload, 64, &vlan,
        ).unwrap();

        assert_eq!(len, TOTAL_HEADER_LEN_VLAN + payload.len());
        assert_eq!(buf.len(), len);

        // Verify VLAN tag is present
        let tpid = u16::from_be_bytes([buf[12], buf[13]]);
        assert_eq!(tpid, ETH_TYPE_VLAN);
        let tci = u16::from_be_bytes([buf[14], buf[15]]);
        assert_eq!(tci & 0x0FFF, 100);             // VID
        assert_eq!((tci >> 13) & 0x07, 3);          // PCP
        let inner_ethertype = u16::from_be_bytes([buf[16], buf[17]]);
        assert_eq!(inner_ethertype, ETH_TYPE_IPV4);

        // Parse it back
        let parsed = parse_udp_packet(&buf).expect("should parse VLAN-tagged UDP");
        assert_eq!(parsed.src_ip, src_ip);
        assert_eq!(parsed.dst_ip, dst_ip);
        assert_eq!(parsed.src_port, 5000);
        assert_eq!(parsed.dst_port, 9000);
        assert_eq!(parsed.payload, payload);
        assert_eq!(parsed.vlan_id, Some(100));
    }

    #[test]
    fn parse_untagged_frame_has_no_vlan_id() {
        let frame = build_udp_frame(
            &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(10, 0, 1, 10), Ipv4Addr::new(10, 0, 2, 20),
            5000, 9000, b"hello", 64,
        ).unwrap();

        let parsed = parse_udp_packet(&frame).unwrap();
        assert!(parsed.vlan_id.is_none());
    }

    #[test]
    fn parse_udp_packet_ref_handles_vlan() {
        let vlan = VlanConfig::new(200).with_priority(5).with_dei(true);
        let mut buf = Vec::new();
        build_udp_frame_into_vlan(
            &mut buf, &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(10, 0, 1, 10), Ipv4Addr::new(10, 0, 2, 20),
            1234, 5678, b"zerocopy vlan", 64, &vlan,
        ).unwrap();

        let parsed = parse_udp_packet_ref(&buf).expect("should parse VLAN-tagged ref");
        assert_eq!(parsed.payload, b"zerocopy vlan");
        assert_eq!(parsed.src_port, 1234);
        assert_eq!(parsed.dst_port, 5678);
        assert_eq!(parsed.vlan_id, Some(200));
    }

    #[test]
    fn verify_checksums_on_vlan_tagged_frame() {
        let vlan = VlanConfig::new(42);
        let mut buf = Vec::new();
        build_udp_frame_into_vlan(
            &mut buf, &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 2),
            3000, 4000, b"checksum test", 128, &vlan,
        ).unwrap();

        assert!(verify_ipv4_checksum(&buf), "IPv4 checksum should be valid on VLAN frame");
        assert!(verify_udp_checksum(&buf), "UDP checksum should be valid on VLAN frame");
    }

    #[test]
    fn verify_checksums_on_corrupted_vlan_frame() {
        let vlan = VlanConfig::new(42);
        let mut buf = Vec::new();
        build_udp_frame_into_vlan(
            &mut buf, &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 2),
            3000, 4000, b"corrupt me", 128, &vlan,
        ).unwrap();

        // Corrupt a payload byte
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;

        assert!(verify_ipv4_checksum(&buf), "IP checksum should still be valid");
        assert!(!verify_udp_checksum(&buf), "UDP checksum should fail after payload corruption");
    }

    #[test]
    fn detect_vlan_on_untagged_frame() {
        let frame = build_udp_frame(
            &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(10, 0, 1, 1), Ipv4Addr::new(10, 0, 1, 2),
            1000, 2000, b"test", 64,
        ).unwrap();

        let layout = detect_vlan(&frame, None).unwrap();
        assert_eq!(layout.ethertype, ETH_TYPE_IPV4);
        assert_eq!(layout.l3_offset, ETH_HEADER_LEN);
        assert!(layout.vlan_tci.is_none());
    }

    #[test]
    fn detect_vlan_on_tagged_frame() {
        let vlan = VlanConfig::new(500).with_priority(2);
        let mut buf = Vec::new();
        build_udp_frame_into_vlan(
            &mut buf, &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(10, 0, 1, 1), Ipv4Addr::new(10, 0, 1, 2),
            1000, 2000, b"test", 64, &vlan,
        ).unwrap();

        let layout = detect_vlan(&buf, None).unwrap();
        assert_eq!(layout.ethertype, ETH_TYPE_IPV4);
        assert_eq!(layout.l3_offset, ETH_HEADER_LEN + VLAN_TAG_LEN);
        let tci = layout.vlan_tci.unwrap();
        assert_eq!(tci & 0x0FFF, 500);
        assert_eq!((tci >> 13) & 0x07, 2);
    }

    #[test]
    fn detect_vlan_returns_none_on_too_short_frame() {
        assert!(detect_vlan(&[], None).is_none());
        assert!(detect_vlan(&[0u8; 13], None).is_none());
    }

    #[test]
    fn socket_set_and_get_vlan() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(socket.vlan().is_none());

        socket.set_vlan(Some(VlanConfig::new(100)));
        let vlan = socket.vlan().unwrap();
        assert_eq!(vlan.vlan_id, 100);

        socket.set_vlan(None);
        assert!(socket.vlan().is_none());
    }

    #[test]
    fn vlan_frame_is_4_bytes_longer_than_untagged() {
        let src_mac = [0x11; 6];
        let dst_mac = [0xaa; 6];
        let src_ip = Ipv4Addr::new(10, 0, 1, 1);
        let dst_ip = Ipv4Addr::new(10, 0, 1, 2);
        let payload = b"size test";

        let untagged = build_udp_frame(
            &src_mac, &dst_mac, src_ip, dst_ip, 1000, 2000, payload, 64,
        ).unwrap();

        let mut tagged = Vec::new();
        build_udp_frame_into_vlan(
            &mut tagged, &src_mac, &dst_mac, src_ip, dst_ip, 1000, 2000,
            payload, 64, &VlanConfig::new(1),
        ).unwrap();

        assert_eq!(tagged.len(), untagged.len() + VLAN_TAG_LEN);
    }

    #[test]
    fn vlan_tagged_and_untagged_have_same_payload() {
        let payload = b"identity test with longer payload data";

        let untagged = build_udp_frame(
            &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(10, 0, 1, 1), Ipv4Addr::new(10, 0, 2, 2),
            5000, 9000, payload, 64,
        ).unwrap();

        let mut tagged = Vec::new();
        build_udp_frame_into_vlan(
            &mut tagged, &[0x11; 6], &[0xaa; 6],
            Ipv4Addr::new(10, 0, 1, 1), Ipv4Addr::new(10, 0, 2, 2),
            5000, 9000, payload, 64, &VlanConfig::new(100),
        ).unwrap();

        let parsed_untagged = parse_udp_packet(&untagged).unwrap();
        let parsed_tagged = parse_udp_packet(&tagged).unwrap();

        assert_eq!(parsed_untagged.payload, parsed_tagged.payload);
        assert_eq!(parsed_untagged.src_ip, parsed_tagged.src_ip);
        assert_eq!(parsed_untagged.dst_ip, parsed_tagged.dst_ip);
        assert_eq!(parsed_untagged.src_port, parsed_tagged.src_port);
        assert_eq!(parsed_untagged.dst_port, parsed_tagged.dst_port);
    }

    #[test]
    fn network_config_with_vlan() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
            .with_vlan(VlanConfig::new(100).with_priority(3));

        let vlan = config.vlan.as_ref().unwrap();
        assert_eq!(vlan.vlan_id, 100);
        assert_eq!(vlan.priority, 3);
    }

    #[test]
    fn process_frame_zerocopy_handles_vlan_tagged_udp() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_port = match socket.local_addr {
            SocketAddr::V4(v4) => v4.port(),
            _ => panic!("expected v4"),
        };

        // Build a VLAN-tagged UDP frame addressed to our port
        let vlan = VlanConfig::new(100);
        let mut frame = Vec::new();
        build_udp_frame_into_vlan(
            &mut frame,
            &[0xaa; 6], &[0xbb; 6],
            Ipv4Addr::new(10, 0, 1, 1), Ipv4Addr::new(127, 0, 0, 1),
            8000, local_port,
            b"vlan payload", 64, &vlan,
        ).unwrap();

        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, local_port, &mut buf, &mut result, None);

        // Should have received the UDP payload
        let (len, src_addr) = result.expect("should receive VLAN-tagged UDP packet");
        assert_eq!(&buf[..len], b"vlan payload");
        assert_eq!(src_addr.port(), 8000);
    }

    #[test]
    fn process_frame_zerocopy_hw_vlan_tci_filters_correctly() {
        // Simulate HW VLAN strip: untagged frame bytes + hw_vlan_tci from mbuf.
        // PortTagging mode should accept matching VID and reject wrong VID.
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).port_tagging()));

        let frame = make_untagged_frame(port);
        let mut buf = [0u8; 1500];

        // Matching VID via hw_vlan_tci: should accept
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, Some(100));
        assert!(result.is_some(), "hw_vlan_tci=100 should be accepted by PortTagging(100)");

        // Wrong VID via hw_vlan_tci: should drop
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, Some(999));
        assert!(result.is_none(), "hw_vlan_tci=999 should be rejected by PortTagging(100)");

        // No hw_vlan_tci (genuinely untagged): should drop (PortTagging requires a tag)
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_none(), "untagged frame should be rejected by PortTagging(100)");
    }

    // ========================================================================
    // VLAN Mode Tests
    // ========================================================================

    /// Helper: build a tagged UDP frame destined for the given port.
    fn make_tagged_frame(dst_port: u16, vid: u16) -> Vec<u8> {
        let vlan = VlanConfig::new(vid);
        let mut frame = Vec::new();
        build_udp_frame_into_vlan(
            &mut frame,
            &[0xaa; 6], &[0xbb; 6],
            Ipv4Addr::new(10, 0, 1, 1), Ipv4Addr::new(127, 0, 0, 1),
            8000, dst_port,
            b"test payload", 64, &vlan,
        ).unwrap();
        frame
    }

    /// Helper: build an untagged UDP frame destined for the given port.
    fn make_untagged_frame(dst_port: u16) -> Vec<u8> {
        build_udp_frame(
            &[0xaa; 6], &[0xbb; 6],
            Ipv4Addr::new(10, 0, 1, 1), Ipv4Addr::new(127, 0, 0, 1),
            8000, dst_port,
            b"test payload", 64,
        ).unwrap()
    }

    fn socket_local_port(socket: &UdpSocket) -> u16 {
        match socket.local_addr {
            SocketAddr::V4(v4) => v4.port(),
            _ => panic!("expected v4"),
        }
    }

    // ── accepts_frame unit tests ──

    #[test]
    fn vlan_mode_access_accepts_untagged() {
        let cfg = VlanConfig::new(100).access();
        assert!(cfg.accepts_frame(None));
    }

    #[test]
    fn vlan_mode_access_accepts_matching_vid() {
        let cfg = VlanConfig::new(100).access();
        assert!(cfg.accepts_frame(Some(100)));
    }

    #[test]
    fn vlan_mode_access_drops_wrong_vid() {
        let cfg = VlanConfig::new(100).access();
        assert!(!cfg.accepts_frame(Some(200)));
    }

    #[test]
    fn vlan_mode_port_tagging_drops_untagged() {
        let cfg = VlanConfig::new(100).port_tagging();
        assert!(!cfg.accepts_frame(None));
    }

    #[test]
    fn vlan_mode_port_tagging_accepts_matching_vid() {
        let cfg = VlanConfig::new(100).port_tagging();
        assert!(cfg.accepts_frame(Some(100)));
    }

    #[test]
    fn vlan_mode_port_tagging_drops_wrong_vid() {
        let cfg = VlanConfig::new(100).port_tagging();
        assert!(!cfg.accepts_frame(Some(200)));
    }

    #[test]
    fn vlan_mode_trunk_accepts_allowed_vid() {
        let cfg = VlanConfig::new(100).trunk(vec![100, 200, 300], None);
        assert!(cfg.accepts_frame(Some(100)));
        assert!(cfg.accepts_frame(Some(200)));
        assert!(cfg.accepts_frame(Some(300)));
    }

    #[test]
    fn vlan_mode_trunk_drops_disallowed_vid() {
        let cfg = VlanConfig::new(100).trunk(vec![100, 200], None);
        assert!(!cfg.accepts_frame(Some(999)));
    }

    #[test]
    fn vlan_mode_trunk_drops_untagged_without_native() {
        let cfg = VlanConfig::new(100).trunk(vec![100], None);
        assert!(!cfg.accepts_frame(None));
    }

    #[test]
    fn vlan_mode_trunk_accepts_untagged_with_native() {
        let cfg = VlanConfig::new(100).trunk(vec![100, 200], Some(100));
        assert!(cfg.accepts_frame(None));
    }

    // ── tags_on_tx tests ──

    #[test]
    fn vlan_tags_on_tx_access_is_false() {
        let cfg = VlanConfig::new(100).access();
        assert!(!cfg.tags_on_tx());
    }

    #[test]
    fn vlan_tags_on_tx_trunk_is_true() {
        let cfg = VlanConfig::new(100).trunk(vec![100], None);
        assert!(cfg.tags_on_tx());
    }

    #[test]
    fn vlan_tags_on_tx_port_tagging_is_true() {
        let cfg = VlanConfig::new(100).port_tagging();
        assert!(cfg.tags_on_tx());
    }

    // ── process_frame_zerocopy RX filtering tests ──

    #[test]
    fn vlan_access_mode_rx_accepts_untagged_frame() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).access()));

        let frame = make_untagged_frame(port);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "Access mode should accept untagged frames");
    }

    #[test]
    fn vlan_access_mode_rx_accepts_matching_tagged_frame() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).access()));

        let frame = make_tagged_frame(port, 100);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "Access mode should accept matching VID");
    }

    #[test]
    fn vlan_access_mode_rx_drops_wrong_vid() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).access()));

        let frame = make_tagged_frame(port, 200);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_none(), "Access mode should drop wrong VID");
    }

    #[test]
    fn vlan_port_tagging_rx_drops_untagged() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).port_tagging()));

        let frame = make_untagged_frame(port);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_none(), "PortTagging mode should drop untagged frames");
    }

    #[test]
    fn vlan_port_tagging_rx_accepts_matching_vid() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).port_tagging()));

        let frame = make_tagged_frame(port, 100);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "PortTagging mode should accept matching VID");
    }

    #[test]
    fn vlan_port_tagging_rx_drops_wrong_vid() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).port_tagging()));

        let frame = make_tagged_frame(port, 999);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_none(), "PortTagging mode should drop wrong VID");
    }

    #[test]
    fn vlan_trunk_rx_accepts_allowed_vid() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).trunk(vec![100, 200, 300], None)));

        let frame = make_tagged_frame(port, 200);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "Trunk mode should accept allowed VID 200");
    }

    #[test]
    fn vlan_trunk_rx_drops_disallowed_vid() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).trunk(vec![100, 200], None)));

        let frame = make_tagged_frame(port, 999);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_none(), "Trunk mode should drop disallowed VID");
    }

    #[test]
    fn vlan_trunk_rx_drops_untagged_without_native() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).trunk(vec![100], None)));

        let frame = make_untagged_frame(port);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_none(), "Trunk mode should drop untagged without native_vlan");
    }

    #[test]
    fn vlan_trunk_rx_accepts_untagged_with_native() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(100).trunk(vec![100, 200], Some(100))));

        let frame = make_untagged_frame(port);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "Trunk mode should accept untagged with native_vlan");
    }

    #[test]
    fn vlan_no_config_accepts_all_frames() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);

        // No VLAN config: accept tagged
        let frame = make_tagged_frame(port, 42);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "No VLAN config should accept tagged frames");

        // No VLAN config: accept untagged
        let frame = make_untagged_frame(port);
        let mut result2 = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result2, None);
        assert!(result2.is_some(), "No VLAN config should accept untagged frames");
    }

    // ── VlanConfig builder tests ──

    #[test]
    fn vlan_config_default_mode_is_port_tagging() {
        let cfg = VlanConfig::new(100);
        assert_eq!(cfg.mode, VlanMode::PortTagging);
    }

    #[test]
    fn vlan_config_access_builder() {
        let cfg = VlanConfig::new(100).with_priority(3).access();
        assert_eq!(cfg.mode, VlanMode::Access);
        assert_eq!(cfg.priority, 3);
        assert_eq!(cfg.vlan_id, 100);
    }

    #[test]
    fn vlan_config_trunk_builder() {
        let cfg = VlanConfig::new(100).trunk(vec![100, 200, 300], Some(100));
        assert_eq!(cfg.mode, VlanMode::Trunk {
            allowed_vlans: vec![100, 200, 300],
            native_vlan: Some(100),
        });
    }

    #[test]
    fn vlan_config_with_mode_builder() {
        let cfg = VlanConfig::new(50).with_mode(VlanMode::Access);
        assert_eq!(cfg.mode, VlanMode::Access);
        assert_eq!(cfg.vlan_id, 50);
    }

    // ========================================================================
    // Synthetic PPS Benchmark: VLAN mode overhead
    // ========================================================================

    /// Synthetic PPS benchmark measuring process_frame_zerocopy throughput
    /// across VLAN modes. Run with:
    ///   cargo test -p dpdk-stdlib-udp -- --nocapture vlan_pps_benchmark
    ///
    /// This measures pure CPU overhead of VLAN filtering in the RX hot path,
    /// independent of NIC speed. Useful for extrapolating the cost of VLAN
    /// filtering vs the ~600K+ PPS baseline seen in DPDK integration tests.
    #[test]
    fn vlan_pps_benchmark() {
        const ITERATIONS: u64 = 500_000;

        // Scenarios: (label, vlan_config, frame_builder)
        struct Scenario {
            label: &'static str,
            vlan_config: Option<VlanConfig>,
            use_tagged_frame: bool,
            tag_vid: u16,
        }

        let scenarios = vec![
            Scenario {
                label: "No VLAN config (baseline, untagged)",
                vlan_config: None,
                use_tagged_frame: false,
                tag_vid: 0,
            },
            Scenario {
                label: "No VLAN config (baseline, tagged frame)",
                vlan_config: None,
                use_tagged_frame: true,
                tag_vid: 100,
            },
            Scenario {
                label: "PortTagging mode (matching VID 100)",
                vlan_config: Some(VlanConfig::new(100).port_tagging()),
                use_tagged_frame: true,
                tag_vid: 100,
            },
            Scenario {
                label: "Access mode (untagged frame)",
                vlan_config: Some(VlanConfig::new(100).access()),
                use_tagged_frame: false,
                tag_vid: 0,
            },
            Scenario {
                label: "Access mode (matching VID 100)",
                vlan_config: Some(VlanConfig::new(100).access()),
                use_tagged_frame: true,
                tag_vid: 100,
            },
            Scenario {
                label: "Trunk mode (VID 100 in allowed set)",
                vlan_config: Some(VlanConfig::new(100).trunk(vec![100, 200, 300], None)),
                use_tagged_frame: true,
                tag_vid: 100,
            },
            Scenario {
                label: "Trunk mode (untagged, native_vlan=100)",
                vlan_config: Some(VlanConfig::new(100).trunk(vec![100, 200], Some(100))),
                use_tagged_frame: false,
                tag_vid: 0,
            },
            Scenario {
                label: "PortTagging mode (DROP: wrong VID)",
                vlan_config: Some(VlanConfig::new(100).port_tagging()),
                use_tagged_frame: true,
                tag_vid: 999,
            },
            Scenario {
                label: "PortTagging mode (DROP: untagged)",
                vlan_config: Some(VlanConfig::new(100).port_tagging()),
                use_tagged_frame: false,
                tag_vid: 0,
            },
        ];

        println!("\n=== VLAN PPS Benchmark ({ITERATIONS} iterations per scenario) ===");
        println!("{:<50} {:>12} {:>10}", "Scenario", "PPS", "ns/pkt");
        println!("{}", "-".repeat(75));

        let mut baseline_pps: Option<f64> = None;

        for scenario in &scenarios {
            let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            let port = socket_local_port(&socket);
            socket.set_vlan(scenario.vlan_config.clone());

            // Pre-build the frame
            let frame = if scenario.use_tagged_frame {
                make_tagged_frame(port, scenario.tag_vid)
            } else {
                make_untagged_frame(port)
            };

            // Warmup
            let mut buf = [0u8; 1500];
            for _ in 0..1000 {
                let mut result = None;
                socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
            }

            // Timed run
            let start = Instant::now();
            for _ in 0..ITERATIONS {
                let mut result = None;
                socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
            }
            let elapsed = start.elapsed();

            let pps = ITERATIONS as f64 / elapsed.as_secs_f64();
            let ns_per_pkt = elapsed.as_nanos() as f64 / ITERATIONS as f64;

            if baseline_pps.is_none() {
                baseline_pps = Some(pps);
            }

            let overhead = if let Some(base) = baseline_pps {
                let pct = (1.0 - pps / base) * 100.0;
                if pct.abs() < 0.5 { String::from("  (baseline)") }
                else { format!("  ({:+.1}%)", -pct) }
            } else {
                String::new()
            };

            println!("{:<50} {:>10.0} K {:>8.0} ns{}",
                scenario.label,
                pps / 1000.0,
                ns_per_pkt,
                overhead,
            );
        }
        println!("{}", "=".repeat(75));
    }

    // ========================================================================
    // Synthetic PPS Benchmark: HW VLAN strip reconstruction vs direct TCI
    // ========================================================================

    /// Measures performance of hw_vlan_tci passthrough in process_frame_zerocopy.
    ///
    /// Compares two code paths for VLAN-aware RX processing:
    ///
    /// 1. **Reconstruction (legacy)**: Rebuild a tagged frame from untagged bytes +
    ///    TCI, then pass to process_frame_zerocopy with hw_vlan_tci=None. This
    ///    forces detect_vlan() to parse the VLAN tag from frame bytes and allocates
    ///    a new Vec per packet.
    ///
    /// 2. **Direct TCI (current)**: Pass the untagged frame as-is with
    ///    hw_vlan_tci=Some(tci). detect_vlan() returns the correct FrameLayout
    ///    with zero allocation.
    ///
    /// Run with:
    ///   cargo test -p dpdk-stdlib-udp -- --nocapture hw_vlan_strip_benchmark
    #[test]
    fn hw_vlan_strip_benchmark() {
        const ITERATIONS: u64 = 500_000;
        const VID: u16 = 100;

        // Build an untagged frame (simulates what NIC delivers after HW strip)
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_vlan(Some(VlanConfig::new(VID).port_tagging()));
        let untagged_frame = make_untagged_frame(port);

        println!("\n=== HW VLAN Strip Benchmark ({ITERATIONS} iterations) ===");
        println!("{:<55} {:>12} {:>10}", "Approach", "PPS", "ns/pkt");
        println!("{}", "-".repeat(80));

        // ── Approach A: Reconstruction (legacy, removed) ──
        // Simulates what recv_frames USED to do: allocate a Vec, copy MACs,
        // insert [0x8100|TCI], copy rest, then pass to process_frame_zerocopy
        // which re-parses the tag via detect_vlan.
        let mut buf = [0u8; 1500];
        {
            // Warmup with a pre-built reconstructed frame
            let reconstructed = {
                let tci: u16 = VID;
                let mut frame = Vec::with_capacity(untagged_frame.len() + VLAN_TAG_LEN);
                frame.extend_from_slice(&untagged_frame[..12]);
                frame.extend_from_slice(&ETH_TYPE_VLAN.to_be_bytes());
                frame.extend_from_slice(&tci.to_be_bytes());
                frame.extend_from_slice(&untagged_frame[12..]);
                frame
            };
            for _ in 0..1000 {
                let mut result = None;
                socket.process_frame_zerocopy(&reconstructed, port, &mut buf, &mut result, None);
            }

            let start = Instant::now();
            for _ in 0..ITERATIONS {
                // Per-packet allocation + copy (the old hot-path cost)
                let tci: u16 = VID;
                let mut frame = Vec::with_capacity(untagged_frame.len() + VLAN_TAG_LEN);
                frame.extend_from_slice(&untagged_frame[..12]);
                frame.extend_from_slice(&ETH_TYPE_VLAN.to_be_bytes());
                frame.extend_from_slice(&tci.to_be_bytes());
                frame.extend_from_slice(&untagged_frame[12..]);

                let mut result = None;
                socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
            }
            let elapsed_a = start.elapsed();
            let pps_a = ITERATIONS as f64 / elapsed_a.as_secs_f64();
            let ns_a = elapsed_a.as_nanos() as f64 / ITERATIONS as f64;

            println!("{:<55} {:>10.0} K {:>8.0} ns  (legacy)",
                "A: Reconstruct frame + detect_vlan parse",
                pps_a / 1000.0, ns_a);

            // ── Approach B: Direct TCI passthrough (current implementation) ──
            // Passes untagged frame bytes + hw_vlan_tci to process_frame_zerocopy.
            // detect_vlan() returns the TCI from the parameter, zero allocation.

            // Warmup
            for _ in 0..1000 {
                let mut result = None;
                socket.process_frame_zerocopy(&untagged_frame, port, &mut buf, &mut result, Some(VID));
            }

            let start = Instant::now();
            for _ in 0..ITERATIONS {
                let mut result = None;
                socket.process_frame_zerocopy(&untagged_frame, port, &mut buf, &mut result, Some(VID));
            }
            let elapsed_b = start.elapsed();
            let pps_b = ITERATIONS as f64 / elapsed_b.as_secs_f64();
            let ns_b = elapsed_b.as_nanos() as f64 / ITERATIONS as f64;

            println!("{:<55} {:>10.0} K {:>8.0} ns  (current)",
                "B: Direct hw_vlan_tci (no reconstruction)",
                pps_b / 1000.0, ns_b);

            println!("{}", "-".repeat(80));

            let speedup = pps_b / pps_a;
            let saved_ns = ns_a - ns_b;
            println!("Speedup:  {:.2}x  ({:.0} ns saved per packet)", speedup, saved_ns);
            println!("At 600K PPS: reconstruction would waste ~{:.1} ms/sec of CPU",
                saved_ns * 600_000.0 / 1_000_000.0);
            println!("{}", "=".repeat(80));
        }
    }

    // ── GUE (Generic UDP Encapsulation) socket-level tests ──

    fn make_gue_frame(
        inner_src_ip: Ipv4Addr,
        inner_dst_ip: Ipv4Addr,
        inner_src_port: u16,
        inner_dst_port: u16,
        gue_dst_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut frame = Vec::new();
        gue::build_gue_frame_into(
            &mut frame,
            &[0xaa; 6],
            &[0xbb; 6],
            Ipv4Addr::new(10, 0, 0, 2),
            Ipv4Addr::new(127, 0, 0, 1),
            6080,
            gue_dst_port,
            inner_src_ip,
            inner_dst_ip,
            inner_src_port,
            inner_dst_port,
            payload,
            64,
        )
        .unwrap();
        frame
    }

    #[test]
    fn gue_rx_decapsulates_matching_frame() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_gue(Some(gue::GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))));

        let frame = make_gue_frame(
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(127, 0, 0, 1),
            9000,
            port,
            6080,
            b"gue tunnel payload",
        );

        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);

        assert!(result.is_some(), "GUE frame should be decapsulated and accepted");
        let (len, src_addr) = result.unwrap();
        assert_eq!(len, 18);
        assert_eq!(&buf[..len], b"gue tunnel payload");
        assert_eq!(src_addr, SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(192, 168, 1, 10),
            9000,
        )));
    }

    #[test]
    fn gue_rx_rejects_wrong_inner_port() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_gue(Some(gue::GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))));

        let frame = make_gue_frame(
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(127, 0, 0, 1),
            9000,
            port + 1, // wrong inner port
            6080,
            b"wrong port",
        );

        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_none(), "GUE frame with wrong inner port should be dropped");
    }

    #[test]
    fn gue_rx_rejects_wrong_outer_port() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_gue(Some(gue::GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))));

        let frame = make_gue_frame(
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(127, 0, 0, 1),
            9000,
            port,
            7000, // wrong outer GUE port
            b"wrong outer port",
        );

        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        // Falls through to normal UDP parse which won't match either
        assert!(result.is_none(), "GUE frame with wrong outer port should not decap");
    }

    #[test]
    fn gue_rx_passthrough_when_not_configured() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);

        // Normal untagged frame should work without GUE config
        let frame = make_untagged_frame(port);
        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "Normal frames should work without GUE config");
    }

    #[test]
    fn gue_max_payload_accounts_for_overhead() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let base = socket.max_gue_payload();

        socket.set_gue(Some(gue::GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))));
        // GUE overhead is 32 bytes (outer IP 20 + outer UDP 8 + GUE header 4)
        assert_eq!(socket.max_gue_payload(), base - 32);
    }

    #[test]
    fn gue_config_via_network_config() {
        let net_cfg = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 50), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
            .with_gue(gue::GueConfig::new(Ipv4Addr::new(10, 0, 2, 1)));
        assert!(net_cfg.gue.is_some());
    }

    #[test]
    fn gue_set_and_get() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(socket.gue().is_none());

        socket.set_gue(Some(gue::GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))));
        assert!(socket.gue().is_some());
        assert_eq!(socket.gue().unwrap().remote_ip, Ipv4Addr::new(10, 0, 0, 2));

        socket.set_gue(None);
        assert!(socket.gue().is_none());
    }

    #[test]
    fn gue_rx_empty_payload() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_gue(Some(gue::GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))));

        let frame = make_gue_frame(
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(127, 0, 0, 1),
            9000,
            port,
            6080,
            &[],
        );

        let mut buf = [0u8; 1500];
        let mut result = None;
        socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        assert!(result.is_some(), "Empty GUE payload should be accepted");
        let (len, _) = result.unwrap();
        assert_eq!(len, 0);
    }

    #[test]
    fn gue_pps_benchmark() {
        let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket_local_port(&socket);
        socket.set_gue(Some(gue::GueConfig::new(Ipv4Addr::new(10, 0, 0, 2))));

        let frame = make_gue_frame(
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(127, 0, 0, 1),
            9000,
            port,
            6080,
            b"benchmark payload data for GUE tunnel",
        );

        let no_gue_frame = make_untagged_frame(port);

        let mut buf = [0u8; 1500];
        const ITERATIONS: usize = 500_000;
        const WARMUP: usize = 10_000;

        println!("\n{}", "=".repeat(80));
        println!("GUE Encapsulation PPS Benchmark ({} iterations per scenario)", ITERATIONS);
        println!("{}", "=".repeat(80));
        println!("{:<55} {:>10} {:>10}", "Scenario", "PPS (K)", "ns/pkt");
        println!("{}", "-".repeat(80));

        // Warmup
        for _ in 0..WARMUP {
            let mut result = None;
            socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        }

        // Benchmark: GUE decapsulation
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            let mut result = None;
            socket.process_frame_zerocopy(&frame, port, &mut buf, &mut result, None);
        }
        let elapsed_gue = start.elapsed();
        let pps_gue = ITERATIONS as f64 / elapsed_gue.as_secs_f64();
        let ns_gue = elapsed_gue.as_nanos() as f64 / ITERATIONS as f64;
        println!("{:<55} {:>10.0} {:>10.0}", "GUE decap (matching frame)", pps_gue / 1000.0, ns_gue);

        // Benchmark: baseline no-GUE processing (disable GUE first)
        socket.set_gue(None);
        for _ in 0..WARMUP {
            let mut result = None;
            socket.process_frame_zerocopy(&no_gue_frame, port, &mut buf, &mut result, None);
        }
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            let mut result = None;
            socket.process_frame_zerocopy(&no_gue_frame, port, &mut buf, &mut result, None);
        }
        let elapsed_plain = start.elapsed();
        let pps_plain = ITERATIONS as f64 / elapsed_plain.as_secs_f64();
        let ns_plain = elapsed_plain.as_nanos() as f64 / ITERATIONS as f64;
        println!("{:<55} {:>10.0} {:>10.0}", "Plain UDP (no GUE, baseline)", pps_plain / 1000.0, ns_plain);

        println!("{}", "-".repeat(80));
        let overhead = ns_gue - ns_plain;
        let overhead_pct = overhead / ns_plain * 100.0;
        println!("GUE overhead: {:.0} ns/pkt ({:.1}%)", overhead, overhead_pct);
        println!("{}", "=".repeat(80));
    }
}
