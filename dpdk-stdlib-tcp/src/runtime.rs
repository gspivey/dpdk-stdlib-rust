//! DPDK TCP runtime bootstrap.
//!
//! [`init_dpdk_tcp_context`] turns a vfio-pci-bound NIC into a running DPDK TCP
//! stack: it initialises EAL, builds a [`DpdkBackend`] over the real NIC, wires
//! an [`ArpResolver`] (gateway MAC for AWS VPC L3 routing), constructs a
//! [`TcpEngine`], spawns the single engine-driver thread, and publishes the
//! process-wide [`TcpContext`] so [`crate::TcpListener::bind`] /
//! [`crate::TcpStream::connect`] work.
//!
//! The engine lives entirely inside the spawned thread; apps only ever touch
//! the global context via the public socket types.

use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dpdk_stdlib_net::backend::{PacketBackend, RxReadiness};
use dpdk_stdlib_net::backend_dpdk::DpdkBackend;
use dpdk_stdlib_net::neighbor::{ArpResolver, NeighborResolver};

use crate::clock::{Clock, SystemClock};
use crate::codec::parse_tcp_packet;
use crate::contract::{CommandSender, EngineCommand, EngineWakeup};
use crate::engine::{EngineConfig, TcpEngine};
use crate::tcp_stream::{init_tcp_context, is_tcp_context_initialized, TcpContext};

/// Configuration for the DPDK TCP runtime.
pub struct DpdkTcpRuntimeConfig {
    /// DPDK port id of the NIC to drive (usually 0).
    pub port_id: u16,
    /// Local IPv4 address used as the source for outbound connections.
    pub local_ip: Ipv4Addr,
    /// Gateway MAC for AWS VPC (L3-routed). All outbound frames use this as the
    /// Ethernet destination. Required for `connect`; servers also need it for
    /// the first reply before any inbound traffic is seen.
    pub gateway_mac: Option<[u8; 6]>,
    /// Explicit EAL arguments (overrides the `DPDK_EAL_ARGS` env var).
    pub eal_args: Option<Vec<String>>,
    /// Interface MTU (informational; the backend uses the 9001 jumbo path).
    pub mtu: u16,
}

impl Default for DpdkTcpRuntimeConfig {
    fn default() -> Self {
        Self {
            port_id: 0,
            local_ip: Ipv4Addr::UNSPECIFIED,
            gateway_mac: None,
            eal_args: None,
            mtu: 9001,
        }
    }
}

/// Keeps the engine-driver thread and its shutdown flag alive for the process.
/// `_shutdown` is the app-side handle reserved for a future graceful-teardown
/// API; the driver thread holds its own clone.
struct DpdkTcpRuntime {
    _shutdown: Arc<AtomicBool>,
    _join: std::thread::JoinHandle<()>,
}

static TCP_RUNTIME: OnceLock<DpdkTcpRuntime> = OnceLock::new();

/// Initialise the global DPDK TCP context and start the engine driver.
///
/// Idempotent: a second call is a no-op once the context exists. Must be called
/// before any `TcpListener::bind` / `TcpStream::connect` on an IPv4 address.
pub fn init_dpdk_tcp_context(cfg: DpdkTcpRuntimeConfig) -> io::Result<()> {
    if is_tcp_context_initialized() {
        return Ok(());
    }

    // Real-NIC backend (no `--no-pci`) — see DpdkBackend::new_real_nic.
    let backend: Arc<dyn PacketBackend> =
        Arc::new(DpdkBackend::new_real_nic(cfg.port_id, cfg.eal_args.as_deref())?);

    // In AWS VPC (L3-routed) all outbound frames go to the gateway MAC.
    let resolver: Arc<dyn NeighborResolver> = match cfg.gateway_mac {
        Some(mac) => Arc::new(ArpResolver::with_gateway_mac(mac)),
        None => Arc::new(ArpResolver::new()),
    };

    let wakeup = Arc::new(EngineWakeup::new());
    let (cmd_tx_raw, cmd_rx) = mpsc::channel::<EngineCommand>();
    let cmd_tx = CommandSender::new(cmd_tx_raw, wakeup.clone());

    // The engine gets the live cmd_tx so accept-side handles route commands and
    // wake the loop on writes.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut engine = TcpEngine::with_cmd_tx(clock, EngineConfig::default(), cmd_tx.clone());

    // Publish the context BEFORE spawning so a fast bind()/connect() finds it.
    init_tcp_context(TcpContext::new(
        backend.clone(),
        resolver,
        cmd_tx,
        wakeup.clone(),
        cfg.local_ip,
    ));

    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = shutdown.clone();
    let join = std::thread::Builder::new()
        .name("dpdk-tcp-engine".into())
        .spawn(move || run_engine_driver(backend, &mut engine, cmd_rx, wakeup, sd))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let _ = TCP_RUNTIME.set(DpdkTcpRuntime {
        _shutdown: shutdown,
        _join: join,
    });
    Ok(())
}

/// Single iteration of the engine driver: drain RX → engine, commands → engine,
/// then timers → engine, transmitting every outbound frame.
///
/// Split out from [`run_engine_driver`] so it can be unit-tested with a mock
/// backend.
fn drive_once(
    backend: &Arc<dyn PacketBackend>,
    engine: &mut TcpEngine,
    cmd_rx: &mpsc::Receiver<EngineCommand>,
) {
    // RX burst → on_segment_with_macs. ParsedTcpSegment carries no MACs, so we
    // pull the Ethernet src/dst from the raw frame; this is what makes the
    // SYN-ACK / ACKs go back to the real peer (gateway) MAC.
    if let Ok(frames) = backend.recv_frames(32) {
        for frame in &frames {
            if frame.len() < 14 {
                continue;
            }
            let mut dst_mac = [0u8; 6];
            let mut src_mac = [0u8; 6];
            dst_mac.copy_from_slice(&frame[0..6]);
            src_mac.copy_from_slice(&frame[6..12]);
            if let Ok(seg) = parse_tcp_packet(frame) {
                for out in engine.on_segment_with_macs(&seg, src_mac, dst_mac) {
                    let _ = backend.send_frame(&out);
                }
            }
        }
    }

    // Commands (Connect/Listen/Accept/Shutdown/SetOption/Close).
    while let Ok(cmd) = cmd_rx.try_recv() {
        for out in engine.on_command(cmd) {
            let _ = backend.send_frame(&out);
        }
    }

    // Timers + tx-ring drain → segments.
    let now = engine.clock().now();
    for out in engine.on_tick(now) {
        let _ = backend.send_frame(&out);
    }
}

/// The engine driver: one thread owns the engine, backend, RX, TX, commands and
/// timers — no locks on the hot path. Loops until `shutdown` is set.
fn run_engine_driver(
    backend: Arc<dyn PacketBackend>,
    engine: &mut TcpEngine,
    cmd_rx: mpsc::Receiver<EngineCommand>,
    wakeup: Arc<EngineWakeup>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        // Bound the wait by the next timer deadline so RTO/keepalive fire on time.
        let now = engine.clock().now();
        let deadline = engine.next_timer_deadline(now);
        let timeout = deadline
            .map(|d| d.saturating_duration_since(now))
            .unwrap_or(Duration::from_millis(10));

        match backend.rx_readiness() {
            RxReadiness::Condvar(pair) => {
                if !wakeup.try_recv() {
                    let (lock, cv) = &*pair;
                    let guard = lock.lock().unwrap();
                    if !*guard && !wakeup.try_recv() {
                        let _ = cv.wait_timeout(guard, timeout).unwrap();
                    }
                }
            }
            RxReadiness::PollOnly => {
                // DPDK: short nap so we still service timers (~sub-ms) without
                // pinning a core when idle.
                if !wakeup.try_recv() {
                    std::thread::sleep(timeout.min(Duration::from_micros(200)));
                }
            }
            RxReadiness::Fd(_fd) => {
                wakeup.wait(timeout);
            }
        }

        drive_once(&backend, engine, &cmd_rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::codec::{build_tcp_frame, parse_tcp_packet, TcpFlags, TcpFrameParams, TcpOptions};
    use crate::contract::oneshot_channel;
    use crate::seq::SeqNum;

    /// Mock backend that replays queued RX frames and captures sent frames.
    struct MockBackend {
        mac: [u8; 6],
        rx: Mutex<VecDeque<Vec<u8>>>,
        tx: Mutex<Vec<Vec<u8>>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
                rx: Mutex::new(VecDeque::new()),
                tx: Mutex::new(Vec::new()),
            }
        }
        fn push_rx(&self, frame: Vec<u8>) {
            self.rx.lock().unwrap().push_back(frame);
        }
        fn sent(&self) -> Vec<Vec<u8>> {
            self.tx.lock().unwrap().clone()
        }
    }

    impl PacketBackend for MockBackend {
        fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
            self.tx.lock().unwrap().push(frame.to_vec());
            Ok(frame.len())
        }
        fn recv_frames(&self, max: usize) -> io::Result<Vec<Vec<u8>>> {
            let mut rx = self.rx.lock().unwrap();
            let mut out = Vec::new();
            while out.len() < max {
                match rx.pop_front() {
                    Some(f) => out.push(f),
                    None => break,
                }
            }
            Ok(out)
        }
        fn mac_address(&self) -> [u8; 6] {
            self.mac
        }
        fn backend_name(&self) -> &'static str {
            "mock"
        }
        fn set_promiscuous(&self, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn is_promiscuous(&self) -> bool {
            false
        }
        fn set_allmulticast(&self, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn is_allmulticast(&self) -> bool {
            false
        }
        fn rx_readiness(&self) -> RxReadiness {
            RxReadiness::PollOnly
        }
    }

    /// Regression for the zeroed-MAC bug: the driver must extract the Ethernet
    /// MACs from the raw frame so a server's SYN-ACK is addressed to the peer
    /// (gateway) MAC, not 00:00:00:00:00:00.
    #[test]
    fn drive_once_replies_to_peer_mac_on_syn() {
        const PEER_MAC: [u8; 6] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        const OUR_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let (raw_tx, cmd_rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(raw_tx, wakeup);
        let mut engine = TcpEngine::with_cmd_tx(clock, EngineConfig::default(), cmd_tx);

        // Register a listener for the SYN's destination.
        let listen_addr = "10.0.0.1:9000".parse().unwrap();
        let (resp_tx, resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 16,
            response: resp_tx,
        });
        let _ = resp_rx.recv();

        // Build an inbound SYN frame: Ethernet dst = OUR_MAC, src = PEER_MAC.
        let syn = build_tcp_frame(&TcpFrameParams {
            src_mac: PEER_MAC,
            dst_mac: OUR_MAC,
            src: "10.0.0.2:5000".parse().unwrap(),
            dst: "10.0.0.1:9000".parse().unwrap(),
            seq: SeqNum(1000),
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: 65535,
            options: TcpOptions {
                mss: Some(1460),
                window_scale: Some(7),
                ..Default::default()
            },
            payload: Vec::new(),
            ttl: 64,
        })
        .unwrap();

        // Keep a typed handle for queue access; the driver sees the trait object
        // (same underlying instance via Arc clone).
        let mock = Arc::new(MockBackend::new());
        mock.push_rx(syn);
        let backend: Arc<dyn PacketBackend> = mock.clone();

        drive_once(&backend, &mut engine, &cmd_rx);

        let sent = mock.sent();
        assert_eq!(sent.len(), 1, "expected exactly one SYN-ACK");
        let reply = &sent[0];
        // Ethernet dst (bytes 0..6) must be the peer MAC, not zeros.
        assert_eq!(&reply[0..6], &PEER_MAC, "SYN-ACK must target the peer MAC");
        assert_ne!(&reply[0..6], &[0u8; 6], "regression: zeroed dst MAC");
        // And it must be a SYN-ACK.
        let parsed = parse_tcp_packet(reply).unwrap();
        assert!(parsed.flags.contains(TcpFlags::SYN));
        assert!(parsed.flags.contains(TcpFlags::ACK));
    }
}
