//! Queue management for DPDK
//!
//! DPDK uses receive (RX) and transmit (TX) queues for packet I/O.
//! Each port can have multiple queues for parallel processing.

use crate::error::DpdkResult;
use crate::mbuf::Mbuf;

/// Configuration for a receive queue
#[derive(Debug, Clone)]
pub struct RxQueueConfig {
    /// Number of receive descriptors
    pub nb_desc: u16,
    /// NUMA socket ID
    pub socket_id: i32,
    /// Enable rx_free_thresh (free RX mbufs lazily)
    pub rx_free_thresh: u16,
}

impl Default for RxQueueConfig {
    fn default() -> Self {
        Self {
            nb_desc: 1024,
            socket_id: 0,
            rx_free_thresh: 32,
        }
    }
}

/// Configuration for a transmit queue
#[derive(Debug, Clone)]
pub struct TxQueueConfig {
    /// Number of transmit descriptors
    pub nb_desc: u16,
    /// NUMA socket ID
    pub socket_id: i32,
    /// TX free threshold
    pub tx_free_thresh: u16,
    /// TX RS bit threshold
    pub tx_rs_thresh: u16,
}

impl Default for TxQueueConfig {
    fn default() -> Self {
        Self {
            nb_desc: 1024,
            socket_id: 0,
            tx_free_thresh: 32,
            tx_rs_thresh: 32,
        }
    }
}

/// A receive queue on a DPDK port
pub struct RxQueue {
    port_id: u16,
    queue_id: u16,
}

impl RxQueue {
    /// Create a new receive queue
    pub fn new(port_id: u16, queue_id: u16, _config: RxQueueConfig) -> DpdkResult<Self> {
        // In a real implementation, this would call rte_eth_rx_queue_setup
        Ok(Self { port_id, queue_id })
    }

    /// Get the port ID this queue belongs to
    pub fn port_id(&self) -> u16 {
        self.port_id
    }

    /// Get the queue ID
    pub fn queue_id(&self) -> u16 {
        self.queue_id
    }

    /// Receive a burst of packets
    ///
    /// Returns a vector of received mbufs, up to `max_packets` count.
    /// This is a non-blocking operation - returns immediately with
    /// whatever packets are available.
    pub fn receive_burst(&self, max_packets: u16) -> DpdkResult<Vec<Mbuf>> {
        // In a real implementation, this would call rte_eth_rx_burst
        // For now, return empty - this is a placeholder
        let _ = max_packets;
        Ok(Vec::new())
    }

    /// Poll for packets with a timeout
    ///
    /// This is a convenience method that polls until either:
    /// - At least one packet is received
    /// - The timeout expires
    pub fn receive_burst_timeout(
        &self,
        max_packets: u16,
        timeout_us: u64,
    ) -> DpdkResult<Vec<Mbuf>> {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_micros(timeout_us);

        loop {
            let packets = self.receive_burst(max_packets)?;
            if !packets.is_empty() {
                return Ok(packets);
            }

            if start.elapsed() >= timeout {
                return Ok(Vec::new());
            }

            // Brief pause before retry to avoid burning CPU
            std::hint::spin_loop();
        }
    }
}

/// A transmit queue on a DPDK port
pub struct TxQueue {
    port_id: u16,
    queue_id: u16,
}

impl TxQueue {
    /// Create a new transmit queue
    pub fn new(port_id: u16, queue_id: u16, _config: TxQueueConfig) -> DpdkResult<Self> {
        // In a real implementation, this would call rte_eth_tx_queue_setup
        Ok(Self { port_id, queue_id })
    }

    /// Get the port ID this queue belongs to
    pub fn port_id(&self) -> u16 {
        self.port_id
    }

    /// Get the queue ID
    pub fn queue_id(&self) -> u16 {
        self.queue_id
    }

    /// Send a burst of packets
    ///
    /// Returns the number of packets successfully queued for transmission.
    /// Packets that are sent are consumed (freed) by DPDK.
    pub fn send_burst(&self, packets: Vec<Mbuf>) -> DpdkResult<u16> {
        // In a real implementation, this would call rte_eth_tx_burst
        // For now, just consume the packets
        let count = packets.len() as u16;

        // Packets are consumed - need to prevent Drop from running
        // since DPDK will free them
        for packet in packets {
            let _ = packet.into_raw(); // Leak intentionally - DPDK frees
        }

        Ok(count)
    }

    /// Send packets with flush
    ///
    /// Same as send_burst but also flushes any buffered packets
    pub fn send_burst_flush(&self, packets: Vec<Mbuf>) -> DpdkResult<u16> {
        // In a real implementation, this would also call rte_eth_tx_done_cleanup
        self.send_burst(packets)
    }
}

/// Queue pair combining RX and TX queues
///
/// This is a common pattern for per-core packet processing
pub struct QueuePair {
    pub rx: RxQueue,
    pub tx: TxQueue,
}

impl QueuePair {
    /// Create a new queue pair
    pub fn new(
        port_id: u16,
        queue_id: u16,
        rx_config: RxQueueConfig,
        tx_config: TxQueueConfig,
    ) -> DpdkResult<Self> {
        Ok(Self {
            rx: RxQueue::new(port_id, queue_id, rx_config)?,
            tx: TxQueue::new(port_id, queue_id, tx_config)?,
        })
    }

    /// Echo packets back (common test pattern)
    ///
    /// Receives packets and immediately sends them back out
    pub fn echo(&self, max_burst: u16) -> DpdkResult<u16> {
        let packets = self.rx.receive_burst(max_burst)?;
        if packets.is_empty() {
            return Ok(0);
        }
        self.tx.send_burst(packets)
    }
}

/// Statistics for a queue
#[derive(Debug, Default, Clone)]
pub struct QueueStats {
    /// Total packets received/transmitted
    pub packets: u64,
    /// Total bytes received/transmitted
    pub bytes: u64,
    /// Packets dropped
    pub dropped: u64,
    /// Errors encountered
    pub errors: u64,
}

impl QueueStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add packet stats
    pub fn add_packet(&mut self, bytes: usize) {
        self.packets += 1;
        self.bytes += bytes as u64;
    }

    /// Add error
    pub fn add_error(&mut self) {
        self.errors += 1;
    }

    /// Add dropped packet
    pub fn add_dropped(&mut self) {
        self.dropped += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rx_queue_creation() {
        let queue = RxQueue::new(0, 0, RxQueueConfig::default());
        assert!(queue.is_ok());
        let queue = queue.unwrap();
        assert_eq!(queue.port_id(), 0);
        assert_eq!(queue.queue_id(), 0);
    }

    #[test]
    fn test_tx_queue_creation() {
        let queue = TxQueue::new(0, 0, TxQueueConfig::default());
        assert!(queue.is_ok());
        let queue = queue.unwrap();
        assert_eq!(queue.port_id(), 0);
        assert_eq!(queue.queue_id(), 0);
    }

    #[test]
    fn test_queue_stats() {
        let mut stats = QueueStats::new();
        stats.add_packet(100);
        stats.add_packet(200);
        assert_eq!(stats.packets, 2);
        assert_eq!(stats.bytes, 300);
    }
}
