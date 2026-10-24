//! TCP state machine states and connection 4-tuple.

use std::net::SocketAddr;

/// The 11 states of the TCP state machine (RFC 9293).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TcpState {
    Closed = 0,
    Listen = 1,
    SynSent = 2,
    SynReceived = 3,
    Established = 4,
    FinWait1 = 5,
    FinWait2 = 6,
    CloseWait = 7,
    Closing = 8,
    LastAck = 9,
    TimeWait = 10,
}

impl TcpState {
    /// Convert from raw u8 (e.g. from AtomicU8).
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Closed),
            1 => Some(Self::Listen),
            2 => Some(Self::SynSent),
            3 => Some(Self::SynReceived),
            4 => Some(Self::Established),
            5 => Some(Self::FinWait1),
            6 => Some(Self::FinWait2),
            7 => Some(Self::CloseWait),
            8 => Some(Self::Closing),
            9 => Some(Self::LastAck),
            10 => Some(Self::TimeWait),
            _ => None,
        }
    }
}

/// A TCP connection identified by its 4-tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourTuple {
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

impl FourTuple {
    /// Serialize to bytes for hashing (ISN generation).
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut out = [0u8; 12];
        match (self.local, self.remote) {
            (SocketAddr::V4(l), SocketAddr::V4(r)) => {
                out[0..4].copy_from_slice(&l.ip().octets());
                out[4..6].copy_from_slice(&l.port().to_be_bytes());
                out[6..10].copy_from_slice(&r.ip().octets());
                out[10..12].copy_from_slice(&r.port().to_be_bytes());
            }
            _ => {} // IPv6 deferred
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_state_roundtrip() {
        for v in 0..=10u8 {
            let state = TcpState::from_u8(v).unwrap();
            assert_eq!(state as u8, v);
        }
        assert!(TcpState::from_u8(11).is_none());
        assert!(TcpState::from_u8(255).is_none());
    }

    #[test]
    fn tcp_state_all_variants() {
        let states = [
            TcpState::Closed,
            TcpState::Listen,
            TcpState::SynSent,
            TcpState::SynReceived,
            TcpState::Established,
            TcpState::FinWait1,
            TcpState::FinWait2,
            TcpState::CloseWait,
            TcpState::Closing,
            TcpState::LastAck,
            TcpState::TimeWait,
        ];
        assert_eq!(states.len(), 11);
    }

    #[test]
    fn four_tuple_eq_and_hash() {
        use std::collections::HashSet;
        let ft1 = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let ft2 = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let ft3 = FourTuple {
            local: "10.0.0.1:1235".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        assert_eq!(ft1, ft2);
        assert_ne!(ft1, ft3);
        let mut set = HashSet::new();
        set.insert(ft1);
        assert!(set.contains(&ft2));
        assert!(!set.contains(&ft3));
    }

    #[test]
    fn four_tuple_to_bytes_deterministic() {
        let ft = FourTuple {
            local: "192.168.1.1:5000".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let b1 = ft.to_bytes();
        let b2 = ft.to_bytes();
        assert_eq!(b1, b2);
        // Check content
        assert_eq!(&b1[0..4], &[192, 168, 1, 1]);
        assert_eq!(&b1[4..6], &5000u16.to_be_bytes());
        assert_eq!(&b1[6..10], &[10, 0, 0, 2]);
        assert_eq!(&b1[10..12], &80u16.to_be_bytes());
    }
}
