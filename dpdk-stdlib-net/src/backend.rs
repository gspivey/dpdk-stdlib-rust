//! Abstract backend trait for packet I/O
//!
//! The `PacketBackend` trait provides a unified interface for sending and receiving
//! raw Ethernet frames, abstracting over different packet I/O implementations:
//!
//! - **DPDK backend** - High-performance userspace networking via DPDK
//! - **Raw socket backend** - Linux AF_PACKET raw sockets with optional PACKET_MMAP
//!
//! This enables runtime backend selection and allows the UDP socket implementation
//! to work with any backend without code changes.

use std::io;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Condvar, Mutex};

/// Describes how a backend can signal RX readiness to the engine loop.
///
/// The engine adapts its wait strategy based on this:
/// - `Fd` → epoll/select on the file descriptor
/// - `PollOnly` → busy-poll (dedicated core, normal for kernel-bypass)
/// - `Condvar` → condvar wait (for stubs/tests, no busy-spin)
#[derive(Clone)]
pub enum RxReadiness {
    /// Backend has a pollable fd (e.g., AF_PACKET socket).
    Fd(RawFd),
    /// Backend is poll-only (e.g., DPDK rx_burst) — engine busy-polls.
    PollOnly,
    /// Stub/test backend — engine uses condvar wait.
    Condvar(Arc<(Mutex<bool>, Condvar)>),
}

/// Abstract trait for raw packet I/O backends.
///
/// Implementations provide the ability to send and receive raw Ethernet frames
/// through a network interface. The trait is designed to be backend-agnostic,
/// allowing the same UDP socket code to work with DPDK, AF_PACKET raw sockets,
/// or any other packet I/O mechanism.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` to allow sharing across threads.
pub trait PacketBackend: Send + Sync {
    /// Send a raw Ethernet frame.
    ///
    /// The frame must be a complete Ethernet frame including the Ethernet header
    /// (destination MAC, source MAC, EtherType) followed by the payload.
    ///
    /// # Arguments
    /// * `frame` - Complete Ethernet frame bytes
    ///
    /// # Returns
    /// The number of bytes sent on success.
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize>;

    /// Receive raw Ethernet frames in burst mode.
    ///
    /// Returns up to `max_frames` complete Ethernet frames received from the
    /// network interface. Each frame includes the full Ethernet header.
    ///
    /// # Arguments
    /// * `max_frames` - Maximum number of frames to receive in one call
    ///
    /// # Returns
    /// A vector of received frames. Returns an empty vector if no frames
    /// are available (non-blocking) or returns `WouldBlock` error.
    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>>;

    /// Get the MAC address of the network interface.
    ///
    /// Returns the 6-byte Ethernet MAC address of the interface this backend
    /// is bound to.
    fn mac_address(&self) -> [u8; 6];

    /// Get the backend name for identification and logging.
    ///
    /// Returns a static string identifying the backend type (e.g., "dpdk",
    /// "af_packet", "af_packet+mmap").
    fn backend_name(&self) -> &'static str;

    /// Set promiscuous mode on the network interface.
    ///
    /// When enabled, the interface receives all packets on the network segment
    /// regardless of destination MAC address.
    fn set_promiscuous(&self, enable: bool) -> io::Result<()>;

    /// Check if promiscuous mode is currently enabled.
    fn is_promiscuous(&self) -> bool;

    /// Set all-multicast mode on the network interface.
    ///
    /// When enabled, the interface receives all multicast packets regardless
    /// of whether they match the configured multicast addresses.
    fn set_allmulticast(&self, enable: bool) -> io::Result<()>;

    /// Check if all-multicast mode is currently enabled.
    fn is_allmulticast(&self) -> bool;

    /// Describe how this backend signals RX readiness.
    ///
    /// The engine loop uses this to decide its wait strategy:
    /// - `Fd` → epoll/select on the returned file descriptor
    /// - `PollOnly` → dedicated-core busy-poll (DPDK)
    /// - `Condvar` → condvar wait (stubs/tests)
    fn rx_readiness(&self) -> RxReadiness;
}

/// Configuration for backend selection and initialization.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// The type of backend to use
    pub backend_type: BackendType,
    /// Network interface name (for raw socket backend)
    pub interface_name: Option<String>,
    /// DPDK port ID (for DPDK backend)
    pub dpdk_port_id: u16,
    /// Whether to use mmap ring buffers (for raw socket backend)
    pub use_mmap: bool,
    /// Ring buffer frame size (for mmap ring buffers)
    pub ring_frame_size: usize,
    /// Number of ring buffer frames (for mmap ring buffers)
    pub ring_frame_count: u32,
    /// Whether to enable promiscuous mode
    pub promiscuous: bool,
}

/// The type of packet I/O backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendType {
    /// DPDK userspace networking (highest performance)
    Dpdk,
    /// Linux AF_PACKET raw sockets with PACKET_MMAP (zero-copy)
    RawSocketMmap,
    /// Linux AF_PACKET raw sockets (basic, no mmap)
    RawSocket,
    /// Automatic selection: try DPDK first, fall back to raw socket
    Auto,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            backend_type: BackendType::Auto,
            interface_name: None,
            dpdk_port_id: 0,
            use_mmap: true,
            ring_frame_size: 2048,
            ring_frame_count: 256,
            promiscuous: false,
        }
    }
}

impl BackendConfig {
    /// Create a new backend configuration with automatic backend selection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use DPDK backend with the given port ID.
    pub fn with_dpdk(mut self, port_id: u16) -> Self {
        self.backend_type = BackendType::Dpdk;
        self.dpdk_port_id = port_id;
        self
    }

    /// Use raw socket backend with the given interface name.
    pub fn with_raw_socket(mut self, interface: &str) -> Self {
        self.backend_type = BackendType::RawSocket;
        self.interface_name = Some(interface.to_string());
        self.use_mmap = false;
        self
    }

    /// Use raw socket backend with mmap ring buffers.
    pub fn with_raw_socket_mmap(mut self, interface: &str) -> Self {
        self.backend_type = BackendType::RawSocketMmap;
        self.interface_name = Some(interface.to_string());
        self.use_mmap = true;
        self
    }

    /// Set the network interface name.
    pub fn with_interface(mut self, interface: &str) -> Self {
        self.interface_name = Some(interface.to_string());
        self
    }

    /// Set ring buffer frame size.
    pub fn with_ring_frame_size(mut self, size: usize) -> Self {
        self.ring_frame_size = size;
        self
    }

    /// Set ring buffer frame count.
    pub fn with_ring_frame_count(mut self, count: u32) -> Self {
        self.ring_frame_count = count;
        self
    }

    /// Enable or disable promiscuous mode.
    pub fn with_promiscuous(mut self, enable: bool) -> Self {
        self.promiscuous = enable;
        self
    }
}
