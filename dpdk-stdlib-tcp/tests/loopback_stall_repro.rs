//! Offline loopback regression for the sustained-exchange (bidir_multi) stall.
//!
//! Wires a CLIENT `TcpEngine` and a SERVER `TcpEngine` through in-memory frame
//! queues (no NIC, no EC2). Drives N sequential 64B echo exchanges on ONE
//! connection and asserts they all complete — mirroring
//! `tcp-test-client --mode bidir --count 20` against `tcp-echo`.
//!
//! FAILS before the fix (stalls at the 2nd exchange), PASSES after.
//!
//! Root cause: `handle_established`'s ACK path prunes the retransmit queue but
//! never drains acknowledged bytes from the front of `send_buf`, and never
//! clears `has_unacked_data`. After the first send+ACK, `send_buf` still holds
//! the stale acked bytes while `already_sent_offset` (derived from the now-empty
//! retransmit queue) resets to 0, so the next small write is misclassified as
//! "unsent" behind Nagle (`has_unacked_data == true`, len < MSS) and is never
//! transmitted.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use dpdk_stdlib_tcp::clock::{Clock, MockClock};
use dpdk_stdlib_tcp::codec::parse_tcp_packet;
use dpdk_stdlib_tcp::contract::{
    oneshot_channel, CommandSender, ConnectionHandle, EngineCommand, EngineWakeup,
};
use dpdk_stdlib_tcp::engine::{EngineConfig, TcpEngine};
use dpdk_stdlib_tcp::state::{FourTuple, TcpState};

const CLIENT_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];
const SERVER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];

/// Feed raw frames into an engine; return the frames it emits in response.
fn deliver(engine: &mut TcpEngine, frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for frame in frames {
        if frame.len() < 14 {
            continue;
        }
        let mut dst_mac = [0u8; 6];
        let mut src_mac = [0u8; 6];
        dst_mac.copy_from_slice(&frame[0..6]);
        src_mac.copy_from_slice(&frame[6..12]);
        if let Ok(seg) = parse_tcp_packet(frame) {
            out.extend(engine.on_segment_with_macs(&seg, src_mac, dst_mac));
        }
    }
    out
}

#[test]
fn loopback_20_echo_exchanges_do_not_stall() {
    let clock = Arc::new(MockClock::new());

    let (ctx_raw, _crx) = mpsc::channel();
    let client_cmd = CommandSender::new(ctx_raw, Arc::new(EngineWakeup::new()));
    let (stx_raw, _srx) = mpsc::channel();
    let server_cmd = CommandSender::new(stx_raw, Arc::new(EngineWakeup::new()));

    let mut client =
        TcpEngine::with_cmd_tx(clock.clone(), EngineConfig::default(), client_cmd.clone());
    let mut server =
        TcpEngine::with_cmd_tx(clock.clone(), EngineConfig::default(), server_cmd.clone());

    let client_addr: SocketAddr = "10.0.0.1:50000".parse().unwrap();
    let server_addr: SocketAddr = "10.0.0.2:9000".parse().unwrap();

    // Server: Listen.
    let (resp_tx, resp_rx) = oneshot_channel();
    server.on_command(EngineCommand::Listen {
        addr: server_addr,
        backlog: 16,
        response: resp_tx,
    });
    let _ = resp_rx.recv();

    // Client: Connect.
    let key = FourTuple { local: client_addr, remote: server_addr };
    let client_handle = Arc::new(ConnectionHandle::new(65536, 65536, client_cmd.clone(), key));
    let (cresp_tx, _cresp_rx) = oneshot_channel();
    let mut client_out = client.on_command(EngineCommand::Connect {
        local: client_addr,
        remote: server_addr,
        src_mac: CLIENT_MAC,
        dst_mac: SERVER_MAC,
        handle: client_handle.clone(),
        response: cresp_tx,
    });

    let mut to_server: VecDeque<Vec<u8>> = client_out.drain(..).collect();
    let mut to_client: VecDeque<Vec<u8>> = VecDeque::new();
    let server_key = FourTuple { local: server_addr, remote: client_addr };

    // Pump the three-way handshake to completion.
    for _ in 0..20 {
        let s_out = deliver(&mut server, &to_server.drain(..).collect::<Vec<_>>());
        to_client.extend(s_out);
        let c_out = deliver(&mut client, &to_client.drain(..).collect::<Vec<_>>());
        to_server.extend(c_out);

        clock.advance(Duration::from_millis(1));
        to_client.extend(server.on_tick(clock.now()));
        to_server.extend(client.on_tick(clock.now()));

        let c_est = client.tcbs.get(&key).map(|t| t.state) == Some(TcpState::Established);
        let s_est = server.tcbs.get(&server_key).map(|t| t.state) == Some(TcpState::Established);
        if c_est && s_est && to_server.is_empty() && to_client.is_empty() {
            break;
        }
    }

    assert_eq!(
        client.tcbs.get(&key).map(|t| t.state),
        Some(TcpState::Established),
        "client failed to establish"
    );
    let server_handle = {
        let tcb = server.tcbs.get(&server_key).expect("server TCB");
        assert_eq!(tcb.state, TcpState::Established, "server failed to establish");
        tcb.handle.clone()
    };

    let payload: Vec<u8> = (0..64u16).map(|i| (i % 256) as u8).collect();

    // 20 sequential echo exchanges on ONE connection.
    for iter in 0..20 {
        let w = client_handle.tx_ring.write(&payload);
        assert_eq!(w, 64, "iter {iter}: client tx_ring write short");

        let mut got_back = 0usize;
        let mut recv_buf = vec![0u8; 64];
        let mut stalled = true;
        for _ in 0..200 {
            clock.advance(Duration::from_millis(1));

            // Client engine drains its tx_ring → frames to server.
            to_server.extend(client.on_tick(clock.now()));
            let s_out = deliver(&mut server, &to_server.drain(..).collect::<Vec<_>>());
            to_client.extend(s_out);

            // Server app: read available bytes and echo them back.
            let mut sbuf = vec![0u8; 4096];
            let n = server_handle.rx_ring.read(&mut sbuf);
            if n > 0 {
                let ww = server_handle.tx_ring.write(&sbuf[..n]);
                assert_eq!(ww, n, "iter {iter}: server tx_ring full");
            }

            // Server engine drains its tx_ring (echo) + ACKs → frames to client.
            to_client.extend(server.on_tick(clock.now()));
            let c_out = deliver(&mut client, &to_client.drain(..).collect::<Vec<_>>());
            to_server.extend(c_out);

            // Client app: read the echo.
            got_back += client_handle.rx_ring.read(&mut recv_buf[got_back..]);
            if got_back >= 64 {
                stalled = false;
                break;
            }
        }

        assert!(
            !stalled,
            "STALL at iteration {iter}: client only got {got_back}/64 echoed bytes back \
             after 200 ticks (send_buf never drained on ACK → next write stuck behind Nagle)"
        );
        assert_eq!(&recv_buf[..], &payload[..], "iter {iter}: echo mismatch");
    }
}
