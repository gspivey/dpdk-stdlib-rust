//! TCP congestion control (RFC 5681 + RFC 6298 RTT/RTO).
//!
//! Implements slow-start, congestion avoidance, fast retransmit (NewReno),
//! and adaptive RTO with Karn's algorithm.

use std::time::Duration;

use crate::seq::SeqNum;

/// Congestion control state per connection.
#[derive(Debug, Clone)]
pub struct CongestionState {
    /// Congestion window (bytes).
    pub cwnd: u32,
    /// Slow-start threshold (bytes).
    pub ssthresh: u32,
    /// Smoothed RTT (None until first sample).
    pub srtt: Option<Duration>,
    /// RTT variance (None until first sample).
    pub rttvar: Option<Duration>,
    /// Retransmission timeout.
    pub rto: Duration,
    /// Whether we are in fast-recovery.
    pub in_recovery: bool,
    /// Recovery point: snd_nxt at time of entering recovery.
    pub recovery_point: SeqNum,
    /// Duplicate ACK counter.
    pub dup_ack_count: u32,
}

impl CongestionState {
    /// Create initial congestion state for a new connection.
    pub fn new(mss: u16) -> Self {
        Self {
            cwnd: Self::initial_window(mss),
            ssthresh: u32::MAX, // Effectively infinite until first loss
            srtt: None,
            rttvar: None,
            rto: Duration::from_secs(1), // RFC 6298: initial RTO = 1s
            in_recovery: false,
            recovery_point: SeqNum(0),
            dup_ack_count: 0,
        }
    }

    /// Compute initial window per RFC 6928: min(10*MSS, max(2*MSS, 14600)).
    pub fn initial_window(mss: u16) -> u32 {
        let mss = mss as u32;
        std::cmp::min(10 * mss, std::cmp::max(2 * mss, 14600))
    }

    /// Update RTT estimators per RFC 6298.
    /// `is_first` should be true for the very first RTT sample on this connection.
    ///
    /// Karn's algorithm: caller must NOT pass samples from retransmitted segments.
    pub fn update_rtt(&mut self, sample: Duration) {
        let is_first = self.srtt.is_none();
        if is_first {
            // RFC 6298 §2.2: first measurement
            self.srtt = Some(sample);
            self.rttvar = Some(sample / 2);
        } else {
            let srtt = self.srtt.unwrap();
            let rttvar = self.rttvar.unwrap();
            // RTTVAR = (1-β)*RTTVAR + β*|SRTT - R|   (β = 1/4)
            let diff = if sample > srtt {
                sample - srtt
            } else {
                srtt - sample
            };
            self.rttvar = Some(rttvar * 3 / 4 + diff / 4);
            // SRTT = (1-α)*SRTT + α*R                (α = 1/8)
            self.srtt = Some(srtt * 7 / 8 + sample / 8);
        }
        // RTO = SRTT + max(G, 4*RTTVAR), G = 1ms clock granularity
        let rto = self.srtt.unwrap()
            + std::cmp::max(Duration::from_millis(1), self.rttvar.unwrap() * 4);
        // Clamp to [1s, 60s]
        self.rto = rto.clamp(Duration::from_secs(1), Duration::from_secs(60));
    }

    /// Process a new ACK (not a dup-ACK) in slow-start or congestion avoidance.
    /// `bytes_acked`: number of newly acknowledged bytes (reserved for future use).
    pub fn on_ack(&mut self, _bytes_acked: u32, mss: u16) {
        let mss = mss as u32;
        if self.in_recovery {
            return;
        }
        self.dup_ack_count = 0;
        if self.cwnd < self.ssthresh {
            // Slow-start: increase cwnd by MSS for each ACK
            self.cwnd = self.cwnd.saturating_add(mss);
        } else {
            // Congestion avoidance: increase cwnd by MSS*(MSS/cwnd) per ACK
            let increment = mss.saturating_mul(mss) / self.cwnd.max(1);
            self.cwnd = self.cwnd.saturating_add(increment.max(1));
        }
    }

    /// Handle triple duplicate ACK: enter fast retransmit / fast recovery.
    /// `flight_size`: bytes currently in flight (snd_nxt - snd_una).
    pub fn on_triple_dup_ack(&mut self, flight_size: u32, mss: u16) {
        let mss = mss as u32;
        // ssthresh = max(FlightSize/2, 2*MSS)
        self.ssthresh = std::cmp::max(flight_size / 2, 2 * mss);
        // cwnd = ssthresh + 3*MSS (accounts for the 3 dup-ACKs leaving the network)
        self.cwnd = self.ssthresh + 3 * mss;
        self.in_recovery = true;
    }

    /// Handle a partial ACK during fast recovery (NewReno).
    /// A partial ACK acknowledges some but not all data up to recovery_point.
    pub fn on_partial_ack(&mut self, bytes_acked: u32, _mss: u16) {
        // Deflate cwnd by the amount of new data acknowledged
        self.cwnd = self.cwnd.saturating_sub(bytes_acked);
        // Stay in recovery (do not exit)
    }

    /// Exit fast recovery when all data up to recovery_point is acknowledged.
    pub fn on_recovery_exit(&mut self) {
        // cwnd = ssthresh (deflate)
        self.cwnd = self.ssthresh;
        self.in_recovery = false;
        self.dup_ack_count = 0;
    }

    /// Effective send window: min(cwnd, rwnd).
    pub fn effective_window(&self, rwnd: u32) -> u32 {
        std::cmp::min(self.cwnd, rwnd)
    }

    /// Double RTO on retransmission timeout (exponential backoff).
    pub fn backoff_rto(&mut self) {
        self.rto = (self.rto * 2).min(Duration::from_secs(60));
    }

    /// On RTO: collapse cwnd to 1 MSS, set ssthresh = max(flight/2, 2*MSS).
    pub fn on_rto(&mut self, flight_size: u32, mss: u16) {
        let mss_u32 = mss as u32;
        self.ssthresh = std::cmp::max(flight_size / 2, 2 * mss_u32);
        self.cwnd = mss_u32;
        self.in_recovery = false;
        self.dup_ack_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_window_standard_mss() {
        // MSS = 1460: min(10*1460, max(2*1460, 14600)) = min(14600, max(2920, 14600)) = min(14600, 14600) = 14600
        assert_eq!(CongestionState::initial_window(1460), 14600);
    }

    #[test]
    fn initial_window_small_mss() {
        // MSS = 536: min(10*536, max(2*536, 14600)) = min(5360, max(1072, 14600)) = min(5360, 14600) = 5360
        assert_eq!(CongestionState::initial_window(536), 5360);
    }

    #[test]
    fn initial_window_large_mss() {
        // MSS = 9000: min(10*9000, max(2*9000, 14600)) = min(90000, max(18000, 14600)) = min(90000, 18000) = 18000
        assert_eq!(CongestionState::initial_window(9000), 18000);
    }

    #[test]
    fn initial_window_formula_general() {
        // For any MSS, IW = min(10*MSS, max(2*MSS, 14600))
        for mss in [64u16, 256, 536, 1000, 1460, 2000, 4000, 9000] {
            let iw = CongestionState::initial_window(mss);
            let expected = std::cmp::min(
                10 * mss as u32,
                std::cmp::max(2 * mss as u32, 14600),
            );
            assert_eq!(iw, expected, "Failed for mss={}", mss);
        }
    }

    #[test]
    fn new_state() {
        let cs = CongestionState::new(1460);
        assert_eq!(cs.cwnd, 14600);
        assert_eq!(cs.ssthresh, u32::MAX);
        assert_eq!(cs.rto, Duration::from_secs(1));
        assert!(cs.srtt.is_none());
        assert!(cs.rttvar.is_none());
        assert!(!cs.in_recovery);
        assert_eq!(cs.dup_ack_count, 0);
    }

    #[test]
    fn update_rtt_first_sample() {
        let mut cs = CongestionState::new(1460);
        cs.update_rtt(Duration::from_millis(100));
        assert_eq!(cs.srtt, Some(Duration::from_millis(100)));
        assert_eq!(cs.rttvar, Some(Duration::from_millis(50)));
        // RTO = 100ms + max(1ms, 4*50ms) = 100ms + 200ms = 300ms, clamped to 1s
        assert_eq!(cs.rto, Duration::from_secs(1));
    }

    #[test]
    fn update_rtt_second_sample() {
        let mut cs = CongestionState::new(1460);
        cs.update_rtt(Duration::from_millis(100));
        cs.update_rtt(Duration::from_millis(120));
        // SRTT = 100 * 7/8 + 120 / 8 = 87.5 + 15 = 102.5ms
        let srtt = cs.srtt.unwrap();
        // RTTVAR = 50 * 3/4 + |100-120|/4 = 37.5 + 5 = 42.5ms
        let rttvar = cs.rttvar.unwrap();
        // Allow some rounding tolerance (integer duration math)
        assert!(srtt.as_millis() >= 102 && srtt.as_millis() <= 103);
        assert!(rttvar.as_millis() >= 42 && rttvar.as_millis() <= 43);
    }

    #[test]
    fn rto_clamped_minimum() {
        let mut cs = CongestionState::new(1460);
        // Very small RTT should still produce RTO >= 1s
        cs.update_rtt(Duration::from_micros(100));
        assert!(cs.rto >= Duration::from_secs(1));
    }

    #[test]
    fn rto_clamped_maximum() {
        let mut cs = CongestionState::new(1460);
        // Very large RTT
        cs.update_rtt(Duration::from_secs(50));
        assert!(cs.rto <= Duration::from_secs(60));
    }

    #[test]
    fn slow_start_growth() {
        let mut cs = CongestionState::new(1460);
        let initial_cwnd = cs.cwnd;
        cs.on_ack(1460, 1460);
        assert_eq!(cs.cwnd, initial_cwnd + 1460);
        cs.on_ack(1460, 1460);
        assert_eq!(cs.cwnd, initial_cwnd + 2 * 1460);
    }

    #[test]
    fn congestion_avoidance_growth() {
        let mut cs = CongestionState::new(1460);
        cs.ssthresh = 14600; // Enter CA immediately
        cs.cwnd = 14600;
        let initial = cs.cwnd;
        cs.on_ack(1460, 1460);
        // Increment = MSS * MSS / cwnd = 1460*1460/14600 = 146
        let expected_increment = (1460u32 * 1460) / 14600;
        assert_eq!(cs.cwnd, initial + expected_increment);
    }

    #[test]
    fn fast_retransmit() {
        let mut cs = CongestionState::new(1460);
        cs.cwnd = 14600;
        let flight_size = 10000u32;
        cs.on_triple_dup_ack(flight_size, 1460);

        // ssthresh = max(10000/2, 2*1460) = max(5000, 2920) = 5000
        assert_eq!(cs.ssthresh, 5000);
        // cwnd = ssthresh + 3*MSS = 5000 + 4380 = 9380
        assert_eq!(cs.cwnd, 5000 + 3 * 1460);
        assert!(cs.in_recovery);
    }

    #[test]
    fn fast_retransmit_small_flight() {
        let mut cs = CongestionState::new(1460);
        cs.cwnd = 4000;
        let flight_size = 2000u32;
        cs.on_triple_dup_ack(flight_size, 1460);

        // ssthresh = max(2000/2, 2*1460) = max(1000, 2920) = 2920
        assert_eq!(cs.ssthresh, 2920);
        // cwnd = 2920 + 3*1460 = 7300
        assert_eq!(cs.cwnd, 2920 + 3 * 1460);
    }

    #[test]
    fn partial_ack_in_recovery() {
        let mut cs = CongestionState::new(1460);
        cs.cwnd = 14600;
        cs.on_triple_dup_ack(10000, 1460);
        let cwnd_after_fast = cs.cwnd;

        // Partial ACK acks 1460 bytes
        cs.on_partial_ack(1460, 1460);
        assert_eq!(cs.cwnd, cwnd_after_fast - 1460);
        assert!(cs.in_recovery); // Still in recovery
    }

    #[test]
    fn recovery_exit() {
        let mut cs = CongestionState::new(1460);
        cs.cwnd = 14600;
        cs.on_triple_dup_ack(10000, 1460);
        assert!(cs.in_recovery);

        cs.on_recovery_exit();
        assert!(!cs.in_recovery);
        // cwnd deflated to ssthresh
        assert_eq!(cs.cwnd, cs.ssthresh);
    }

    #[test]
    fn effective_window() {
        let cs = CongestionState::new(1460);
        assert_eq!(cs.effective_window(10000), 10000); // rwnd < cwnd
        assert_eq!(cs.effective_window(100000), cs.cwnd); // rwnd > cwnd
    }

    #[test]
    fn backoff_rto() {
        let mut cs = CongestionState::new(1460);
        assert_eq!(cs.rto, Duration::from_secs(1));
        cs.backoff_rto();
        assert_eq!(cs.rto, Duration::from_secs(2));
        cs.backoff_rto();
        assert_eq!(cs.rto, Duration::from_secs(4));
        // Keep doubling up to 60s
        for _ in 0..10 {
            cs.backoff_rto();
        }
        assert_eq!(cs.rto, Duration::from_secs(60));
    }

    #[test]
    fn on_rto_collapses_cwnd() {
        let mut cs = CongestionState::new(1460);
        cs.cwnd = 14600;
        cs.in_recovery = true;
        cs.on_rto(10000, 1460);

        assert_eq!(cs.cwnd, 1460); // Collapsed to 1 MSS
        assert_eq!(cs.ssthresh, 5000); // max(10000/2, 2*1460)
        assert!(!cs.in_recovery);
    }

    #[test]
    fn on_ack_ignored_during_recovery() {
        let mut cs = CongestionState::new(1460);
        cs.cwnd = 14600;
        cs.on_triple_dup_ack(10000, 1460);
        let cwnd_in_recovery = cs.cwnd;

        // Regular on_ack during recovery should not change cwnd
        cs.on_ack(1460, 1460);
        assert_eq!(cs.cwnd, cwnd_in_recovery);
    }
}
