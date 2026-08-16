//! Property tests for task 5.9: established in-order data + ACK.
//!
//! Property 9: In-order ACK correctness — ack_num matches rcv_nxt + payload_len.
//! Property 23: Window scaling round-trip — encode+decode bounds effective send window.

use std::net::SocketAddr;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use dpdk_stdlib_tcp::codec::{parse_tcp_packet, ParsedTcpSegment, TcpFlags, TcpOptions};
use dpdk_stdlib_tcp::clock::MockClock;
use dpdk_stdlib_tcp::contract::{
    oneshot_channel, CommandSender, ConnectionHandle, EngineCommand, EngineWakeup,
};
use dpdk_stdlib_tcp::engine::{EngineConfig, TcpEngine};
use dpdk_stdlib_tcp::seq::SeqNum;
use dpdk_stdlib_tcp::state::FourTuple;

use proptest::prelude::*;

fn make_engine() -> (TcpEngine, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new());
    let config = EngineConfig::default();
    let engine = TcpEngine::new(clock.clone(), config);
    (engine, clock)
}

fn make_handle(four_tuple: FourTuple) -> Arc<ConnectionHandle> {
    let (tx, _rx) = mpsc::channel();
    let wakeup = Arc::new(EngineWakeup::new());
    let cmd_tx = CommandSender::new(tx, wakeup);
    Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx, four_tuple))
}

/// Complete a three-way handshake with a given peer wscale, returning the four_tuple and handle.
fn setup_established(
    engine: &mut TcpEngine,
    peer_wscale: u8,
) -> (FourTuple, Arc<ConnectionHandle>) {
    let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
    let four_tuple = FourTuple { local, remote };
    let handle = make_handle(four_tuple);
    let (resp_tx, _resp_rx) = oneshot_channel();

    engine.on_command(EngineCommand::Connect {
        local,
        remote,
        src_mac: [0x02, 0, 0, 0, 0, 1],
        dst_mac: [0x02, 0, 0, 0, 0, 2],
        handle: handle.clone(),
        response: resp_tx,
    });

    let tcb = engine.tcbs.get(&four_tuple).unwrap();
    let iss = tcb.iss;

    // SYN-ACK with specified wscale
    let syn_ack = ParsedTcpSegment {
        src: remote,
        dst: local,
        seq: SeqNum(2000),
        ack: iss.add(1),
        flags: TcpFlags::SYN | TcpFlags::ACK,
        window: 65535,
        options: TcpOptions {
            mss: Some(1460),
            window_scale: Some(peer_wscale),
            sack_permitted: true,
            timestamps: Some((0, 0)),
            ..Default::default()
        },
        payload: Vec::new(),
    };
    engine.on_segment(&syn_ack);

    (four_tuple, handle)
}

// === Property 9: In-order ACK correctness ===
// With delayed-ACK: the engine defers ACKs for the first segment and sends
// on every-other-segment. Property 9 validates that the ACK ack_num is correct
// when it is finally sent (after the 2nd segment arrives).

proptest! {
    /// For any in-order payload (1..1460 bytes), the engine defers the first
    /// segment's ACK and sends a cumulative ACK after the second segment,
    /// with ack_num == rcv_nxt + payload_len * 2.
    #[test]
    fn prop_inorder_ack_correctness(payload_len in 1usize..=1460) {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established(&mut engine, 7);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // First segment: delayed (no ACK produced)
        let payload1: Vec<u8> = (0..payload_len).map(|i| (i % 256) as u8).collect();
        let seg1 = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt,
            flags: TcpFlags::ACK | TcpFlags::PSH,
            window: 65535,
            options: TcpOptions::default(),
            payload: payload1.clone(),
        };
        let frames = engine.on_segment(&seg1);
        prop_assert_eq!(frames.len(), 0); // Delayed-ACK: deferred

        // Second segment: every-other-segment rule → immediate ACK
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt2 = tcb.rcv_nxt;
        let payload2: Vec<u8> = (0..payload_len).map(|i| ((i + payload_len) % 256) as u8).collect();
        let seg2 = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt2,
            ack: snd_nxt,
            flags: TcpFlags::ACK | TcpFlags::PSH,
            window: 65535,
            options: TcpOptions::default(),
            payload: payload2.clone(),
        };
        let frames = engine.on_segment(&seg2);

        // Must produce exactly one ACK
        prop_assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        prop_assert!(ack.flags.contains(TcpFlags::ACK));

        // ACK number covers both segments
        let expected_ack = rcv_nxt.add((payload_len * 2) as u32);
        prop_assert_eq!(ack.ack, expected_ack);

        // All data must be in rx_ring
        let mut buf = vec![0u8; payload_len * 2 + 1];
        let n = handle.rx_ring.read(&mut buf);
        prop_assert_eq!(n, payload_len * 2);
        prop_assert_eq!(&buf[..payload_len], &payload1[..]);
        prop_assert_eq!(&buf[payload_len..payload_len*2], &payload2[..]);
    }

    /// Multiple consecutive in-order segments produce correct cumulative ACKs
    /// following the delayed-ACK every-other-segment rule.
    #[test]
    fn prop_inorder_multiple_segments(
        seg_count in 2usize..=8,
        seg_size in 1usize..=500,
    ) {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established(&mut engine, 7);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let initial_rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        let mut expected_rcv_nxt = initial_rcv_nxt;
        let mut total_payload = Vec::new();
        let mut ack_count = 0u32;

        for i in 0..seg_count {
            let payload: Vec<u8> = (0..seg_size).map(|j| ((i * seg_size + j) % 256) as u8).collect();
            total_payload.extend_from_slice(&payload);

            let seg = ParsedTcpSegment {
                src: four_tuple.remote,
                dst: four_tuple.local,
                seq: expected_rcv_nxt,
                ack: snd_nxt,
                flags: TcpFlags::ACK | TcpFlags::PSH,
                window: 65535,
                options: TcpOptions::default(),
                payload: payload.clone(),
            };
            let frames = engine.on_segment(&seg);
            expected_rcv_nxt = expected_rcv_nxt.add(seg_size as u32);

            // With delayed-ACK: ACK on every 2nd segment (i=1,3,5,...)
            if (i + 1) % 2 == 0 {
                prop_assert_eq!(frames.len(), 1);
                let ack = parse_tcp_packet(&frames[0]).unwrap();
                prop_assert_eq!(ack.ack, expected_rcv_nxt);
                ack_count += 1;
            } else {
                prop_assert_eq!(frames.len(), 0);
            }
        }

        // Verify we got the expected number of ACKs
        prop_assert_eq!(ack_count, (seg_count / 2) as u32);

        // All data should be readable from rx_ring
        let mut buf = vec![0u8; total_payload.len() + 1];
        let n = handle.rx_ring.read(&mut buf);
        prop_assert_eq!(n, total_payload.len());
        prop_assert_eq!(&buf[..n], &total_payload[..]);
    }
}

// === Property 23: Window scaling encoding round-trip ===

proptest! {
    /// For any window value and scale factor, encoding (right-shift by rcv_scale)
    /// then decoding (left-shift by snd_scale) correctly bounds the effective
    /// send window to the intended receiver buffer size.
    #[test]
    fn prop_window_scaling_roundtrip(
        raw_window in 0u32..=65535,
        scale in 0u8..=14,
    ) {
        // Encode: peer encodes their window with their scale factor
        // The raw window in the TCP header = actual_window >> scale
        // Decode: we apply snd_scale = peer's scale to get actual window
        let encoded = (raw_window >> scale) as u16;
        let decoded = (encoded as u32) << scale;

        // The decoded window should never exceed the original raw window
        // (truncation from right-shift means we underestimate, never overestimate)
        prop_assert!(decoded <= raw_window);

        // The error is bounded by (1 << scale) - 1
        let max_error = if scale > 0 { (1u32 << scale) - 1 } else { 0 };
        prop_assert!(raw_window - decoded <= max_error);
    }

    /// End-to-end: a peer's window advertisement with a given scale is correctly
    /// recorded in the TCB's snd_wnd during established data exchange.
    #[test]
    fn prop_window_scaling_in_established(
        peer_window in 1u16..=65535,
        scale in 0u8..=14,
    ) {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established(&mut engine, scale);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Peer sends data with updated window
        let seg = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt,
            flags: TcpFlags::ACK | TcpFlags::PSH,
            window: peer_window,
            options: TcpOptions::default(),
            payload: b"x".to_vec(),
        };
        engine.on_segment(&seg);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        // snd_wnd should be peer_window << scale
        let expected = (peer_window as u32) << scale;
        prop_assert_eq!(tcb.snd_wnd, expected);
    }
}
