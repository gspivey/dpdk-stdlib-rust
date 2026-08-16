//! Transmit queue for collecting outbound datagrams from the s2n-quic endpoint.

use crate::ecn::ecn_to_tos_bits;
use crate::frame::build_quic_frame;
use crate::path_handle::DpdkPathHandle;
use dpdk_udp::MAX_UDP_PAYLOAD;
use s2n_quic_core::inet::SocketAddress;
use s2n_quic_core::io::tx;
use s2n_quic_core::path::Handle as _;
use std::net::{Ipv4Addr, SocketAddr};

/// A complete Ethernet frame ready to send via the backend.
pub struct TxDatagram {
    pub frame: Vec<u8>,
}

/// Transmit queue for outbound QUIC datagrams.
pub struct DpdkTxQueue {
    pending: Vec<TxDatagram>,
    capacity: usize,
    local_addr: SocketAddr,
    src_mac: [u8; 6],
    gateway_mac: [u8; 6],
    frame_buf: Vec<u8>,
}

impl DpdkTxQueue {
    pub fn new(
        local_addr: SocketAddr,
        capacity: usize,
        src_mac: [u8; 6],
        gateway_mac: [u8; 6],
    ) -> Self {
        Self {
            pending: Vec::with_capacity(capacity),
            capacity,
            local_addr,
            src_mac,
            gateway_mac,
            frame_buf: Vec::new(),
        }
    }

    /// Drain all pending frames for transmission.
    pub fn drain(&mut self) -> std::vec::Drain<'_, TxDatagram> {
        self.pending.drain(..)
    }
}

/// Extract IPv4 address and port from an s2n-quic SocketAddress.
/// Returns None for IPv6.
fn socket_address_to_v4(addr: &SocketAddress) -> Option<(Ipv4Addr, u16)> {
    match addr {
        SocketAddress::IpV4(v4) => {
            let ip = Ipv4Addr::from(*v4.ip());
            Some((ip, v4.port()))
        }
        SocketAddress::IpV6(_) => None,
    }
}

impl tx::Queue for DpdkTxQueue {
    type Handle = DpdkPathHandle;

    const SUPPORTS_ECN: bool = true;
    const SUPPORTS_PACING: bool = false;
    const SUPPORTS_FLOW_LABELS: bool = false;

    fn push<M: tx::Message<Handle = Self::Handle>>(
        &mut self,
        mut message: M,
    ) -> Result<tx::Outcome, tx::Error> {
        if self.pending.len() >= self.capacity {
            return Err(tx::Error::AtCapacity);
        }

        let path = message.path_handle();
        let remote = path.remote_address();
        let (dst_ip, dst_port) = socket_address_to_v4(&remote)
            .expect("IPv6 should be rejected at path handle construction");

        let local = path.local_address();
        let (src_ip, src_port) = match socket_address_to_v4(&local) {
            Some(v) => v,
            None => {
                // Fall back to bound local address
                let ip = match self.local_addr.ip() {
                    std::net::IpAddr::V4(v4) => v4,
                    _ => Ipv4Addr::UNSPECIFIED,
                };
                (ip, self.local_addr.port())
            }
        };
        // Use local_addr port if the path handle's local port is 0
        let src_port = if src_port == 0 {
            self.local_addr.port()
        } else {
            src_port
        };

        let ecn = message.ecn();
        let tos = ecn_to_tos_bits(ecn);

        let segment_len = MAX_UDP_PAYLOAD;
        let index = self.pending.len();
        let mut total_len = 0usize;

        // First segment (gso_offset = 0)
        let mut payload_buf = vec![0u8; segment_len];
        let buf = tx::PayloadBuffer::new(&mut payload_buf);
        let written = message.write_payload(buf, 0)?;
        if written == 0 {
            return Err(tx::Error::EmptyPayload);
        }
        total_len += written;

        self.frame_buf.clear();
        build_quic_frame(
            &mut self.frame_buf,
            &self.src_mac,
            &self.gateway_mac,
            src_ip,
            dst_ip,
            src_port,
            dst_port,
            &payload_buf[..written],
            tos,
        )
        .map_err(|_| tx::Error::UndersizedBuffer)?;
        self.pending.push(TxDatagram {
            frame: self.frame_buf.clone(),
        });

        // GSO: produce additional segments if the message supports it
        let mut gso_offset = written;
        let mut segment_count = 1;
        while message.can_gso(written, segment_count + 1) {
            if self.pending.len() >= self.capacity {
                break;
            }
            let mut seg_buf = vec![0u8; segment_len];
            let buf = tx::PayloadBuffer::new(&mut seg_buf);
            match message.write_payload(buf, gso_offset) {
                Ok(n) if n > 0 => {
                    self.frame_buf.clear();
                    build_quic_frame(
                        &mut self.frame_buf,
                        &self.src_mac,
                        &self.gateway_mac,
                        src_ip,
                        dst_ip,
                        src_port,
                        dst_port,
                        &seg_buf[..n],
                        tos,
                    )
                    .map_err(|_| tx::Error::UndersizedBuffer)?;
                    self.pending.push(TxDatagram {
                        frame: self.frame_buf.clone(),
                    });
                    gso_offset += n;
                    total_len += n;
                    segment_count += 1;
                }
                _ => break,
            }
        }

        Ok(tx::Outcome {
            len: total_len,
            index,
        })
    }

    fn capacity(&self) -> usize {
        self.capacity.saturating_sub(self.pending.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecn::extract_ecn;
    use dpdk_udp::ETH_HEADER_LEN;
    use s2n_quic_core::inet::{ExplicitCongestionNotification, IpV4Address};
    use s2n_quic_core::io::tx::{self, Queue as _};
    use s2n_quic_core::path;
    use std::time::Duration;

    /// Test message that produces a single segment.
    struct SingleSegmentMessage {
        handle: DpdkPathHandle,
        ecn: ExplicitCongestionNotification,
        payload: Vec<u8>,
    }

    impl tx::Message for SingleSegmentMessage {
        type Handle = DpdkPathHandle;

        fn path_handle(&self) -> &Self::Handle {
            &self.handle
        }

        fn ecn(&mut self) -> ExplicitCongestionNotification {
            self.ecn
        }

        fn delay(&mut self) -> Duration {
            Duration::ZERO
        }

        fn ipv6_flow_label(&mut self) -> u32 {
            0
        }

        fn can_gso(&self, _segment_len: usize, _segment_count: usize) -> bool {
            false
        }

        fn write_payload(
            &mut self,
            buffer: tx::PayloadBuffer,
            gso_offset: usize,
        ) -> Result<usize, tx::Error> {
            if gso_offset >= self.payload.len() {
                return Ok(0);
            }
            let remaining = &self.payload[gso_offset..];
            // Safety: we control this test buffer
            let buf = unsafe { buffer.into_mut_slice() };
            let len = remaining.len().min(buf.len());
            buf[..len].copy_from_slice(&remaining[..len]);
            Ok(len)
        }
    }

    /// Test message that supports GSO segmentation.
    struct GsoMessage {
        handle: DpdkPathHandle,
        ecn: ExplicitCongestionNotification,
        payload: Vec<u8>,
        segment_size: usize,
        max_segments: usize,
    }

    impl tx::Message for GsoMessage {
        type Handle = DpdkPathHandle;

        fn path_handle(&self) -> &Self::Handle {
            &self.handle
        }

        fn ecn(&mut self) -> ExplicitCongestionNotification {
            self.ecn
        }

        fn delay(&mut self) -> Duration {
            Duration::ZERO
        }

        fn ipv6_flow_label(&mut self) -> u32 {
            0
        }

        fn can_gso(&self, _segment_len: usize, segment_count: usize) -> bool {
            segment_count <= self.max_segments
                && self.payload.len() > self.segment_size
        }

        fn write_payload(
            &mut self,
            buffer: tx::PayloadBuffer,
            gso_offset: usize,
        ) -> Result<usize, tx::Error> {
            if gso_offset >= self.payload.len() {
                return Ok(0);
            }
            let remaining = &self.payload[gso_offset..];
            let buf = unsafe { buffer.into_mut_slice() };
            let len = remaining.len().min(buf.len()).min(self.segment_size);
            buf[..len].copy_from_slice(&remaining[..len]);
            Ok(len)
        }
    }

    fn test_handle() -> DpdkPathHandle {
        let remote = path::RemoteAddress::from(IpV4Address::from([10, 0, 0, 2]).with_port(4433));
        let local = path::LocalAddress::from(IpV4Address::from([10, 0, 0, 1]).with_port(5000));
        let mut h = DpdkPathHandle::from_remote_address(remote);
        h.set_local_address(local);
        h
    }

    #[test]
    fn push_single_segment_produces_one_frame() {
        let mut queue = DpdkTxQueue::new(
            "10.0.0.1:5000".parse().unwrap(),
            32,
            [0x11; 6],
            [0x22; 6],
        );

        let msg = SingleSegmentMessage {
            handle: test_handle(),
            ecn: ExplicitCongestionNotification::NotEct,
            payload: b"hello quic".to_vec(),
        };

        let outcome = queue.push(msg).unwrap();
        assert_eq!(outcome.len, 10); // "hello quic".len()
        assert_eq!(outcome.index, 0);
        assert_eq!(queue.drain().count(), 1);
    }

    #[test]
    fn gso_segmentation_produces_multiple_frames() {
        let mut queue = DpdkTxQueue::new(
            "10.0.0.1:5000".parse().unwrap(),
            32,
            [0x11; 6],
            [0x22; 6],
        );

        // 300 bytes of payload with 100-byte segments -> 3 frames
        let msg = GsoMessage {
            handle: test_handle(),
            ecn: ExplicitCongestionNotification::Ect0,
            payload: vec![0xAB; 300],
            segment_size: 100,
            max_segments: 4,
        };

        let outcome = queue.push(msg).unwrap();
        assert_eq!(outcome.len, 300);
        assert_eq!(outcome.index, 0);

        let frames: Vec<_> = queue.drain().collect();
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn capacity_decreases_after_push() {
        let mut queue = DpdkTxQueue::new(
            "10.0.0.1:5000".parse().unwrap(),
            4,
            [0x11; 6],
            [0x22; 6],
        );

        assert_eq!(queue.capacity(), 4);

        let msg = SingleSegmentMessage {
            handle: test_handle(),
            ecn: ExplicitCongestionNotification::NotEct,
            payload: b"data".to_vec(),
        };
        queue.push(msg).unwrap();
        assert_eq!(queue.capacity(), 3);
    }

    #[test]
    fn drain_empties_queue() {
        let mut queue = DpdkTxQueue::new(
            "10.0.0.1:5000".parse().unwrap(),
            32,
            [0x11; 6],
            [0x22; 6],
        );

        for _ in 0..3 {
            let msg = SingleSegmentMessage {
                handle: test_handle(),
                ecn: ExplicitCongestionNotification::NotEct,
                payload: b"pkt".to_vec(),
            };
            queue.push(msg).unwrap();
        }

        assert_eq!(queue.capacity(), 29);
        let _: Vec<_> = queue.drain().collect();
        assert_eq!(queue.capacity(), 32);
    }

    #[test]
    fn ecn_tos_byte_correct_in_outgoing_frame() {
        let mut queue = DpdkTxQueue::new(
            "10.0.0.1:5000".parse().unwrap(),
            32,
            [0x11; 6],
            [0x22; 6],
        );

        for ecn in [
            ExplicitCongestionNotification::NotEct,
            ExplicitCongestionNotification::Ect1,
            ExplicitCongestionNotification::Ect0,
            ExplicitCongestionNotification::Ce,
        ] {
            let msg = SingleSegmentMessage {
                handle: test_handle(),
                ecn,
                payload: b"ecn test".to_vec(),
            };
            queue.push(msg).unwrap();
        }

        let frames: Vec<_> = queue.drain().collect();
        assert_eq!(frames.len(), 4);

        // Verify TOS byte at ETH_HEADER_LEN + 1
        assert_eq!(extract_ecn(frames[0].frame[ETH_HEADER_LEN + 1]), ExplicitCongestionNotification::NotEct);
        assert_eq!(extract_ecn(frames[1].frame[ETH_HEADER_LEN + 1]), ExplicitCongestionNotification::Ect1);
        assert_eq!(extract_ecn(frames[2].frame[ETH_HEADER_LEN + 1]), ExplicitCongestionNotification::Ect0);
        assert_eq!(extract_ecn(frames[3].frame[ETH_HEADER_LEN + 1]), ExplicitCongestionNotification::Ce);
    }

    #[test]
    fn at_capacity_returns_error() {
        let mut queue = DpdkTxQueue::new(
            "10.0.0.1:5000".parse().unwrap(),
            2,
            [0x11; 6],
            [0x22; 6],
        );

        for _ in 0..2 {
            let msg = SingleSegmentMessage {
                handle: test_handle(),
                ecn: ExplicitCongestionNotification::NotEct,
                payload: b"x".to_vec(),
            };
            queue.push(msg).unwrap();
        }

        let msg = SingleSegmentMessage {
            handle: test_handle(),
            ecn: ExplicitCongestionNotification::NotEct,
            payload: b"overflow".to_vec(),
        };
        let result = queue.push(msg);
        assert!(matches!(result, Err(tx::Error::AtCapacity)));
    }
}
