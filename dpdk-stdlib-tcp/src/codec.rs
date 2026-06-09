//! TCP codec types: flags, options, parsed segments, and frame parameters.

use std::net::SocketAddr;

use crate::seq::SeqNum;

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
}
