//! GSO segmentation integration test.
//!
//! Pushes a message with payload > MSS and `can_gso` returning true,
//! verifies the TX queue produces the correct number of frames,
//! each frame payload is at most segment_len bytes, and all payload
//! bytes are accounted for across segments.

use dpdk_stdlib_quic::path_handle::DpdkPathHandle;
use dpdk_stdlib_quic::tx::DpdkTxQueue;
use dpdk_udp::{ETH_HEADER_LEN, IPV4_HEADER_LEN, UDP_HEADER_LEN};
use s2n_quic_core::inet::{ExplicitCongestionNotification, IpV4Address};
use s2n_quic_core::io::tx::{self, Queue as _};
use s2n_quic_core::path::{self, Handle as _};
use std::time::Duration;

const TOTAL_HEADER_LEN: usize = ETH_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN;

/// Test message supporting GSO segmentation.
struct GsoTestMessage {
    handle: DpdkPathHandle,
    payload: Vec<u8>,
    segment_size: usize,
    max_segments: usize,
}

impl tx::Message for GsoTestMessage {
    type Handle = DpdkPathHandle;

    fn path_handle(&self) -> &Self::Handle {
        &self.handle
    }

    fn ecn(&mut self) -> ExplicitCongestionNotification {
        ExplicitCongestionNotification::Ect0
    }

    fn delay(&mut self) -> Duration {
        Duration::ZERO
    }

    fn ipv6_flow_label(&mut self) -> u32 {
        0
    }

    fn can_gso(&self, _segment_len: usize, segment_count: usize) -> bool {
        segment_count <= self.max_segments && self.payload.len() > self.segment_size
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

fn make_path_handle() -> DpdkPathHandle {
    let remote = path::RemoteAddress::from(IpV4Address::from([10, 0, 0, 2]).with_port(4433));
    let local = path::LocalAddress::from(IpV4Address::from([10, 0, 0, 1]).with_port(5000));
    let mut h = DpdkPathHandle::from_remote_address(remote);
    h.set_local_address(local);
    h
}

#[test]
fn gso_correct_frame_count() {
    let segment_size = 100;
    let total_payload = 350; // → 4 segments (100 + 100 + 100 + 50)
    let payload: Vec<u8> = (0..total_payload).map(|i| (i % 256) as u8).collect();

    let mut queue = DpdkTxQueue::new(
        "10.0.0.1:5000".parse().unwrap(),
        64,
        [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
    );

    let msg = GsoTestMessage {
        handle: make_path_handle(),
        payload: payload.clone(),
        segment_size,
        max_segments: 10,
    };

    let outcome = queue.push(msg).unwrap();
    assert_eq!(outcome.len, total_payload);

    let frames: Vec<_> = queue.drain().collect();
    let expected_frames = (total_payload + segment_size - 1) / segment_size;
    assert_eq!(
        frames.len(),
        expected_frames,
        "payload={total_payload} segment_size={segment_size} → expected {expected_frames} frames"
    );
}

#[test]
fn gso_payload_boundaries() {
    let segment_size = 200;
    let total_payload = 500; // → 3 segments (200 + 200 + 100)
    let payload: Vec<u8> = (0..total_payload).map(|i| (i % 256) as u8).collect();

    let mut queue = DpdkTxQueue::new(
        "10.0.0.1:5000".parse().unwrap(),
        64,
        [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
    );

    let msg = GsoTestMessage {
        handle: make_path_handle(),
        payload: payload.clone(),
        segment_size,
        max_segments: 10,
    };

    queue.push(msg).unwrap();
    let frames: Vec<_> = queue.drain().collect();

    // Each frame has: ETH(14) + IP(20) + UDP(8) + payload
    let mut reassembled = Vec::new();
    for (i, frame) in frames.iter().enumerate() {
        let udp_payload = &frame.frame[TOTAL_HEADER_LEN..];
        // Each segment except the last must be exactly segment_size
        if i < frames.len() - 1 {
            assert_eq!(
                udp_payload.len(),
                segment_size,
                "segment {i} should be exactly {segment_size} bytes"
            );
        } else {
            // Last segment may be shorter
            assert!(
                udp_payload.len() <= segment_size,
                "last segment should be ≤ {segment_size} bytes, got {}",
                udp_payload.len()
            );
        }
        reassembled.extend_from_slice(udp_payload);
    }

    // All payload bytes accounted for
    assert_eq!(
        reassembled.len(),
        total_payload,
        "reassembled payload length must equal original"
    );
    assert_eq!(
        reassembled, payload,
        "reassembled payload must match original byte-for-byte"
    );
}

#[test]
fn gso_single_segment_when_payload_fits() {
    let segment_size = 1000;
    let total_payload = 500; // fits in one segment

    let mut queue = DpdkTxQueue::new(
        "10.0.0.1:5000".parse().unwrap(),
        64,
        [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
    );

    let msg = GsoTestMessage {
        handle: make_path_handle(),
        payload: vec![0xAA; total_payload],
        segment_size,
        max_segments: 10,
    };

    queue.push(msg).unwrap();
    let frames: Vec<_> = queue.drain().collect();

    // payload (500) <= segment_size (1000), can_gso returns false
    // → only 1 frame
    assert_eq!(frames.len(), 1);
    let udp_payload = &frames[0].frame[TOTAL_HEADER_LEN..];
    assert_eq!(udp_payload.len(), total_payload);
}
