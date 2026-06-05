//! Transmit queue for collecting outbound datagrams from the s2n-quic endpoint.

/// A complete Ethernet frame ready to send via the backend.
pub struct TxDatagram {
    pub frame: Vec<u8>,
}

/// Transmit queue for outbound QUIC datagrams.
pub struct DpdkTxQueue {
    pending: Vec<TxDatagram>,
    capacity: usize,
}

impl DpdkTxQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: Vec::new(),
            capacity,
        }
    }

    /// Drain all pending frames for transmission.
    pub fn drain(&mut self) -> std::vec::Drain<'_, TxDatagram> {
        self.pending.drain(..)
    }
}
