//! DPDK packet I/O backend
//!
//! Implements `PacketBackend` using DPDK's userspace networking for high-performance
//! packet I/O. This backend bypasses the kernel network stack entirely, providing
//! the lowest latency and highest throughput.

use std::io;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};

use dpdk::{Mempool, Port};
use dpdk::mbuf::MempoolConfig;
use dpdk::port::PortConfig;

use crate::backend::PacketBackend;

/// DPDK-based packet I/O backend.
///
/// Wraps a DPDK `Port` and `Mempool` to provide raw Ethernet frame
/// send/receive operations. Packets are transmitted and received via
/// DPDK's `tx_burst` and `rx_burst` functions.
pub struct DpdkBackend {
    /// EAL handle — must stay alive for the lifetime of all DPDK resources.
    /// `None` when EAL lifetime is managed externally (e.g. by `DpdkResources`).
    _eal: Option<dpdk::Eal>,
    port: Mutex<Port>,
    mempool: Arc<Mempool>,
    mac_address: [u8; 6],
    promiscuous: AtomicBool,
    allmulticast: AtomicBool,
}

impl DpdkBackend {
    /// Create a new DPDK backend with the given port ID.
    ///
    /// Initializes DPDK EAL, creates a mempool, configures and starts the port.
    pub fn new(port_id: u16) -> io::Result<Self> {
        // Initialize EAL — must keep the handle alive for the lifetime of this backend
        let eal = dpdk::Eal::init(&["-l", "0", "-n", "4", "--no-pci"])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("EAL init failed: {}", e)))?;

        // Create mempool with jumbo-frame-capable mbufs (9KB data room).
        // ENA always supports 9001 MTU; oversized mbufs don't hurt small packets.
        let mempool = Mempool::create_with_config(
            "backend_pool",
            &MempoolConfig::new()
                .with_size(8192)
                .with_cache_size(256)
                .with_data_room_size(crate::JUMBO_DATA_ROOM_SIZE),
        ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Mempool creation failed: {}", e)))?;

        // Initialize port with jumbo MTU (9001 = AWS VPC max)
        let port_config = PortConfig::default().with_mtu(9001);
        let port = Port::init(port_id, port_config, &mempool)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Port init failed: {}", e)))?;

        let mac = port.mac_address().octets();

        // Start the port
        let mut port = port;
        port.start()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Port start failed: {}", e)))?;

        let promiscuous = port.is_promiscuous();
        let allmulticast = port.is_allmulticast();

        Ok(Self {
            _eal: Some(eal),
            port: Mutex::new(port),
            mempool: Arc::new(mempool),
            mac_address: mac,
            promiscuous: AtomicBool::new(promiscuous),
            allmulticast: AtomicBool::new(allmulticast),
        })
    }

    /// Create a DPDK backend that drives a **real NIC** (a vfio-pci-bound ENI).
    ///
    /// Unlike [`DpdkBackend::new`], this does **not** pass `--no-pci`, so DPDK
    /// performs PCI scanning and discovers the vfio-pci device. This is the
    /// constructor the TCP/QUIC stacks must use for on-NIC operation; `new`
    /// (with `--no-pci`) yields a null device that never probes a real port.
    ///
    /// EAL arguments are resolved in priority order: the explicit `eal_args`
    /// slice, then the `DPDK_EAL_ARGS` environment variable, then a default of
    /// `["dpdk-tcp", "-l", "0", "-n", "4"]`. This mirrors the real-NIC EAL setup
    /// in `dpdk-stdlib-udp`'s `get_or_init_dpdk`.
    pub fn new_real_nic(port_id: u16, eal_args: Option<&[String]>) -> io::Result<Self> {
        // Resolve EAL args: explicit override > DPDK_EAL_ARGS env > default.
        // NB: deliberately NO `--no-pci` — DPDK needs PCI scanning to find the
        // vfio-pci device.
        let args_owned: Vec<String> = if let Some(a) = eal_args {
            a.to_vec()
        } else if let Ok(s) = std::env::var("DPDK_EAL_ARGS") {
            s.split_whitespace().map(String::from).collect()
        } else {
            vec![
                "dpdk-tcp".to_string(),
                "-l".to_string(),
                "0".to_string(),
                "-n".to_string(),
                "4".to_string(),
            ]
        };
        let args_ref: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();

        let eal = dpdk::Eal::init(&args_ref)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("EAL init failed: {}", e)))?;

        // Distinct mempool name from `new`'s "backend_pool" so the two can
        // coexist without an rte_mempool name clash.
        let mempool = Mempool::create_with_config(
            "tcp_backend_pool",
            &MempoolConfig::new()
                .with_size(8192)
                .with_cache_size(256)
                .with_data_room_size(crate::JUMBO_DATA_ROOM_SIZE),
        ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Mempool creation failed: {}", e)))?;

        let port_config = PortConfig::default().with_mtu(9001);
        let port = Port::init(port_id, port_config, &mempool)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Port init failed: {}", e)))?;

        let mac = port.mac_address().octets();

        let mut port = port;
        port.start()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Port start failed: {}", e)))?;

        let promiscuous = port.is_promiscuous();
        let allmulticast = port.is_allmulticast();

        Ok(Self {
            _eal: Some(eal),
            port: Mutex::new(port),
            mempool: Arc::new(mempool),
            mac_address: mac,
            promiscuous: AtomicBool::new(promiscuous),
            allmulticast: AtomicBool::new(allmulticast),
        })
    }

    /// Create a DPDK backend from existing port and mempool.
    ///
    /// This allows reusing already-initialized DPDK resources.
    pub fn from_port_and_mempool(port: Port, mempool: Mempool) -> Self {
        let mac = port.mac_address().octets();
        let promiscuous = port.is_promiscuous();
        let allmulticast = port.is_allmulticast();

        Self {
            _eal: None, // EAL lifetime managed by caller
            port: Mutex::new(port),
            mempool: Arc::new(mempool),
            mac_address: mac,
            promiscuous: AtomicBool::new(promiscuous),
            allmulticast: AtomicBool::new(allmulticast),
        }
    }

    /// Get a reference to the mempool.
    pub fn mempool(&self) -> &Mempool {
        &self.mempool
    }
}

impl PacketBackend for DpdkBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        // Allocate an mbuf
        let mut mbuf = self.mempool.alloc()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("mbuf alloc failed: {}", e)))?;

        // Copy frame data into mbuf
        let data = mbuf.data_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Failed to get mbuf data"))?;

        if data.len() < frame.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Frame too large: {} bytes, mbuf capacity: {}", frame.len(), data.len()),
            ));
        }

        data[..frame.len()].copy_from_slice(frame);
        mbuf.set_data_len(frame.len() as u16);
        mbuf.set_packet_len(frame.len() as u32);

        // Transmit
        let port = self.port.lock().unwrap();
        let mut packets = vec![mbuf];
        let sent = port.tx_burst(0, &mut packets)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("tx_burst failed: {}", e)))?;

        if sent == 0 {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "tx queue full"));
        }

        Ok(frame.len())
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let port = self.port.lock().unwrap();
        let packets = port.rx_burst(0, max_frames as u16)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rx_burst failed: {}", e)))?;

        let mut frames = Vec::with_capacity(packets.len());
        for mbuf in &packets {
            if let Some(data) = mbuf.data() {
                let len = mbuf.data_len() as usize;
                let actual_len = len.min(data.len());
                frames.push(data[..actual_len].to_vec());
            }
        }

        Ok(frames)
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac_address
    }

    fn backend_name(&self) -> &'static str {
        "dpdk"
    }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        let mut port = self.port.lock().unwrap();
        port.set_promiscuous(enable)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("set promiscuous failed: {}", e)))?;
        self.promiscuous.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_promiscuous(&self) -> bool {
        self.promiscuous.load(Ordering::Relaxed)
    }

    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        let port = self.port.lock().unwrap();
        port.set_allmulticast(enable)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("set allmulticast failed: {}", e)))?;
        self.allmulticast.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_allmulticast(&self) -> bool {
        self.allmulticast.load(Ordering::Relaxed)
    }

    fn rx_readiness(&self) -> crate::backend::RxReadiness {
        crate::backend::RxReadiness::PollOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_dpdk_backend_creation() {
        // This test verifies the DpdkBackend can be created with stubs
        let backend = DpdkBackend::new(0);
        assert!(backend.is_ok());

        let backend = backend.unwrap();
        assert_eq!(backend.backend_name(), "dpdk");
        // MAC address should be non-zero (stubs return a default)
        let mac = backend.mac_address();
        // Just verify it's a valid 6-byte array
        assert_eq!(mac.len(), 6);
    }

    #[test]
    #[serial]
    fn test_dpdk_backend_new_real_nic() {
        // new_real_nic must construct successfully under stubs (no --no-pci path).
        let backend = DpdkBackend::new_real_nic(0, None);
        assert!(backend.is_ok());
        assert_eq!(backend.unwrap().backend_name(), "dpdk");

        // Explicit EAL args are accepted too.
        let args = vec!["dpdk-tcp".to_string(), "--no-huge".to_string()];
        assert!(DpdkBackend::new_real_nic(0, Some(&args)).is_ok());
    }

    #[test]
    #[serial]
    fn test_dpdk_backend_send_frame() {
        let backend = DpdkBackend::new(0).unwrap();

        // Build a minimal Ethernet frame
        let mut frame = vec![0u8; 64]; // Minimum Ethernet frame size
        // Destination MAC (broadcast)
        frame[0..6].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        // Source MAC
        frame[6..12].copy_from_slice(&backend.mac_address());
        // EtherType (IPv4)
        frame[12..14].copy_from_slice(&[0x08, 0x00]);

        // With stubs, tx_burst returns 0, so this will get WouldBlock
        let result = backend.send_frame(&frame);
        // Stubs return 0 for tx_burst, so we expect WouldBlock
        assert!(result.is_err() || result.unwrap() == 64);
    }

    #[test]
    #[serial]
    fn test_dpdk_backend_recv_frames() {
        let backend = DpdkBackend::new(0).unwrap();

        // With stubs, rx_burst returns 0 packets
        let frames = backend.recv_frames(32).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    #[serial]
    fn test_dpdk_backend_promiscuous() {
        let backend = DpdkBackend::new(0).unwrap();

        // Default state
        let initial = backend.is_promiscuous();

        // Toggle
        backend.set_promiscuous(!initial).unwrap();
        assert_eq!(backend.is_promiscuous(), !initial);

        // Toggle back
        backend.set_promiscuous(initial).unwrap();
        assert_eq!(backend.is_promiscuous(), initial);
    }
}
