//! Property tests for task 5.10: out-of-order reorder buffer.
//!
//! Property 10: OOO dup-ACK — OOO segments produce dup-ACK with ack_num == rcv_nxt.
//! Property 21: Reorder buffer soundness — OOO delivery (including sequence-number
//!              wrap-around) produces byte-identical output to in-order assembly.

use std::net::SocketAddr;
use std::sync::{mpsc, Arc};

use dpdk_stdlib_tcp::clock::MockClock;
use dpdk_stdlib_tcp::codec::{parse_tcp_packet, ParsedTcpSegment, TcpFlags, TcpOptions};
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

/// Set up an established connection with rcv_nxt starting at `start_seq`.
fn setup_established_at(
    engine: &mut TcpEngine,
    start_seq: u32,
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

    // SYN-ACK with specified start_seq as the peer's ISN
    let syn_ack = ParsedTcpSegment {
        src: remote,
        dst: local,
        seq: SeqNum(start_seq),
        ack: iss.add(1),
        flags: TcpFlags::SYN | TcpFlags::ACK,
        window: 65535,
        options: TcpOptions {
            mss: Some(1460),
            window_scale: Some(0),
            sack_permitted: true,
            timestamps: Some((0, 0)),
            ..Default::default()
        },
        payload: Vec::new(),
    };
    engine.on_segment(&syn_ack);

    // rcv_nxt is now start_seq + 1 (SYN consumes 1 seq)
    (four_tuple, handle)
}

// === Property 10: OOO dup-ACK correctness ===

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Any out-of-order segment (seq > rcv_nxt) produces a dup-ACK with
    /// ack_num == rcv_nxt (unchanged).
    #[test]
    fn prop_ooo_dup_ack_has_correct_ack_num(
        gap in 1u32..=10000,
        payload_len in 1usize..=1000,
    ) {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_at(&mut engine, 5000);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Send OOO segment (gap bytes ahead of rcv_nxt)
        let payload: Vec<u8> = (0..payload_len).map(|i| (i % 256) as u8).collect();
        let ooo_seg = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt.add(gap),
            ack: snd_nxt,
            flags: TcpFlags::ACK | TcpFlags::PSH,
            window: 65535,
            options: TcpOptions::default(),
            payload,
        };
        let frames = engine.on_segment(&ooo_seg);

        // Must produce exactly one dup-ACK
        prop_assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        prop_assert!(ack.flags.contains(TcpFlags::ACK));
        // dup-ACK: ack_num must equal rcv_nxt (NOT advanced)
        prop_assert_eq!(ack.ack, rcv_nxt);

        // rcv_nxt must not advance
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        prop_assert_eq!(tcb.rcv_nxt, rcv_nxt);
    }
}

// === Property 21: Reorder buffer soundness ===

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Segments delivered in any permutation produce the same byte-stream
    /// as in-order delivery. Tests with sequence number values that may
    /// wrap around u32::MAX.
    #[test]
    fn prop_reorder_buffer_soundness(
        start_seq in prop::num::u32::ANY,
        seg_count in 2usize..=6,
        seg_size in 1usize..=200,
        permutation_seed in prop::num::u64::ANY,
    ) {
        // Build the expected data (in-order byte stream)
        let total_len = seg_count * seg_size;
        let expected: Vec<u8> = (0..total_len).map(|i| (i % 256) as u8).collect();

        // Generate segment payloads
        let segments: Vec<Vec<u8>> = (0..seg_count)
            .map(|i| expected[i * seg_size..(i + 1) * seg_size].to_vec())
            .collect();

        // Create a permutation (Fisher-Yates with seed)
        let mut indices: Vec<usize> = (0..seg_count).collect();
        let mut rng = permutation_seed;
        for i in (1..seg_count).rev() {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let j = (rng >> 33) as usize % (i + 1);
            indices.swap(i, j);
        }

        // Deliver segments in permuted order
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_at(&mut engine, start_seq);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let initial_rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        for &idx in &indices {
            let seq = initial_rcv_nxt.add((idx * seg_size) as u32);
            let seg = ParsedTcpSegment {
                src: four_tuple.remote,
                dst: four_tuple.local,
                seq,
                ack: snd_nxt,
                flags: TcpFlags::ACK | TcpFlags::PSH,
                window: 65535,
                options: TcpOptions::default(),
                payload: segments[idx].clone(),
            };
            engine.on_segment(&seg);
        }

        // After all segments delivered, rcv_nxt should advance by total_len
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        prop_assert_eq!(tcb.rcv_nxt, initial_rcv_nxt.add(total_len as u32));

        // Reorder buffer should be empty
        prop_assert!(tcb.reorder_buffer.is_empty());

        // Data in rx_ring must match the expected in-order byte stream
        let mut buf = vec![0u8; total_len + 1];
        let n = handle.rx_ring.read(&mut buf);
        prop_assert_eq!(n, total_len);
        prop_assert_eq!(&buf[..n], &expected[..]);
    }
}
