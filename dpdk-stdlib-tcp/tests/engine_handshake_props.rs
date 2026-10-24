//! Property tests for TCP engine state machine (task 5.8).
//!
//! Property 8: State machine validity — any valid event sequence from any
//! initial state produces one of the 11 defined TcpState values.

use proptest::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dpdk_stdlib_tcp::clock::MockClock;
use dpdk_stdlib_tcp::codec::{ParsedTcpSegment, TcpFlags, TcpOptions};
use dpdk_stdlib_tcp::contract::{oneshot_channel, CommandSender, EngineCommand, EngineWakeup};
use dpdk_stdlib_tcp::engine::{EngineConfig, TcpEngine};
use dpdk_stdlib_tcp::seq::SeqNum;
use dpdk_stdlib_tcp::state::{FourTuple, TcpState};

/// All valid TcpState values.
const ALL_STATES: [TcpState; 11] = [
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

/// Generate an arbitrary TCP segment with valid flag combinations.
fn arb_segment(src: SocketAddr, dst: SocketAddr) -> impl Strategy<Value = ParsedTcpSegment> {
    (
        any::<u32>(),       // seq
        any::<u32>(),       // ack
        prop_oneof![
            Just(TcpFlags::SYN),
            Just(TcpFlags::SYN | TcpFlags::ACK),
            Just(TcpFlags::ACK),
            Just(TcpFlags::RST | TcpFlags::ACK),
            Just(TcpFlags::FIN | TcpFlags::ACK),
        ],
        any::<u16>(), // window
        proptest::option::of(1u16..=9000u16), // mss
        proptest::option::of(0u8..=14u8),     // wscale
    )
        .prop_map(move |(seq, ack, flags, window, mss, wscale)| ParsedTcpSegment {
            src,
            dst,
            seq: SeqNum(seq),
            ack: SeqNum(ack),
            flags,
            window,
            options: TcpOptions {
                mss,
                window_scale: wscale,
                sack_permitted: mss.is_some(),
                timestamps: Some((0, 0)),
                ..Default::default()
            },
            payload: Vec::new(),
        })
}

/// Generate a sequence of segment events.
fn arb_event_sequence() -> impl Strategy<Value = Vec<ParsedTcpSegment>> {
    let src: SocketAddr = "10.0.0.2:5000".parse().unwrap();
    let dst: SocketAddr = "10.0.0.1:80".parse().unwrap();
    proptest::collection::vec(arb_segment(src, dst), 1..=10)
}

proptest! {
    /// Property 8: After any sequence of valid TCP events, every TCB in the engine
    /// has a state that is one of the 11 defined TcpState values.
    #[test]
    fn state_machine_always_valid(events in arb_event_sequence()) {
        let clock = Arc::new(MockClock::new());
        let config = EngineConfig::default();
        let mut engine = TcpEngine::new(clock.clone(), config);

        // Set up a listener so SYN events can create TCBs
        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let (resp_tx, _resp_rx) = oneshot_channel::<Result<(), dpdk_stdlib_tcp::error::TcpError>>();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });

        // Also set up an active connection so SYN-ACK/RST events have a target
        let local: SocketAddr = "10.0.0.1:6000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let four_tuple = FourTuple { local, remote };
        let (tx, _rx) = std::sync::mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let handle = Arc::new(dpdk_stdlib_tcp::contract::ConnectionHandle::new(
            65536, 65536, cmd_tx, four_tuple,
        ));
        let (resp_tx, _resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Connect {
            local,
            remote,
            src_mac: [0x02, 0, 0, 0, 0, 1],
            dst_mac: [0x02, 0, 0, 0, 0, 2],
            handle,
            response: resp_tx,
        });

        // Feed events
        for seg in &events {
            let _ = engine.on_segment_with_macs(
                seg,
                [0x02, 0, 0, 0, 0, 2],
                [0x02, 0, 0, 0, 0, 1],
            );
        }

        // Verify all TCB states are valid
        for tcb in engine.tcbs.values() {
            prop_assert!(
                ALL_STATES.contains(&tcb.state),
                "TCB has invalid state: {:?}",
                tcb.state
            );
        }
    }

    /// Property: SYN on a listening port always results in SYN_RECEIVED or RST (resource limits).
    #[test]
    fn syn_produces_syn_received_or_rst(
        client_port in 1024u16..65535,
        seq in any::<u32>(),
        mss in 100u16..9000u16,
        wscale in 0u8..14u8,
    ) {
        let clock = Arc::new(MockClock::new());
        let config = EngineConfig::default();
        let mut engine = TcpEngine::new(clock, config);

        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let (resp_tx, _) = oneshot_channel::<Result<(), dpdk_stdlib_tcp::error::TcpError>>();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });

        let client: SocketAddr = format!("10.0.0.2:{}", client_port).parse().unwrap();
        let syn = ParsedTcpSegment {
            src: client,
            dst: listen_addr,
            seq: SeqNum(seq),
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: 65535,
            options: TcpOptions {
                mss: Some(mss),
                window_scale: Some(wscale),
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

        // Should produce exactly one response frame
        prop_assert_eq!(frames.len(), 1);

        // The response should be a SYN-ACK
        let parsed = dpdk_stdlib_tcp::parse_tcp_packet(&frames[0]).unwrap();
        prop_assert!(parsed.flags.contains(TcpFlags::SYN));
        prop_assert!(parsed.flags.contains(TcpFlags::ACK));
        prop_assert_eq!(parsed.ack, SeqNum(seq.wrapping_add(1)));

        // TCB must be in SYN_RECEIVED
        let four_tuple = FourTuple { local: listen_addr, remote: client };
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        prop_assert_eq!(tcb.state, TcpState::SynReceived);
        prop_assert_eq!(tcb.peer_mss, mss);
        prop_assert_eq!(tcb.snd_scale, wscale);
    }

    /// Property: Active open completes correctly for any valid ISS and peer seq/ack.
    #[test]
    fn active_open_handshake_completes(
        peer_seq in any::<u32>(),
        peer_mss in 100u16..9000u16,
        peer_wscale in 0u8..14u8,
        peer_window in 1u16..65535u16,
    ) {
        let clock = Arc::new(MockClock::new());
        let config = EngineConfig::default();
        let mut engine = TcpEngine::new(clock, config);

        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let four_tuple = FourTuple { local, remote };
        let (tx, _rx) = std::sync::mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let handle = Arc::new(dpdk_stdlib_tcp::contract::ConnectionHandle::new(
            65536, 65536, cmd_tx, four_tuple,
        ));
        let (resp_tx, resp_rx) = oneshot_channel();

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

        // Peer sends SYN-ACK with correct ack
        let syn_ack = ParsedTcpSegment {
            src: remote,
            dst: local,
            seq: SeqNum(peer_seq),
            ack: iss.add(1),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: peer_window,
            options: TcpOptions {
                mss: Some(peer_mss),
                window_scale: Some(peer_wscale),
                sack_permitted: true,
                timestamps: Some((0, 0)),
                ..Default::default()
            },
            payload: Vec::new(),
        };

        let frames = engine.on_segment(&syn_ack);

        // Should produce ACK
        prop_assert_eq!(frames.len(), 1);
        let parsed = dpdk_stdlib_tcp::parse_tcp_packet(&frames[0]).unwrap();
        prop_assert!(parsed.flags.contains(TcpFlags::ACK));
        prop_assert!(!parsed.flags.contains(TcpFlags::SYN));
        prop_assert_eq!(parsed.ack, SeqNum(peer_seq.wrapping_add(1)));

        // TCB should be ESTABLISHED
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        prop_assert_eq!(tcb.state, TcpState::Established);
        prop_assert_eq!(tcb.peer_mss, peer_mss);
        prop_assert_eq!(tcb.snd_scale, peer_wscale);
        prop_assert_eq!(tcb.snd_wnd, (peer_window as u32) << peer_wscale);

        // Connect response should be Ok
        let result = resp_rx.recv_timeout(Duration::from_millis(100));
        prop_assert!(result.is_some());
        prop_assert_eq!(result.unwrap().unwrap(), four_tuple);
    }
}
