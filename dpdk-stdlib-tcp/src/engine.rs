//! TCP protocol engine — stateful segment processing.
//!
//! The engine owns all TCBs and processes inbound segments via `on_segment`.
//! This module implements the handshake path (task 5.8):
//! - SYN → SYN_RECEIVED (send SYN-ACK)
//! - SYN-ACK → ESTABLISHED (send ACK, wake connect oneshot)
//! - RST in SYN_SENT → ConnectionRefused

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::clock::Clock;
use crate::codec::{
    build_tcp_frame, ParsedTcpSegment, TcpFlags, TcpFrameParams, TcpOptions,
};
use crate::congestion::CongestionState;
use crate::contract::{
    ConnectionHandle, EngineCommand, OneshotSender,
};
use crate::error::TcpError;
use crate::isn::IsnGenerator;
use crate::seq::SeqNum;
use crate::state::{FourTuple, TcpState};
use crate::tcb::Tcb;

// --- Engine configuration ---

/// Configuration for the TCP engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Maximum concurrent TCBs allowed.
    pub max_tcbs: usize,
    /// Default accept backlog size.
    pub default_backlog: usize,
    /// Local MSS (derived from MTU - 40).
    pub local_mss: u16,
    /// Default receive window to advertise.
    pub default_rcv_wnd: u32,
    /// Default window scale factor.
    pub default_rcv_scale: u8,
    /// Default RTO for initial SYN retransmit.
    pub initial_rto: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_tcbs: 1024,
            default_backlog: 128,
            local_mss: 1460,
            default_rcv_wnd: 65535,
            default_rcv_scale: 7,
            initial_rto: Duration::from_secs(1),
        }
    }
}

// --- Listen state ---

/// Per-listener state maintained by the engine.
pub struct ListenState {
    /// Maximum pending connections (accept backlog).
    pub backlog: usize,
    /// Pending accept oneshot senders (waiting callers).
    pub accept_queue: Vec<OneshotSender<Result<(FourTuple, Arc<ConnectionHandle>), TcpError>>>,
    /// Completed connections waiting for accept.
    pub completed: Vec<(FourTuple, Arc<ConnectionHandle>)>,
}

// --- Connect state (pending SYN_SENT) ---

/// State for an outbound connection attempt awaiting SYN-ACK.
struct PendingConnect {
    response: OneshotSender<Result<FourTuple, TcpError>>,
}

// --- TcpEngine ---

/// The TCP protocol engine. Owns all TCBs and processes segments.
pub struct TcpEngine {
    /// Active connections keyed by 4-tuple.
    pub tcbs: HashMap<FourTuple, Tcb>,
    /// Listening sockets keyed by local address.
    pub listeners: HashMap<SocketAddr, ListenState>,
    /// Pending connect responses (SYN_SENT state).
    pending_connects: HashMap<FourTuple, PendingConnect>,
    /// ISN generator.
    isn_gen: IsnGenerator,
    /// Injectable clock.
    clock: Arc<dyn Clock>,
    /// Engine configuration.
    pub config: EngineConfig,
}

impl TcpEngine {
    /// Create a new engine with the given clock and config.
    pub fn new(clock: Arc<dyn Clock>, config: EngineConfig) -> Self {
        let isn_gen = IsnGenerator::new(clock.as_ref());
        Self {
            tcbs: HashMap::new(),
            listeners: HashMap::new(),
            pending_connects: HashMap::new(),
            isn_gen,
            clock,
            config,
        }
    }

    /// Process a parsed inbound TCP segment. Returns outbound frames to send.
    pub fn on_segment(&mut self, seg: &ParsedTcpSegment) -> Vec<Vec<u8>> {
        let four_tuple = FourTuple {
            local: seg.dst,
            remote: seg.src,
        };

        // --- RST handling for existing connections ---
        if seg.flags.contains(TcpFlags::RST) {
            return self.handle_rst(seg, &four_tuple);
        }

        // --- Existing TCB lookup (SYN-ACK for active open, etc.) ---
        if self.tcbs.contains_key(&four_tuple) {
            return self.handle_existing_tcb(seg, &four_tuple);
        }

        // --- SYN on a listening port (passive open) ---
        if seg.flags.contains(TcpFlags::SYN) && !seg.flags.contains(TcpFlags::ACK) {
            return self.handle_syn(seg, &four_tuple);
        }

        Vec::new()
    }

    /// Process a control command from the app thread. Returns outbound frames.
    pub fn on_command(&mut self, cmd: EngineCommand) -> Vec<Vec<u8>> {
        match cmd {
            EngineCommand::Connect {
                local,
                remote,
                src_mac,
                dst_mac,
                handle,
                response,
            } => self.handle_connect(local, remote, src_mac, dst_mac, handle, response),
            EngineCommand::Listen {
                addr,
                backlog,
                response,
            } => {
                self.handle_listen(addr, backlog, response);
                Vec::new()
            }
            EngineCommand::Accept {
                listen_addr,
                response,
            } => {
                self.handle_accept(listen_addr, response);
                Vec::new()
            }
            _ => Vec::new(), // Shutdown, SetOption, Close — future tasks
        }
    }

    /// Service timers (placeholder for task 5.14+).
    pub fn on_tick(&mut self, _now: std::time::Instant) -> Vec<Vec<u8>> {
        Vec::new()
    }

    // ===== Handshake: Active Open (Connect) =====

    fn handle_connect(
        &mut self,
        local: SocketAddr,
        remote: SocketAddr,
        src_mac: [u8; 6],
        dst_mac: [u8; 6],
        handle: Arc<ConnectionHandle>,
        response: OneshotSender<Result<FourTuple, TcpError>>,
    ) -> Vec<Vec<u8>> {
        let four_tuple = FourTuple { local, remote };

        // Enforce max TCBs
        if self.tcbs.len() >= self.config.max_tcbs {
            response.send(Err(TcpError::ResourceLimit(
                "max TCBs exceeded".to_string(),
            )));
            return Vec::new();
        }

        // Generate ISN
        let iss = self.isn_gen.generate(&four_tuple, self.clock.as_ref());

        // Create TCB in SYN_SENT state
        let mut tcb = Tcb::new(four_tuple, iss, self.config.local_mss, handle.clone(), src_mac, dst_mac);
        tcb.state = TcpState::SynSent;
        tcb.snd_nxt = iss.add(1); // SYN consumes one sequence number
        tcb.rcv_wnd = self.config.default_rcv_wnd;
        tcb.rcv_scale = self.config.default_rcv_scale;
        tcb.rto_deadline = Some(self.clock.now() + self.config.initial_rto);

        // Update handle state
        handle.set_state(TcpState::SynSent);

        // Build SYN frame
        let syn_frame = self.build_syn_frame(&tcb);

        self.tcbs.insert(four_tuple, tcb);
        self.pending_connects.insert(four_tuple, PendingConnect { response });

        match syn_frame {
            Ok(frame) => vec![frame],
            Err(_) => Vec::new(),
        }
    }

    // ===== Handshake: Passive Open (Listen + Accept) =====

    fn handle_listen(
        &mut self,
        addr: SocketAddr,
        backlog: usize,
        response: OneshotSender<Result<(), TcpError>>,
    ) {
        let backlog = if backlog == 0 {
            self.config.default_backlog
        } else {
            backlog
        };
        self.listeners.insert(
            addr,
            ListenState {
                backlog,
                accept_queue: Vec::new(),
                completed: Vec::new(),
            },
        );
        response.send(Ok(()));
    }

    fn handle_accept(
        &mut self,
        listen_addr: SocketAddr,
        response: OneshotSender<Result<(FourTuple, Arc<ConnectionHandle>), TcpError>>,
    ) {
        if let Some(listener) = self.listeners.get_mut(&listen_addr) {
            // If there's a completed connection waiting, deliver it immediately
            if let Some(conn) = listener.completed.pop() {
                response.send(Ok(conn));
            } else {
                // Park the accept request until a connection completes
                listener.accept_queue.push(response);
            }
        } else {
            response.send(Err(TcpError::NotConnected));
        }
    }

    // ===== Inbound SYN (passive open) =====

    fn handle_syn(&mut self, seg: &ParsedTcpSegment, four_tuple: &FourTuple) -> Vec<Vec<u8>> {
        // Find a listener for the destination address
        let listen_addr = seg.dst;
        let listener = match self.listeners.get_mut(&listen_addr) {
            Some(l) => l,
            None => {
                // Also try wildcard (0.0.0.0:port)
                let wildcard = SocketAddr::from(([0, 0, 0, 0], port_of(seg.dst)));
                match self.listeners.get_mut(&wildcard) {
                    Some(l) => l,
                    None => return self.send_rst_for_segment(seg),
                }
            }
        };

        // Accept backlog check
        let pending_count = self.tcbs.values().filter(|t| {
            t.state == TcpState::SynReceived
                && (t.key.local == listen_addr
                    || t.key.local.port() == listen_addr.port())
        }).count() + listener.completed.len();

        if pending_count >= listener.backlog {
            return self.send_rst_for_segment(seg);
        }

        // Enforce max TCBs
        if self.tcbs.len() >= self.config.max_tcbs {
            return self.send_rst_for_segment(seg);
        }

        // Generate ISN for the server side
        let iss = self.isn_gen.generate(four_tuple, self.clock.as_ref());

        // Extract peer options
        let peer_mss = seg.options.mss.unwrap_or(crate::DEFAULT_PEER_MSS);
        let peer_wscale = seg.options.window_scale.unwrap_or(0);

        // Create TCB — populate src_mac/dst_mac from the parsed frame's Ethernet context
        // (accept-side: the frame arrived on our interface, so dst_mac in the frame is our MAC,
        //  and src_mac in the frame is the peer's MAC — we reverse them for outbound)
        let (src_mac, dst_mac) = extract_macs_from_segment(seg);

        // Create a ConnectionHandle for this new connection
        let cmd_tx = self.make_dummy_cmd_sender();
        let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx, *four_tuple));

        let mut tcb = Tcb::new(*four_tuple, iss, self.config.local_mss, handle.clone(), src_mac, dst_mac);
        tcb.state = TcpState::SynReceived;
        tcb.snd_nxt = iss.add(1); // SYN-ACK consumes one sequence number
        tcb.rcv_nxt = seg.seq.add(1); // SYN consumes one seq from peer
        tcb.irs = seg.seq;
        tcb.peer_mss = peer_mss;
        tcb.snd_scale = peer_wscale;
        tcb.rcv_scale = self.config.default_rcv_scale;
        tcb.rcv_wnd = self.config.default_rcv_wnd;
        tcb.snd_wnd = (seg.window as u32) << peer_wscale;
        tcb.rto_deadline = Some(self.clock.now() + self.config.initial_rto);
        tcb.congestion = CongestionState::new(std::cmp::min(self.config.local_mss, peer_mss));

        // Update handle state
        handle.set_state(TcpState::SynReceived);

        // Build SYN-ACK frame
        let syn_ack = self.build_syn_ack_frame(&tcb);

        self.tcbs.insert(*four_tuple, tcb);

        match syn_ack {
            Ok(frame) => vec![frame],
            Err(_) => Vec::new(),
        }
    }

    // ===== Inbound SYN-ACK (active open completion) =====

    fn handle_existing_tcb(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        match tcb.state {
            TcpState::SynSent => {
                if seg.flags.contains(TcpFlags::SYN) && seg.flags.contains(TcpFlags::ACK) {
                    self.handle_syn_ack(seg, four_tuple)
                } else {
                    Vec::new()
                }
            }
            TcpState::SynReceived => {
                if seg.flags.contains(TcpFlags::ACK) && !seg.flags.contains(TcpFlags::SYN) {
                    self.handle_ack_for_syn_received(seg, four_tuple)
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(), // Future tasks handle other states
        }
    }

    fn handle_syn_ack(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        // Validate: ACK must acknowledge our SYN (ack == iss + 1)
        if seg.ack != tcb.iss.add(1) {
            return Vec::new();
        }

        // Transition to ESTABLISHED
        tcb.state = TcpState::Established;
        tcb.snd_una = seg.ack;
        tcb.rcv_nxt = seg.seq.add(1); // SYN-ACK consumes one seq
        tcb.irs = seg.seq;
        tcb.rto_deadline = None; // Cancel SYN retransmit timer

        // Process peer options
        let peer_mss = seg.options.mss.unwrap_or(crate::DEFAULT_PEER_MSS);
        let peer_wscale = seg.options.window_scale.unwrap_or(0);
        tcb.peer_mss = peer_mss;
        tcb.snd_scale = peer_wscale;
        tcb.snd_wnd = (seg.window as u32) << peer_wscale;
        tcb.congestion = CongestionState::new(std::cmp::min(tcb.local_mss, peer_mss));

        // Update handle
        tcb.handle.set_state(TcpState::Established);
        tcb.handle.notify_all();

        // Build ACK to complete three-way handshake
        let ack_frame = build_tcp_frame(&TcpFrameParams {
            src_mac: tcb.src_mac,
            dst_mac: tcb.dst_mac,
            src: tcb.key.local,
            dst: tcb.key.remote,
            seq: tcb.snd_nxt,
            ack: tcb.rcv_nxt,
            flags: TcpFlags::ACK,
            window: encode_window(tcb.rcv_wnd, tcb.rcv_scale),
            options: TcpOptions::default(),
            payload: Vec::new(),
            ttl: tcb.ttl,
        });

        // Wake connect oneshot
        if let Some(pending) = self.pending_connects.remove(four_tuple) {
            pending.response.send(Ok(*four_tuple));
        }

        match ack_frame {
            Ok(frame) => vec![frame],
            Err(_) => Vec::new(),
        }
    }

    // ===== ACK completing passive handshake (SYN_RECEIVED → ESTABLISHED) =====

    fn handle_ack_for_syn_received(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        if tcb.state != TcpState::SynReceived {
            return Vec::new();
        }

        // Validate: ACK must acknowledge our SYN-ACK (ack == iss + 1)
        if seg.ack != tcb.iss.add(1) {
            return Vec::new();
        }

        // Transition to ESTABLISHED
        tcb.state = TcpState::Established;
        tcb.snd_una = seg.ack;
        tcb.rto_deadline = None; // Cancel SYN-ACK retransmit timer

        // Update handle
        tcb.handle.set_state(TcpState::Established);
        tcb.handle.notify_all();

        // Deliver to accept queue
        let listen_addr = tcb.key.local;
        let handle = tcb.handle.clone();
        let key = *four_tuple;

        // Find the listener (try exact match then wildcard)
        let wildcard = SocketAddr::from(([0, 0, 0, 0], listen_addr.port()));
        let listener_addr = if self.listeners.contains_key(&listen_addr) {
            Some(listen_addr)
        } else if self.listeners.contains_key(&wildcard) {
            Some(wildcard)
        } else {
            None
        };

        if let Some(addr) = listener_addr {
            let listener = self.listeners.get_mut(&addr).unwrap();
            if let Some(accept_sender) = listener.accept_queue.pop() {
                accept_sender.send(Ok((key, handle)));
            } else {
                listener.completed.push((key, handle));
            }
        }

        Vec::new()
    }

    // ===== RST handling =====

    fn handle_rst(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        if let Some(tcb) = self.tcbs.get(four_tuple) {
            match tcb.state {
                TcpState::SynSent => {
                    // RST in SYN_SENT → ConnectionRefused
                    // Validate: RST must ACK our SYN (ack == iss + 1)
                    if seg.flags.contains(TcpFlags::ACK) && seg.ack == tcb.iss.add(1) {
                        let handle = tcb.handle.clone();
                        handle.latch_error(TcpError::ConnectionRefused);
                        handle.set_state(TcpState::Closed);
                        handle.notify_all();

                        // Wake pending connect with error
                        if let Some(pending) = self.pending_connects.remove(four_tuple) {
                            pending.response.send(Err(TcpError::ConnectionRefused));
                        }

                        self.tcbs.remove(four_tuple);
                    }
                }
                TcpState::SynReceived => {
                    // RST in SYN_RECEIVED — abort the embryonic connection
                    if seg.seq == tcb.rcv_nxt {
                        let handle = tcb.handle.clone();
                        handle.set_state(TcpState::Closed);
                        handle.latch_error(TcpError::ConnectionReset);
                        handle.notify_all();
                        self.tcbs.remove(four_tuple);
                    }
                }
                _ => {
                    // Other states: future task 5.12 (RFC 5961 RST validation)
                }
            }
        }
        Vec::new()
    }

    // ===== Frame builders =====

    fn build_syn_frame(&self, tcb: &Tcb) -> Result<Vec<u8>, TcpError> {
        build_tcp_frame(&TcpFrameParams {
            src_mac: tcb.src_mac,
            dst_mac: tcb.dst_mac,
            src: tcb.key.local,
            dst: tcb.key.remote,
            seq: tcb.iss,
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: encode_window(tcb.rcv_wnd, 0), // Window scale not yet negotiated
            options: TcpOptions {
                mss: Some(tcb.local_mss),
                window_scale: Some(tcb.rcv_scale),
                sack_permitted: true,
                timestamps: Some((0, 0)),
                ..Default::default()
            },
            payload: Vec::new(),
            ttl: tcb.ttl,
        })
    }

    fn build_syn_ack_frame(&self, tcb: &Tcb) -> Result<Vec<u8>, TcpError> {
        build_tcp_frame(&TcpFrameParams {
            src_mac: tcb.src_mac,
            dst_mac: tcb.dst_mac,
            src: tcb.key.local,
            dst: tcb.key.remote,
            seq: tcb.iss,
            ack: tcb.rcv_nxt,
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: encode_window(tcb.rcv_wnd, 0), // Window scale not yet active until ACK
            options: TcpOptions {
                mss: Some(tcb.local_mss),
                window_scale: Some(tcb.rcv_scale),
                sack_permitted: true,
                timestamps: Some((0, 0)),
                ..Default::default()
            },
            payload: Vec::new(),
            ttl: tcb.ttl,
        })
    }

    fn send_rst_for_segment(&self, seg: &ParsedTcpSegment) -> Vec<Vec<u8>> {
        // Build RST+ACK in response to unexpected segment
        let (src_mac, dst_mac) = extract_macs_from_segment(seg);
        let frame = build_tcp_frame(&TcpFrameParams {
            src_mac,
            dst_mac,
            src: seg.dst,
            dst: seg.src,
            seq: if seg.flags.contains(TcpFlags::ACK) {
                seg.ack
            } else {
                SeqNum(0)
            },
            ack: if seg.flags.contains(TcpFlags::ACK) {
                SeqNum(0)
            } else {
                seg.seq.add(segment_len(seg))
            },
            flags: if seg.flags.contains(TcpFlags::ACK) {
                TcpFlags::RST
            } else {
                TcpFlags::RST | TcpFlags::ACK
            },
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
            ttl: 64,
        });
        match frame {
            Ok(f) => vec![f],
            Err(_) => Vec::new(),
        }
    }

    /// Create a dummy CommandSender for engine-created handles (accept side).
    /// The actual cmd_tx will be replaced when the handle is delivered to the app.
    fn make_dummy_cmd_sender(&self) -> crate::contract::CommandSender {
        let (tx, _rx) = std::sync::mpsc::channel();
        let wakeup = Arc::new(crate::contract::EngineWakeup::new());
        crate::contract::CommandSender::new(tx, wakeup)
    }
}

// ===== Helpers =====

/// Encode receive window for the TCP header (right-shift by scale during active connection).
/// During SYN/SYN-ACK exchange, window scale is not yet active (use unscaled).
#[inline]
fn encode_window(rcv_wnd: u32, _rcv_scale: u8) -> u16 {
    // During handshake, advertise unscaled. Cap at u16::MAX.
    std::cmp::min(rcv_wnd, u16::MAX as u32) as u16
}

/// Compute the "length" of a segment for RST/ACK sequence calculation.
#[inline]
fn segment_len(seg: &ParsedTcpSegment) -> u32 {
    let mut len = seg.payload.len() as u32;
    if seg.flags.contains(TcpFlags::SYN) {
        len += 1;
    }
    if seg.flags.contains(TcpFlags::FIN) {
        len += 1;
    }
    len
}

/// Extract MAC addresses from a parsed segment.
/// For accept-side: we need to know our own MAC and the peer's MAC.
/// Since ParsedTcpSegment doesn't carry Ethernet MACs, we use placeholder
/// addresses that will be populated from the raw frame in the engine loop.
/// For now, use zeros — the engine_loop will extract MACs from the raw frame
/// before calling on_segment in the future, or we extend ParsedTcpSegment.
///
/// NOTE: In this implementation, we extend the approach to carry MACs.
/// For the handshake task, we use default MACs (the engine_loop or test harness
/// should set them via a frame-level extraction before on_segment).
#[inline]
fn extract_macs_from_segment(_seg: &ParsedTcpSegment) -> ([u8; 6], [u8; 6]) {
    // ParsedTcpSegment doesn't include Ethernet headers.
    // The real engine_loop extracts MACs from the raw frame and passes them in.
    // For now, return zeros — the accept-side TCB MACs will be populated via
    // on_segment_with_macs or set externally.
    ([0u8; 6], [0u8; 6])
}

/// Extract port number from a SocketAddr.
#[inline]
fn port_of(addr: SocketAddr) -> u16 {
    addr.port()
}

// --- Extended API for accept-side MAC population ---

impl TcpEngine {
    /// Process an inbound segment with explicit MAC addresses from the Ethernet frame.
    /// This is the preferred entry point from the engine_loop which has access to
    /// the raw frame bytes.
    pub fn on_segment_with_macs(
        &mut self,
        seg: &ParsedTcpSegment,
        frame_src_mac: [u8; 6],
        frame_dst_mac: [u8; 6],
    ) -> Vec<Vec<u8>> {
        let four_tuple = FourTuple {
            local: seg.dst,
            remote: seg.src,
        };

        // RST handling
        if seg.flags.contains(TcpFlags::RST) {
            return self.handle_rst(seg, &four_tuple);
        }

        // Existing TCB
        if self.tcbs.contains_key(&four_tuple) {
            return self.handle_existing_tcb(seg, &four_tuple);
        }

        // SYN on listening port — use provided MACs
        if seg.flags.contains(TcpFlags::SYN) && !seg.flags.contains(TcpFlags::ACK) {
            return self.handle_syn_with_macs(seg, &four_tuple, frame_src_mac, frame_dst_mac);
        }

        Vec::new()
    }

    /// Handle SYN with explicit MAC addresses (accept-side MAC population).
    fn handle_syn_with_macs(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
        frame_src_mac: [u8; 6],
        frame_dst_mac: [u8; 6],
    ) -> Vec<Vec<u8>> {
        let listen_addr = seg.dst;
        let listener = match self.listeners.get_mut(&listen_addr) {
            Some(l) => l,
            None => {
                let wildcard = SocketAddr::from(([0, 0, 0, 0], port_of(seg.dst)));
                match self.listeners.get_mut(&wildcard) {
                    Some(l) => l,
                    None => return self.send_rst_for_segment(seg),
                }
            }
        };

        // Accept backlog check
        let pending_count = self.tcbs.values().filter(|t| {
            t.state == TcpState::SynReceived
                && (t.key.local == listen_addr
                    || t.key.local.port() == listen_addr.port())
        }).count() + listener.completed.len();

        if pending_count >= listener.backlog {
            return self.send_rst_for_segment(seg);
        }

        if self.tcbs.len() >= self.config.max_tcbs {
            return self.send_rst_for_segment(seg);
        }

        let iss = self.isn_gen.generate(four_tuple, self.clock.as_ref());
        let peer_mss = seg.options.mss.unwrap_or(crate::DEFAULT_PEER_MSS);
        let peer_wscale = seg.options.window_scale.unwrap_or(0);

        // Accept-side: frame_dst_mac is OUR MAC (frame was destined to us),
        // frame_src_mac is the PEER's MAC. For outbound frames we swap them:
        // our src_mac = frame_dst_mac, our dst_mac = frame_src_mac.
        let src_mac = frame_dst_mac;
        let dst_mac = frame_src_mac;

        let cmd_tx = self.make_dummy_cmd_sender();
        let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx, *four_tuple));

        let mut tcb = Tcb::new(*four_tuple, iss, self.config.local_mss, handle.clone(), src_mac, dst_mac);
        tcb.state = TcpState::SynReceived;
        tcb.snd_nxt = iss.add(1);
        tcb.rcv_nxt = seg.seq.add(1);
        tcb.irs = seg.seq;
        tcb.peer_mss = peer_mss;
        tcb.snd_scale = peer_wscale;
        tcb.rcv_scale = self.config.default_rcv_scale;
        tcb.rcv_wnd = self.config.default_rcv_wnd;
        tcb.snd_wnd = (seg.window as u32) << peer_wscale;
        tcb.rto_deadline = Some(self.clock.now() + self.config.initial_rto);
        tcb.congestion = CongestionState::new(std::cmp::min(self.config.local_mss, peer_mss));

        handle.set_state(TcpState::SynReceived);

        let syn_ack = self.build_syn_ack_frame(&tcb);
        self.tcbs.insert(*four_tuple, tcb);

        match syn_ack {
            Ok(frame) => vec![frame],
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use crate::codec::parse_tcp_packet;
    use crate::contract::{oneshot_channel, CommandSender, EngineWakeup};
    use std::sync::mpsc;

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

    fn make_syn_segment(src: SocketAddr, dst: SocketAddr, seq: u32) -> ParsedTcpSegment {
        ParsedTcpSegment {
            src,
            dst,
            seq: SeqNum(seq),
            ack: SeqNum(0),
            flags: TcpFlags::SYN,
            window: 65535,
            options: TcpOptions {
                mss: Some(1460),
                window_scale: Some(7),
                sack_permitted: true,
                timestamps: Some((12345, 0)),
                ..Default::default()
            },
            payload: Vec::new(),
        }
    }

    fn make_syn_ack_segment(
        src: SocketAddr,
        dst: SocketAddr,
        seq: u32,
        ack: u32,
    ) -> ParsedTcpSegment {
        ParsedTcpSegment {
            src,
            dst,
            seq: SeqNum(seq),
            ack: SeqNum(ack),
            flags: TcpFlags::SYN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions {
                mss: Some(1460),
                window_scale: Some(7),
                sack_permitted: true,
                timestamps: Some((67890, 12345)),
                ..Default::default()
            },
            payload: Vec::new(),
        }
    }

    fn make_ack_segment(
        src: SocketAddr,
        dst: SocketAddr,
        seq: u32,
        ack: u32,
    ) -> ParsedTcpSegment {
        ParsedTcpSegment {
            src,
            dst,
            seq: SeqNum(seq),
            ack: SeqNum(ack),
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        }
    }

    fn make_rst_segment(
        src: SocketAddr,
        dst: SocketAddr,
        seq: u32,
        ack: u32,
    ) -> ParsedTcpSegment {
        ParsedTcpSegment {
            src,
            dst,
            seq: SeqNum(seq),
            ack: SeqNum(ack),
            flags: TcpFlags::RST | TcpFlags::ACK,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        }
    }

    // === Active Open Tests ===

    #[test]
    fn connect_sends_syn_and_transitions_to_syn_sent() {
        let (mut engine, _clock) = make_engine();
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let four_tuple = FourTuple { local, remote };
        let handle = make_handle(four_tuple);
        let (resp_tx, _resp_rx) = oneshot_channel();

        let frames = engine.on_command(EngineCommand::Connect {
            local,
            remote,
            src_mac: [0x02, 0, 0, 0, 0, 1],
            dst_mac: [0x02, 0, 0, 0, 0, 2],
            handle: handle.clone(),
            response: resp_tx,
        });

        // Should produce one SYN frame
        assert_eq!(frames.len(), 1);

        // TCB should exist in SYN_SENT
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::SynSent);

        // Handle should reflect SYN_SENT
        assert_eq!(handle.tcp_state(), TcpState::SynSent);

        // Parse the outbound SYN frame
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::SYN));
        assert!(!parsed.flags.contains(TcpFlags::ACK));
        assert!(parsed.options.mss.is_some());
        assert!(parsed.options.window_scale.is_some());
        assert!(parsed.options.sack_permitted);
    }

    #[test]
    fn syn_ack_completes_active_open() {
        let (mut engine, _clock) = make_engine();
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let four_tuple = FourTuple { local, remote };
        let handle = make_handle(four_tuple);
        let (resp_tx, resp_rx) = oneshot_channel();

        // Send Connect command
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

        // Simulate receiving SYN-ACK from peer
        let syn_ack = make_syn_ack_segment(remote, local, 2000, iss.add(1).0);
        let frames = engine.on_segment(&syn_ack);

        // Should produce ACK frame
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::ACK));
        assert!(!parsed.flags.contains(TcpFlags::SYN));

        // TCB should be ESTABLISHED
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::Established);
        assert_eq!(tcb.peer_mss, 1460);

        // Handle should reflect ESTABLISHED
        assert_eq!(handle.tcp_state(), TcpState::Established);

        // Connect response should be fulfilled
        let result = resp_rx.recv_timeout(Duration::from_millis(100));
        assert!(result.is_some());
        assert_eq!(result.unwrap().unwrap(), four_tuple);
    }

    #[test]
    fn rst_in_syn_sent_causes_connection_refused() {
        let (mut engine, _clock) = make_engine();
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let four_tuple = FourTuple { local, remote };
        let handle = make_handle(four_tuple);
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

        // Peer sends RST+ACK (ack = our ISS + 1)
        let rst = make_rst_segment(remote, local, 0, iss.add(1).0);
        engine.on_segment(&rst);

        // TCB should be removed
        assert!(!engine.tcbs.contains_key(&four_tuple));

        // Handle should have ConnectionRefused error latched
        assert!(matches!(
            handle.peek_error(),
            Some(TcpError::ConnectionRefused)
        ));
        assert_eq!(handle.tcp_state(), TcpState::Closed);

        // Connect response should be ConnectionRefused
        let result = resp_rx.recv_timeout(Duration::from_millis(100));
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Err(TcpError::ConnectionRefused)));
    }

    // === Passive Open Tests ===

    #[test]
    fn syn_on_listening_port_sends_syn_ack() {
        let (mut engine, _clock) = make_engine();
        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();

        // Set up listener
        let (resp_tx, resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });
        assert!(resp_rx.recv_timeout(Duration::from_millis(100)).unwrap().is_ok());

        // Receive SYN from client
        let client: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let syn = make_syn_segment(client, listen_addr, 1000);
        let frames = engine.on_segment_with_macs(
            &syn,
            [0x02, 0, 0, 0, 0, 0x02], // frame_src_mac (client's MAC)
            [0x02, 0, 0, 0, 0, 0x01], // frame_dst_mac (our MAC)
        );

        // Should produce SYN-ACK
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::SYN));
        assert!(parsed.flags.contains(TcpFlags::ACK));
        assert_eq!(parsed.ack, SeqNum(1001)); // client seq + 1

        // SYN-ACK must include required options
        assert!(parsed.options.mss.is_some());
        assert!(parsed.options.window_scale.is_some());
        assert!(parsed.options.sack_permitted);

        // TCB should be in SYN_RECEIVED
        let four_tuple = FourTuple {
            local: listen_addr,
            remote: client,
        };
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::SynReceived);

        // Accept-side: verify MACs are populated from frame
        assert_eq!(tcb.src_mac, [0x02, 0, 0, 0, 0, 0x01]); // our MAC
        assert_eq!(tcb.dst_mac, [0x02, 0, 0, 0, 0, 0x02]); // peer MAC
    }

    #[test]
    fn ack_completes_passive_handshake() {
        let (mut engine, _clock) = make_engine();
        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let client: SocketAddr = "10.0.0.2:5000".parse().unwrap();

        // Listen
        let (resp_tx, resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });
        resp_rx.recv_timeout(Duration::from_millis(100)).unwrap().unwrap();

        // SYN
        let syn = make_syn_segment(client, listen_addr, 1000);
        let frames = engine.on_segment_with_macs(
            &syn,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );
        assert_eq!(frames.len(), 1);

        // Get server ISS from SYN-ACK
        let syn_ack_parsed = parse_tcp_packet(&frames[0]).unwrap();
        let server_iss = syn_ack_parsed.seq;

        // Client sends ACK
        let four_tuple = FourTuple {
            local: listen_addr,
            remote: client,
        };
        let ack = make_ack_segment(client, listen_addr, 1001, server_iss.add(1).0);
        let frames = engine.on_segment(&ack);
        assert!(frames.is_empty()); // No response needed for handshake ACK

        // TCB should be ESTABLISHED
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::Established);
    }

    #[test]
    fn accept_receives_completed_connection() {
        let (mut engine, _clock) = make_engine();
        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let client: SocketAddr = "10.0.0.2:5000".parse().unwrap();

        // Listen
        let (resp_tx, resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });
        resp_rx.recv_timeout(Duration::from_millis(100)).unwrap().unwrap();

        // Register accept before connection completes
        let (accept_tx, accept_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Accept {
            listen_addr,
            response: accept_tx,
        });

        // Complete three-way handshake
        let syn = make_syn_segment(client, listen_addr, 1000);
        let frames = engine.on_segment_with_macs(
            &syn,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );
        let syn_ack_parsed = parse_tcp_packet(&frames[0]).unwrap();
        let server_iss = syn_ack_parsed.seq;

        let ack = make_ack_segment(client, listen_addr, 1001, server_iss.add(1).0);
        engine.on_segment(&ack);

        // Accept should have received the connection
        let result = accept_rx.recv_timeout(Duration::from_millis(100));
        assert!(result.is_some());
        let (key, handle) = result.unwrap().unwrap();
        assert_eq!(key.local, listen_addr);
        assert_eq!(key.remote, client);
        assert_eq!(handle.tcp_state(), TcpState::Established);
    }

    #[test]
    fn syn_to_non_listening_port_sends_rst() {
        let (mut engine, _clock) = make_engine();
        let target: SocketAddr = "10.0.0.1:9999".parse().unwrap();
        let client: SocketAddr = "10.0.0.2:5000".parse().unwrap();

        let syn = make_syn_segment(client, target, 1000);
        let frames = engine.on_segment(&syn);

        // Should send RST
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::RST));
    }

    #[test]
    fn syn_at_max_tcbs_sends_rst() {
        let (mut engine, _clock) = make_engine();
        engine.config.max_tcbs = 0; // No room

        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let (resp_tx, resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });
        resp_rx.recv_timeout(Duration::from_millis(100)).unwrap().unwrap();

        let client: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let syn = make_syn_segment(client, listen_addr, 1000);
        let frames = engine.on_segment_with_macs(
            &syn,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );

        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::RST));
    }

    #[test]
    fn syn_at_backlog_limit_sends_rst() {
        let (mut engine, _clock) = make_engine();
        let listen_addr: SocketAddr = "10.0.0.1:80".parse().unwrap();

        // Listen with backlog of 1
        let (resp_tx, resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 1,
            response: resp_tx,
        });
        resp_rx.recv_timeout(Duration::from_millis(100)).unwrap().unwrap();

        // First SYN — should succeed
        let client1: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let syn1 = make_syn_segment(client1, listen_addr, 1000);
        let frames = engine.on_segment_with_macs(
            &syn1,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::SYN)); // SYN-ACK

        // Second SYN — backlog full, should RST
        let client2: SocketAddr = "10.0.0.2:5001".parse().unwrap();
        let syn2 = make_syn_segment(client2, listen_addr, 2000);
        let frames = engine.on_segment_with_macs(
            &syn2,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::RST));
    }

    #[test]
    fn connect_at_max_tcbs_returns_error() {
        let (mut engine, _clock) = make_engine();
        engine.config.max_tcbs = 0;

        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let four_tuple = FourTuple { local, remote };
        let handle = make_handle(four_tuple);
        let (resp_tx, resp_rx) = oneshot_channel();

        let frames = engine.on_command(EngineCommand::Connect {
            local,
            remote,
            src_mac: [0x02, 0, 0, 0, 0, 1],
            dst_mac: [0x02, 0, 0, 0, 0, 2],
            handle,
            response: resp_tx,
        });

        assert!(frames.is_empty());
        let result = resp_rx.recv_timeout(Duration::from_millis(100));
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Err(TcpError::ResourceLimit(_))));
    }

    #[test]
    fn window_scale_negotiated_from_peer_options() {
        let (mut engine, _clock) = make_engine();
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
            handle,
            response: resp_tx,
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let iss = tcb.iss;

        // SYN-ACK with window_scale = 5
        let mut syn_ack = make_syn_ack_segment(remote, local, 3000, iss.add(1).0);
        syn_ack.options.window_scale = Some(5);
        syn_ack.window = 1024;

        engine.on_segment(&syn_ack);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.snd_scale, 5);
        // snd_wnd should be window << scale = 1024 << 5 = 32768
        assert_eq!(tcb.snd_wnd, 1024 << 5);
    }

    #[test]
    fn peer_mss_defaults_to_536_when_absent() {
        let (mut engine, _clock) = make_engine();
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
            handle,
            response: resp_tx,
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let iss = tcb.iss;

        // SYN-ACK without MSS option
        let mut syn_ack = make_syn_ack_segment(remote, local, 3000, iss.add(1).0);
        syn_ack.options.mss = None;

        engine.on_segment(&syn_ack);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.peer_mss, crate::DEFAULT_PEER_MSS); // 536
    }

    #[test]
    fn rst_with_wrong_ack_ignored_in_syn_sent() {
        let (mut engine, _clock) = make_engine();
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

        // RST with wrong ack number
        let rst = make_rst_segment(remote, local, 0, 9999);
        engine.on_segment(&rst);

        // TCB should still exist (RST ignored)
        assert!(engine.tcbs.contains_key(&four_tuple));
        assert_eq!(handle.tcp_state(), TcpState::SynSent);
    }

    #[test]
    fn wildcard_listener_accepts_connections() {
        let (mut engine, _clock) = make_engine();
        // Listen on 0.0.0.0:80
        let listen_addr: SocketAddr = "0.0.0.0:80".parse().unwrap();
        let (resp_tx, resp_rx) = oneshot_channel();
        engine.on_command(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 128,
            response: resp_tx,
        });
        resp_rx.recv_timeout(Duration::from_millis(100)).unwrap().unwrap();

        // SYN to specific IP on port 80
        let client: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let target: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let syn = make_syn_segment(client, target, 1000);
        let frames = engine.on_segment_with_macs(
            &syn,
            [0x02, 0, 0, 0, 0, 2],
            [0x02, 0, 0, 0, 0, 1],
        );

        // Should produce SYN-ACK (wildcard matched)
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::SYN));
        assert!(parsed.flags.contains(TcpFlags::ACK));
    }
}
