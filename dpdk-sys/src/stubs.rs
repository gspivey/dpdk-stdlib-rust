//! Stub implementations of DPDK functions and types
//!
//! These stubs allow the crate to compile and run without DPDK installed.
//! They provide the same API surface but don't perform actual packet I/O.

use libc::{c_char, c_int, c_uint, c_void};
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

// NUMA socket constants
pub const SOCKET_ID_ANY: c_int = -1;

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

/// Memory pool (simplified stub version for testing)
#[repr(C)]
#[derive(Debug, Default)]
pub struct rte_mempool {
    /// Pool name
    pub name: [c_char; 32],
    /// Total size of the mempool
    pub size: c_uint,
    /// Number of elements populated
    pub populated_size: c_uint,
    /// Element size
    pub elt_size: c_uint,
    /// Flags
    pub flags: c_uint,
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
#[derive(Debug)]
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

impl Default for rte_eth_dev_info {
    fn default() -> Self {
        Self {
            device: ptr::null_mut(),
            driver_name: ptr::null(),
            if_index: 0,
            min_mtu: 0,
            max_mtu: 0,
            dev_flags: ptr::null(),
            min_rx_bufsize: 0,
            max_rx_pktlen: 0,
            max_lro_pkt_size: 0,
            max_rx_queues: 0,
            max_tx_queues: 0,
            max_mac_addrs: 0,
            max_vfs: 0,
            max_vmdq_pools: 0,
            rx_offload_capa: 0,
            tx_offload_capa: 0,
            rx_queue_offload_capa: 0,
            tx_queue_offload_capa: 0,
            reta_size: 0,
            hash_key_size: 0,
            flow_type_rss_offloads: 0,
            default_rxconf: rte_eth_rxconf::default(),
            default_txconf: rte_eth_txconf::default(),
            vmdq_queue_base: 0,
            vmdq_queue_num: 0,
            vmdq_pool_base: 0,
            rx_desc_lim: rte_eth_desc_lim::default(),
            tx_desc_lim: rte_eth_desc_lim::default(),
            speed_capa: 0,
            nb_rx_queues: 0,
            nb_tx_queues: 0,
            dev_capa: 0,
        }
    }
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
#[derive(Debug, Clone)]
pub struct rte_eth_rxmode {
    pub mq_mode: u32,
    pub mtu: u32,
    pub max_lro_pkt_size: u32,
    pub offloads: u64,
    pub reserved_64s: [u64; 2],
    pub reserved_ptrs: [*mut c_void; 2],
}

impl Default for rte_eth_rxmode {
    fn default() -> Self {
        Self {
            mq_mode: 0,
            mtu: 0,
            max_lro_pkt_size: 0,
            offloads: 0,
            reserved_64s: [0; 2],
            reserved_ptrs: [ptr::null_mut(); 2],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct rte_eth_txmode {
    pub mq_mode: u32,
    pub offloads: u64,
    pub pvid: u16,
    pub reserved_64s: [u64; 2],
    pub reserved_ptrs: [*mut c_void; 2],
}

impl Default for rte_eth_txmode {
    fn default() -> Self {
        Self {
            mq_mode: 0,
            offloads: 0,
            pvid: 0,
            reserved_64s: [0; 2],
            reserved_ptrs: [ptr::null_mut(); 2],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
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

impl Default for rte_eth_rxconf {
    fn default() -> Self {
        Self {
            rx_thresh: rte_eth_thresh::default(),
            rx_free_thresh: 0,
            rx_drop_en: 0,
            rx_deferred_start: 0,
            rx_nseg: 0,
            share_group: 0,
            share_qid: 0,
            offloads: 0,
            rx_seg: ptr::null_mut(),
            reserved_64s: [0; 2],
            reserved_ptrs: [ptr::null_mut(); 2],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct rte_eth_txconf {
    pub tx_thresh: rte_eth_thresh,
    pub tx_rs_thresh: u16,
    pub tx_free_thresh: u16,
    pub tx_deferred_start: u8,
    pub offloads: u64,
    pub reserved_64s: [u64; 2],
    pub reserved_ptrs: [*mut c_void; 2],
}

impl Default for rte_eth_txconf {
    fn default() -> Self {
        Self {
            tx_thresh: rte_eth_thresh::default(),
            tx_rs_thresh: 0,
            tx_free_thresh: 0,
            tx_deferred_start: 0,
            offloads: 0,
            reserved_64s: [0; 2],
            reserved_ptrs: [ptr::null_mut(); 2],
        }
    }
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
#[derive(Debug, Clone)]
pub struct rte_eth_rss_conf {
    pub rss_key: *mut u8,
    pub rss_key_len: u8,
    pub rss_hf: u64,
}

impl Default for rte_eth_rss_conf {
    fn default() -> Self {
        Self {
            rss_key: ptr::null_mut(),
            rss_key_len: 0,
            rss_hf: 0,
        }
    }
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

// Accessor methods matching the bindgen-generated bitfield accessors
// so that `link.link_duplex()` works identically for stubs and real DPDK.
impl rte_eth_link {
    pub fn link_duplex(&self) -> u16 {
        self.link_duplex
    }
    pub fn link_autoneg(&self) -> u16 {
        self.link_autoneg
    }
    pub fn link_status(&self) -> u16 {
        self.link_status
    }
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

/// EAL lifecycle state tracking — detects use-after-cleanup bugs in stubs.
/// Real DPDK segfaults when you call rte_pktmbuf_pool_create after rte_eal_cleanup
/// because rte_config->mem_config is NULL. This state lets stubs catch the same bug.
///
/// Three states:
///   0 = never initialized (permissive — allows mempool creation for backward compat)
///   1 = initialized (rte_eal_init called)
///  -1 = cleaned up (rte_eal_cleanup called after init — mempool creation denied)
use std::sync::atomic::{AtomicI32, Ordering};
static STUB_EAL_STATE: AtomicI32 = AtomicI32::new(0);

/// Returns true if EAL is currently initialized (init called, cleanup not yet called).
/// Exposed for tests to verify lifecycle behavior.
pub fn stub_eal_is_initialized() -> bool {
    STUB_EAL_STATE.load(Ordering::SeqCst) == 1
}

/// Returns true if EAL was cleaned up after being initialized.
/// This is the state that causes segfaults with real DPDK.
pub fn stub_eal_is_cleaned_up() -> bool {
    STUB_EAL_STATE.load(Ordering::SeqCst) == -1
}

#[no_mangle]
pub extern "C" fn rte_eal_init(_argc: c_int, _argv: *mut *mut c_char) -> c_int {
    STUB_EAL_STATE.store(1, Ordering::SeqCst);
    0 // Success
}

#[no_mangle]
pub extern "C" fn rte_eal_cleanup() -> c_int {
    STUB_EAL_STATE.store(-1, Ordering::SeqCst);
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

/// Stub mempool tracking for testing
/// We use thread_local to track allocated mempools and mbufs
use std::cell::RefCell;

thread_local! {
    static STUB_MEMPOOL_COUNTER: RefCell<u32> = const { RefCell::new(0) };
    static STUB_MBUF_COUNTER: RefCell<u32> = const { RefCell::new(0) };
}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_pool_create(
    _name: *const c_char,
    n: c_uint,
    _cache_size: c_uint,
    _priv_size: u16,
    _data_room_size: u16,
    _socket_id: c_int,
) -> *mut rte_mempool {
    // Guard: real DPDK segfaults if EAL was cleaned up (rte_config->mem_config is NULL).
    // Return NULL to surface the same class of bug without crashing.
    // Only block when EAL was explicitly cleaned up (-1), not when never initialized (0),
    // so existing tests that create mempools without calling rte_eal_init() still work.
    if stub_eal_is_cleaned_up() {
        // Set rte_errno to ENODEV so callers get a meaningful error
        STUB_RTE_ERRNO.store(19, Ordering::SeqCst); // ENODEV
        return ptr::null_mut();
    }

    // Create a stub mempool for testing
    let mempool = Box::new(rte_mempool {
        size: n,
        populated_size: n,
        ..Default::default()
    });
    Box::into_raw(mempool)
}

#[no_mangle]
pub extern "C" fn rte_mempool_free(mp: *mut rte_mempool) {
    if !mp.is_null() {
        unsafe {
            let _ = Box::from_raw(mp);
        }
    }
}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_alloc(_mp: *mut rte_mempool) -> *mut rte_mbuf {
    // Allocate a stub mbuf with a real buffer
    let buf_size = 2048usize;
    let buf: Vec<u8> = vec![0u8; buf_size];
    let buf_ptr = Box::into_raw(buf.into_boxed_slice()) as *mut c_void;

    let mbuf = Box::new(rte_mbuf {
        buf_addr: buf_ptr,
        buf_len: buf_size as u16,
        data_off: RTE_PKTMBUF_HEADROOM,
        data_len: 0,
        pkt_len: 0,
        ..Default::default()
    });
    Box::into_raw(mbuf)
}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_alloc_bulk(
    _mp: *mut rte_mempool,
    mbufs: *mut *mut rte_mbuf,
    count: c_uint,
) -> c_int {
    if mbufs.is_null() {
        return -1;
    }

    for i in 0..count as usize {
        let mbuf = rte_pktmbuf_alloc(_mp);
        if mbuf.is_null() {
            // Free previously allocated mbufs
            for j in 0..i {
                unsafe {
                    rte_pktmbuf_free(*mbufs.add(j));
                }
            }
            return -1;
        }
        unsafe {
            *mbufs.add(i) = mbuf;
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_free(m: *mut rte_mbuf) {
    if !m.is_null() {
        unsafe {
            let mbuf = Box::from_raw(m);
            // Free the buffer if it exists
            // We allocated it with Box::into_raw(vec.into_boxed_slice())
            // so we need to reconstruct the boxed slice to free it
            if !mbuf.buf_addr.is_null() && mbuf.buf_len > 0 {
                let slice = std::slice::from_raw_parts_mut(
                    mbuf.buf_addr as *mut u8,
                    mbuf.buf_len as usize,
                );
                let _ = Box::from_raw(slice as *mut [u8]);
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn rte_pktmbuf_clone(
    _md: *mut rte_mbuf,
    _mp: *mut rte_mempool,
) -> *mut rte_mbuf {
    ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn rte_mempool_avail_count(mp: *mut rte_mempool) -> c_uint {
    if mp.is_null() {
        return 0;
    }
    unsafe { (*mp).populated_size }
}

#[no_mangle]
pub extern "C" fn rte_mempool_in_use_count(_mp: *mut rte_mempool) -> c_uint {
    // Stub: always return 0 (nothing in use)
    0
}

#[no_mangle]
pub extern "C" fn rte_mempool_full(mp: *mut rte_mempool) -> c_int {
    // Stub: return 1 (true) if pool has elements
    if mp.is_null() {
        return 0;
    }
    1
}

#[no_mangle]
pub extern "C" fn rte_mempool_empty(mp: *mut rte_mempool) -> c_int {
    // Stub: return 0 (false) - pool is not empty
    if mp.is_null() {
        return 1;
    }
    0
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
pub extern "C" fn rte_eth_promiscuous_get(_port_id: u16) -> c_int {
    1 // Promiscuous is enabled by default in stubs
}

#[no_mangle]
pub extern "C" fn rte_eth_allmulticast_enable(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_allmulticast_disable(_port_id: u16) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn rte_eth_allmulticast_get(_port_id: u16) -> c_int {
    0 // All-multicast disabled by default
}

#[no_mangle]
pub extern "C" fn rte_eth_dev_set_mc_addr_list(
    _port_id: u16,
    _mc_addr_set: *mut rte_ether_addr,
    _nb_mc_addr: u32,
) -> c_int {
    0 // Success
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

/// Stub rte_errno — set by stub functions that need to report errors
static STUB_RTE_ERRNO: AtomicI32 = AtomicI32::new(0);

#[no_mangle]
pub extern "C" fn rte_errno() -> c_int {
    STUB_RTE_ERRNO.load(Ordering::SeqCst)
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
