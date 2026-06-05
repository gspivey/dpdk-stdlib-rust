//! Receive queue for delivering datagrams to the s2n-quic endpoint.

use crate::path_handle::DpdkPathHandle;
use s2n_quic_core::inet::datagram;

/// A parsed inbound datagram ready for delivery to s2n-quic.
pub struct RxDatagram {
    pub header: datagram::Header<DpdkPathHandle>,
    pub payload: Vec<u8>,
}

/// Receive queue buffering parsed datagrams from `recv_frames()`.
pub struct DpdkRxQueue {
    datagrams: Vec<RxDatagram>,
}

impl DpdkRxQueue {
    pub fn new() -> Self {
        Self {
            datagrams: Vec::new(),
        }
    }

    pub fn push(&mut self, datagram: RxDatagram) {
        self.datagrams.push(datagram);
    }
}
