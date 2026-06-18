//! DPDK-accelerated TCP stack
//!
//! This crate provides a drop-in replacement for `std::net::TcpStream` and
//! `std::net::TcpListener` using DPDK userspace networking.
//!
//! Depends on `dpdk-stdlib-net` for `PacketBackend` — does NOT depend on `dpdk-udp`.

pub mod clock;
pub mod codec;
pub mod congestion;
pub mod contract;
pub mod engine;
pub mod error;
pub mod isn;
pub mod ring;
pub mod seq;
pub mod state;
pub mod stream;
pub mod tcb;
pub mod timer;

// Re-export codec public API at crate root for convenience.
pub use codec::{
    build_tcp_frame, build_tcp_packet, compute_mss, parse_tcp_packet, tcp_checksum,
};

// --- Constants ---

/// Maximum TCP payload for IPv4 (MTU 1500 - 20 IPv4 - 20 TCP).
pub const MAX_TCP_PAYLOAD: usize = 1460;

/// Maximum TCP payload for IPv6 (MTU 1500 - 40 IPv6 - 20 TCP).
pub const MAX_TCP_PAYLOAD_V6: usize = 1440;

/// Default peer MSS when no MSS option is present in SYN/SYN-ACK (RFC 9293).
pub const DEFAULT_PEER_MSS: u16 = 536;

// --- Reserved public function names for IPv6 follow-on spec ---

/// Reserved for IPv6 TCP frame building (follow-on spec).
pub fn build_tcp6_frame(_params: &codec::TcpFrameParams) -> Result<Vec<u8>, error::TcpError> {
    Err(error::TcpError::InvalidPacket(
        "IPv6 TCP not yet implemented".to_string(),
    ))
}

/// Reserved for IPv6 TCP packet parsing (follow-on spec).
pub fn parse_tcp6_packet(_frame: &[u8]) -> Result<codec::ParsedTcpSegment, error::TcpError> {
    Err(error::TcpError::InvalidPacket(
        "IPv6 TCP not yet implemented".to_string(),
    ))
}

/// Reserved for IPv6 TCP checksum (follow-on spec).
pub fn tcp6_checksum(_src_ip: &[u8], _dst_ip: &[u8], _tcp_segment: &[u8]) -> u16 {
    0
}
