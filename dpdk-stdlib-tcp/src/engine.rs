//! TCP protocol engine — stateful segment processing.
//!
//! The engine owns all TCBs and processes inbound segments via `on_segment`.
//! Timer-driven behavior is serviced via `on_tick` (tasks 5.14–5.17):
//! - TX drain: tx_ring → send_buf → segment (respecting effective_window) → transmit
//! - RTO: retransmit oldest unacked segment, exponential backoff, abort after max_retries
//! - Persist: zero-window probe at exponential backoff (capped 60s), NEVER aborts
//! - Keepalive: probe after idle timeout, abort after max probes → TimedOut
//! - TIME_WAIT: transition to CLOSED after 2*MSL, free TCB
//! - FIN_WAIT_2: timeout → free TCB
//! - Delayed-ACK: send cumulative ACK at 200ms deadline

use std::collections::{BTreeMap, HashMap};
use std::net::{Shutdown, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use crate::clock::Clock;
use crate::codec::{
    build_tcp_frame, ParsedTcpSegment, TcpFlags, TcpFrameParams, TcpOptions,
};
use crate::congestion::CongestionState;
use crate::contract::{
    ConnectionHandle, EngineCommand, OneshotSender, SocketOption,
};
use crate::error::TcpError;
use crate::isn::IsnGenerator;
use crate::seq::SeqNum;
use crate::state::{FourTuple, TcpState};
use crate::tcb::{RetransmitEntry, Tcb};

/// 2×MSL timeout for TIME_WAIT state (RFC 9293: 120 seconds).
pub const TIME_WAIT_DURATION: Duration = Duration::from_secs(120);

/// FIN_WAIT_2 timeout to prevent indefinite resource consumption.
pub const FIN_WAIT2_TIMEOUT: Duration = Duration::from_secs(60);

/// Delayed-ACK timeout: coalesce ACKs up to 200ms.
pub const DELAYED_ACK_TIMEOUT: Duration = Duration::from_millis(200);

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
    /// Maximum retransmission attempts before aborting with TimedOut.
    pub max_retries: u32,
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
            max_retries: 15,
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
            EngineCommand::Shutdown { key, how } => self.handle_shutdown(key, how),
            EngineCommand::SetOption { key, option } => {
                self.handle_set_option(key, option);
                Vec::new()
            }
            EngineCommand::Close { key, linger } => self.handle_close(key, linger),
        }
    }

    /// Service timers: drain tx_rings, segment and transmit, handle RTO,
    /// persist probes, keepalive, TIME_WAIT/FIN_WAIT_2 expiry, delayed-ACK.
    pub fn on_tick(&mut self, now: std::time::Instant) -> Vec<Vec<u8>> {
        let mut outbound = Vec::new();

        // Collect keys to iterate (avoid borrow conflict with &mut self).
        let keys: Vec<FourTuple> = self.tcbs.keys().copied().collect();

        // --- Pass 1: TIME_WAIT / FIN_WAIT_2 expiry (task 5.17) ---
        // These remove TCBs, so process them first.
        let mut expired_keys = Vec::new();
        for key in &keys {
            if let Some(tcb) = self.tcbs.get(key) {
                // TIME_WAIT → CLOSED after 2*MSL
                if tcb.state == TcpState::TimeWait {
                    if let Some(deadline) = tcb.time_wait_deadline {
                        if now >= deadline {
                            expired_keys.push(*key);
                        }
                    }
                }
                // FIN_WAIT_2 timeout → free TCB
                if tcb.state == TcpState::FinWait2 {
                    if let Some(deadline) = tcb.fin_wait2_deadline {
                        if now >= deadline {
                            expired_keys.push(*key);
                        }
                    }
                }
            }
        }
        for key in &expired_keys {
            if let Some(tcb) = self.tcbs.remove(key) {
                tcb.handle.set_state(TcpState::Closed);
                tcb.handle.set_eof();
                tcb.handle.notify_all();
            }
        }

        // --- Pass 2: Per-TCB timer processing ---
        let keys: Vec<FourTuple> = self.tcbs.keys().copied().collect();
        for key in keys {
            // --- RTO check (task 5.15) ---
            // Must run before tx-drain so aborted connections don't send new data.
            if let Some(tcb) = self.tcbs.get(&key) {
                if let Some(deadline) = tcb.rto_deadline {
                    if now >= deadline && !tcb.retransmit_queue.is_empty() {
                        // RTO expired — retransmit or abort
                        let tcb = self.tcbs.get_mut(&key).unwrap();
                        tcb.retransmit_count += 1;

                        if tcb.retransmit_count > self.config.max_retries {
                            // Abort: latch TimedOut, remove TCB
                            tcb.handle.latch_error(TcpError::TimedOut);
                            tcb.handle.set_state(TcpState::Closed);
                            tcb.handle.set_eof();
                            tcb.handle.notify_all();

                            // Fulfil pending connect if any
                            if let Some(pending) = self.pending_connects.remove(&key) {
                                pending.response.send(Err(TcpError::TimedOut));
                            }

                            self.tcbs.remove(&key);
                            continue;
                        }

                        // Retransmit oldest unacked segment
                        let flight_size = tcb.flight_size();
                        let mss = tcb.effective_mss();
                        tcb.congestion.on_rto(flight_size, mss);
                        tcb.congestion.backoff_rto();

                        // Rearm RTO with backed-off value
                        tcb.rto_deadline = Some(now + tcb.congestion.rto);

                        // Build retransmit frame from first entry
                        if let Some(entry) = tcb.retransmit_queue.first() {
                            let seq = entry.seq;
                            let offset = entry.offset;
                            let len = entry.len;
                            // Extract payload from send_buf
                            let payload: Vec<u8> = tcb.send_buf
                                .iter()
                                .skip(offset)
                                .take(len)
                                .copied()
                                .collect();

                            if !payload.is_empty() {
                                let frame = build_tcp_frame(&TcpFrameParams {
                                    src_mac: tcb.src_mac,
                                    dst_mac: tcb.dst_mac,
                                    src: tcb.key.local,
                                    dst: tcb.key.remote,
                                    seq,
                                    ack: tcb.rcv_nxt,
                                    flags: TcpFlags::ACK,
                                    window: encode_established_window(tcb),
                                    options: TcpOptions::default(),
                                    payload,
                                    ttl: tcb.ttl,
                                });
                                if let Ok(f) = frame {
                                    outbound.push(f);
                                }
                            }
                        }

                        continue;
                    }
                }
            }

            // --- Persist timer (task 5.16) ---
            // Send 1-byte zero-window probe; exponential backoff capped 60s; NEVER abort.
            if let Some(tcb) = self.tcbs.get(&key) {
                if let Some(deadline) = tcb.persist_deadline {
                    if now >= deadline {
                        let tcb = self.tcbs.get_mut(&key).unwrap();
                        // Send a 1-byte window probe (seq = snd_una - 1, carries no new data).
                        // The probe uses snd_nxt as seq to be a valid segment the peer can ACK.
                        let probe_seq = tcb.snd_una.add(tcb.flight_size());
                        let frame = build_tcp_frame(&TcpFrameParams {
                            src_mac: tcb.src_mac,
                            dst_mac: tcb.dst_mac,
                            src: tcb.key.local,
                            dst: tcb.key.remote,
                            seq: probe_seq,
                            ack: tcb.rcv_nxt,
                            flags: TcpFlags::ACK,
                            window: encode_established_window(tcb),
                            options: TcpOptions::default(),
                            payload: vec![0u8; 1],
                            ttl: tcb.ttl,
                        });
                        if let Ok(f) = frame {
                            outbound.push(f);
                        }

                        // Double backoff, cap at 60s
                        tcb.persist_backoff = std::cmp::min(
                            tcb.persist_backoff * 2,
                            Duration::from_secs(60),
                        );
                        // Rearm persist timer — NEVER abort
                        tcb.persist_deadline = Some(now + tcb.persist_backoff);
                        continue;
                    }
                }
            }

            // --- Keepalive timer (task 5.16) ---
            // Send probe after idle timeout, abort after max probes → TimedOut.
            if let Some(tcb) = self.tcbs.get(&key) {
                if let Some(deadline) = tcb.keepalive_deadline {
                    if now >= deadline && tcb.keepalive.is_some() {
                        let tcb = self.tcbs.get_mut(&key).unwrap();
                        let ka = tcb.keepalive.unwrap();

                        tcb.keepalive_probes_sent += 1;

                        if tcb.keepalive_probes_sent > ka.count {
                            // Max probes exceeded → abort with TimedOut
                            tcb.handle.latch_error(TcpError::TimedOut);
                            tcb.handle.set_state(TcpState::Closed);
                            tcb.handle.set_eof();
                            tcb.handle.notify_all();
                            self.tcbs.remove(&key);
                            continue;
                        }

                        // Send keepalive probe: ACK with seq = snd_una - 1
                        let probe_seq = tcb.snd_una.add(u32::MAX); // snd_una - 1
                        let frame = build_tcp_frame(&TcpFrameParams {
                            src_mac: tcb.src_mac,
                            dst_mac: tcb.dst_mac,
                            src: tcb.key.local,
                            dst: tcb.key.remote,
                            seq: probe_seq,
                            ack: tcb.rcv_nxt,
                            flags: TcpFlags::ACK,
                            window: encode_established_window(tcb),
                            options: TcpOptions::default(),
                            payload: Vec::new(),
                            ttl: tcb.ttl,
                        });
                        if let Ok(f) = frame {
                            outbound.push(f);
                        }

                        // Rearm at interval
                        tcb.keepalive_deadline = Some(now + ka.interval);
                        continue;
                    }
                }
            }

            // --- Delayed-ACK timer fire (task 5.17) ---
            // Send cumulative ACK when 200ms deadline expires.
            if let Some(tcb) = self.tcbs.get(&key) {
                if let Some(deadline) = tcb.delayed_ack_deadline {
                    if now >= deadline {
                        let tcb = self.tcbs.get_mut(&key).unwrap();
                        tcb.delayed_ack_deadline = None;
                        tcb.segments_since_ack = 0;
                        let frame = build_ack_for_tcb(tcb);
                        if let Ok(f) = frame {
                            outbound.push(f);
                        }
                        // Don't continue — still need to run tx-drain for this TCB
                    }
                }
            }

            // --- TX drain (task 5.14) ---
            // Only drain for ESTABLISHED connections (or CLOSE_WAIT where app can still write).
            let tcb = match self.tcbs.get_mut(&key) {
                Some(t) => t,
                None => continue,
            };

            if tcb.state != TcpState::Established && tcb.state != TcpState::CloseWait {
                continue;
            }

            // Step 1: Drain tx_ring → send_buf
            let mut drain_buf = [0u8; 4096];
            loop {
                let n = tcb.handle.tx_ring.read(&mut drain_buf);
                if n == 0 {
                    break;
                }
                tcb.send_buf.extend(&drain_buf[..n]);
            }

            // Step 2: Segment send_buf and transmit respecting effective_window + Nagle
            let prev_available = tcb.available_send_window();
            let mss = tcb.effective_mss() as usize;

            loop {
                let available_window = tcb.available_send_window() as usize;
                if available_window == 0 || tcb.send_buf.is_empty() {
                    break;
                }

                // Compute bytes already sent but not yet in retransmit queue
                // send_buf tracks all unsent data. The retransmit_queue tracks
                // already-sent bytes (by offset into send_buf).
                let already_sent_offset = tcb.retransmit_queue.last()
                    .map(|e| e.offset + e.len)
                    .unwrap_or(0);
                let unsent_in_buf = tcb.send_buf.len().saturating_sub(already_sent_offset);

                if unsent_in_buf == 0 {
                    break;
                }

                // Nagle check: should we send now?
                if !tcb.nagle_should_send(unsent_in_buf) {
                    break;
                }

                let segment_len = std::cmp::min(
                    std::cmp::min(unsent_in_buf, mss),
                    available_window,
                );

                if segment_len == 0 {
                    break;
                }

                // Extract payload from send_buf at the appropriate offset
                let payload: Vec<u8> = tcb.send_buf
                    .iter()
                    .skip(already_sent_offset)
                    .take(segment_len)
                    .copied()
                    .collect();

                let seq = tcb.snd_nxt;
                let frame = build_tcp_frame(&TcpFrameParams {
                    src_mac: tcb.src_mac,
                    dst_mac: tcb.dst_mac,
                    src: tcb.key.local,
                    dst: tcb.key.remote,
                    seq,
                    ack: tcb.rcv_nxt,
                    flags: TcpFlags::ACK,
                    window: encode_established_window(tcb),
                    options: TcpOptions::default(),
                    payload,
                    ttl: tcb.ttl,
                });

                if let Ok(f) = frame {
                    outbound.push(f);
                }

                // Update send state
                tcb.snd_nxt = tcb.snd_nxt.add(segment_len as u32);
                tcb.has_unacked_data = true;

                // Add to retransmit queue
                tcb.retransmit_queue.push(RetransmitEntry {
                    seq,
                    offset: already_sent_offset,
                    len: segment_len,
                    sent_at: now,
                    retransmit_count: 0,
                });

                // Arm RTO if not already armed
                if tcb.rto_deadline.is_none() {
                    tcb.rto_deadline = Some(now + tcb.congestion.rto);
                }
            }

            // Arm persist timer if window is zero and there's unsent data.
            // Also ensure we don't double-arm if persist is already running.
            let has_unsent = {
                let already_sent_offset = tcb.retransmit_queue.last()
                    .map(|e| e.offset + e.len)
                    .unwrap_or(0);
                tcb.send_buf.len() > already_sent_offset
            };
            if tcb.available_send_window() == 0 && has_unsent && tcb.persist_deadline.is_none() {
                tcb.persist_backoff = tcb.congestion.rto;
                tcb.persist_deadline = Some(now + tcb.persist_backoff);
            }

            // Clear persist timer if window opened
            if tcb.available_send_window() > 0 && tcb.persist_deadline.is_some() {
                tcb.persist_deadline = None;
                tcb.persist_backoff = Duration::from_secs(1);
            }

            // Arm keepalive timer for established connections with keepalive enabled.
            if tcb.state == TcpState::Established
                && tcb.keepalive.is_some()
                && tcb.keepalive_deadline.is_none()
            {
                let ka = tcb.keepalive.unwrap();
                tcb.keepalive_deadline = Some(now + ka.idle);
            }

            // Step 2b: If fin_pending and all data has been sent, emit FIN.
            if tcb.fin_pending {
                let already_sent_offset = tcb.retransmit_queue.last()
                    .map(|e| e.offset + e.len)
                    .unwrap_or(0);
                let unsent = tcb.send_buf.len().saturating_sub(already_sent_offset);
                if unsent == 0 && tcb.handle.tx_ring.available_read() == 0 {
                    // All data flushed — send FIN now.
                    let fin_frames = self.send_fin(&key);
                    outbound.extend(fin_frames);
                    // Re-borrow tcb after &mut self method call.
                    // The TCB may still exist (transitioned to FinWait1/LastAck).
                    // Skip the wake step below — connection is closing.
                    continue;
                }
            }

            // Step 3: Wake write_waker + condvar if send window opened
            // (i.e., if the tx_ring has space now that we drained it)
            let new_available = tcb.available_send_window();
            if new_available > 0 && (prev_available == 0 || tcb.handle.tx_ring.available_write() > 0) {
                tcb.handle.write_waker.wake();
                // Also notify via condvar for blocking writers
                let _guard = tcb.handle.notify_lock.lock().unwrap();
                tcb.handle.condvar.notify_all();
            }
        }

        outbound
    }

    // ===== Shutdown (task 5.19) =====

    /// Handle Shutdown command: set fin_pending, flush tx_ring → send_buf → FIN.
    fn handle_shutdown(&mut self, key: FourTuple, how: Shutdown) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(&key) {
            Some(t) => t,
            None => return Vec::new(),
        };

        match how {
            Shutdown::Write | Shutdown::Both => {
                // Only initiate FIN if in a state that allows sending.
                match tcb.state {
                    TcpState::Established | TcpState::CloseWait => {}
                    _ => return Vec::new(),
                }

                // Set fin_pending — on_tick will drain tx_ring → send_buf,
                // transmit all remaining data, then emit FIN.
                tcb.fin_pending = true;

                // Drain any remaining tx_ring data into send_buf now.
                let mut drain_buf = [0u8; 4096];
                loop {
                    let n = tcb.handle.tx_ring.read(&mut drain_buf);
                    if n == 0 {
                        break;
                    }
                    tcb.send_buf.extend(&drain_buf[..n]);
                }

                // If send_buf is empty (no unsent data), emit FIN immediately.
                let already_sent_offset = tcb.retransmit_queue.last()
                    .map(|e| e.offset + e.len)
                    .unwrap_or(0);
                let unsent = tcb.send_buf.len().saturating_sub(already_sent_offset);

                if unsent == 0 {
                    return self.send_fin(&key);
                }
                // Otherwise, on_tick will send remaining data then FIN.
            }
            Shutdown::Read => {
                // Shutdown(Read): set EOF so reads return 0. No FIN sent.
                tcb.handle.set_eof();
                tcb.handle.notify_all();
            }
        }

        Vec::new()
    }

    /// Emit a FIN segment and transition state appropriately.
    fn send_fin(&mut self, key: &FourTuple) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(key) {
            Some(t) => t,
            None => return Vec::new(),
        };

        tcb.fin_pending = false;

        let new_state = match tcb.state {
            TcpState::Established => TcpState::FinWait1,
            TcpState::CloseWait => TcpState::LastAck,
            _ => return Vec::new(),
        };

        let seq = tcb.snd_nxt;
        let frame = build_tcp_frame(&TcpFrameParams {
            src_mac: tcb.src_mac,
            dst_mac: tcb.dst_mac,
            src: tcb.key.local,
            dst: tcb.key.remote,
            seq,
            ack: tcb.rcv_nxt,
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: encode_established_window(tcb),
            options: TcpOptions::default(),
            payload: Vec::new(),
            ttl: tcb.ttl,
        });

        // FIN consumes one sequence number.
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        tcb.state = new_state;
        tcb.handle.set_state(new_state);

        // Arm RTO for the FIN.
        if tcb.rto_deadline.is_none() {
            tcb.rto_deadline = Some(self.clock.now() + tcb.congestion.rto);
        }

        match frame {
            Ok(f) => vec![f],
            Err(_) => Vec::new(),
        }
    }

    // ===== Close (task 5.19) =====

    /// Handle Close command: honor SO_LINGER semantics.
    /// - linger = None (default): initiate graceful FIN (same as Shutdown::Write).
    /// - linger = Some(Duration::ZERO): send RST, discard unsent data.
    /// - linger = Some(t) where t > 0: same as graceful FIN (blocking handled app-side).
    fn handle_close(&mut self, key: FourTuple, linger: Option<Duration>) -> Vec<Vec<u8>> {
        if let Some(dur) = linger {
            if dur.is_zero() {
                // SO_LINGER with timeout=0 → RST, discard unsent data.
                return self.handle_close_rst(key);
            }
        }
        // Default / non-zero linger: graceful FIN (flush-before-FIN).
        self.handle_shutdown(key, Shutdown::Write)
    }

    /// Close with RST: discard all pending data, send RST, remove TCB.
    fn handle_close_rst(&mut self, key: FourTuple) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get(&key) {
            Some(t) => t,
            None => return Vec::new(),
        };

        // Build RST segment.
        let frame = build_tcp_frame(&TcpFrameParams {
            src_mac: tcb.src_mac,
            dst_mac: tcb.dst_mac,
            src: tcb.key.local,
            dst: tcb.key.remote,
            seq: tcb.snd_nxt,
            ack: tcb.rcv_nxt,
            flags: TcpFlags::RST | TcpFlags::ACK,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
            ttl: tcb.ttl,
        });

        // Notify handle and remove TCB.
        let handle = tcb.handle.clone();
        handle.latch_error(TcpError::ConnectionAborted);
        handle.set_state(TcpState::Closed);
        handle.set_eof();
        handle.notify_all();
        self.tcbs.remove(&key);

        match frame {
            Ok(f) => vec![f],
            Err(_) => Vec::new(),
        }
    }

    // ===== SetOption (task 5.19) =====

    /// Handle SetOption command: update Tcb fields for the specified socket option.
    fn handle_set_option(&mut self, key: FourTuple, option: SocketOption) {
        let tcb = match self.tcbs.get_mut(&key) {
            Some(t) => t,
            None => return,
        };

        match option {
            SocketOption::Nodelay(val) => {
                tcb.nodelay = val;
            }
            SocketOption::Keepalive(config) => {
                tcb.keepalive = config;
                if config.is_none() {
                    // Disable keepalive: clear timer.
                    tcb.keepalive_deadline = None;
                    tcb.keepalive_probes_sent = 0;
                }
            }
            SocketOption::Linger(val) => {
                tcb.linger = val;
                // Also update the handle so Drop can read it.
                *tcb.handle.linger.lock().unwrap() = val;
            }
            SocketOption::RecvBufSize(size) => {
                // Cannot resize ring post-creation; update rwnd cap.
                tcb.recv_buf_size = size;
                tcb.rcv_wnd = std::cmp::min(tcb.rcv_wnd, size as u32);
            }
            SocketOption::SendBufSize(size) => {
                tcb.send_buf_size = size;
            }
            SocketOption::ReuseAddr(val) => {
                tcb.reuseaddr = val;
            }
            SocketOption::Ttl(val) => {
                tcb.ttl = val;
            }
            SocketOption::ReadTimeout(_) | SocketOption::WriteTimeout(_) | SocketOption::Nonblocking(_) => {
                // These are handled app-side (condvar wait_timeout / WouldBlock).
                // No engine-side action needed.
            }
        }
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
            TcpState::Established => self.handle_established(seg, four_tuple),
            TcpState::FinWait1 => self.handle_fin_wait_1(seg, four_tuple),
            TcpState::FinWait2 => self.handle_fin_wait_2(seg, four_tuple),
            TcpState::CloseWait => self.handle_close_wait(seg, four_tuple),
            TcpState::LastAck => self.handle_last_ack(seg, four_tuple),
            TcpState::Closing => self.handle_closing(seg, four_tuple),
            TcpState::TimeWait => self.handle_time_wait(seg, four_tuple),
            _ => Vec::new(),
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

    // ===== Established state: in-order data delivery + cumulative ACK =====

    fn handle_established(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut outbound = Vec::new();

        // --- Process ACK (cumulative acknowledgment) ---
        if seg.flags.contains(TcpFlags::ACK) {
            // Validate: ack must be in range (snd_una, snd_nxt]
            if seg.ack.gt(tcb.snd_una) && seg.ack.le(tcb.snd_nxt) {
                let bytes_acked = seg.ack.diff(tcb.snd_una);

                // Advance snd_una
                tcb.snd_una = seg.ack;

                // Free acknowledged retransmit entries
                tcb.retransmit_queue.retain(|entry| {
                    let entry_end = entry.seq.add(entry.len as u32);
                    entry_end.gt(tcb.snd_una)
                });

                // Update congestion control
                let mss = tcb.effective_mss();
                tcb.congestion.on_ack(bytes_acked, mss);

                // Wake write_waker — send window may have opened
                tcb.handle.write_waker.wake();
            }

            // Update send window from peer's advertisement (apply window scale)
            if seg.seq.gt(tcb.snd_wl1)
                || (seg.seq == tcb.snd_wl1 && seg.ack.le(tcb.snd_nxt) && seg.ack.gt(tcb.snd_wl2)
                    || seg.ack == tcb.snd_wl2)
            {
                tcb.snd_wnd = (seg.window as u32) << tcb.snd_scale;
                tcb.snd_wl1 = seg.seq;
                tcb.snd_wl2 = seg.ack;
            }
        }

        // --- Process data delivery (in-order + out-of-order) ---
        if !seg.payload.is_empty() {
            // Reset keepalive state on data receipt (connection is active)
            tcb.last_data_received = Some(self.clock.now());
            tcb.keepalive_probes_sent = 0;
            if tcb.keepalive.is_some() {
                let ka = tcb.keepalive.unwrap();
                tcb.keepalive_deadline = Some(self.clock.now() + ka.idle);
            }

            if seg.seq == tcb.rcv_nxt {
                // In-order: deliver payload to rx_ring
                let written = tcb.handle.rx_ring.write(&seg.payload);
                tcb.rcv_nxt = tcb.rcv_nxt.add(written as u32);

                // Drain contiguous segments from reorder_buffer.
                // Rebase keys by subtracting `written`, then drain key==0 entries.
                // Each drained entry counts toward the delayed-ACK segment counter.
                let mut drained_count = 0u32;
                if !tcb.reorder_buffer.is_empty() {
                    let mut rebased: BTreeMap<u32, Vec<u8>> = std::mem::take(&mut tcb.reorder_buffer)
                        .into_iter()
                        .map(|(k, v)| (k - written as u32, v))
                        .collect();

                    while let Some(data) = rebased.remove(&0) {
                        let w = tcb.handle.rx_ring.write(&data);
                        tcb.rcv_nxt = tcb.rcv_nxt.add(w as u32);
                        drained_count += 1;
                        if w > 0 && !rebased.is_empty() {
                            rebased = rebased
                                .into_iter()
                                .map(|(k, v)| (k - w as u32, v))
                                .collect();
                        }
                    }

                    tcb.reorder_buffer = rebased;
                }

                // Wake app readers
                tcb.handle.notify_all();

                // Delayed-ACK: coalesce ACKs up to 200ms or every-other-segment.
                // Count this segment plus any drained reorder buffer entries.
                tcb.segments_since_ack += 1 + drained_count;

                if tcb.segments_since_ack >= 2 {
                    // Every-other-segment rule: send ACK immediately
                    tcb.segments_since_ack = 0;
                    tcb.delayed_ack_deadline = None;

                    let ack_frame = build_tcp_frame(&TcpFrameParams {
                        src_mac: tcb.src_mac,
                        dst_mac: tcb.dst_mac,
                        src: tcb.key.local,
                        dst: tcb.key.remote,
                        seq: tcb.snd_nxt,
                        ack: tcb.rcv_nxt,
                        flags: TcpFlags::ACK,
                        window: encode_established_window(tcb),
                        options: TcpOptions::default(),
                        payload: Vec::new(),
                        ttl: tcb.ttl,
                    });

                    if let Ok(frame) = ack_frame {
                        outbound.push(frame);
                    }
                } else if tcb.delayed_ack_deadline.is_none() {
                    // Arm 200ms delayed-ACK timer
                    tcb.delayed_ack_deadline =
                        Some(self.clock.now() + DELAYED_ACK_TIMEOUT);
                }
            } else if seg.seq.gt(tcb.rcv_nxt) {
                // Out-of-order: buffer in reorder_buffer keyed on seq.diff(rcv_nxt)
                let offset = seg.seq.diff(tcb.rcv_nxt);
                tcb.reorder_buffer.insert(offset, seg.payload.clone());

                // Send immediate ACK for OOO (dup-ACK with ack_num == rcv_nxt)
                // Delayed-ACK spec: send immediately on out-of-order segments
                tcb.segments_since_ack = 0;
                tcb.delayed_ack_deadline = None;

                let dup_ack = build_tcp_frame(&TcpFrameParams {
                    src_mac: tcb.src_mac,
                    dst_mac: tcb.dst_mac,
                    src: tcb.key.local,
                    dst: tcb.key.remote,
                    seq: tcb.snd_nxt,
                    ack: tcb.rcv_nxt,
                    flags: TcpFlags::ACK,
                    window: encode_established_window(tcb),
                    options: TcpOptions::default(),
                    payload: Vec::new(),
                    ttl: tcb.ttl,
                });

                if let Ok(frame) = dup_ack {
                    outbound.push(frame);
                }
            }
            // else: seg.seq < rcv_nxt — retransmitted/old data, silently drop
        }

        // --- FIN handling in ESTABLISHED state ---
        // FIN occupies the seq number after the last data byte.
        // After in-order data delivery, rcv_nxt has advanced past the data.
        // The FIN is valid if seg.seq + payload.len() == rcv_nxt (FIN is in-order).
        if seg.flags.contains(TcpFlags::FIN) {
            let fin_seq = seg.seq.add(seg.payload.len() as u32);
            if fin_seq == tcb.rcv_nxt {
                // FIN received: advance rcv_nxt past the FIN, set EOF, transition to CLOSE_WAIT
                tcb.rcv_nxt = tcb.rcv_nxt.add(1); // FIN consumes one sequence number
                tcb.handle.set_eof();
                tcb.state = TcpState::CloseWait;
                tcb.handle.set_state(TcpState::CloseWait);
                tcb.handle.notify_all();

                // FIN triggers immediate ACK — cancel delayed-ACK
                tcb.segments_since_ack = 0;
                tcb.delayed_ack_deadline = None;

                // Send ACK for the FIN (replaces any data ACK already in outbound)
                outbound.clear();
                let ack_frame = build_tcp_frame(&TcpFrameParams {
                    src_mac: tcb.src_mac,
                    dst_mac: tcb.dst_mac,
                    src: tcb.key.local,
                    dst: tcb.key.remote,
                    seq: tcb.snd_nxt,
                    ack: tcb.rcv_nxt,
                    flags: TcpFlags::ACK,
                    window: encode_established_window(tcb),
                    options: TcpOptions::default(),
                    payload: Vec::new(),
                    ttl: tcb.ttl,
                });
                if let Ok(frame) = ack_frame {
                    outbound.push(frame);
                }
            }
        }

        outbound
    }

    // ===== FIN teardown state handlers (task 5.11) =====

    /// FIN_WAIT_1: We sent FIN, waiting for ACK of our FIN.
    /// Possible transitions:
    /// - ACK of our FIN → FIN_WAIT_2
    /// - FIN from peer (simultaneous close) → CLOSING
    /// - FIN+ACK from peer → TIME_WAIT (our FIN acked + peer FIN)
    fn handle_fin_wait_1(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut outbound = Vec::new();

        // Process ACK — check if it acknowledges our FIN
        let our_fin_acked = if seg.flags.contains(TcpFlags::ACK) {
            // Our FIN's seq is snd_nxt - 1 (FIN was the last thing we sent).
            // ACK for our FIN means ack >= snd_nxt.
            if seg.ack.gt(tcb.snd_una) && seg.ack.le(tcb.snd_nxt) {
                let bytes_acked = seg.ack.diff(tcb.snd_una);
                tcb.snd_una = seg.ack;
                tcb.retransmit_queue.retain(|entry| {
                    entry.seq.add(entry.len as u32).gt(tcb.snd_una)
                });
                let mss = tcb.effective_mss();
                tcb.congestion.on_ack(bytes_acked, mss);
            }
            seg.ack == tcb.snd_nxt
        } else {
            false
        };

        // Deliver any data payload before processing FIN
        if !seg.payload.is_empty() && seg.seq == tcb.rcv_nxt {
            let written = tcb.handle.rx_ring.write(&seg.payload);
            tcb.rcv_nxt = tcb.rcv_nxt.add(written as u32);
            tcb.handle.notify_all();
        }

        // Check for peer's FIN (FIN is at seg.seq + payload.len())
        let fin_seq = seg.seq.add(seg.payload.len() as u32);
        let peer_fin = seg.flags.contains(TcpFlags::FIN) && fin_seq == tcb.rcv_nxt;

        if our_fin_acked && peer_fin {
            // Both FINs: ACK our FIN + peer FIN → TIME_WAIT
            tcb.rcv_nxt = tcb.rcv_nxt.add(1); // FIN consumes one seq
            tcb.handle.set_eof();
            tcb.state = TcpState::TimeWait;
            tcb.handle.set_state(TcpState::TimeWait);
            tcb.time_wait_deadline = Some(self.clock.now() + TIME_WAIT_DURATION);
            tcb.handle.notify_all();

            let ack = build_ack_for_tcb(tcb);
            if let Ok(frame) = ack {
                outbound.push(frame);
            }
        } else if our_fin_acked {
            // Only our FIN acked → FIN_WAIT_2
            tcb.state = TcpState::FinWait2;
            tcb.handle.set_state(TcpState::FinWait2);
            tcb.fin_wait2_deadline = Some(self.clock.now() + FIN_WAIT2_TIMEOUT);
            tcb.rto_deadline = None;
        } else if peer_fin {
            // Only peer's FIN (simultaneous close) → CLOSING
            tcb.rcv_nxt = tcb.rcv_nxt.add(1);
            tcb.handle.set_eof();
            tcb.state = TcpState::Closing;
            tcb.handle.set_state(TcpState::Closing);
            tcb.handle.notify_all();

            let ack = build_ack_for_tcb(tcb);
            if let Ok(frame) = ack {
                outbound.push(frame);
            }
        }

        outbound
    }

    /// FIN_WAIT_2: Our FIN was acked, waiting for peer's FIN.
    fn handle_fin_wait_2(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut outbound = Vec::new();

        // Deliver any data payload
        if !seg.payload.is_empty() && seg.seq == tcb.rcv_nxt {
            let written = tcb.handle.rx_ring.write(&seg.payload);
            tcb.rcv_nxt = tcb.rcv_nxt.add(written as u32);
            tcb.handle.notify_all();

            let ack = build_ack_for_tcb(tcb);
            if let Ok(frame) = ack {
                outbound.push(frame);
            }
        }

        // Check for peer's FIN (FIN is at seg.seq + payload.len())
        let fin_seq = seg.seq.add(seg.payload.len() as u32);
        if seg.flags.contains(TcpFlags::FIN) && fin_seq == tcb.rcv_nxt {
            tcb.rcv_nxt = tcb.rcv_nxt.add(1);
            tcb.handle.set_eof();
            tcb.state = TcpState::TimeWait;
            tcb.handle.set_state(TcpState::TimeWait);
            tcb.time_wait_deadline = Some(self.clock.now() + TIME_WAIT_DURATION);
            tcb.fin_wait2_deadline = None;
            tcb.handle.notify_all();

            // Replace any data ACK with a FIN ACK covering everything
            outbound.clear();
            let ack = build_ack_for_tcb(tcb);
            if let Ok(frame) = ack {
                outbound.push(frame);
            }
        }

        outbound
    }

    /// CLOSE_WAIT: Peer sent FIN, we haven't sent ours yet.
    /// Process ACKs for outstanding data. The app will eventually close/shutdown
    /// which sends our FIN (via on_command → Shutdown/Close).
    fn handle_close_wait(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        // Process ACKs (advance snd_una, free retransmit entries)
        if seg.flags.contains(TcpFlags::ACK) {
            if seg.ack.gt(tcb.snd_una) && seg.ack.le(tcb.snd_nxt) {
                let bytes_acked = seg.ack.diff(tcb.snd_una);
                tcb.snd_una = seg.ack;
                tcb.retransmit_queue.retain(|entry| {
                    entry.seq.add(entry.len as u32).gt(tcb.snd_una)
                });
                let mss = tcb.effective_mss();
                tcb.congestion.on_ack(bytes_acked, mss);
                tcb.handle.write_waker.wake();
            }
        }

        Vec::new()
    }

    /// LAST_ACK: We sent our FIN (after being in CLOSE_WAIT), waiting for ACK.
    fn handle_last_ack(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        // ACK of our FIN → CLOSED, remove TCB
        if seg.flags.contains(TcpFlags::ACK) && seg.ack == tcb.snd_nxt {
            let handle = tcb.handle.clone();
            handle.set_state(TcpState::Closed);
            handle.notify_all();
            self.tcbs.remove(four_tuple);
        }

        Vec::new()
    }

    /// CLOSING: Simultaneous close — waiting for ACK of our FIN.
    fn handle_closing(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        // ACK of our FIN → TIME_WAIT
        if seg.flags.contains(TcpFlags::ACK) && seg.ack == tcb.snd_nxt {
            tcb.state = TcpState::TimeWait;
            tcb.handle.set_state(TcpState::TimeWait);
            tcb.time_wait_deadline = Some(self.clock.now() + TIME_WAIT_DURATION);
            tcb.rto_deadline = None;
        }

        Vec::new()
    }

    /// TIME_WAIT: Both FINs exchanged, holding TCB for 2*MSL.
    /// Only ACKs (retransmitted FIN) should arrive here; respond with ACK and restart timer.
    fn handle_time_wait(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get_mut(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        // If peer retransmits FIN, re-ACK and restart the timer
        if seg.flags.contains(TcpFlags::FIN) {
            tcb.time_wait_deadline = Some(self.clock.now() + TIME_WAIT_DURATION);
            let ack = build_ack_for_tcb(tcb);
            return match ack {
                Ok(frame) => vec![frame],
                Err(_) => Vec::new(),
            };
        }

        Vec::new()
    }

    // ===== RST handling (RFC 5961) =====

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
                TcpState::Established
                | TcpState::FinWait1
                | TcpState::FinWait2
                | TcpState::CloseWait
                | TcpState::Closing
                | TcpState::LastAck
                | TcpState::TimeWait => {
                    // RFC 5961 RST validation for established/close states
                    return self.handle_rst_rfc5961(seg, four_tuple);
                }
                _ => {}
            }
        }
        Vec::new()
    }

    /// RFC 5961 RST validation for established and close-state connections.
    /// - Exact seq (== rcv_nxt) → abort connection, latch ConnectionReset
    /// - In-window non-exact → send challenge ACK
    /// - Out-of-window → silently drop
    fn handle_rst_rfc5961(
        &mut self,
        seg: &ParsedTcpSegment,
        four_tuple: &FourTuple,
    ) -> Vec<Vec<u8>> {
        let tcb = match self.tcbs.get(four_tuple) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let rcv_nxt = tcb.rcv_nxt;
        let rcv_wnd = tcb.rcv_wnd;

        if seg.seq == rcv_nxt {
            // Exact match: abort connection immediately
            let handle = tcb.handle.clone();
            handle.latch_error(TcpError::ConnectionReset);
            handle.set_state(TcpState::Closed);
            handle.set_eof();
            handle.notify_all();
            self.tcbs.remove(four_tuple);
            Vec::new()
        } else if is_in_window(seg.seq, rcv_nxt, rcv_wnd) {
            // In-window but not exact: send challenge ACK
            let challenge_ack = build_tcp_frame(&TcpFrameParams {
                src_mac: tcb.src_mac,
                dst_mac: tcb.dst_mac,
                src: tcb.key.local,
                dst: tcb.key.remote,
                seq: tcb.snd_nxt,
                ack: tcb.rcv_nxt,
                flags: TcpFlags::ACK,
                window: encode_established_window(tcb),
                options: TcpOptions::default(),
                payload: Vec::new(),
                ttl: tcb.ttl,
            });
            match challenge_ack {
                Ok(frame) => vec![frame],
                Err(_) => Vec::new(),
            }
        } else {
            // Out-of-window: silently drop
            Vec::new()
        }
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

/// Build a plain ACK frame for the given TCB state.
/// Free function to avoid borrow conflicts when tcb is borrowed from self.tcbs.
fn build_ack_for_tcb(tcb: &Tcb) -> Result<Vec<u8>, TcpError> {
    build_tcp_frame(&TcpFrameParams {
        src_mac: tcb.src_mac,
        dst_mac: tcb.dst_mac,
        src: tcb.key.local,
        dst: tcb.key.remote,
        seq: tcb.snd_nxt,
        ack: tcb.rcv_nxt,
        flags: TcpFlags::ACK,
        window: encode_established_window(tcb),
        options: TcpOptions::default(),
        payload: Vec::new(),
        ttl: tcb.ttl,
    })
}

/// Check if a sequence number falls within the receive window [rcv_nxt, rcv_nxt + rcv_wnd).
/// Uses modular arithmetic for wrap-around safety.
#[inline]
fn is_in_window(seq: SeqNum, rcv_nxt: SeqNum, rcv_wnd: u32) -> bool {
    if rcv_wnd == 0 {
        return seq == rcv_nxt;
    }
    let rcv_end = rcv_nxt.add(rcv_wnd);
    // seq is in window if rcv_nxt <= seq < rcv_end (modular comparison)
    seq == rcv_nxt || (seq.gt(rcv_nxt) && rcv_end.gt(seq))
}

/// Encode receive window for an established connection (apply window scale).
/// Implements Silly Window Syndrome (SWS) avoidance on the receiver side:
/// withhold window update until available space >= min(MSS, half buffer).
/// This prevents the receiver from advertising tiny window increments.
#[inline]
fn encode_established_window(tcb: &Tcb) -> u16 {
    let available = tcb.handle.rx_ring.available_write() as u32;
    let half_buffer = (tcb.recv_buf_size as u32) / 2;
    let mss = tcb.effective_mss() as u32;
    let threshold = std::cmp::min(mss, half_buffer);

    // SWS avoidance: if available space is below threshold, advertise zero window
    let wnd = if available < threshold {
        0u32
    } else {
        std::cmp::min(available, tcb.rcv_wnd)
    };
    let scaled = wnd >> tcb.rcv_scale;
    std::cmp::min(scaled, u16::MAX as u32) as u16
}

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
    use crate::contract::{oneshot_channel, CommandSender, EngineWakeup, KeepaliveConfig, SocketOption};
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

    // === Established state tests (task 5.9) ===

    /// Helper: complete a three-way handshake and return the engine + four_tuple.
    /// The client side connects and transitions to Established.
    fn setup_established_connection(
        engine: &mut TcpEngine,
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

        // SYN-ACK from peer
        let syn_ack = make_syn_ack_segment(remote, local, 2000, iss.add(1).0);
        engine.on_segment(&syn_ack);

        assert_eq!(
            engine.tcbs.get(&four_tuple).unwrap().state,
            TcpState::Established
        );
        (four_tuple, handle)
    }

    fn make_data_segment(
        src: SocketAddr,
        dst: SocketAddr,
        seq: u32,
        ack: u32,
        payload: &[u8],
    ) -> ParsedTcpSegment {
        ParsedTcpSegment {
            src,
            dst,
            seq: SeqNum(seq),
            ack: SeqNum(ack),
            flags: TcpFlags::ACK | TcpFlags::PSH,
            window: 65535,
            options: TcpOptions::default(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn established_in_order_data_delivers_to_rx_ring_and_acks() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // First segment: delayed-ACK defers the ACK
        let payload1 = b"Hello";
        let seg1 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            payload1,
        );
        let frames = engine.on_segment(&seg1);
        assert!(frames.is_empty()); // Delayed-ACK: no immediate ACK

        // Data should still be delivered to rx_ring
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, payload1.len());
        assert_eq!(&buf[..n], payload1);

        // Second segment: every-other-segment rule → immediate ACK
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt2 = tcb.rcv_nxt;
        let payload2 = b", TCP!";
        let seg2 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt2.0,
            snd_nxt.0,
            payload2,
        );
        let frames = engine.on_segment(&seg2);

        // Should produce exactly one ACK
        assert_eq!(frames.len(), 1);
        let ack_parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack_parsed.flags.contains(TcpFlags::ACK));
        assert_eq!(
            ack_parsed.ack,
            rcv_nxt.add((payload1.len() + payload2.len()) as u32)
        );

        // rcv_nxt should have advanced past both payloads
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(
            tcb.rcv_nxt,
            rcv_nxt.add((payload1.len() + payload2.len()) as u32)
        );
    }

    #[test]
    fn established_cumulative_ack_advances_snd_una_and_frees_retransmit() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        let snd_una_before = tcb.snd_una;
        let rcv_nxt = tcb.rcv_nxt;

        // Simulate sent data by advancing snd_nxt and adding retransmit entry
        tcb.snd_nxt = tcb.snd_nxt.add(100);
        tcb.retransmit_queue.push(crate::tcb::RetransmitEntry {
            seq: snd_una_before,
            offset: 0,
            len: 50,
            sent_at: std::time::Instant::now(),
            retransmit_count: 0,
        });
        tcb.retransmit_queue.push(crate::tcb::RetransmitEntry {
            seq: snd_una_before.add(50),
            offset: 50,
            len: 50,
            sent_at: std::time::Instant::now(),
            retransmit_count: 0,
        });

        // Peer ACKs the first 50 bytes
        let ack_seg = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_una_before.add(50),
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&ack_seg);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        // snd_una should advance to ack value
        assert_eq!(tcb.snd_una, snd_una_before.add(50));
        // First retransmit entry should be freed, second retained
        assert_eq!(tcb.retransmit_queue.len(), 1);
        assert_eq!(tcb.retransmit_queue[0].seq, snd_una_before.add(50));
    }

    #[test]
    fn established_window_scale_applied_to_peer_window() {
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

        // SYN-ACK with wscale=5 and window=512
        let mut syn_ack = make_syn_ack_segment(remote, local, 2000, iss.add(1).0);
        syn_ack.options.window_scale = Some(5);
        syn_ack.window = 512;
        engine.on_segment(&syn_ack);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.snd_scale, 5);
        // After handshake, snd_wnd = window << scale = 512 << 5 = 16384
        assert_eq!(tcb.snd_wnd, 512 << 5);

        // Now in established: peer sends data with updated window
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;
        let data_seg = ParsedTcpSegment {
            src: remote,
            dst: local,
            seq: rcv_nxt,
            ack: SeqNum(snd_nxt.0),
            flags: TcpFlags::ACK | TcpFlags::PSH,
            window: 1024, // raw window in header
            options: TcpOptions::default(),
            payload: b"x".to_vec(),
        };
        engine.on_segment(&data_seg);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        // snd_wnd updated with scale: 1024 << 5 = 32768
        assert_eq!(tcb.snd_wnd, 1024 << 5);
    }

    #[test]
    fn established_multiple_in_order_segments() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let mut rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Send 3 segments in order. With delayed-ACK:
        // seg 1: deferred (segments_since_ack = 1)
        // seg 2: immediate ACK (every-other-segment, segments_since_ack resets to 0)
        // seg 3: deferred (segments_since_ack = 1)
        let payloads = [b"AAA".as_slice(), b"BBBB".as_slice(), b"CC".as_slice()];

        // Segment 1: deferred
        let seg1 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            payloads[0],
        );
        let frames = engine.on_segment(&seg1);
        assert!(frames.is_empty()); // Delayed
        rcv_nxt = rcv_nxt.add(payloads[0].len() as u32);

        // Segment 2: immediate ACK (every-other-segment)
        let seg2 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            payloads[1],
        );
        let frames = engine.on_segment(&seg2);
        assert_eq!(frames.len(), 1);
        rcv_nxt = rcv_nxt.add(payloads[1].len() as u32);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(ack.ack, rcv_nxt);

        // Segment 3: deferred (counter reset after seg 2)
        let seg3 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            payloads[2],
        );
        let frames = engine.on_segment(&seg3);
        assert!(frames.is_empty()); // Delayed

        // All data should be in rx_ring
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, 9); // 3 + 4 + 2
        assert_eq!(&buf[..n], b"AAABBBBCC");
    }

    #[test]
    fn established_pure_ack_no_data_produces_no_response() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_una = tcb.snd_una;

        // Peer sends pure ACK (no data) — should produce no response
        let pure_ack = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_una, // Not advancing (same as current snd_una)
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&pure_ack);
        assert!(frames.is_empty());
    }

    #[test]
    fn established_ack_beyond_snd_nxt_ignored() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_una_before = tcb.snd_una;
        let snd_nxt = tcb.snd_nxt;

        // Peer sends ACK for data we never sent (ack > snd_nxt)
        let bad_ack = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt.add(1000), // Way beyond what we sent
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&bad_ack);

        // snd_una should NOT advance
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.snd_una, snd_una_before);
    }

    #[test]
    fn established_ack_below_snd_una_ignored() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        // Simulate: advance snd_una past initial
        tcb.snd_nxt = tcb.snd_nxt.add(100);
        tcb.snd_una = tcb.snd_una.add(50);
        let snd_una_before = tcb.snd_una;

        // Peer sends old ACK (ack <= snd_una — duplicate/stale)
        let old_ack = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_una_before, // Same as current snd_una, not advancing
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&old_ack);

        // snd_una unchanged
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.snd_una, snd_una_before);
    }

    // === Out-of-order reorder buffer tests (task 5.10) ===

    #[test]
    fn ooo_segment_buffered_and_dup_ack_sent() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Send segment 2 before segment 1 (gap at rcv_nxt)
        let ooo_payload = b"WORLD";
        let ooo_seg = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.add(5).0, // Skip 5 bytes ahead
            snd_nxt.0,
            ooo_payload,
        );
        let frames = engine.on_segment(&ooo_seg);

        // Should produce dup-ACK with ack_num == rcv_nxt (unchanged)
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert_eq!(ack.ack, rcv_nxt); // dup-ACK: ack_num == rcv_nxt

        // rcv_nxt should NOT advance
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.rcv_nxt, rcv_nxt);

        // Segment should be buffered
        assert_eq!(tcb.reorder_buffer.len(), 1);
        assert!(tcb.reorder_buffer.contains_key(&5));

        // rx_ring should be empty
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, 0);
    }

    #[test]
    fn gap_fill_drains_reorder_buffer() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Send segment 2 (OOO)
        let seg2 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.add(5).0,
            snd_nxt.0,
            b"WORLD",
        );
        engine.on_segment(&seg2);

        // Now send segment 1 (fills the gap)
        let seg1 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"HELLO",
        );
        let frames = engine.on_segment(&seg1);

        // Should produce ACK covering both segments
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(ack.ack, rcv_nxt.add(10)); // 5 + 5 = 10

        // rcv_nxt should advance past both segments
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.rcv_nxt, rcv_nxt.add(10));

        // Reorder buffer should be empty
        assert!(tcb.reorder_buffer.is_empty());

        // All data should be in rx_ring in correct order
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, 10);
        assert_eq!(&buf[..n], b"HELLOWORLD");
    }

    #[test]
    fn multiple_ooo_segments_reassembled_correctly() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Send segments 3, 2 (out of order), then 1 (fills gap)
        // Segment 3: offset 8..11
        let seg3 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.add(8).0,
            snd_nxt.0,
            b"CCC",
        );
        engine.on_segment(&seg3);

        // Segment 2: offset 4..8
        let seg2 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.add(4).0,
            snd_nxt.0,
            b"BBBB",
        );
        engine.on_segment(&seg2);

        // Reorder buffer should have 2 entries
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.reorder_buffer.len(), 2);

        // Segment 1: offset 0..4 (fills the gap)
        let seg1 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"AAAA",
        );
        let frames = engine.on_segment(&seg1);

        // ACK should cover all three segments
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(ack.ack, rcv_nxt.add(11));

        // Reorder buffer empty
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.reorder_buffer.is_empty());

        // Data reassembled in order
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, 11);
        assert_eq!(&buf[..n], b"AAAABBBBCCC");
    }

    #[test]
    fn partial_gap_fill_drains_only_contiguous() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Buffer seg at offset 3 and seg at offset 10
        let seg_near = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.add(3).0,
            snd_nxt.0,
            b"BBB",
        );
        engine.on_segment(&seg_near);

        let seg_far = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.add(10).0,
            snd_nxt.0,
            b"DDD",
        );
        engine.on_segment(&seg_far);

        // Fill the first gap (bytes 0..3)
        let seg_fill = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"AAA",
        );
        let frames = engine.on_segment(&seg_fill);

        // ACK should cover only up to where contiguous data ends (offset 6)
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(ack.ack, rcv_nxt.add(6)); // AAA + BBB = 6 bytes

        // Reorder buffer should still have the far segment (now at offset 4)
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.reorder_buffer.len(), 1);
        assert!(tcb.reorder_buffer.contains_key(&4)); // 10 - 6 = 4

        // rx_ring has the first 6 bytes
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, 6);
        assert_eq!(&buf[..n], b"AAABBB");
    }

    #[test]
    fn retransmitted_segment_below_rcv_nxt_ignored() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Deliver an in-order segment first
        let seg1 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"HELLO",
        );
        engine.on_segment(&seg1);

        // Now send a retransmit of the same data (seq < rcv_nxt)
        let retransmit = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0, // Old seq, now < rcv_nxt
            snd_nxt.0,
            b"HELLO",
        );
        let frames = engine.on_segment(&retransmit);

        // Should produce no output (silently dropped)
        assert!(frames.is_empty());

        // rx_ring should only have the original data
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"HELLO");
    }

    // === FIN teardown tests (task 5.11) ===

    fn make_fin_segment(
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
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        }
    }

    fn make_fin_data_segment(
        src: SocketAddr,
        dst: SocketAddr,
        seq: u32,
        ack: u32,
        payload: &[u8],
    ) -> ParsedTcpSegment {
        ParsedTcpSegment {
            src,
            dst,
            seq: SeqNum(seq),
            ack: SeqNum(ack),
            flags: TcpFlags::FIN | TcpFlags::ACK | TcpFlags::PSH,
            window: 65535,
            options: TcpOptions::default(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn established_fin_transitions_to_close_wait() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Peer sends FIN
        let fin = make_fin_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
        );
        let frames = engine.on_segment(&fin);

        // Should produce ACK for the FIN
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert_eq!(ack.ack, rcv_nxt.add(1)); // FIN consumes 1 seq

        // State should be CLOSE_WAIT
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::CloseWait);
        assert_eq!(handle.tcp_state(), TcpState::CloseWait);

        // EOF should be set
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn established_fin_with_data_delivers_data_then_eof() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Peer sends FIN with data
        let fin_data = make_fin_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"final data",
        );
        let frames = engine.on_segment(&fin_data);

        // Should produce ACK covering data + FIN
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(ack.ack, rcv_nxt.add(10 + 1)); // 10 bytes data + 1 FIN

        // Data should be in rx_ring
        let mut buf = vec![0u8; 1024];
        let n = handle.rx_ring.read(&mut buf);
        assert_eq!(n, 10);
        assert_eq!(&buf[..n], b"final data");

        // EOF should be set
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(handle.tcp_state(), TcpState::CloseWait);
    }

    #[test]
    fn fin_wait_1_ack_of_fin_transitions_to_fin_wait_2() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Simulate: our side sends FIN (transition to FIN_WAIT_1)
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::FinWait1;
        tcb.snd_nxt = tcb.snd_nxt.add(1); // FIN consumes 1 seq
        handle.set_state(TcpState::FinWait1);

        let snd_nxt = tcb.snd_nxt;
        let rcv_nxt = tcb.rcv_nxt;

        // Peer ACKs our FIN
        let ack = make_ack_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0, // acks our FIN
        );
        engine.on_segment(&ack);

        // Should transition to FIN_WAIT_2
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::FinWait2);
        assert_eq!(handle.tcp_state(), TcpState::FinWait2);
        // FIN_WAIT_2 timer should be armed
        assert!(tcb.fin_wait2_deadline.is_some());
    }

    #[test]
    fn fin_wait_1_peer_fin_simultaneous_close_transitions_to_closing() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Simulate: our side sends FIN (transition to FIN_WAIT_1)
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::FinWait1;
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        handle.set_state(TcpState::FinWait1);

        let snd_una = tcb.snd_una;
        let rcv_nxt = tcb.rcv_nxt;

        // Peer sends FIN without ACKing our FIN (simultaneous close)
        let fin = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_una, // Does NOT ack our FIN
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&fin);

        // Should send ACK for peer's FIN
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert_eq!(ack.ack, rcv_nxt.add(1));

        // Should transition to CLOSING
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::Closing);
        assert_eq!(handle.tcp_state(), TcpState::Closing);
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn fin_wait_1_peer_fin_ack_transitions_to_time_wait() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Simulate FIN_WAIT_1
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::FinWait1;
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        handle.set_state(TcpState::FinWait1);

        let snd_nxt = tcb.snd_nxt;
        let rcv_nxt = tcb.rcv_nxt;

        // Peer sends FIN+ACK that also ACKs our FIN
        let fin_ack = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_nxt, // ACKs our FIN
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&fin_ack);

        // Should send ACK and transition to TIME_WAIT
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::TimeWait);
        assert_eq!(handle.tcp_state(), TcpState::TimeWait);
        assert!(tcb.time_wait_deadline.is_some());
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn fin_wait_2_peer_fin_transitions_to_time_wait() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Simulate FIN_WAIT_2
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::FinWait2;
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        tcb.snd_una = tcb.snd_nxt;
        handle.set_state(TcpState::FinWait2);

        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Peer sends FIN
        let fin = make_fin_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
        );
        let frames = engine.on_segment(&fin);

        // Should ACK and transition to TIME_WAIT
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(ack.ack, rcv_nxt.add(1));

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::TimeWait);
        assert!(tcb.time_wait_deadline.is_some());
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn closing_ack_of_our_fin_transitions_to_time_wait() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Simulate CLOSING state
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::Closing;
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        handle.set_state(TcpState::Closing);

        let snd_nxt = tcb.snd_nxt;
        let rcv_nxt = tcb.rcv_nxt;

        // Peer ACKs our FIN
        let ack = make_ack_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
        );
        engine.on_segment(&ack);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::TimeWait);
        assert!(tcb.time_wait_deadline.is_some());
    }

    #[test]
    fn last_ack_ack_of_our_fin_removes_tcb() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Simulate LAST_ACK state
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::LastAck;
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        handle.set_state(TcpState::LastAck);

        let snd_nxt = tcb.snd_nxt;
        let rcv_nxt = tcb.rcv_nxt;

        // Peer ACKs our FIN
        let ack = make_ack_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
        );
        engine.on_segment(&ack);

        // TCB should be removed
        assert!(!engine.tcbs.contains_key(&four_tuple));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
    }

    #[test]
    fn time_wait_retransmitted_fin_restarts_timer_and_acks() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Simulate TIME_WAIT
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::TimeWait;
        let now = clock.now();
        tcb.time_wait_deadline = Some(now + super::TIME_WAIT_DURATION);
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        tcb.snd_una = tcb.snd_nxt;
        handle.set_state(TcpState::TimeWait);

        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Advance time a bit
        clock.advance(Duration::from_secs(30));

        // Peer retransmits FIN
        let fin = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: SeqNum(rcv_nxt.0.wrapping_sub(1)), // FIN's seq is one before rcv_nxt
            ack: SeqNum(snd_nxt.0),
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&fin);

        // Should re-ACK
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));

        // Timer should be restarted (deadline > previous)
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.time_wait_deadline.unwrap() > now + Duration::from_secs(30));
        assert_eq!(tcb.state, TcpState::TimeWait);
    }

    #[test]
    fn close_wait_processes_acks() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        // Simulate CLOSE_WAIT with data in flight
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::CloseWait;
        let snd_una_before = tcb.snd_una;
        tcb.snd_nxt = tcb.snd_nxt.add(100);
        tcb.retransmit_queue.push(crate::tcb::RetransmitEntry {
            seq: snd_una_before,
            offset: 0,
            len: 100,
            sent_at: std::time::Instant::now(),
            retransmit_count: 0,
        });

        let rcv_nxt = tcb.rcv_nxt;

        // Peer ACKs some data
        let ack = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt,
            ack: snd_una_before.add(50),
            flags: TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        engine.on_segment(&ack);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.snd_una, snd_una_before.add(50));
        assert_eq!(tcb.state, TcpState::CloseWait); // Stays in CLOSE_WAIT
    }

    // === RST validation tests (task 5.12 — RFC 5961) ===

    #[test]
    fn rst_exact_seq_aborts_established_connection() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;

        // RST with exact seq == rcv_nxt
        let rst = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt, // Exact match
            ack: SeqNum(0),
            flags: TcpFlags::RST,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&rst);

        // Should produce no outbound frames
        assert!(frames.is_empty());

        // TCB should be removed
        assert!(!engine.tcbs.contains_key(&four_tuple));

        // Handle should have ConnectionReset latched
        assert!(matches!(
            handle.peek_error(),
            Some(TcpError::ConnectionReset)
        ));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn rst_in_window_non_exact_sends_challenge_ack() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;

        // RST with seq in window but not exact (rcv_nxt + 5)
        let rst = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt.add(5), // In-window but not exact
            ack: SeqNum(0),
            flags: TcpFlags::RST,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&rst);

        // Should send challenge ACK
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert!(!ack.flags.contains(TcpFlags::RST));
        assert_eq!(ack.ack, rcv_nxt); // Challenge ACK has current rcv_nxt

        // TCB should still exist (not aborted)
        assert!(engine.tcbs.contains_key(&four_tuple));
        assert_eq!(handle.tcp_state(), TcpState::Established);
        assert!(handle.peek_error().is_none());
    }

    #[test]
    fn rst_out_of_window_silently_dropped() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let rcv_wnd = tcb.rcv_wnd;

        // RST with seq way out of window
        let rst = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt.add(rcv_wnd + 1000), // Out of window
            ack: SeqNum(0),
            flags: TcpFlags::RST,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&rst);

        // Should produce no output (silently dropped)
        assert!(frames.is_empty());

        // TCB should still exist
        assert!(engine.tcbs.contains_key(&four_tuple));
        assert_eq!(handle.tcp_state(), TcpState::Established);
        assert!(handle.peek_error().is_none());
    }

    #[test]
    fn rst_exact_seq_aborts_fin_wait_1() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Move to FIN_WAIT_1
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::FinWait1;
        tcb.snd_nxt = tcb.snd_nxt.add(1);
        handle.set_state(TcpState::FinWait1);
        let rcv_nxt = tcb.rcv_nxt;

        // RST with exact seq
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
        engine.on_segment(&rst);

        assert!(!engine.tcbs.contains_key(&four_tuple));
        assert!(matches!(handle.peek_error(), Some(TcpError::ConnectionReset)));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
    }

    #[test]
    fn rst_in_window_non_exact_in_fin_wait_2_sends_challenge_ack() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Move to FIN_WAIT_2
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::FinWait2;
        handle.set_state(TcpState::FinWait2);
        let rcv_nxt = tcb.rcv_nxt;

        // RST in-window non-exact
        let rst = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt.add(10),
            ack: SeqNum(0),
            flags: TcpFlags::RST,
            window: 0,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&rst);

        // Challenge ACK
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert!(engine.tcbs.contains_key(&four_tuple));
    }

    #[test]
    fn rst_exact_seq_aborts_time_wait() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Move to TIME_WAIT
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::TimeWait;
        handle.set_state(TcpState::TimeWait);
        let rcv_nxt = tcb.rcv_nxt;

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
        engine.on_segment(&rst);

        assert!(!engine.tcbs.contains_key(&four_tuple));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
    }

    // === Delayed-ACK tests (task 5.13) ===

    #[test]
    fn delayed_ack_first_segment_defers_ack() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // First in-order data segment: ACK should be deferred
        let seg = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"hello",
        );
        let frames = engine.on_segment(&seg);

        // No immediate ACK — delayed
        assert!(frames.is_empty());

        // delayed_ack_deadline should be armed
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.delayed_ack_deadline.is_some());
        assert_eq!(tcb.segments_since_ack, 1);

        // Verify deadline is ~200ms from now
        let deadline = tcb.delayed_ack_deadline.unwrap();
        let expected = clock.now() + DELAYED_ACK_TIMEOUT;
        assert_eq!(deadline, expected);
    }

    #[test]
    fn delayed_ack_second_segment_triggers_immediate_ack() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // First segment: deferred
        let seg1 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"hello",
        );
        let frames = engine.on_segment(&seg1);
        assert!(frames.is_empty());

        // Second segment: every-other-segment rule → immediate ACK
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt2 = tcb.rcv_nxt;
        let seg2 = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt2.0,
            snd_nxt.0,
            b"world",
        );
        let frames = engine.on_segment(&seg2);

        // Should produce an ACK
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert_eq!(ack.ack, rcv_nxt.add(10)); // "hello" + "world" = 10 bytes

        // Counter reset, deadline cleared
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.segments_since_ack, 0);
        assert!(tcb.delayed_ack_deadline.is_none());
    }

    #[test]
    fn delayed_ack_ooo_sends_immediate_ack() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // Out-of-order segment (gap of 10 bytes)
        let seg = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0 + 10, // gap
            snd_nxt.0,
            b"world",
        );
        let frames = engine.on_segment(&seg);

        // OOO → immediate dup-ACK
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert_eq!(ack.ack, rcv_nxt); // dup-ACK: still at old rcv_nxt

        // Counter and deadline reset
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.segments_since_ack, 0);
        assert!(tcb.delayed_ack_deadline.is_none());
    }

    #[test]
    fn delayed_ack_fin_sends_immediate_ack() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;

        // First segment to arm delayed-ACK timer
        let seg = make_data_segment(
            four_tuple.remote,
            four_tuple.local,
            rcv_nxt.0,
            snd_nxt.0,
            b"data",
        );
        engine.on_segment(&seg);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.delayed_ack_deadline.is_some());
        let rcv_nxt_after = tcb.rcv_nxt;

        // FIN arrives (immediately after data)
        let fin = ParsedTcpSegment {
            src: four_tuple.remote,
            dst: four_tuple.local,
            seq: rcv_nxt_after,
            ack: SeqNum(snd_nxt.0),
            flags: TcpFlags::FIN | TcpFlags::ACK,
            window: 65535,
            options: TcpOptions::default(),
            payload: Vec::new(),
        };
        let frames = engine.on_segment(&fin);

        // FIN → immediate ACK (delayed-ACK cancelled)
        assert_eq!(frames.len(), 1);
        let ack = parse_tcp_packet(&frames[0]).unwrap();
        assert!(ack.flags.contains(TcpFlags::ACK));

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.segments_since_ack, 0);
        assert!(tcb.delayed_ack_deadline.is_none());
        assert_eq!(tcb.state, TcpState::CloseWait);
    }

    // === Nagle algorithm tests (task 5.13) ===

    #[test]
    fn nagle_buffers_small_write_when_unacked_data() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.has_unacked_data = true;

        // Small write (< MSS) with unacked data → should buffer
        assert!(!tcb.nagle_should_send(100));
    }

    #[test]
    fn nagle_sends_when_no_unacked_data() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.has_unacked_data = false;

        // No unacked data → send immediately regardless of size
        assert!(tcb.nagle_should_send(100));
    }

    #[test]
    fn nagle_sends_when_nodelay_set() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.has_unacked_data = true;
        tcb.nodelay = true;

        // TCP_NODELAY → always send
        assert!(tcb.nagle_should_send(1));
    }

    #[test]
    fn nagle_sends_when_data_fills_mss() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.has_unacked_data = true;
        tcb.nodelay = false;

        // Data fills MSS → send even with unacked data
        assert!(tcb.nagle_should_send(1460));
        assert!(tcb.nagle_should_send(2000));
    }

    // === SWS avoidance tests (task 5.13) ===

    #[test]
    fn sws_avoidance_withholds_small_window() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.rcv_scale = 0; // No scaling for simpler test
        tcb.rcv_wnd = 65535;
        tcb.recv_buf_size = 65536;

        // Fill rx_ring almost completely — leave only a small amount free
        // rx_ring capacity is 65536. Write enough to leave < min(MSS, half_buf)
        // half_buf = 32768, MSS = 1460, threshold = min(1460, 32768) = 1460
        // Need to fill ring so available_write < 1460
        let fill_data = vec![0u8; 65536 - 1000]; // Leave only 1000 bytes free
        tcb.handle.rx_ring.write(&fill_data);

        let window = encode_established_window(tcb);

        // SWS avoidance: available (1000) < threshold (1460) → advertise 0
        assert_eq!(window, 0);
    }

    #[test]
    fn sws_avoidance_opens_window_above_threshold() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.rcv_scale = 0;
        tcb.rcv_wnd = 65535;
        tcb.recv_buf_size = 65536;

        // Leave more than threshold free
        // threshold = min(1460, 32768) = 1460
        let fill_data = vec![0u8; 65536 - 2000]; // Leave 2000 bytes free
        tcb.handle.rx_ring.write(&fill_data);

        let window = encode_established_window(tcb);

        // Available (2000) >= threshold (1460) → advertise actual window
        assert!(window > 0);
        assert_eq!(window, 2000); // min(2000, 65535) >> 0 = 2000
    }

    #[test]
    fn sws_avoidance_empty_ring_advertises_full_window() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.rcv_scale = 0;
        tcb.rcv_wnd = 65535;
        tcb.recv_buf_size = 65536;

        // Empty ring → full window
        let window = encode_established_window(tcb);
        assert!(window > 0);
        // available = 65536, min(65536, 65535) = 65535, >> 0 = 65535
        assert_eq!(window, 65535);
    }

    // === on_tick: TX drain tests (task 5.14) ===

    #[test]
    fn on_tick_drains_tx_ring_and_sends_segments() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Set up peer_mss so effective_mss is 1460
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;

        // App writes data to tx_ring
        let data = b"Hello, world!";
        handle.tx_ring.write(data);

        let now = clock.now();
        let frames = engine.on_tick(now);

        // Should produce one data frame
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::ACK));
        assert_eq!(parsed.payload, data);
    }

    #[test]
    fn on_tick_respects_effective_window() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 10; // Very small receiver window

        // Write more data than the window allows
        let data = vec![0xAA; 100];
        handle.tx_ring.write(&data);

        let now = clock.now();
        let frames = engine.on_tick(now);

        // Should only send up to effective_window bytes
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(parsed.payload.len(), 10); // Limited by snd_wnd
    }

    #[test]
    fn on_tick_segments_at_mss_boundary() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 100; // Small MSS for testing
        tcb.snd_wnd = 65535;
        tcb.nodelay = true; // Disable Nagle so all segments go out

        // Write data larger than one MSS
        let data = vec![0xBB; 250];
        handle.tx_ring.write(&data);

        let now = clock.now();
        let frames = engine.on_tick(now);

        // Should produce 3 segments: 100 + 100 + 50
        assert_eq!(frames.len(), 3);
        let p1 = parse_tcp_packet(&frames[0]).unwrap();
        let p2 = parse_tcp_packet(&frames[1]).unwrap();
        let p3 = parse_tcp_packet(&frames[2]).unwrap();
        assert_eq!(p1.payload.len(), 100);
        assert_eq!(p2.payload.len(), 100);
        assert_eq!(p3.payload.len(), 50);
    }

    #[test]
    fn on_tick_arms_rto_on_first_send() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        // Clear any existing RTO deadline from the handshake
        tcb.rto_deadline = None;

        handle.tx_ring.write(b"test data");

        let now = clock.now();
        engine.on_tick(now);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.rto_deadline.is_some());
        // RTO should be armed at now + congestion.rto
        assert_eq!(tcb.rto_deadline.unwrap(), now + tcb.congestion.rto);
    }

    #[test]
    fn on_tick_populates_retransmit_queue() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.rto_deadline = None;

        handle.tx_ring.write(b"segment data");

        let now = clock.now();
        engine.on_tick(now);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.retransmit_queue.len(), 1);
        assert_eq!(tcb.retransmit_queue[0].len, 12); // "segment data".len()
        assert_eq!(tcb.retransmit_queue[0].retransmit_count, 0);
    }

    #[test]
    fn on_tick_advances_snd_nxt() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        let snd_nxt_before = tcb.snd_nxt;
        tcb.rto_deadline = None;

        handle.tx_ring.write(b"hello");

        let now = clock.now();
        engine.on_tick(now);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.snd_nxt, snd_nxt_before.add(5));
    }

    #[test]
    fn on_tick_wakes_write_waker_when_window_open() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{RawWaker, RawWakerVTable, Waker};

        static WOKEN: AtomicBool = AtomicBool::new(false);

        fn clone_fn(ptr: *const ()) -> RawWaker { RawWaker::new(ptr, &VTABLE) }
        fn wake_fn(_: *const ()) { WOKEN.store(true, Ordering::Release); }
        fn wake_by_ref_fn(_: *const ()) { WOKEN.store(true, Ordering::Release); }
        fn drop_fn(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.rto_deadline = None;

        // Register a write waker
        let raw = RawWaker::new(std::ptr::null(), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw) };
        handle.write_waker.register(&waker);
        WOKEN.store(false, Ordering::Release);

        handle.tx_ring.write(b"data");

        let now = clock.now();
        engine.on_tick(now);

        // Write waker should have been woken (window is open after drain)
        assert!(WOKEN.load(Ordering::Acquire));
    }

    #[test]
    fn on_tick_no_send_when_zero_window() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 0; // Zero receiver window
        tcb.rto_deadline = None;

        handle.tx_ring.write(b"data that should not be sent");

        let now = clock.now();
        let frames = engine.on_tick(now);

        // No frames should be sent when window is zero
        assert!(frames.is_empty());
    }

    #[test]
    fn on_tick_nagle_buffers_small_write() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.nodelay = false; // Nagle enabled
        tcb.has_unacked_data = true; // Data in flight
        tcb.rto_deadline = None;

        // Small write (< MSS) with unacked data → Nagle buffers it
        handle.tx_ring.write(b"hi");

        let now = clock.now();
        let frames = engine.on_tick(now);

        // Nagle should prevent sending
        assert!(frames.is_empty());
    }

    #[test]
    fn on_tick_nodelay_sends_immediately() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.nodelay = true; // TCP_NODELAY set
        tcb.has_unacked_data = true;
        tcb.rto_deadline = None;

        handle.tx_ring.write(b"hi");

        let now = clock.now();
        let frames = engine.on_tick(now);

        // TCP_NODELAY → send immediately regardless of Nagle
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(parsed.payload, b"hi");
    }

    // === on_tick: RTO tests (task 5.15) ===

    #[test]
    fn on_tick_rto_retransmits_oldest_segment() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.rto_deadline = None;

        // Send initial data
        handle.tx_ring.write(b"retransmit me");
        let t0 = clock.now();
        engine.on_tick(t0);

        // Verify data was sent
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.retransmit_queue.len(), 1);
        let rto = tcb.congestion.rto;

        // Advance clock past RTO
        clock.advance(rto + Duration::from_millis(1));
        let t1 = clock.now();

        let frames = engine.on_tick(t1);

        // Should retransmit
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(parsed.payload, b"retransmit me");
    }

    #[test]
    fn on_tick_rto_doubles_on_each_retransmit() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.rto_deadline = None;

        handle.tx_ring.write(b"data");
        let t0 = clock.now();
        engine.on_tick(t0);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let initial_rto = tcb.congestion.rto; // Should be 1s (initial)

        // First RTO expiry
        clock.advance(initial_rto + Duration::from_millis(1));
        engine.on_tick(clock.now());

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.retransmit_count, 1);
        // RTO should have doubled
        assert_eq!(tcb.congestion.rto, (initial_rto * 2).min(Duration::from_secs(60)));
    }

    #[test]
    fn on_tick_rto_aborts_after_max_retries() {
        let (mut engine, clock) = make_engine();
        let mut config = EngineConfig::default();
        config.max_retries = 3; // Low for testing
        engine.config = config.clone();

        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.rto_deadline = None;

        handle.tx_ring.write(b"will timeout");
        engine.on_tick(clock.now());

        // Expire RTO max_retries + 1 times to trigger abort
        for _ in 0..=config.max_retries {
            let tcb = engine.tcbs.get(&four_tuple);
            if tcb.is_none() {
                break; // Already aborted
            }
            let rto = tcb.unwrap().congestion.rto;
            clock.advance(rto + Duration::from_millis(1));
            engine.on_tick(clock.now());
        }

        // TCB should be removed
        assert!(!engine.tcbs.contains_key(&four_tuple));

        // Handle should have TimedOut error latched
        assert!(matches!(handle.peek_error(), Some(TcpError::TimedOut)));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn on_tick_rto_collapses_cwnd() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.rto_deadline = None;
        let initial_cwnd = tcb.congestion.cwnd;

        handle.tx_ring.write(b"data");
        engine.on_tick(clock.now());

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rto = tcb.congestion.rto;

        // Trigger RTO
        clock.advance(rto + Duration::from_millis(1));
        engine.on_tick(clock.now());

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        // cwnd should have collapsed to 1 MSS
        assert_eq!(tcb.congestion.cwnd, tcb.effective_mss() as u32);
        // ssthresh should be max(flight/2, 2*MSS)
        assert!(tcb.congestion.ssthresh >= 2 * tcb.effective_mss() as u32);
        // cwnd was at initial, so after collapse it's 1 MSS < initial
        assert!(tcb.congestion.cwnd < initial_cwnd);
    }

    #[test]
    fn on_tick_no_rto_when_retransmit_queue_empty() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        // Set an RTO deadline but with empty retransmit queue
        tcb.rto_deadline = Some(clock.now());
        tcb.retransmit_queue.clear();

        clock.advance(Duration::from_secs(2));
        let frames = engine.on_tick(clock.now());

        // No retransmit should fire with empty queue
        assert!(frames.is_empty());
        // TCB should still exist
        assert!(engine.tcbs.contains_key(&four_tuple));
    }

    #[test]
    fn on_tick_only_runs_for_established_connections() {
        let (mut engine, clock) = make_engine();
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let four_tuple = FourTuple { local, remote };
        let handle = make_handle(four_tuple);
        let (resp_tx, _resp_rx) = oneshot_channel();

        // Set up a connection in SYN_SENT state
        engine.on_command(EngineCommand::Connect {
            local,
            remote,
            src_mac: [0x02, 0, 0, 0, 0, 1],
            dst_mac: [0x02, 0, 0, 0, 0, 2],
            handle: handle.clone(),
            response: resp_tx,
        });

        // Write data to tx_ring
        handle.tx_ring.write(b"should not be sent");

        let now = clock.now();
        let frames = engine.on_tick(now);

        // Should NOT send data in SYN_SENT state (tx-drain only for ESTABLISHED/CLOSE_WAIT)
        assert!(frames.is_empty());
    }

    // === on_tick: Persist timer tests (task 5.16) ===

    #[test]
    fn on_tick_persist_sends_probe_when_zero_window() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 0; // Zero window from peer

        // App writes data that can't be sent (window = 0)
        handle.tx_ring.write(b"Hello");

        // First tick: drains tx_ring into send_buf, detects zero window, arms persist
        let now = clock.now();
        let frames = engine.on_tick(now);
        assert!(frames.is_empty()); // No data sent (zero window)

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.persist_deadline.is_some());
        let persist_deadline = tcb.persist_deadline.unwrap();

        // Advance past persist deadline
        clock.advance(persist_deadline.duration_since(now) + Duration::from_millis(1));
        let now2 = clock.now();
        let frames = engine.on_tick(now2);

        // Should send a 1-byte probe
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert_eq!(parsed.payload.len(), 1);
        assert!(parsed.flags.contains(TcpFlags::ACK));
    }

    #[test]
    fn on_tick_persist_exponential_backoff_capped_60s() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 0;

        handle.tx_ring.write(b"data");

        // Arm persist
        let now = clock.now();
        engine.on_tick(now);

        // Fire persist multiple times and check backoff
        let mut last_backoff = Duration::from_secs(0);
        for i in 0..10 {
            let tcb = engine.tcbs.get(&four_tuple).unwrap();
            let deadline = tcb.persist_deadline.unwrap();
            let backoff = deadline.duration_since(clock.now());
            // Each backoff should be <= 60s
            assert!(backoff <= Duration::from_secs(60));

            if i > 0 {
                // Backoff should be roughly double the previous (or capped)
                assert!(backoff >= last_backoff || backoff == Duration::from_secs(60));
            }
            last_backoff = backoff;

            clock.advance(backoff + Duration::from_millis(1));
            let frames = engine.on_tick(clock.now());
            assert_eq!(frames.len(), 1); // Probe sent
        }

        // After many probes, backoff should be capped at 60s
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.persist_backoff <= Duration::from_secs(60));
    }

    #[test]
    fn on_tick_persist_never_aborts() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 0;

        handle.tx_ring.write(b"data");
        engine.on_tick(clock.now());

        // Fire 100 persist probes — connection must NOT be aborted
        for _ in 0..100 {
            let tcb = engine.tcbs.get(&four_tuple).unwrap();
            let deadline = tcb.persist_deadline.unwrap();
            let advance = deadline.duration_since(clock.now()) + Duration::from_millis(1);
            clock.advance(advance);
            engine.on_tick(clock.now());
        }

        // TCB still exists and not TimedOut
        assert!(engine.tcbs.contains_key(&four_tuple));
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::Established);
        assert!(handle.peek_error().is_none());
    }

    #[test]
    fn on_tick_persist_cleared_when_window_opens() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 0;

        handle.tx_ring.write(b"data");
        engine.on_tick(clock.now());

        // Persist should be armed
        assert!(engine.tcbs.get(&four_tuple).unwrap().persist_deadline.is_some());

        // Peer opens window (via ACK with non-zero window)
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.snd_wnd = 65535;

        clock.advance(Duration::from_millis(1));
        engine.on_tick(clock.now());

        // Persist should be cleared since window opened
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.persist_deadline.is_none());
    }

    // === on_tick: Keepalive tests (task 5.16) ===

    #[test]
    fn on_tick_keepalive_sends_probe_after_idle() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.keepalive = Some(KeepaliveConfig {
            idle: Duration::from_secs(10),
            interval: Duration::from_secs(2),
            count: 3,
        });

        // First tick arms keepalive
        engine.on_tick(clock.now());
        assert!(engine.tcbs.get(&four_tuple).unwrap().keepalive_deadline.is_some());

        // Advance past idle timeout
        clock.advance(Duration::from_secs(11));
        let frames = engine.on_tick(clock.now());

        // Should send a keepalive probe
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::ACK));
        assert!(parsed.payload.is_empty()); // Keepalive probe has no payload

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.keepalive_probes_sent, 1);
    }

    #[test]
    fn on_tick_keepalive_aborts_after_max_probes() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.keepalive = Some(KeepaliveConfig {
            idle: Duration::from_secs(5),
            interval: Duration::from_secs(1),
            count: 3,
        });

        // Arm keepalive
        engine.on_tick(clock.now());

        // Fire idle timeout + 3 interval probes + 1 more (exceeds count)
        clock.advance(Duration::from_secs(6)); // past idle
        engine.on_tick(clock.now()); // probe 1

        clock.advance(Duration::from_secs(2)); // past interval
        engine.on_tick(clock.now()); // probe 2

        clock.advance(Duration::from_secs(2));
        engine.on_tick(clock.now()); // probe 3

        clock.advance(Duration::from_secs(2));
        engine.on_tick(clock.now()); // probe 4 → exceeds count(3), abort

        // Connection should be removed and TimedOut latched
        assert!(!engine.tcbs.contains_key(&four_tuple));
        assert!(matches!(handle.peek_error(), Some(TcpError::TimedOut)));
    }

    #[test]
    fn on_tick_keepalive_reset_on_data_receipt() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        tcb.keepalive = Some(KeepaliveConfig {
            idle: Duration::from_secs(5),
            interval: Duration::from_secs(1),
            count: 3,
        });

        // Arm keepalive
        engine.on_tick(clock.now());

        // Advance past idle timeout and send one probe
        clock.advance(Duration::from_secs(6));
        engine.on_tick(clock.now());
        assert_eq!(engine.tcbs.get(&four_tuple).unwrap().keepalive_probes_sent, 1);

        // Now receive data from peer — should reset keepalive
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;
        let data_seg = make_data_segment(remote, local, rcv_nxt.0, snd_nxt.0, b"hello");
        engine.on_segment(&data_seg);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.keepalive_probes_sent, 0);
        assert!(tcb.last_data_received.is_some());
    }

    // === on_tick: TIME_WAIT / FIN_WAIT_2 expiry tests (task 5.17) ===

    #[test]
    fn on_tick_time_wait_expires_after_2msl() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Transition to TIME_WAIT manually
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::TimeWait;
        tcb.handle.set_state(TcpState::TimeWait);
        tcb.time_wait_deadline = Some(clock.now() + TIME_WAIT_DURATION);

        // Advance just short of 2*MSL — TCB should still exist
        clock.advance(TIME_WAIT_DURATION - Duration::from_millis(1));
        engine.on_tick(clock.now());
        assert!(engine.tcbs.contains_key(&four_tuple));

        // Advance past 2*MSL
        clock.advance(Duration::from_millis(2));
        engine.on_tick(clock.now());

        // TCB should be removed and state set to CLOSED
        assert!(!engine.tcbs.contains_key(&four_tuple));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
    }

    #[test]
    fn on_tick_fin_wait2_expires_after_timeout() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Transition to FIN_WAIT_2 manually
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::FinWait2;
        tcb.handle.set_state(TcpState::FinWait2);
        tcb.fin_wait2_deadline = Some(clock.now() + FIN_WAIT2_TIMEOUT);

        // Advance just short — still exists
        clock.advance(FIN_WAIT2_TIMEOUT - Duration::from_millis(1));
        engine.on_tick(clock.now());
        assert!(engine.tcbs.contains_key(&four_tuple));

        // Advance past timeout
        clock.advance(Duration::from_millis(2));
        engine.on_tick(clock.now());

        // TCB should be removed
        assert!(!engine.tcbs.contains_key(&four_tuple));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
    }

    // === on_tick: Delayed-ACK timer fire tests (task 5.17) ===

    #[test]
    fn on_tick_delayed_ack_fires_at_200ms() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        // Receive one data segment — should arm delayed-ACK (not immediate)
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;
        let data_seg = make_data_segment(remote, local, rcv_nxt.0, snd_nxt.0, b"hello");
        let frames = engine.on_segment(&data_seg);
        // First segment: no immediate ACK (delayed-ACK defers)
        assert!(frames.is_empty());

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.delayed_ack_deadline.is_some());

        // Advance to 200ms deadline
        clock.advance(DELAYED_ACK_TIMEOUT + Duration::from_millis(1));
        let frames = engine.on_tick(clock.now());

        // Should send cumulative ACK
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::ACK));
        assert!(parsed.payload.is_empty());

        // delayed_ack_deadline should be cleared
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.delayed_ack_deadline.is_none());
        assert_eq!(tcb.segments_since_ack, 0);
    }

    #[test]
    fn on_tick_delayed_ack_not_before_deadline() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let rcv_nxt = tcb.rcv_nxt;
        let snd_nxt = tcb.snd_nxt;
        let data_seg = make_data_segment(remote, local, rcv_nxt.0, snd_nxt.0, b"hi");
        engine.on_segment(&data_seg);

        // Advance less than 200ms
        clock.advance(Duration::from_millis(100));
        let frames = engine.on_tick(clock.now());

        // No ACK should fire (not yet at deadline)
        assert!(frames.is_empty());
        assert!(engine.tcbs.get(&four_tuple).unwrap().delayed_ack_deadline.is_some());
    }

    // === on_command: Shutdown tests (task 5.18/5.19) ===

    #[test]
    fn shutdown_write_with_empty_send_buf_sends_fin_immediately() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let frames = engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Write,
        });

        // FIN should be sent immediately (no pending data)
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::FIN));
        assert!(parsed.flags.contains(TcpFlags::ACK));

        // State should transition to FIN_WAIT_1
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::FinWait1);
        assert_eq!(handle.tcp_state(), TcpState::FinWait1);
        assert!(!tcb.fin_pending); // Should be cleared after FIN sent
    }

    #[test]
    fn shutdown_write_in_close_wait_transitions_to_last_ack() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Move to CLOSE_WAIT (peer sent FIN)
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.state = TcpState::CloseWait;
        handle.set_state(TcpState::CloseWait);

        let frames = engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Write,
        });

        // FIN should be sent
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::FIN));

        // State should be LAST_ACK
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::LastAck);
        assert_eq!(handle.tcp_state(), TcpState::LastAck);
    }

    #[test]
    fn shutdown_write_with_pending_data_sets_fin_pending() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Write data to tx_ring that hasn't been drained yet
        handle.tx_ring.write(b"pending data");

        let frames = engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Write,
        });

        // No FIN yet — data needs to be sent first
        assert!(frames.is_empty());

        // fin_pending should be set
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.fin_pending);
        // Data should have been drained from tx_ring to send_buf
        assert!(!tcb.send_buf.is_empty());
        assert_eq!(tcb.send_buf.len(), 12); // "pending data"

        // on_tick should drain data then send FIN
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;

        let frames = engine.on_tick(clock.now());
        // Should send data segment + FIN
        assert!(frames.len() >= 1);

        // After draining, FIN should have been sent
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::FinWait1);
    }

    #[test]
    fn shutdown_read_sets_eof() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let frames = engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Read,
        });

        // No outbound frames for read shutdown
        assert!(frames.is_empty());

        // EOF should be set
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));

        // State should remain ESTABLISHED
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::Established);
    }

    #[test]
    fn shutdown_both_sends_fin() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let frames = engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Both,
        });

        // FIN should be sent (no pending data)
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::FIN));

        // State should be FIN_WAIT_1
        assert_eq!(handle.tcp_state(), TcpState::FinWait1);
    }

    #[test]
    fn shutdown_on_nonexistent_key_does_nothing() {
        let (mut engine, _clock) = make_engine();
        let fake_key = FourTuple {
            local: "1.1.1.1:9999".parse().unwrap(),
            remote: "2.2.2.2:8888".parse().unwrap(),
        };

        let frames = engine.on_command(EngineCommand::Shutdown {
            key: fake_key,
            how: Shutdown::Write,
        });
        assert!(frames.is_empty());
    }

    #[test]
    fn shutdown_in_syn_sent_does_nothing() {
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

        // Shutdown in SYN_SENT should do nothing
        let frames = engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Write,
        });
        assert!(frames.is_empty());
        assert_eq!(handle.tcp_state(), TcpState::SynSent);
    }

    #[test]
    fn shutdown_fin_consumes_sequence_number() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        let snd_nxt_before = tcb.snd_nxt;

        engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Write,
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.snd_nxt, snd_nxt_before.add(1)); // FIN = 1 seq
    }

    #[test]
    fn shutdown_arms_rto_for_fin() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        // Clear any existing RTO
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.rto_deadline = None;

        engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Write,
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.rto_deadline.is_some());
    }

    // === on_command: Close tests (task 5.19) ===

    #[test]
    fn close_default_linger_sends_fin() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let frames = engine.on_command(EngineCommand::Close {
            key: four_tuple,
            linger: None,
        });

        // Default close → graceful FIN
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::FIN));
        assert_eq!(handle.tcp_state(), TcpState::FinWait1);
    }

    #[test]
    fn close_linger_nonzero_sends_fin() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let frames = engine.on_command(EngineCommand::Close {
            key: four_tuple,
            linger: Some(Duration::from_secs(5)),
        });

        // Non-zero linger → graceful FIN (blocking handled app-side)
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::FIN));
        assert_eq!(handle.tcp_state(), TcpState::FinWait1);
    }

    #[test]
    fn close_linger_zero_sends_rst() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let frames = engine.on_command(EngineCommand::Close {
            key: four_tuple,
            linger: Some(Duration::ZERO),
        });

        // Linger=0 → RST, discard data
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::RST));
        assert!(parsed.flags.contains(TcpFlags::ACK));

        // TCB removed
        assert!(!engine.tcbs.contains_key(&four_tuple));

        // Error latched as ConnectionAborted
        assert!(matches!(handle.peek_error(), Some(TcpError::ConnectionAborted)));
        assert_eq!(handle.tcp_state(), TcpState::Closed);
        assert!(handle.eof.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn close_linger_zero_discards_pending_data() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        // Write data that hasn't been sent
        handle.tx_ring.write(b"unsent data that should be discarded");

        let frames = engine.on_command(EngineCommand::Close {
            key: four_tuple,
            linger: Some(Duration::ZERO),
        });

        // RST sent, data discarded
        assert_eq!(frames.len(), 1);
        let parsed = parse_tcp_packet(&frames[0]).unwrap();
        assert!(parsed.flags.contains(TcpFlags::RST));
        assert!(!engine.tcbs.contains_key(&four_tuple));
    }

    #[test]
    fn close_on_nonexistent_key_does_nothing() {
        let (mut engine, _clock) = make_engine();
        let fake_key = FourTuple {
            local: "1.1.1.1:9999".parse().unwrap(),
            remote: "2.2.2.2:8888".parse().unwrap(),
        };

        let frames = engine.on_command(EngineCommand::Close {
            key: fake_key,
            linger: None,
        });
        assert!(frames.is_empty());
    }

    // === on_command: SetOption tests (task 5.19) ===

    #[test]
    fn set_option_nodelay() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        assert!(!engine.tcbs.get(&four_tuple).unwrap().nodelay);

        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::Nodelay(true),
        });

        assert!(engine.tcbs.get(&four_tuple).unwrap().nodelay);
    }

    #[test]
    fn set_option_keepalive() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        assert!(engine.tcbs.get(&four_tuple).unwrap().keepalive.is_none());

        let ka = KeepaliveConfig {
            idle: Duration::from_secs(60),
            interval: Duration::from_secs(10),
            count: 5,
        };
        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::Keepalive(Some(ka)),
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.keepalive, Some(ka));
    }

    #[test]
    fn set_option_keepalive_disable_clears_timer() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        // Enable keepalive and arm timer
        let ka = KeepaliveConfig {
            idle: Duration::from_secs(60),
            interval: Duration::from_secs(10),
            count: 5,
        };
        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.keepalive = Some(ka);
        tcb.keepalive_deadline = Some(clock.now() + Duration::from_secs(60));
        tcb.keepalive_probes_sent = 2;

        // Disable keepalive
        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::Keepalive(None),
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.keepalive.is_none());
        assert!(tcb.keepalive_deadline.is_none());
        assert_eq!(tcb.keepalive_probes_sent, 0);
    }

    #[test]
    fn set_option_linger_updates_tcb_and_handle() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::Linger(Some(Duration::from_secs(10))),
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.linger, Some(Duration::from_secs(10)));
        assert_eq!(*handle.linger.lock().unwrap(), Some(Duration::from_secs(10)));
    }

    #[test]
    fn set_option_ttl() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::Ttl(128),
        });

        assert_eq!(engine.tcbs.get(&four_tuple).unwrap().ttl, 128);
    }

    #[test]
    fn set_option_reuseaddr() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::ReuseAddr(true),
        });

        assert!(engine.tcbs.get(&four_tuple).unwrap().reuseaddr);
    }

    #[test]
    fn set_option_recv_buf_size() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::RecvBufSize(32768),
        });

        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.recv_buf_size, 32768);
    }

    #[test]
    fn set_option_send_buf_size() {
        let (mut engine, _clock) = make_engine();
        let (four_tuple, _handle) = setup_established_connection(&mut engine);

        engine.on_command(EngineCommand::SetOption {
            key: four_tuple,
            option: SocketOption::SendBufSize(16384),
        });

        assert_eq!(engine.tcbs.get(&four_tuple).unwrap().send_buf_size, 16384);
    }

    #[test]
    fn set_option_on_nonexistent_key_does_nothing() {
        let (mut engine, _clock) = make_engine();
        let fake_key = FourTuple {
            local: "1.1.1.1:9999".parse().unwrap(),
            remote: "2.2.2.2:8888".parse().unwrap(),
        };

        // Should not panic
        engine.on_command(EngineCommand::SetOption {
            key: fake_key,
            option: SocketOption::Nodelay(true),
        });
    }

    // === Flush-before-FIN integration (task 5.19) ===

    #[test]
    fn shutdown_flushes_tx_ring_before_fin() {
        let (mut engine, clock) = make_engine();
        let (four_tuple, handle) = setup_established_connection(&mut engine);

        let tcb = engine.tcbs.get_mut(&four_tuple).unwrap();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;

        // App writes data then calls shutdown
        handle.tx_ring.write(b"flush me first");

        engine.on_command(EngineCommand::Shutdown {
            key: four_tuple,
            how: Shutdown::Write,
        });

        // fin_pending should be set since there's data in send_buf
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert!(tcb.fin_pending);
        assert_eq!(tcb.state, TcpState::Established); // Not yet transitioned

        // on_tick should send data then FIN
        let frames = engine.on_tick(clock.now());

        // Should have data segment + FIN
        assert!(frames.len() >= 2);

        // First frame should be data
        let data_frame = parse_tcp_packet(&frames[0]).unwrap();
        assert!(data_frame.flags.contains(TcpFlags::ACK));
        assert!(!data_frame.flags.contains(TcpFlags::FIN));
        assert_eq!(data_frame.payload, b"flush me first");

        // Last frame should be FIN
        let fin_frame = parse_tcp_packet(frames.last().unwrap()).unwrap();
        assert!(fin_frame.flags.contains(TcpFlags::FIN));

        // State transitioned
        let tcb = engine.tcbs.get(&four_tuple).unwrap();
        assert_eq!(tcb.state, TcpState::FinWait1);
    }
}
