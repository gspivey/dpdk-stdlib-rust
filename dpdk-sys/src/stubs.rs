//! Stub implementations of DPDK functions and types
//!
//! These stubs allow the crate to compile and run without DPDK installed.
//! They provide the same API surface but don't perform actual packet I/O.

use libc::{c_char, c_int, c_uint, c_void, size_t};
use std::ptr;

// ============================================================================
// Constants
// ============================================================================

pub const RTE_MAX_ETHPORTS: usize = 32;
pub const RTE_MAX_LCORE: usize = 128;
pub const RTE_ETHER_ADDR_LEN: usize = 6;
pub const RTE_ETHER_TYPE_IPV4: u16 = 0x0800;
pub const RTE_ETHER_TYPE_IPV6: u16 = 0x86DD;
pub const RTE_ETHER_TYPE_ARP: u16 = 0x0806;
pub const RTE_ETHER_TYPE_VLAN: u16 = 0x8100;

pub const RTE_MBUF_DEFAULT_BUF_SIZE: u16 = 2048 + 128; // RTE_PKTMBUF_HEADROOM
pub const RTE_PKTMBUF_HEADROOM: u16 = 128;

pub const RTE_ETH_TX_OFFLOAD_VLAN_INSERT: u64 = 0x00000001;
pub const RTE_ETH_TX_OFFLOAD_IPV4_CKSUM: u64 = 0x00000002;
pub const RTE_ETH_TX_OFFLOAD_UDP_CKSUM: u64 = 0x00000004;
pub const RTE_ETH_TX_OFFLOAD_TCP_CKSUM: u64 = 0x00000008;

pub const RTE_ETH_RX_OFFLOAD_VLAN_STRIP: u64 = 0x00000001;
pub const RTE_ETH_RX_OFFLOAD_IPV4_CKSUM: u64 = 0x00000002;
pub const RTE_ETH_RX_OFFLOAD_UDP_CKSUM: u64 = 0x00000004;
pub const RTE_ETH_RX_OFFLOAD_TCP_CKSUM: u64 = 0x00000008;

// Error codes
pub const RTE_ERRNO_BASE: c_int = 1000;

// ============================================================================
// Core Types
// ============================================================================

/// Ethernet address (MAC address)
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct rte_ether_addr {
    pub addr_bytes: [u8; RTE_ETHER_ADDR_LEN],
}

/// Ethernet header
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct rte_ether_hdr {
    pub dst_addr: rte_ether_addr,
    pub src_addr: rte_ether_addr,
    pub ether_type: u16,
}

/// IPv4 header
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct rte_ipv4_hdr {
    pub version_ihl: u8,
    pub type_of_service: u8,
    pub total_length: u16,
    pub packet_id: u16,
    pub fragment_offset: u16,
    pub time_to_live: u8,
    pub next_proto_id: u8,
    pub hdr_checksum: u16,
    pub src_addr: u32,
    pub dst_addr: u32,
}

/// UDP header
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct rte_udp_hdr {
    pub src_port: u16,
    pub dst_port: u16,
    pub dgram_len: u16,
    pub dgram_cksum: u16,
}

/// TCP header
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct rte_tcp_hdr {
    pub src_port: u16,
    pub dst_port: u16,
    pub sent_seq: u32,
    pub recv_ack: u32,
    pub data_off: u8,
    pub tcp_flags: u8,
    pub rx_win: u16,
    pub cksum: u16,
    pub tcp_urp: u16,
}

// ============================================================================
// Memory Buffer (mbuf) Types
// ============================================================================

/// Memory buffer for packet data
#[repr(C)]
#[derive(Debug)]
pub struct rte_mbuf {
    pub buf_addr: *mut c_void,
    pub buf_iova: u64,

    // First cache line
    pub data_off: u16,
    pub refcnt: u16,
    pub nb_segs: u16,
    pub port: u16,
    pub ol_flags: u64,

    // Packet metadata
    pub packet_type: u32,
    pub pkt_len: u32,
    pub data_len: u16,
    pub vlan_tci: u16,

    // Hash
    pub hash: rte_mbuf_hash,

    pub vlan_tci_outer: u16,
    pub buf_len: u16,

    pub pool: *mut rte_mempool,

    // Second cache line
    pub next: *mut rte_mbuf,

    // Tx offload
    pub tx_offload: u64,

    pub priv_size: u16,
    pub timesync: u16,
    pub seqn: u32,

    pub dynfield1: [u64; 2],
}

impl Default for rte_mbuf {
    fn default() -> Self {
        Self {
            buf_addr: ptr::null_mut(),
            buf_iova: 0,
            data_off: RTE_PKTMBUF_HEADROOM,
            refcnt: 1,
            nb_segs: 1,
            port: 0,
            ol_flags: 0,
            packet_type: 0,
            pkt_len: 0,
            data_len: 0,
            vlan_tci: 0,
            hash: rte_mbuf_hash::default(),
            vlan_tci_outer: 0,
            buf_len: 0,
            pool: ptr::null_mut(),
            next: ptr::null_mut(),
            tx_offload: 0,
            priv_size: 0,
            timesync: 0,
            seqn: 0,
            dynfield1: [0; 2],
        }
    }
}

/// Hash union for rte_mbuf
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct rte_mbuf_hash {
    pub rss: u32,
}

// ============================================================================
// Memory Pool Types
// ============================================================================

/// Memory pool (opaque)
#[repr(C)]
pub struct rte_mempool {
    _private: [u8; 0],
}

/// Memory pool cache (opaque)
#[repr(C)]
pub struct rte_mempool_cache {
    _private: [u8; 0],
}

// ============================================================================
// Ethernet Device Types
// ============================================================================

/// Ethernet device info
#[repr(C)]
#[derive(Debug, Default)]
pub struct rte_eth_dev_info {
    pub device: *mut c_void,
    pub driver_name: *const c_char,
    pub if_index: c_uint,
    pub min_mtu: u16,
    pub max_mtu: u16,
    pub dev_flags: *const u32,
    pub min_rx_bufsize: u32,
    pub max_rx_pktlen: u32,
    pub max_lro_pkt_size: u32,
    pub max_rx_queues: u16,
    pub max_tx_queues: u16,
    pub max_mac_addrs: u32,
    pub max_vfs: u16,
    pub max_vmdq_pools: u16,
    pub rx_offload_capa: u64,
    pub tx_offload_capa: u64,
    pub rx_queue_offload_capa: u64,
    pub tx_queue_offload_capa: u64,
    pub reta_size: u16,
    pub hash_key_size: u8,
    pub flow_type_rss_offloads: u64,
    pub default_rxconf: rte_eth_rxconf,
    pub default_txconf: rte_eth_txconf,
    pub vmdq_queue_base: u16,
    pub vmdq_queue_num: u16,
    pub vmdq_pool_base: u16,
    pub rx_desc_lim: rte_eth_desc_lim,
    pub tx_desc_lim: rte_eth_desc_lim,
    pub speed_capa: u32,
    pub nb_rx_queues: u16,
    pub nb_tx_queues: u16,
    pub dev_capa: u64,
}

/// Ethernet device configuration
#[repr(C)]
#[derive(Debug, Default, Clone)]
pub struct rte_eth_conf {
    pub link_speeds: u32,
    pub rxmode: rte_eth_rxmode,
    pub txmode: rte_eth_txmode,
    pub lpbk_mode: u32,
    pub rx_adv_conf: rte_eth_rx_adv_conf,
    pub tx_adv_conf: rte_eth_tx_adv_conf,
    pub dcb_capability_en: u32,
    pub intr_conf: rte_eth_intr_conf,
}

#[repr(C)]
#[derive(Debug, Default, Clone)]
pub struct rte_eth_rxmode {
    pub mq_mode: u32,
    pub mtu: u32,
    pub max_lro_pkt_size: u32,
    pub offloads: u64,
    pub reserved_64s: [u64; 2],
    pub reserved_ptrs: [*mut c_void; 2],
}

#[repr(C)]
#[derive(Debug, Default, Clone)]
pub struct rte_eth_txmode {
    pub mq_mode: u32,
    pub offloads: u64,
    pub pvid: u16,
    pub reserved_64s: [u64; 2],
    pub reserved_ptrs: [*mut c_void; 2],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rte_eth_rxconf {
    pub rx_thresh: rte_eth_thresh,
    pub rx_free_thresh: u16,
    pub rx_drop_en: u8,
    pub rx_deferred_start: u8,
    pub rx_nseg: u16,
    pub share_group: u16,
    pub share_qid: u16,
    pub offloads: u64,
    pub rx_seg: *mut c_void,
    pub reserved_64s: [u64; 2],
    pub reserved_ptrs: [*mut c_void; 2],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rte_eth_txconf {
    pub tx_thresh: rte_eth_thresh,
    pub tx_rs_thresh: u16,
    pub tx_free_thresh: u16,
    pub tx_deferred_start: u8,
    pub offloads: u64,
    pub reserved_64s: [u64; 2],
    pub reserved_ptrs: [*mut c_void; 2],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rte_eth_thresh {
    pub pthresh: u8,
    pub hthresh: u8,
    pub wthresh: u8,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rte_eth_desc_lim {
    pub nb_max: u16,
    pub nb_min: u16,
    pub nb_align: u16,
    pub nb_seg_max: u16,
    pub nb_mtu_seg_max: u16,
}

#[repr(C)]
#[derive(Debug, Default, Clone)]
pub struct rte_eth_rx_adv_conf {
    pub rss_conf: rte_eth_rss_conf,
}

#[repr(C)]
#[derive(Debug, Default, Clone)]
pub struct rte_eth_tx_adv_conf {
    _placeholder: u8,
}

#[repr(C)]
#[derive(Debug, Default, Clone)]
pub struct rte_eth_rss_conf {
    pub rss_key: *mut u8,
    pub rss_key_len: u8,
    pub rss_hf: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rte_eth_intr_conf {
    pub lsc: u16,
    pub rxq: u16,
    pub rmv: u16,
}

/// Link status
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rte_eth_link {
    pub link_speed: u32,
    pub link_duplex: u16,
    pub link_autoneg: u16,
    pub link_status: u16,
}

/// Ethernet statistics
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rte_eth_stats {
    pub ipackets: u64,
    pub opackets: u64,
    pub ibytes: u64,
    pub obytes: u64,
    pub imissed: u64,
    pub ierrors: u64,
    pub oerrors: u64,
    pub rx_nombuf: u64,
    pub q_ipackets: [u64; 16],
    pub q_opackets: [u64; 16],
    pub q_ibytes: [u64; 16],
    pub q_obytes: [u64; 16],
    pub q_errors: [u64; 16],
}

// ============================================================================
// Ring Buffer Types
// ============================================================================

/// Ring buffer (opaque)
#[repr(C)]
pub struct rte_ring {
    _private: [u8; 0],
}

// ============================================================================
// Stub Function Implementations
// ============================================================================

// EAL Functions
#[no_mangle]
pub extern "C" fn rte_eal_init(_argc: c_int, _argv: *mut *mut c_char) -> c_int {
    0 // Success
}

#[no_mangle]
pub extern "C" fn rte_eal_cleanup() -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_lcore_id() -> c_uint {
    0
}

#[no_mangle]
pub extern "C" fn rte_lcore_count() -> c_uint {
    1
}

#[no_mangle]
pub extern "C" fn rte_get_main_lcore() -> c_uint {
    0
}

#[no_mangle]
pub extern "C" fn rte_socket_id() -> c_int {
    0
}

// Memory Pool Functions
#[no_mangle]
pub extern "C" fn rte_pktmbuf_pool_create(
    _name: *const c_char,
    _n: c_uint,
    _cache_size: c_uint,
    _priv_size: u16,
    _data_room_size: u16,
    _socket_id: c_int,
) -> *mut rte_mempool {
    ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn rte_mempool_free(_mp: *mut rte_mempool) {}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_alloc(_mp: *mut rte_mempool) -> *mut rte_mbuf {
    ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_free(_m: *mut rte_mbuf) {}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_clone(
    _md: *mut rte_mbuf,
    _mp: *mut rte_mempool,
) -> *mut rte_mbuf {
    ptr::null_mut()
}

// Ethernet Device Functions
#[no_mangle]
pub extern "C" fn rte_eth_dev_count_avail() -> u16 {
    1 // Pretend we have one device for testing
}

#[no_mangle]
pub extern "C" fn rte_eth_dev_configure(
    _port_id: u16,
    _nb_rx_queue: u16,
    _nb_tx_queue: u16,
    _eth_conf: *const rte_eth_conf,
) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_rx_queue_setup(
    _port_id: u16,
    _rx_queue_id: u16,
    _nb_rx_desc: u16,
    _socket_id: c_uint,
    _rx_conf: *const rte_eth_rxconf,
    _mb_pool: *mut rte_mempool,
) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_tx_queue_setup(
    _port_id: u16,
    _tx_queue_id: u16,
    _nb_tx_desc: u16,
    _socket_id: c_uint,
    _tx_conf: *const rte_eth_txconf,
) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_dev_start(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_dev_stop(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_dev_close(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_promiscuous_enable(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_promiscuous_disable(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_dev_info_get(
    _port_id: u16,
    dev_info: *mut rte_eth_dev_info,
) -> c_int {
    if !dev_info.is_null() {
        unsafe {
            (*dev_info).max_rx_queues = 16;
            (*dev_info).max_tx_queues = 16;
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_link_get(_port_id: u16, link: *mut rte_eth_link) -> c_int {
    if !link.is_null() {
        unsafe {
            (*link).link_speed = 10000; // 10 Gbps
            (*link).link_duplex = 1;
            (*link).link_status = 1; // Link up
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_link_get_nowait(_port_id: u16, link: *mut rte_eth_link) -> c_int {
    rte_eth_link_get(_port_id, link)
}

#[no_mangle]
pub extern "C" fn rte_eth_stats_get(_port_id: u16, _stats: *mut rte_eth_stats) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_stats_reset(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_macaddr_get(_port_id: u16, mac_addr: *mut rte_ether_addr) -> c_int {
    if !mac_addr.is_null() {
        unsafe {
            (*mac_addr).addr_bytes = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        }
    }
    0
}

// Packet RX/TX Functions
#[no_mangle]
pub extern "C" fn rte_eth_rx_burst(
    _port_id: u16,
    _queue_id: u16,
    _rx_pkts: *mut *mut rte_mbuf,
    _nb_pkts: u16,
) -> u16 {
    0 // No packets received (stub)
}

#[no_mangle]
pub extern "C" fn rte_eth_tx_burst(
    _port_id: u16,
    _queue_id: u16,
    _tx_pkts: *mut *mut rte_mbuf,
    _nb_pkts: u16,
) -> u16 {
    0 // No packets sent (stub)
}

// Error handling
#[no_mangle]
pub extern "C" fn rte_errno() -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_strerror(_errnum: c_int) -> *const c_char {
    b"No error (stub)\0".as_ptr() as *const c_char
}

// Utility functions
#[no_mangle]
pub extern "C" fn rte_cpu_to_be_16(x: u16) -> u16 {
    x.to_be()
}

#[no_mangle]
pub extern "C" fn rte_cpu_to_be_32(x: u32) -> u32 {
    x.to_be()
}

#[no_mangle]
pub extern "C" fn rte_be_to_cpu_16(x: u16) -> u16 {
    u16::from_be(x)
}

#[no_mangle]
pub extern "C" fn rte_be_to_cpu_32(x: u32) -> u32 {
    u32::from_be(x)
}

// Checksum functions
#[no_mangle]
pub extern "C" fn rte_ipv4_cksum(_ipv4_hdr: *const rte_ipv4_hdr) -> u16 {
    0 // Stub - would compute checksum
}

#[no_mangle]
pub extern "C" fn rte_ipv4_udptcp_cksum(
    _ipv4_hdr: *const rte_ipv4_hdr,
    _l4_hdr: *const c_void,
) -> u16 {
    0 // Stub - would compute checksum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eal_init() {
        let result = rte_eal_init(0, ptr::null_mut());
        assert_eq!(result, 0);
    }

    #[test]
    fn test_eth_dev_count() {
        let count = rte_eth_dev_count_avail();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_lcore_id() {
        let id = rte_lcore_id();
        assert_eq!(id, 0);
    }
}
