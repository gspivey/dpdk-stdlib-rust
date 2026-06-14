//! Transmission Control Block (TCB) — per-connection engine-internal state.
//!
//! The TCB holds all protocol state for a single TCP connection. It is owned
//! exclusively by the engine thread and never accessed from app threads directly.
//! App threads interact through the shared `ConnectionHandle`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::congestion::CongestionState;
use crate::contract::{ConnectionHandle, KeepaliveConfig};
use crate::seq::SeqNum;
use crate::state::{FourTuple, TcpState};

/// A retransmit queue entry tracking a sent segment awaiting acknowledgment.
#[derive(Debug, Clone)]
pub struct RetransmitEntry {
    /// Sequence number of the first byte in this segment.
    pub seq: SeqNum,
    /// Offset into `send_buf` where this segment's data starts.
    pub offset: usize,
    /// Number of payload bytes in this segment.
    pub len: usize,
    /// When this segment was (last) sent.
    pub sent_at: Instant,
    /// Number of times this segment has been retransmitted.
    pub retransmit_count: u32,
}

/// Transmission Control Block — all per-connection state owned by the engine.
pub struct Tcb {
    /// Connection 4-tuple key.
    pub key: FourTuple,
    /// Current TCP state.
    pub state: TcpState,

    // --- Send sequence variables (RFC 9293 §3.3.1) ---
    /// Oldest unacknowledged sequence number.
    pub snd_una: SeqNum,
    /// Next sequence number to send.
    pub snd_nxt: SeqNum,
    /// Peer's advertised window (already scaled).
    pub snd_wnd: u32,
    /// Segment seq used for last window update.
    pub snd_wl1: SeqNum,
    /// Segment ack used for last window update.
    pub snd_wl2: SeqNum,
    /// Initial send sequence number.
    pub iss: SeqNum,

    // --- Receive sequence variables ---
    /// Next expected receive sequence number.
    pub rcv_nxt: SeqNum,
    /// Our advertised receive window.
    pub rcv_wnd: u32,
    /// Initial receive sequence number.
    pub irs: SeqNum,

    // --- Window scaling ---
    /// Peer's scale factor (left-shift their advertised window).
    pub snd_scale: u8,
    /// Our scale factor (right-shift when encoding our window).
    pub rcv_scale: u8,

    // --- MSS ---
    /// Our MSS (derived from local interface MTU).
    pub local_mss: u16,
    /// Peer's MSS (learned from SYN/SYN-ACK option, default 536).
    pub peer_mss: u16,

    // --- Congestion control ---
    pub congestion: CongestionState,

    // --- Timer deadlines (None = timer not armed) ---
    pub rto_deadline: Option<Instant>,
    pub persist_deadline: Option<Instant>,
    pub keepalive_deadline: Option<Instant>,
    pub time_wait_deadline: Option<Instant>,
    pub fin_wait2_deadline: Option<Instant>,
    pub delayed_ack_deadline: Option<Instant>,
    /// Number of consecutive retransmissions of the same segment.
    pub retransmit_count: u32,

    // --- Engine-internal buffers (NOT shared with app) ---
    /// Data from tx_ring waiting to be segmented and sent.
    pub send_buf: VecDeque<u8>,
    /// Sent segments awaiting acknowledgment.
    pub retransmit_queue: Vec<RetransmitEntry>,
    /// Out-of-order received segments, keyed by seq.diff(rcv_nxt).
    pub reorder_buffer: BTreeMap<u32, Vec<u8>>,

    // --- Nagle state ---
    /// TCP_NODELAY: disable Nagle algorithm.
    pub nodelay: bool,
    /// Whether there is unacknowledged data in flight (for Nagle decision).
    pub has_unacked_data: bool,
    /// FIN has been requested but not yet sent (flush-before-FIN).
    pub fin_pending: bool,

    // --- Delayed-ACK state ---
    /// Number of data segments received since last ACK was sent.
    /// Used for the "every-other-segment" rule: send ACK when this reaches 2.
    pub segments_since_ack: u32,

    // --- Socket options ---
    pub keepalive: Option<KeepaliveConfig>,
    pub linger: Option<Duration>,
    pub recv_buf_size: usize,
    pub send_buf_size: usize,
    pub reuseaddr: bool,
    pub ttl: u8,

    // --- Frame building ---
    /// Source MAC address for outgoing frames.
    pub src_mac: [u8; 6],
    /// Destination MAC address for outgoing frames.
    pub dst_mac: [u8; 6],

    // --- Shared handle (app↔engine bridge) ---
    pub handle: Arc<ConnectionHandle>,
}

impl Tcb {
    /// Create a new TCB with the given parameters (called on connect/accept).
    pub fn new(
        key: FourTuple,
        iss: SeqNum,
        local_mss: u16,
        handle: Arc<ConnectionHandle>,
        src_mac: [u8; 6],
        dst_mac: [u8; 6],
    ) -> Self {
        let congestion = CongestionState::new(local_mss);
        Self {
            key,
            state: TcpState::Closed,
            snd_una: iss,
            snd_nxt: iss,
            snd_wnd: 0,
            snd_wl1: SeqNum(0),
            snd_wl2: SeqNum(0),
            iss,
            rcv_nxt: SeqNum(0),
            rcv_wnd: 0,
            irs: SeqNum(0),
            snd_scale: 0,
            rcv_scale: 0,
            local_mss,
            peer_mss: crate::DEFAULT_PEER_MSS,
            congestion,
            rto_deadline: None,
            persist_deadline: None,
            keepalive_deadline: None,
            time_wait_deadline: None,
            fin_wait2_deadline: None,
            delayed_ack_deadline: None,
            retransmit_count: 0,
            send_buf: VecDeque::new(),
            retransmit_queue: Vec::new(),
            reorder_buffer: BTreeMap::new(),
            nodelay: false,
            has_unacked_data: false,
            fin_pending: false,
            segments_since_ack: 0,
            keepalive: None,
            linger: None,
            recv_buf_size: 65536,
            send_buf_size: 65536,
            reuseaddr: false,
            ttl: 64,
            src_mac,
            dst_mac,
            handle,
        }
    }

    /// Effective MSS: min(local_mss, peer_mss).
    #[inline]
    pub fn effective_mss(&self) -> u16 {
        std::cmp::min(self.local_mss, self.peer_mss)
    }

    /// Bytes currently in flight (unacknowledged).
    #[inline]
    pub fn flight_size(&self) -> u32 {
        self.snd_nxt.diff(self.snd_una)
    }

    /// Available send window respecting both congestion and receiver window.
    #[inline]
    pub fn available_send_window(&self) -> u32 {
        let effective = self.congestion.effective_window(self.snd_wnd);
        effective.saturating_sub(self.flight_size())
    }

    /// Nagle algorithm decision: should we send data now?
    /// Returns true if data should be sent immediately, false to buffer.
    #[inline]
    pub fn nagle_should_send(&self, pending_bytes: usize) -> bool {
        // Always send immediately if TCP_NODELAY is set
        if self.nodelay {
            return true;
        }
        // Send if no unacked data (nothing in flight)
        if !self.has_unacked_data {
            return true;
        }
        // Send if the pending data fills a full MSS segment
        if pending_bytes >= self.effective_mss() as usize {
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{CommandSender, EngineWakeup};
    use std::sync::mpsc;

    fn make_test_tcb() -> Tcb {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx, key));
        Tcb::new(
            key,
            SeqNum(1000),
            1460,
            handle,
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        )
    }

    #[test]
    fn new_tcb_defaults() {
        let tcb = make_test_tcb();
        assert_eq!(tcb.state, TcpState::Closed);
        assert_eq!(tcb.snd_una, SeqNum(1000));
        assert_eq!(tcb.snd_nxt, SeqNum(1000));
        assert_eq!(tcb.local_mss, 1460);
        assert_eq!(tcb.peer_mss, 536); // DEFAULT_PEER_MSS
        assert_eq!(tcb.ttl, 64);
        assert!(!tcb.nodelay);
        assert!(!tcb.fin_pending);
        assert_eq!(tcb.congestion.cwnd, CongestionState::initial_window(1460));
    }

    #[test]
    fn effective_mss() {
        let mut tcb = make_test_tcb();
        assert_eq!(tcb.effective_mss(), 536); // min(1460, 536)
        tcb.peer_mss = 1460;
        assert_eq!(tcb.effective_mss(), 1460); // min(1460, 1460)
        tcb.peer_mss = 9000;
        assert_eq!(tcb.effective_mss(), 1460); // min(1460, 9000)
    }

    #[test]
    fn flight_size() {
        let mut tcb = make_test_tcb();
        assert_eq!(tcb.flight_size(), 0);
        tcb.snd_nxt = SeqNum(1000 + 2920);
        assert_eq!(tcb.flight_size(), 2920);
    }

    #[test]
    fn available_send_window() {
        let mut tcb = make_test_tcb();
        tcb.peer_mss = 1460;
        tcb.snd_wnd = 65535;
        // cwnd = initial_window(1460) = 14600, flight = 0
        // effective = min(14600, 65535) = 14600
        assert_eq!(tcb.available_send_window(), 14600);

        // Some data in flight
        tcb.snd_nxt = SeqNum(1000 + 5000);
        // effective = 14600, flight = 5000, available = 9600
        assert_eq!(tcb.available_send_window(), 9600);
    }

    #[test]
    fn available_send_window_limited_by_rwnd() {
        let mut tcb = make_test_tcb();
        tcb.snd_wnd = 1000; // Small receiver window
        // effective = min(14600, 1000) = 1000, flight = 0
        assert_eq!(tcb.available_send_window(), 1000);
    }
}
