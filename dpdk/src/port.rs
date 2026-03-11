//! DPDK Ethernet Port Management
//!
//! This module provides safe Rust wrappers for DPDK ethernet port operations.
//! It handles port configuration, queue setup, and packet I/O.

use crate::error::{DpdkError, DpdkResult};
use crate::mbuf::{Mbuf, Mempool};
use std::ptr;

/// Default number of RX/TX descriptors per queue
pub const DEFAULT_RX_DESC: u16 = 1024;
pub const DEFAULT_TX_DESC: u16 = 1024;

/// Default burst size for packet I/O
pub const DEFAULT_BURST_SIZE: u16 = 32;

/// Hardware offload flags for RX
#[derive(Debug, Clone, Copy, Default)]
pub struct RxOffload {
    /// Hardware VLAN stripping
    pub vlan_strip: bool,
    /// Hardware IPv4 checksum verification
    pub ipv4_cksum: bool,
    /// Hardware UDP checksum verification
    pub udp_cksum: bool,
    /// Hardware TCP checksum verification
    pub tcp_cksum: bool,
}

impl RxOffload {
    /// Convert to DPDK offload flags
    pub fn to_flags(&self) -> u64 {
        let mut flags = 0u64;
        if self.vlan_strip {
            flags |= dpdk_sys::RTE_ETH_RX_OFFLOAD_VLAN_STRIP;
        }
        if self.ipv4_cksum {
            flags |= dpdk_sys::RTE_ETH_RX_OFFLOAD_IPV4_CKSUM;
        }
        if self.udp_cksum {
            flags |= dpdk_sys::RTE_ETH_RX_OFFLOAD_UDP_CKSUM;
        }
        if self.tcp_cksum {
            flags |= dpdk_sys::RTE_ETH_RX_OFFLOAD_TCP_CKSUM;
        }
        flags
    }
}

/// Hardware offload flags for TX
#[derive(Debug, Clone, Copy, Default)]
pub struct TxOffload {
    /// Hardware VLAN insertion
    pub vlan_insert: bool,
    /// Hardware IPv4 checksum calculation
    pub ipv4_cksum: bool,
    /// Hardware UDP checksum calculation
    pub udp_cksum: bool,
    /// Hardware TCP checksum calculation
    pub tcp_cksum: bool,
}

impl TxOffload {
    /// Convert to DPDK offload flags
    pub fn to_flags(&self) -> u64 {
        let mut flags = 0u64;
        if self.vlan_insert {
            flags |= dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT;
        }
        if self.ipv4_cksum {
            flags |= dpdk_sys::RTE_ETH_TX_OFFLOAD_IPV4_CKSUM;
        }
        if self.udp_cksum {
            flags |= dpdk_sys::RTE_ETH_TX_OFFLOAD_UDP_CKSUM;
        }
        if self.tcp_cksum {
            flags |= dpdk_sys::RTE_ETH_TX_OFFLOAD_TCP_CKSUM;
        }
        flags
    }
}

/// Configuration for an Ethernet port
#[derive(Debug, Clone)]
pub struct PortConfig {
    /// Number of RX queues
    pub nb_rx_queues: u16,
    /// Number of TX queues
    pub nb_tx_queues: u16,
    /// Number of RX descriptors per queue
    pub nb_rx_desc: u16,
    /// Number of TX descriptors per queue
    pub nb_tx_desc: u16,
    /// Enable promiscuous mode
    pub promiscuous: bool,
    /// MTU size (0 for default)
    pub mtu: u32,
    /// RX hardware offload configuration
    pub rx_offload: RxOffload,
    /// TX hardware offload configuration
    pub tx_offload: TxOffload,
}

impl Default for PortConfig {
    fn default() -> Self {
        Self {
            nb_rx_queues: 1,
            nb_tx_queues: 1,
            nb_rx_desc: DEFAULT_RX_DESC,
            nb_tx_desc: DEFAULT_TX_DESC,
            promiscuous: true,
            mtu: 0,
            rx_offload: RxOffload::default(),
            tx_offload: TxOffload::default(),
        }
    }
}

impl PortConfig {
    /// Create a new PortConfig with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the number of RX/TX queues
    pub fn with_queues(mut self, rx: u16, tx: u16) -> Self {
        self.nb_rx_queues = rx;
        self.nb_tx_queues = tx;
        self
    }

    /// Set the number of RX/TX descriptors
    pub fn with_descriptors(mut self, rx: u16, tx: u16) -> Self {
        self.nb_rx_desc = rx;
        self.nb_tx_desc = tx;
        self
    }

    /// Enable or disable promiscuous mode
    pub fn with_promiscuous(mut self, enable: bool) -> Self {
        self.promiscuous = enable;
        self
    }

    /// Set the MTU
    pub fn with_mtu(mut self, mtu: u32) -> Self {
        self.mtu = mtu;
        self
    }

    /// Configure RX hardware offloads
    pub fn with_rx_offload(mut self, offload: RxOffload) -> Self {
        self.rx_offload = offload;
        self
    }

    /// Configure TX hardware offloads
    pub fn with_tx_offload(mut self, offload: TxOffload) -> Self {
        self.tx_offload = offload;
        self
    }

    /// Enable all checksum offloads (RX and TX)
    pub fn with_checksum_offload(mut self) -> Self {
        self.rx_offload = RxOffload {
            ipv4_cksum: true,
            udp_cksum: true,
            tcp_cksum: true,
            ..Default::default()
        };
        self.tx_offload = TxOffload {
            ipv4_cksum: true,
            udp_cksum: true,
            tcp_cksum: true,
            ..Default::default()
        };
        self
    }
}

/// Ethernet MAC address
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    /// Create a new MAC address from bytes
    pub fn new(bytes: [u8; 6]) -> Self {
        Self(bytes)
    }

    /// Create a broadcast MAC address (ff:ff:ff:ff:ff:ff)
    pub fn broadcast() -> Self {
        Self([0xff; 6])
    }

    /// Create a zero MAC address (00:00:00:00:00:00)
    pub fn zero() -> Self {
        Self([0; 6])
    }

    /// Get the bytes of the MAC address as a slice
    pub fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }

    /// Get the bytes of the MAC address as an array (alias for common API)
    pub fn octets(&self) -> [u8; 6] {
        self.0
    }

    /// Check if this is a broadcast address (ff:ff:ff:ff:ff:ff)
    pub fn is_broadcast(&self) -> bool {
        self.0 == [0xff; 6]
    }

    /// Check if this is a multicast address
    pub fn is_multicast(&self) -> bool {
        (self.0[0] & 0x01) != 0
    }

    /// Check if this is a zero/unset address
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 6]
    }
}

impl std::fmt::Display for MacAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Link status information
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkStatus {
    /// Link speed in Mbps (0 if down)
    pub speed: u32,
    /// Full duplex if true
    pub full_duplex: bool,
    /// Auto-negotiation enabled
    pub autoneg: bool,
    /// Link is up
    pub link_up: bool,
}

/// Port statistics
#[derive(Debug, Clone, Copy, Default)]
pub struct PortStats {
    /// Total received packets
    pub rx_packets: u64,
    /// Total transmitted packets
    pub tx_packets: u64,
    /// Total received bytes
    pub rx_bytes: u64,
    /// Total transmitted bytes
    pub tx_bytes: u64,
    /// Missed packets (no buffer available)
    pub rx_missed: u64,
    /// RX errors
    pub rx_errors: u64,
    /// TX errors
    pub tx_errors: u64,
}

/// Device capability information
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceCapabilities {
    /// Supported RX offloads
    pub rx_offload_capa: u64,
    /// Supported TX offloads
    pub tx_offload_capa: u64,
    /// Maximum RX queues
    pub max_rx_queues: u16,
    /// Maximum TX queues
    pub max_tx_queues: u16,
}

impl DeviceCapabilities {
    /// Check if RX IPv4 checksum offload is supported
    pub fn supports_rx_ipv4_cksum(&self) -> bool {
        (self.rx_offload_capa & dpdk_sys::RTE_ETH_RX_OFFLOAD_IPV4_CKSUM) != 0
    }

    /// Check if RX UDP checksum offload is supported
    pub fn supports_rx_udp_cksum(&self) -> bool {
        (self.rx_offload_capa & dpdk_sys::RTE_ETH_RX_OFFLOAD_UDP_CKSUM) != 0
    }

    /// Check if TX IPv4 checksum offload is supported
    pub fn supports_tx_ipv4_cksum(&self) -> bool {
        (self.tx_offload_capa & dpdk_sys::RTE_ETH_TX_OFFLOAD_IPV4_CKSUM) != 0
    }

    /// Check if TX UDP checksum offload is supported
    pub fn supports_tx_udp_cksum(&self) -> bool {
        (self.tx_offload_capa & dpdk_sys::RTE_ETH_TX_OFFLOAD_UDP_CKSUM) != 0
    }

    /// Check if VLAN stripping is supported
    pub fn supports_vlan_strip(&self) -> bool {
        (self.rx_offload_capa & dpdk_sys::RTE_ETH_RX_OFFLOAD_VLAN_STRIP) != 0
    }

    /// Check if VLAN insertion is supported
    pub fn supports_vlan_insert(&self) -> bool {
        (self.tx_offload_capa & dpdk_sys::RTE_ETH_TX_OFFLOAD_VLAN_INSERT) != 0
    }
}

/// A DPDK Ethernet port
pub struct Port {
    port_id: u16,
    config: PortConfig,
    started: bool,
    mac_address: MacAddress,
    /// Device capabilities (what the NIC supports)
    capabilities: DeviceCapabilities,
    /// Actual offloads enabled (may differ from config if not supported)
    active_rx_offload: u64,
    active_tx_offload: u64,
}

impl Port {
    /// Get the number of available DPDK ports
    pub fn count_available() -> u16 {
        unsafe { dpdk_sys::rte_eth_dev_count_avail() }
    }

    /// Check if a port ID is valid
    pub fn is_valid(port_id: u16) -> bool {
        port_id < Self::count_available()
    }

    /// Initialize a port with the given configuration
    ///
    /// This will:
    /// 1. Validate the port ID
    /// 2. Get device info to check capabilities
    /// 3. Configure the port
    /// 4. Set up RX and TX queues
    pub fn init(port_id: u16, config: PortConfig, mempool: &Mempool) -> DpdkResult<Self> {
        // Validate port ID
        if !Self::is_valid(port_id) {
            return Err(DpdkError::InvalidPortId(port_id));
        }

        // Get device info
        let mut dev_info = dpdk_sys::rte_eth_dev_info::default();
        let ret = unsafe { dpdk_sys::rte_eth_dev_info_get(port_id, &mut dev_info) };
        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }

        // Store capabilities
        let capabilities = DeviceCapabilities {
            rx_offload_capa: dev_info.rx_offload_capa,
            tx_offload_capa: dev_info.tx_offload_capa,
            max_rx_queues: dev_info.max_rx_queues,
            max_tx_queues: dev_info.max_tx_queues,
        };

        // Validate queue counts against device limits
        let nb_rx_queues = config.nb_rx_queues.min(dev_info.max_rx_queues);
        let nb_tx_queues = config.nb_tx_queues.min(dev_info.max_tx_queues);

        // Only enable offloads that are supported by the device
        let requested_rx_offload = config.rx_offload.to_flags();
        let requested_tx_offload = config.tx_offload.to_flags();
        let active_rx_offload = requested_rx_offload & dev_info.rx_offload_capa;
        let active_tx_offload = requested_tx_offload & dev_info.tx_offload_capa;

        // Log if any requested offloads were not available
        if active_rx_offload != requested_rx_offload {
            let unsupported = requested_rx_offload & !dev_info.rx_offload_capa;
            eprintln!("Warning: Some RX offloads not supported by device (flags: 0x{:x})", unsupported);
        }
        if active_tx_offload != requested_tx_offload {
            let unsupported = requested_tx_offload & !dev_info.tx_offload_capa;
            eprintln!("Warning: Some TX offloads not supported by device (flags: 0x{:x})", unsupported);
        }

        // Configure the port with only supported offloads
        let eth_conf = dpdk_sys::rte_eth_conf {
            rxmode: dpdk_sys::rte_eth_rxmode {
                mtu: config.mtu,
                offloads: active_rx_offload,
                ..Default::default()
            },
            txmode: dpdk_sys::rte_eth_txmode {
                offloads: active_tx_offload,
                ..Default::default()
            },
            ..Default::default()
        };

        let ret = unsafe {
            dpdk_sys::rte_eth_dev_configure(port_id, nb_rx_queues, nb_tx_queues, &eth_conf)
        };
        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }

        // Get socket ID for this port (for NUMA awareness)
        let socket_id = unsafe { dpdk_sys::rte_socket_id() };

        // Set up RX queues
        for queue_id in 0..nb_rx_queues {
            let ret = unsafe {
                dpdk_sys::rte_eth_rx_queue_setup(
                    port_id,
                    queue_id,
                    config.nb_rx_desc,
                    socket_id as u32,
                    ptr::null(),
                    mempool.as_raw(),
                )
            };
            if ret != 0 {
                return Err(DpdkError::PortConfigFailed(ret));
            }
        }

        // Set up TX queues
        for queue_id in 0..nb_tx_queues {
            let ret = unsafe {
                dpdk_sys::rte_eth_tx_queue_setup(
                    port_id,
                    queue_id,
                    config.nb_tx_desc,
                    socket_id as u32,
                    ptr::null(),
                )
            };
            if ret != 0 {
                return Err(DpdkError::PortConfigFailed(ret));
            }
        }

        // Get MAC address
        let mut mac_addr = dpdk_sys::rte_ether_addr::default();
        let ret = unsafe { dpdk_sys::rte_eth_macaddr_get(port_id, &mut mac_addr) };
        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }

        let mac_address = MacAddress::new(mac_addr.addr_bytes);

        Ok(Self {
            port_id,
            config: PortConfig {
                nb_rx_queues,
                nb_tx_queues,
                ..config
            },
            started: false,
            mac_address,
            capabilities,
            active_rx_offload,
            active_tx_offload,
        })
    }

    /// Create a port handle without full initialization (for testing)
    pub fn new(port_id: u16) -> DpdkResult<Self> {
        if !Self::is_valid(port_id) {
            return Err(DpdkError::InvalidPortId(port_id));
        }

        Ok(Self {
            port_id,
            config: PortConfig::default(),
            started: false,
            mac_address: MacAddress::default(),
            capabilities: DeviceCapabilities::default(),
            active_rx_offload: 0,
            active_tx_offload: 0,
        })
    }

    /// Get the device capabilities
    pub fn capabilities(&self) -> &DeviceCapabilities {
        &self.capabilities
    }

    /// Get the NUMA node of this NIC port.
    ///
    /// Returns the NUMA socket ID where the NIC is attached. Use this to
    /// allocate mempools and pin lcores on the same NUMA node for optimal
    /// memory access latency.
    pub fn numa_node(&self) -> i32 {
        dpdk_sys::rte_eth_dev_socket_id(self.port_id)
    }

    /// Get the active RX offload flags
    pub fn active_rx_offload(&self) -> u64 {
        self.active_rx_offload
    }

    /// Get the active TX offload flags
    pub fn active_tx_offload(&self) -> u64 {
        self.active_tx_offload
    }

    /// Check if a specific RX offload is active
    pub fn is_rx_offload_active(&self, offload: u64) -> bool {
        (self.active_rx_offload & offload) != 0
    }

    /// Check if a specific TX offload is active
    pub fn is_tx_offload_active(&self, offload: u64) -> bool {
        (self.active_tx_offload & offload) != 0
    }

    /// Get the port ID
    pub fn port_id(&self) -> u16 {
        self.port_id
    }

    /// Get the MAC address of this port
    pub fn mac_address(&self) -> MacAddress {
        self.mac_address
    }

    /// Get the port configuration
    pub fn config(&self) -> &PortConfig {
        &self.config
    }

    /// Check if the port is started
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Start the port
    pub fn start(&mut self) -> DpdkResult<()> {
        if self.started {
            return Ok(());
        }

        let ret = unsafe { dpdk_sys::rte_eth_dev_start(self.port_id) };
        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }

        // Enable promiscuous mode if configured
        if self.config.promiscuous {
            unsafe {
                dpdk_sys::rte_eth_promiscuous_enable(self.port_id);
            }
        }

        self.started = true;
        Ok(())
    }

    /// Stop the port
    pub fn stop(&mut self) -> DpdkResult<()> {
        if !self.started {
            return Ok(());
        }

        let ret = unsafe { dpdk_sys::rte_eth_dev_stop(self.port_id) };
        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }

        self.started = false;
        Ok(())
    }

    /// Get link status
    pub fn link_status(&self) -> LinkStatus {
        let mut link = dpdk_sys::rte_eth_link::default();
        unsafe {
            dpdk_sys::rte_eth_link_get_nowait(self.port_id, &mut link);
        }

        LinkStatus {
            speed: link.link_speed,
            full_duplex: link.link_duplex() != 0,
            autoneg: link.link_autoneg() != 0,
            link_up: link.link_status() != 0,
        }
    }

    /// Get port statistics
    pub fn stats(&self) -> DpdkResult<PortStats> {
        let mut stats = dpdk_sys::rte_eth_stats::default();
        let ret = unsafe { dpdk_sys::rte_eth_stats_get(self.port_id, &mut stats) };
        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }

        Ok(PortStats {
            rx_packets: stats.ipackets,
            tx_packets: stats.opackets,
            rx_bytes: stats.ibytes,
            tx_bytes: stats.obytes,
            rx_missed: stats.imissed,
            rx_errors: stats.ierrors,
            tx_errors: stats.oerrors,
        })
    }

    /// Reset port statistics
    pub fn reset_stats(&self) -> DpdkResult<()> {
        let ret = unsafe { dpdk_sys::rte_eth_stats_reset(self.port_id) };
        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }
        Ok(())
    }

    /// Receive a burst of packets from the specified queue
    ///
    /// Returns a vector of received Mbufs. The maximum number of packets
    /// returned is limited by `max_packets`.
    pub fn rx_burst(&self, queue_id: u16, max_packets: u16) -> DpdkResult<Vec<Mbuf>> {
        if !self.started {
            return Ok(Vec::new());
        }

        let mut rx_pkts: Vec<*mut dpdk_sys::rte_mbuf> = vec![ptr::null_mut(); max_packets as usize];

        let nb_rx = unsafe {
            dpdk_sys::rte_eth_rx_burst(
                self.port_id,
                queue_id,
                rx_pkts.as_mut_ptr(),
                max_packets,
            )
        };

        let mut mbufs = Vec::with_capacity(nb_rx as usize);
        for i in 0..nb_rx as usize {
            if let Some(mbuf) = unsafe { Mbuf::from_raw(rx_pkts[i]) } {
                mbufs.push(mbuf);
            }
        }

        Ok(mbufs)
    }

    /// Transmit a burst of packets on the specified queue
    ///
    /// Returns the number of packets successfully transmitted.
    /// Packets that are sent are consumed (freed) by the driver.
    pub fn tx_burst(&self, queue_id: u16, packets: &mut Vec<Mbuf>) -> DpdkResult<u16> {
        if !self.started || packets.is_empty() {
            return Ok(0);
        }

        // Convert Mbufs to raw pointers
        let mut tx_pkts: Vec<*mut dpdk_sys::rte_mbuf> = packets
            .iter()
            .map(|m| m.as_raw())
            .collect();

        let nb_tx = unsafe {
            dpdk_sys::rte_eth_tx_burst(
                self.port_id,
                queue_id,
                tx_pkts.as_mut_ptr(),
                tx_pkts.len() as u16,
            )
        };

        // Remove sent packets from the vector (they're now owned by the driver)
        // We need to forget them to prevent double-free
        for mbuf in packets.drain(..nb_tx as usize) {
            std::mem::forget(mbuf);
        }

        Ok(nb_tx)
    }

    // ========================================================================
    // Promiscuous Mode
    // ========================================================================

    /// Enable promiscuous mode on the port
    ///
    /// In promiscuous mode, the port receives all packets regardless of
    /// destination MAC address.
    pub fn set_promiscuous(&mut self, enable: bool) -> DpdkResult<()> {
        let ret = if enable {
            unsafe { dpdk_sys::rte_eth_promiscuous_enable(self.port_id) }
        } else {
            unsafe { dpdk_sys::rte_eth_promiscuous_disable(self.port_id) }
        };

        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }

        self.config.promiscuous = enable;
        Ok(())
    }

    /// Check if promiscuous mode is enabled
    pub fn is_promiscuous(&self) -> bool {
        unsafe { dpdk_sys::rte_eth_promiscuous_get(self.port_id) != 0 }
    }

    // ========================================================================
    // All-Multicast Mode
    // ========================================================================

    /// Enable all-multicast mode on the port
    ///
    /// In all-multicast mode, the port receives all multicast packets
    /// regardless of whether they match configured multicast addresses.
    pub fn set_allmulticast(&self, enable: bool) -> DpdkResult<()> {
        let ret = if enable {
            unsafe { dpdk_sys::rte_eth_allmulticast_enable(self.port_id) }
        } else {
            unsafe { dpdk_sys::rte_eth_allmulticast_disable(self.port_id) }
        };

        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }
        Ok(())
    }

    /// Check if all-multicast mode is enabled
    pub fn is_allmulticast(&self) -> bool {
        unsafe { dpdk_sys::rte_eth_allmulticast_get(self.port_id) != 0 }
    }

    // ========================================================================
    // Multicast Address Management
    // ========================================================================

    /// Set the list of multicast MAC addresses to receive
    ///
    /// This configures the hardware multicast filter. Pass an empty slice
    /// to clear all multicast addresses.
    pub fn set_multicast_addrs(&self, addrs: &[MacAddress]) -> DpdkResult<()> {
        if addrs.is_empty() {
            // Clear multicast list
            let ret = unsafe {
                dpdk_sys::rte_eth_dev_set_mc_addr_list(
                    self.port_id,
                    ptr::null_mut(),
                    0,
                )
            };
            if ret != 0 {
                return Err(DpdkError::PortConfigFailed(ret));
            }
            return Ok(());
        }

        // Convert MacAddresses to rte_ether_addr
        let mut mc_addrs: Vec<dpdk_sys::rte_ether_addr> = addrs
            .iter()
            .map(|mac| dpdk_sys::rte_ether_addr {
                addr_bytes: mac.octets(),
            })
            .collect();

        let ret = unsafe {
            dpdk_sys::rte_eth_dev_set_mc_addr_list(
                self.port_id,
                mc_addrs.as_mut_ptr(),
                mc_addrs.len() as u32,
            )
        };

        if ret != 0 {
            return Err(DpdkError::PortConfigFailed(ret));
        }
        Ok(())
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        if self.started {
            let _ = self.stop();
        }
        unsafe {
            dpdk_sys::rte_eth_dev_close(self.port_id);
        }
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mac_address_display() {
        let mac = MacAddress::new([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(mac.to_string(), "02:00:00:00:00:01");
    }

    #[test]
    fn test_mac_address_broadcast() {
        let broadcast = MacAddress::new([0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert!(broadcast.is_broadcast());
        assert!(broadcast.is_multicast());

        let unicast = MacAddress::new([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert!(!unicast.is_broadcast());
        assert!(!unicast.is_multicast());
    }

    #[test]
    fn test_mac_address_multicast() {
        // Multicast addresses have LSB of first byte set
        let multicast = MacAddress::new([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        assert!(multicast.is_multicast());
        assert!(!multicast.is_broadcast());
    }

    #[test]
    fn test_mac_address_zero() {
        let zero = MacAddress::default();
        assert!(zero.is_zero());

        let non_zero = MacAddress::new([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert!(!non_zero.is_zero());
    }

    #[test]
    fn test_port_config_default() {
        let config = PortConfig::default();
        assert_eq!(config.nb_rx_queues, 1);
        assert_eq!(config.nb_tx_queues, 1);
        assert_eq!(config.nb_rx_desc, DEFAULT_RX_DESC);
        assert_eq!(config.nb_tx_desc, DEFAULT_TX_DESC);
        assert!(config.promiscuous);
    }

    #[test]
    fn test_link_status_default() {
        let status = LinkStatus::default();
        assert_eq!(status.speed, 0);
        assert!(!status.full_duplex);
        assert!(!status.link_up);
    }

    #[test]
    fn test_port_stats_default() {
        let stats = PortStats::default();
        assert_eq!(stats.rx_packets, 0);
        assert_eq!(stats.tx_packets, 0);
        assert_eq!(stats.rx_bytes, 0);
        assert_eq!(stats.tx_bytes, 0);
    }

    #[test]
    fn test_port_count_available() {
        // With stubs, this returns 1
        let count = Port::count_available();
        assert!(count >= 1);
    }

    #[test]
    fn test_port_is_valid() {
        // Port 0 should be valid (stubs return count of 1)
        assert!(Port::is_valid(0));
        // Port 100 should be invalid
        assert!(!Port::is_valid(100));
    }

    #[test]
    fn test_port_new() {
        let port = Port::new(0);
        assert!(port.is_ok());
        let port = port.unwrap();
        assert_eq!(port.port_id(), 0);
        assert!(!port.is_started());
    }

    #[test]
    fn test_port_new_invalid() {
        let port = Port::new(100);
        assert!(port.is_err());
        match port {
            Err(DpdkError::InvalidPortId(id)) => assert_eq!(id, 100),
            _ => panic!("Expected InvalidPortId error"),
        }
    }

    #[test]
    fn test_port_start_stop() {
        let mut port = Port::new(0).unwrap();

        // Start
        assert!(port.start().is_ok());
        assert!(port.is_started());

        // Start again should be no-op
        assert!(port.start().is_ok());

        // Stop
        assert!(port.stop().is_ok());
        assert!(!port.is_started());

        // Stop again should be no-op
        assert!(port.stop().is_ok());
    }

    #[test]
    fn test_port_link_status() {
        let port = Port::new(0).unwrap();
        let status = port.link_status();
        // With stubs, link shows as up at 10 Gbps
        assert!(status.link_up);
        assert_eq!(status.speed, 10000);
    }

    #[test]
    fn test_port_stats() {
        let port = Port::new(0).unwrap();
        let stats = port.stats();
        assert!(stats.is_ok());
        let stats = stats.unwrap();
        assert_eq!(stats.rx_packets, 0);
        assert_eq!(stats.tx_packets, 0);
    }

    #[test]
    fn test_port_reset_stats() {
        let port = Port::new(0).unwrap();
        assert!(port.reset_stats().is_ok());
    }

    #[test]
    fn test_port_rx_burst_not_started() {
        let port = Port::new(0).unwrap();
        let packets = port.rx_burst(0, 32);
        assert!(packets.is_ok());
        assert!(packets.unwrap().is_empty()); // Not started, returns empty
    }

    #[test]
    fn test_port_tx_burst_not_started() {
        let port = Port::new(0).unwrap();
        let mut packets: Vec<Mbuf> = Vec::new();
        let sent = port.tx_burst(0, &mut packets);
        assert!(sent.is_ok());
        assert_eq!(sent.unwrap(), 0); // Not started, returns 0
    }
}
