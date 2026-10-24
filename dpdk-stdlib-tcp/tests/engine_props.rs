//! Property tests for TCP engine (tasks 6.1, 6.2, 6.3).
//!
//! Properties 11–14 (engine state machine), 15–18, 22, 25 (congestion control).

use std::net::SocketAddr;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use proptest::prelude::*;

use dpdk_stdlib_tcp::clock::{Clock, MockClock};
use dpdk_stdlib_tcp::codec::{parse_tcp_packet, ParsedTcpSegment, TcpFlags, TcpOptions};
use dpdk_stdlib_tcp::congestion::CongestionState;
use dpdk_stdlib_tcp::contract::{
    oneshot_channel, CommandSender, ConnectionHandle, EngineCommand, EngineWakeup,
};
use dpdk_stdlib_tcp::engine::{EngineConfig, TcpEngine};
use dpdk_stdlib_tcp::error::TcpError;
use dpdk_stdlib_tcp::seq::SeqNum;
use dpdk_stdlib_tcp::state::{FourTuple, TcpState};

// --- Helpers ---

fn make_engine() -> (TcpEngine, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new());
    let config = EngineConfig::default();
    let engine = TcpEngine::new(clock.clone(), config);
    (engine, clock)
}

fn make_engine_with_config(config: EngineConfig) -> (TcpEngine, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new());
    let engine = TcpEngine::new(clock.clone(), config);
    (engine, clock)
}

fn make_handle(four_tuple: FourTuple) -> Arc<ConnectionHandle> {
    let (tx, _rx) = mpsc::channel();
    let wakeup = Arc::new(EngineWakeup::new());
    let cmd_tx = CommandSender::new(tx, wakeup);
    Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx, four_tuple))
}

/// Complete a three-way handshake returning (four_tuple, handle).
fn setup_established(engine: &mut TcpEngine) -> (FourTuple, Arc<ConnectionHandle>) {
    setup_established_with_wscale(engine, 0)
}

fn setup_established_with_wscale(
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

/// Send data from the app side into the tx_ring so the engine can segment it.
fn write_app_data(handle: &ConnectionHandle, data: &[u8]) {
    handle.tx_ring.write(data);
}

// === Property 11: Timer-driven segment generation ===
// Any expired timer produces at least one outbound segment without app call.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_timer_driven_segment_generation(
        data_len in 1usize..=1460,
    ) {
        let (mut engine, clock) = make_engine();
        let (_four_tuple, handle) = setup_established(&mut engine);

        // Write data into tx_ring and let engine segment it
        let data: Vec<u8> = (0..data_len).map(|i| (i % 256) as u8).collect();
        write_app_data(&handle, &data);

        // First tick: engine segments and sends
        let now = clock.now();
        let frames = engine.on_tick(now);
        prop_assert!(!frames.is_empty(), "First tick should produce segments");

        // Advance clock past RTO (initial 1s)
        clock.advance(Duration::from_millis(1100));
        let now = clock.now();

        // No app call — just on_tick. RTO expired → retransmit segment.
        let frames = engine.on_tick(now);
        prop_assert!(
            !frames.is_empty(),
            "Expired RTO timer must produce outbound segment without app call"
        );
    }

    #[test]
    fn prop_persist_timer_generates_probe(
        data_len in 1usize..=1460,
    ) {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established(&mut engine);

        // Set peer window to 0 via a segment
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        let zero_win = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt,
            flags: TcpFlags::ACK,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&zero_win);

        // Write data — can't send because window is 0
        let data: Vec<u8> = (0..data_len).map(|i| (i % 256) as u8).collect();
        write_app_data(&handle, &data);

        // Tick to arm persist timer
        let now = clock.now();
        engine.on_tick(now);

        // Advance past persist timeout (starts at RTO = 1s)
        clock.advance(Duration::from_millis(1100));
        let now = clock.now();

        // on_tick should generate a persist probe without any app call
        let frames = engine.on_tick(now);
        prop_assert!(
            !frames.is_empty(),
            "Persist timer must generate probe segment without app call"
        );
    }
}

// === Property 12: TIME_WAIT/FIN_WAIT_2 cleanup ===

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_time_wait_cleanup(extra_ms in 0u64..=5000) {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established(&mut engine);

        // Initiate close: send Shutdown(Write)
        engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: std::net::Shutdown::Write,
        });
        // Tick to flush FIN
        let now = clock.now();
        engine.on_tick(now);

        // Peer ACKs our FIN → FIN_WAIT_2
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let snd_nxt = tcb.snd_nxt;
        let rcv_nxt = tcb.rcv_nxt;
        let fin_ack = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt,
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&fin_ack);

        // Peer sends FIN → TIME_WAIT
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let peer_fin = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt,
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&peer_fin);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        prop_assert_eq!(tcb.state, TcpState::TimeWait);

        // Advance past 2*MSL (120s) + extra
        clock.advance(Duration::from_secs(120) + Duration::from_millis(extra_ms + 1));
        let now = clock.now();
        engine.on_tick(now);

        // TCB should be removed (CLOSED)
        prop_assert!(
            !engine.tcbs.contains_key(&four_tuple),
            "TCB must be removed after TIME_WAIT expires"
        );
    }

    #[test]
    fn prop_fin_wait2_cleanup(extra_ms in 0u64..=5000) {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established(&mut engine);

        // Initiate close
        engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: std::net::Shutdown::Write,
        });
        let now = clock.now();
        engine.on_tick(now);

        // Peer ACKs our FIN → FIN_WAIT_2
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let snd_nxt = tcb.snd_nxt;
        let rcv_nxt = tcb.rcv_nxt;
        let fin_ack = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt,
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&fin_ack);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        prop_assert_eq!(tcb.state, TcpState::FinWait2);

        // Peer never sends FIN. Advance past FIN_WAIT_2 timeout (60s).
        clock.advance(Duration::from_secs(60) + Duration::from_millis(extra_ms + 1));
        let now = clock.now();
        engine.on_tick(now);

        // TCB must be cleaned up
        prop_assert!(
            !engine.tcbs.contains_key(&four_tuple),
            "TCB must be freed after FIN_WAIT_2 timeout"
        );
    }
}

// === Property 13: Resource limit enforcement ===
// max_tcbs/accept_backlog exceeded → RST

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_max_tcbs_produces_rst(num_conns in 1usize..=5) {
        let config = EngineConfig {
            max_tcbs: num_conns,
            ..Default::default()
        };
        let (mut engine, _clock) = make_engine_with_config(config);

        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let (resp_tx, _) = oneshot_channel::<Result<(), TcpError>>();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });

        // Fill up to max_tcbs
        for i in 0..num_conns {
            let client: SocketAddr = format!("10.0.0.2:{}", 3000 + i).parse().unwrap();
            let syn = ParsedTcpSegment {
                src: client,
                dst: listen_addr,
                seq: SeqNum(1000 + i as u32),
                ack: SeqNum(0),
                flags: TcpFlags::SYN,
                window: 65535,
                options: TcpOptions {
                    mss: Some(1460),
                    window_scale: Some(7),
                    sack_permitted: true,
                    timestamps: Some((0, 0)),
                    ..Default::default()
                },
                payload: Vec::new(),
            };
            let frames = engine.on_segment_with_macs(
                &syn,
                [0x02, 0, 0, 0, 0, 2],
                [0x02, 0, 0, 0, 0, 1],
            );
            prop_assert_eq!(frames.len(), 1);
            let resp = parse_tcp_packet(&frames[0]).unwrap();
            prop_assert!(resp.flags.contains(TcpFlags::SYN));
            prop_assert!(resp.flags.contains(TcpFlags::ACK));
        }

        // One more connection should get RST
        let overflow_client: SocketAddr = "10.0.0.2:9999".parse().unwrap();
        let syn = ParsedTcpSegment {
            src: overflow_client,
            dst: listen_addr,
            seq: SeqNum(5000),
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: 65535,
            options: TcpOptions {
                mss: Some(1460),
                window_scale: Some(7),
                sack_permitted: true,
                timestamps: Some((0, 0)),
                ..Default::default()
            },
            payload: Vec::new(),
        };
        let frames = engine.on_segment_with_macs(
            &syn,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );
        prop_assert_eq!(frames.len(), 1);
        let resp = parse_tcp_packet(&frames[0]).unwrap();
        prop_assert!(
            resp.flags.contains(TcpFlags::RST),
            "Exceeding max_tcbs must produce RST"
        );
    }

    #[test]
    fn prop_backlog_exceeded_produces_rst(backlog in 1usize..=4) {
        let config = EngineConfig {
            max_tcbs: 1024,
            ..Default::default()
        };
        let (mut engine, _clock) = make_engine_with_config(config);

        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let (resp_tx, _) = oneshot_channel::<Result<(), TcpError>>();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog,
            response: resp_tx,
        });

        // Fill backlog
        for i in 0..backlog {
            let client: SocketAddr = format!("10.0.0.2:{}", 4000 + i).parse().unwrap();
            let syn = ParsedTcpSegment {
                src: client,
                dst: listen_addr,
                seq: SeqNum(2000 + i as u32),
                ack: SeqNum(0),
                flags: TcpFlags::SYN,
                window: 65535,
                options: TcpOptions {
                    mss: Some(1460),
                    window_scale: Some(7),
                    sack_permitted: true,
                    timestamps: Some((0, 0)),
                    ..Default::default()
                },
                payload: Vec::new(),
            };
            let frames = engine.on_segment_with_macs(
                &syn,
                [0x02, 0, 0, 0, 0, 2],
                [0x02, 0, 0, 0, 0, 1],
            );
            prop_assert_eq!(frames.len(), 1);
            let resp = parse_tcp_packet(&frames[0]).unwrap();
            prop_assert!(resp.flags.contains(TcpFlags::SYN | TcpFlags::ACK));
        }

        // One more SYN should get RST (backlog full)
        let overflow: SocketAddr = "10.0.0.2:8888".parse().unwrap();
        let syn = ParsedTcpSegment {
            src: overflow,
            dst: listen_addr,
            seq: SeqNum(9000),
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: 65535,
            options: TcpOptions {
                mss: Some(1460),
                window_scale: Some(7),
                sack_permitted: true,
                timestamps: Some((0, 0)),
                ..Default::default()
            },
            payload: Vec::new(),
        };
        let frames = engine.on_segment_with_macs(
            &syn,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );
        prop_assert_eq!(frames.len(), 1);
        let resp = parse_tcp_packet(&frames[0]).unwrap();
        prop_assert!(
            resp.flags.contains(TcpFlags::RST),
            "Exceeding accept backlog must produce RST"
        );
    }
}

// === Property 14: RST validation per RFC 5961 ===
// Exact seq → abort. In-window non-exact → challenge ACK. Out-of-window → drop.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_rst_exact_seq_aborts(_data_len in 1usize..=100) {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;

        // RST with exact seq == rcv_nxt → abort
        let rst = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: SeqNum(0),
            flags: TcpFlags::RST,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let _frames = engine.on_segment(&rst);

        // Connection should be aborted (removed)
        prop_assert!(
            !engine.tcbs.contains_key(&four_tuple),
            "Exact-seq RST must abort the connection"
        );
        // Error should be latched
        let err = handle.error.lock().unwrap();
        prop_assert!(err.is_some());
    }

    #[test]
    fn prop_rst_in_window_non_exact_sends_challenge_ack(offset in 1u32..=1000) {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;

        // RST with in-window but non-exact seq → challenge ACK
        let rst = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt.add(offset),
            ack: SeqNum(0),
            flags: TcpFlags::RST,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&rst);

        // Connection should NOT be aborted
        prop_assert!(
            engine.tcbs.contains_key(&four_tuple),
            "In-window non-exact RST must NOT abort"
        );
        // Should send a challenge ACK
        prop_assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        prop_assert!(ack.flags.contains(TcpFlags::ACK));
        prop_assert!(!ack.flags.contains(TcpFlags::RST));
    }

    #[test]
    fn prop_rst_out_of_window_silently_dropped(offset in 70000u32..=u32::MAX / 2) {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;

        // RST with seq far outside receive window → silent drop
        let rst = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt.add(offset),
            ack: SeqNum(0),
            flags: TcpFlags::RST,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&rst);

        // Connection should NOT be aborted
        prop_assert!(
            engine.tcbs.contains_key(&four_tuple),
            "Out-of-window RST must be silently dropped"
        );
        // No response frames
        prop_assert_eq!(frames.len(), 0);
    }
}

// === Property 15: Flight-size invariant ===
// Unacked bytes (snd_nxt - snd_una) never exceed min(cwnd, rwnd).

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_flight_size_invariant(
        data_chunks in proptest::collection::vec(1usize..=2000, 1..=5),
    ) {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established(&mut engine);

        let now = clock.now();

        for chunk_size in &data_chunks {
            let data: Vec<u8> = (0..*chunk_size).map(|i| (i % 256) as u8).collect();
            write_app_data(&handle, &data);
            engine.on_tick(now);

            // Verify flight-size invariant after each tick
            if let Some(tcb) = engine.tcbs.get(&four_tuple) {
                let flight = tcb.flight_size();
                let effective_wnd = tcb.congestion.effective_window(tcb.snd_wnd);
                prop_assert!(
                    flight <= effective_wnd,
                    "Flight size {} exceeds effective window {} (cwnd={}, rwnd={})",
                    flight,
                    effective_wnd,
                    tcb.congestion.cwnd,
                    tcb.snd_wnd,
                );
            }
        }
    }
}

// === Property 16: Slow-start cwnd growth ===
// In slow-start (cwnd < ssthresh), each ACK increases cwnd by MSS.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_slow_start_cwnd_growth(num_acks in 1u32..=10) {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established(&mut engine);

        // Write enough data to send multiple segments
        let total_data = 1460 * (num_acks as usize + 2);
        let data: Vec<u8> = (0..total_data).map(|i| (i % 256) as u8).collect();
        write_app_data(&handle, &data);

        let now = clock.now();
        engine.on_tick(now);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let initial_cwnd = tcb.congestion.cwnd;
        let mss = tcb.effective_mss() as u32;
        let snd_una = tcb.snd_una;

        // Send ACKs advancing snd_una by MSS each
        for i in 0..num_acks {
            let ack_num = snd_una.add(mss * (i + 1));
            let ack_seg = ParsedTcpSegment {
                src: four_tuple.remote,
                dst: four_tuple.local,
                seq: engine.tcbs.get(&four_tuple).unwrap().rcv_nxt,
                ack: ack_num,
                flags: TcpFlags::ACK,
                window: 65535,
                options: TcpOptions::default(),
                payload: Vec::new(),
            };
            engine.on_segment(&ack_seg);
        }

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        // In slow-start: cwnd should increase by MSS per ACK
        let expected_cwnd = initial_cwnd + mss * num_acks;
        prop_assert_eq!(
            tcb.congestion.cwnd, expected_cwnd,
            "cwnd should grow by MSS per ACK in slow-start"
        );
    }
}

// === Property 17: Initial window formula ===
// IW = min(10*MSS, max(2*MSS, 14600)) for any MSS.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_initial_window_formula(mss in 64u16..=9000) {
        let iw = CongestionState::initial_window(mss);
        let mss32 = mss as u32;
        let expected = std::cmp::min(10 * mss32, std::cmp::max(2 * mss32, 14600));
        prop_assert_eq!(iw, expected);
    }
}

// === Property 18: Fast retransmit formula ===
// On 3 dup-ACKs: ssthresh = max(flight/2, 2*MSS), cwnd = ssthresh + 3*MSS.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_fast_retransmit_formula(
        flight_size in 2920u32..=100000,
        mss in 536u16..=9000,
    ) {
        let mut cs = CongestionState::new(mss);
        cs.cwnd = flight_size + 10000; // Ensure enough room
        let mss32 = mss as u32;

        cs.on_triple_dup_ack(flight_size, mss);

        let expected_ssthresh = std::cmp::max(flight_size / 2, 2 * mss32);
        let expected_cwnd = expected_ssthresh + 3 * mss32;

        prop_assert_eq!(cs.ssthresh, expected_ssthresh);
        prop_assert_eq!(cs.cwnd, expected_cwnd);
        prop_assert!(cs.in_recovery);
    }
}

// === Property 22: Partial ACK in recovery ===
// Partial ACK deflates cwnd by bytes acked, stays in recovery.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_partial_ack_in_recovery(
        flight_size in 5000u32..=50000,
        partial_bytes in 1u32..=2920,
    ) {
        let mss: u16 = 1460;
        let mut cs = CongestionState::new(mss);
        cs.cwnd = flight_size + 10000;

        // Enter recovery
        cs.on_triple_dup_ack(flight_size, mss);
        let cwnd_after_enter = cs.cwnd;
        prop_assert!(cs.in_recovery);

        // Partial ACK: acks some but not all data
        let acked = std::cmp::min(partial_bytes, cwnd_after_enter.saturating_sub(1));
        cs.on_partial_ack(acked, mss);

        // cwnd deflated by bytes acked
        prop_assert_eq!(cs.cwnd, cwnd_after_enter.saturating_sub(acked));
        // Must still be in recovery
        prop_assert!(cs.in_recovery, "Partial ACK must NOT exit recovery");
    }
}

// === Property 25: Persist-never-aborts ===
// Zero-window probes indefinitely without surfacing TimedOut.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_persist_never_aborts(probe_count in 5u32..=30) {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established(&mut engine);

        // Set peer window to 0
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;
        let zero_win = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt,
            flags: TcpFlags::ACK,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&zero_win);

        // Write data that can't be sent
        write_app_data(&handle, &[42u8; 100]);

        // Initial tick to arm persist
        let now = clock.now();
        engine.on_tick(now);

        // Fire persist timer repeatedly — should NEVER abort
        for _ in 0..probe_count {
            // Advance by 61s (exceeds max persist backoff cap of 60s)
            clock.advance(Duration::from_secs(61));
            let now = clock.now();
            engine.on_tick(now);

            // Connection must still exist and NOT be in error state
            prop_assert!(
                engine.tcbs.contains_key(&four_tuple),
                "Persist timer must NEVER abort the connection"
            );
            let err = handle.error.lock().unwrap();
            prop_assert!(
                err.is_none(),
                "Persist must not latch TimedOut"
            );
        }
    }
}
