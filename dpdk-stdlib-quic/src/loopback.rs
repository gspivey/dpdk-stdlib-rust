//! Loopback backend for testing the QUIC provider without DPDK.

use dpdk_udp::{PacketBackend, RxReadiness};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// A loopback `PacketBackend` that routes sent frames back to recv.
///
/// Enables full QUIC handshake testing without DPDK installed.
pub struct LoopbackBackend {
    tx_to_rx: Mutex<VecDeque<Vec<u8>>>,
    mac: [u8; 6],
    promiscuous: AtomicBool,
    allmulticast: AtomicBool,
}

impl LoopbackBackend {
    pub fn new() -> Self {
        Self {
            tx_to_rx: Mutex::new(VecDeque::new()),
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            promiscuous: AtomicBool::new(false),
            allmulticast: AtomicBool::new(false),
        }
    }
}

impl PacketBackend for LoopbackBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.tx_to_rx.lock().unwrap().push_back(frame.to_vec());
        Ok(frame.len())
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut q = self.tx_to_rx.lock().unwrap();
        let n = max_frames.min(q.len());
        Ok(q.drain(..n).collect())
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn backend_name(&self) -> &'static str {
        "loopback"
    }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        self.promiscuous.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_promiscuous(&self) -> bool {
        self.promiscuous.load(Ordering::Relaxed)
    }

    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        self.allmulticast.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_allmulticast(&self) -> bool {
        self.allmulticast.load(Ordering::Relaxed)
    }

    fn rx_readiness(&self) -> RxReadiness {
        RxReadiness::PollOnly
    }
}
