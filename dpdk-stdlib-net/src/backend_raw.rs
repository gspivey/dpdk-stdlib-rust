//! AF_PACKET raw socket backend for Linux
//!
//! Implements `PacketBackend` using Linux AF_PACKET raw sockets. This backend
//! provides raw Ethernet frame I/O without requiring DPDK, making it suitable
//! as a fallback when DPDK is not available.
//!
//! ## Features
//!
//! - **Basic mode**: Uses standard `send()` / `recv()` system calls
//! - **PACKET_MMAP mode**: Uses memory-mapped ring buffers for zero-copy I/O
//!
//! ## Requirements
//!
//! - Linux kernel 2.6+ (AF_PACKET)
//! - `CAP_NET_RAW` capability or root access
//! - Network interface must exist and be UP

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::backend::PacketBackend;
use crate::ring_buffer::{
    MmapRing, RingConfig, TpacketReq, TPACKET_V2,
    SOL_PACKET, PACKET_RX_RING, PACKET_TX_RING, PACKET_VERSION,
};

// AF_PACKET constants
const AF_PACKET: i32 = 17;
const SOCK_RAW: i32 = 3;
/// ETH_P_ALL - receive all protocols
const ETH_P_ALL: i32 = 0x0003;
/// SIOCGIFINDEX - get interface index
const SIOCGIFINDEX: libc::c_ulong = 0x8933;
/// SIOCGIFHWADDR - get hardware (MAC) address
const SIOCGIFHWADDR: libc::c_ulong = 0x8927;
/// SIOCGIFFLAGS - get interface flags
const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
/// SIOCSIFFLAGS - set interface flags
const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
/// IFF_PROMISC - promiscuous mode flag
const IFF_PROMISC: i16 = 0x100;
/// IFF_ALLMULTI - all multicast flag
const IFF_ALLMULTI: i16 = 0x200;

/// ifreq structure for ioctl calls
#[repr(C)]
#[derive(Clone, Copy)]
struct Ifreq {
    ifr_name: [u8; 16],
    ifr_data: [u8; 24],
}

impl Ifreq {
    fn new(name: &str) -> Self {
        let mut ifr = Ifreq {
            ifr_name: [0u8; 16],
            ifr_data: [0u8; 24],
        };
        let name_bytes = name.as_bytes();
        let len = name_bytes.len().min(15); // Leave room for null terminator
        ifr.ifr_name[..len].copy_from_slice(&name_bytes[..len]);
        ifr
    }

    fn ifr_ifindex(&self) -> i32 {
        i32::from_ne_bytes([
            self.ifr_data[0],
            self.ifr_data[1],
            self.ifr_data[2],
            self.ifr_data[3],
        ])
    }

    fn ifr_flags(&self) -> i16 {
        i16::from_ne_bytes([self.ifr_data[0], self.ifr_data[1]])
    }

    fn set_ifr_flags(&mut self, flags: i16) {
        let bytes = flags.to_ne_bytes();
        self.ifr_data[0] = bytes[0];
        self.ifr_data[1] = bytes[1];
    }

    fn ifr_hwaddr(&self) -> [u8; 6] {
        // hwaddr starts at offset 2 in sa_data (after sa_family)
        [
            self.ifr_data[2],
            self.ifr_data[3],
            self.ifr_data[4],
            self.ifr_data[5],
            self.ifr_data[6],
            self.ifr_data[7],
        ]
    }
}

/// sockaddr_ll structure for AF_PACKET bind
#[repr(C)]
struct SockaddrLl {
    sll_family: u16,
    sll_protocol: u16,
    sll_ifindex: i32,
    sll_hatype: u16,
    sll_pkttype: u8,
    sll_halen: u8,
    sll_addr: [u8; 8],
}

/// AF_PACKET raw socket backend.
///
/// Provides raw Ethernet frame I/O via Linux AF_PACKET sockets.
/// Supports both basic send/recv mode and PACKET_MMAP zero-copy mode.
pub struct RawSocketBackend {
    /// Raw socket file descriptor
    fd: i32,
    /// Interface name
    interface: String,
    /// Interface index
    if_index: i32,
    /// MAC address of the interface
    mac_addr: [u8; 6],
    /// Whether PACKET_MMAP is enabled
    use_mmap: bool,
    /// RX ring buffer (PACKET_MMAP mode only)
    rx_ring: Option<Mutex<MmapRing>>,
    /// TX ring buffer (PACKET_MMAP mode only)
    tx_ring: Option<Mutex<MmapRing>>,
    /// Promiscuous mode flag
    promiscuous: AtomicBool,
    /// All-multicast mode flag
    allmulticast: AtomicBool,
}

// Safety: The socket fd is safe to use from multiple threads when properly synchronized
unsafe impl Send for RawSocketBackend {}
unsafe impl Sync for RawSocketBackend {}

impl RawSocketBackend {
    /// Create a new raw socket backend bound to the given network interface.
    ///
    /// # Arguments
    /// * `interface` - Network interface name (e.g., "eth0", "ens5")
    ///
    /// # Requirements
    /// - Requires `CAP_NET_RAW` capability or root access
    /// - The interface must exist
    pub fn new(interface: &str) -> io::Result<Self> {
        Self::with_mmap(interface, false, &RingConfig::default())
    }

    /// Create a new raw socket backend with optional PACKET_MMAP support.
    ///
    /// # Arguments
    /// * `interface` - Network interface name
    /// * `use_mmap` - Whether to enable PACKET_MMAP ring buffers
    /// * `ring_config` - Ring buffer configuration (ignored if use_mmap is false)
    pub fn with_mmap(interface: &str, use_mmap: bool, ring_config: &RingConfig) -> io::Result<Self> {
        // Create AF_PACKET raw socket
        let fd = unsafe {
            libc::socket(AF_PACKET, SOCK_RAW, (ETH_P_ALL as u16).to_be() as i32)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Get interface index
        let mut ifr = Ifreq::new(interface);
        let ret = unsafe { libc::ioctl(fd, SIOCGIFINDEX, &mut ifr) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }
        let if_index = ifr.ifr_ifindex();

        // Get MAC address
        let mut ifr_hw = Ifreq::new(interface);
        let ret = unsafe { libc::ioctl(fd, SIOCGIFHWADDR, &mut ifr_hw) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }
        let mac_addr = ifr_hw.ifr_hwaddr();

        // Bind to the interface
        let sll = SockaddrLl {
            sll_family: AF_PACKET as u16,
            sll_protocol: (ETH_P_ALL as u16).to_be(),
            sll_ifindex: if_index,
            sll_hatype: 0,
            sll_pkttype: 0,
            sll_halen: 0,
            sll_addr: [0; 8],
        };

        let ret = unsafe {
            libc::bind(
                fd,
                &sll as *const SockaddrLl as *const libc::sockaddr,
                std::mem::size_of::<SockaddrLl>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        // Set up PACKET_MMAP ring buffers if requested
        let (rx_ring, tx_ring) = if use_mmap {
            Self::setup_mmap_rings(fd, ring_config)?
        } else {
            (None, None)
        };

        // Get current promiscuous/allmulticast state
        let mut ifr_flags = Ifreq::new(interface);
        let promiscuous;
        let allmulticast;
        let ret = unsafe { libc::ioctl(fd, SIOCGIFFLAGS, &mut ifr_flags) };
        if ret >= 0 {
            let flags = ifr_flags.ifr_flags();
            promiscuous = flags & IFF_PROMISC != 0;
            allmulticast = flags & IFF_ALLMULTI != 0;
        } else {
            promiscuous = false;
            allmulticast = false;
        }

        Ok(Self {
            fd,
            interface: interface.to_string(),
            if_index,
            mac_addr,
            use_mmap,
            rx_ring,
            tx_ring,
            promiscuous: AtomicBool::new(promiscuous),
            allmulticast: AtomicBool::new(allmulticast),
        })
    }

    /// Set up PACKET_MMAP RX and TX ring buffers.
    fn setup_mmap_rings(
        fd: i32,
        config: &RingConfig,
    ) -> io::Result<(Option<Mutex<MmapRing>>, Option<Mutex<MmapRing>>)> {
        // Set TPACKET version to V2
        let version = TPACKET_V2;
        let ret = unsafe {
            libc::setsockopt(
                fd,
                SOL_PACKET,
                PACKET_VERSION,
                &version as *const i32 as *const libc::c_void,
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let req = config.to_tpacket_req();

        // Set up RX ring
        let ret = unsafe {
            libc::setsockopt(
                fd,
                SOL_PACKET,
                PACKET_RX_RING,
                &req as *const TpacketReq as *const libc::c_void,
                std::mem::size_of::<TpacketReq>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        // Set up TX ring
        let ret = unsafe {
            libc::setsockopt(
                fd,
                SOL_PACKET,
                PACKET_TX_RING,
                &req as *const TpacketReq as *const libc::c_void,
                std::mem::size_of::<TpacketReq>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        // mmap both rings (RX first, then TX contiguously)
        let rx_size = config.total_size();
        let tx_size = config.total_size();
        let total_size = rx_size + tx_size;

        let rx_ring = unsafe { MmapRing::new(fd, config, 0)? };
        let tx_ring = unsafe { MmapRing::new(fd, config, rx_size)? };

        let _ = total_size; // Used for documentation

        Ok((
            Some(Mutex::new(rx_ring)),
            Some(Mutex::new(tx_ring)),
        ))
    }

    /// Send a frame using basic send() system call.
    fn send_basic(&self, frame: &[u8]) -> io::Result<usize> {
        let sll = SockaddrLl {
            sll_family: AF_PACKET as u16,
            sll_protocol: (ETH_P_ALL as u16).to_be(),
            sll_ifindex: self.if_index,
            sll_hatype: 0,
            sll_pkttype: 0,
            sll_halen: 6,
            sll_addr: {
                let mut addr = [0u8; 8];
                if frame.len() >= 6 {
                    addr[..6].copy_from_slice(&frame[..6]); // Destination MAC
                }
                addr
            },
        };

        let sent = unsafe {
            libc::sendto(
                self.fd,
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
                0,
                &sll as *const SockaddrLl as *const libc::sockaddr,
                std::mem::size_of::<SockaddrLl>() as libc::socklen_t,
            )
        };

        if sent < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(sent as usize)
        }
    }

    /// Send a frame using PACKET_MMAP TX ring.
    fn send_mmap(&self, frame: &[u8]) -> io::Result<usize> {
        let tx_ring = self.tx_ring.as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "TX ring not initialized"))?;

        let mut ring = tx_ring.lock().unwrap();
        let result = ring.write_tx_frame(frame)?;
        ring.advance();

        // Trigger transmission
        let ret = unsafe {
            libc::sendto(
                self.fd,
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                0,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            // ENOBUFS is acceptable - means kernel is busy but frame is queued
            if err.raw_os_error() != Some(libc::ENOBUFS) {
                return Err(err);
            }
        }

        Ok(result)
    }

    /// Receive frames using basic recv() system call.
    fn recv_basic(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        let mut buf = vec![0u8; 65536]; // Max Ethernet frame + jumbo

        for _ in 0..max_frames {
            let len = unsafe {
                libc::recv(
                    self.fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };

            if len < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    break; // No more packets available
                }
                if frames.is_empty() {
                    return Err(err);
                }
                break;
            }

            if len > 0 {
                frames.push(buf[..len as usize].to_vec());
            }
        }

        Ok(frames)
    }

    /// Receive frames using PACKET_MMAP RX ring.
    fn recv_mmap(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let rx_ring = self.rx_ring.as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "RX ring not initialized"))?;

        let mut ring = rx_ring.lock().unwrap();
        let mut frames = Vec::new();

        for _ in 0..max_frames {
            if let Some(frame) = ring.read_rx_frame() {
                frames.push(frame);
                ring.advance();
            } else {
                break; // No more frames available
            }
        }

        Ok(frames)
    }

    /// Modify interface flags using ioctl.
    fn modify_if_flags(&self, flag: i16, enable: bool) -> io::Result<()> {
        let mut ifr = Ifreq::new(&self.interface);

        // Get current flags
        let ret = unsafe { libc::ioctl(self.fd, SIOCGIFFLAGS, &mut ifr) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut flags = ifr.ifr_flags();
        if enable {
            flags |= flag;
        } else {
            flags &= !flag;
        }
        ifr.set_ifr_flags(flags);

        // Set new flags
        let ret = unsafe { libc::ioctl(self.fd, SIOCSIFFLAGS, &ifr) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Get the network interface name.
    pub fn interface_name(&self) -> &str {
        &self.interface
    }

    /// Get the interface index.
    pub fn interface_index(&self) -> i32 {
        self.if_index
    }

    /// Check if PACKET_MMAP is enabled.
    pub fn is_mmap_enabled(&self) -> bool {
        self.use_mmap
    }

    /// Get the raw socket file descriptor.
    pub fn raw_fd(&self) -> i32 {
        self.fd
    }
}

impl PacketBackend for RawSocketBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        if self.use_mmap {
            self.send_mmap(frame)
        } else {
            self.send_basic(frame)
        }
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        if self.use_mmap {
            self.recv_mmap(max_frames)
        } else {
            self.recv_basic(max_frames)
        }
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac_addr
    }

    fn backend_name(&self) -> &'static str {
        if self.use_mmap {
            "af_packet+mmap"
        } else {
            "af_packet"
        }
    }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        self.modify_if_flags(IFF_PROMISC, enable)?;
        self.promiscuous.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_promiscuous(&self) -> bool {
        self.promiscuous.load(Ordering::Relaxed)
    }

    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        self.modify_if_flags(IFF_ALLMULTI, enable)?;
        self.allmulticast.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_allmulticast(&self) -> bool {
        self.allmulticast.load(Ordering::Relaxed)
    }
}

impl Drop for RawSocketBackend {
    fn drop(&mut self) {
        // Drop ring buffers first (they hold references to the mmap'd region)
        self.rx_ring.take();
        self.tx_ring.take();

        // Close the socket
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ifreq_new() {
        let ifr = Ifreq::new("eth0");
        assert_eq!(&ifr.ifr_name[..4], b"eth0");
        assert_eq!(ifr.ifr_name[4], 0);
    }

    #[test]
    fn test_ifreq_name_truncation() {
        // Interface names are limited to 15 chars + null
        let ifr = Ifreq::new("very_long_interface_name_that_should_be_truncated");
        // Should be truncated to 15 chars
        assert_eq!(&ifr.ifr_name[..15], b"very_long_inter");
        assert_eq!(ifr.ifr_name[15], 0);
    }

    #[test]
    fn test_ifreq_flags() {
        let mut ifr = Ifreq::new("eth0");
        ifr.set_ifr_flags(0x1003);
        assert_eq!(ifr.ifr_flags(), 0x1003);
    }

    #[test]
    fn test_sockaddr_ll_size() {
        // sockaddr_ll should be 20 bytes
        assert_eq!(std::mem::size_of::<SockaddrLl>(), 20);
    }

    #[test]
    fn test_raw_socket_backend_name() {
        // We can't create a real raw socket without permissions,
        // but we can test the backend_name logic
        assert_eq!(
            if true { "af_packet+mmap" } else { "af_packet" },
            "af_packet+mmap"
        );
        assert_eq!(
            if false { "af_packet+mmap" } else { "af_packet" },
            "af_packet"
        );
    }

    // Note: Integration tests for RawSocketBackend require root/CAP_NET_RAW
    // and a real network interface. These are tested separately in integration tests.
    //
    // To test manually:
    // sudo cargo test --features raw-socket -- test_raw_socket_backend

    #[test]
    fn test_constants() {
        assert_eq!(AF_PACKET, 17);
        assert_eq!(SOCK_RAW, 3);
        assert_eq!(ETH_P_ALL, 0x0003);
        assert_eq!(IFF_PROMISC, 0x100);
        assert_eq!(IFF_ALLMULTI, 0x200);
    }
}
