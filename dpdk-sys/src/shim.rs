//! Wrappers for DPDK static inline functions and macro constants
//! that bindgen cannot generate bindings for.
//!
//! When building with real DPDK (`dpdk_bindgen` cfg), this module provides:
//! - Rust-visible `pub` functions that forward to C shim wrappers
//!   (compiled from `csrc/dpdk_shim.c`)
//! - Constants defined as C preprocessor macros that bindgen skips

use super::{rte_mbuf, rte_mempool};

// ---------------------------------------------------------------------------
// Extern declarations for C shim wrappers (csrc/dpdk_shim.c)
// ---------------------------------------------------------------------------
extern "C" {
    fn dpdk_shim_rte_eth_rx_burst(
        port_id: u16,
        queue_id: u16,
        rx_pkts: *mut *mut rte_mbuf,
        nb_pkts: u16,
    ) -> u16;

    fn dpdk_shim_rte_eth_tx_burst(
        port_id: u16,
        queue_id: u16,
        tx_pkts: *mut *mut rte_mbuf,
        nb_pkts: u16,
    ) -> u16;

    fn dpdk_shim_rte_pktmbuf_alloc(mp: *mut rte_mempool) -> *mut rte_mbuf;

    fn dpdk_shim_rte_pktmbuf_alloc_bulk(
        pool: *mut rte_mempool,
        mbufs: *mut *mut rte_mbuf,
        count: libc::c_uint,
    ) -> libc::c_int;

    fn dpdk_shim_rte_pktmbuf_free(m: *mut rte_mbuf);

    fn dpdk_shim_rte_mempool_full(mp: *const rte_mempool) -> libc::c_int;
    fn dpdk_shim_rte_mempool_empty(mp: *const rte_mempool) -> libc::c_int;

    fn dpdk_shim_rte_errno() -> libc::c_int;

    fn dpdk_shim_set_mbuf_tx_offload(m: *mut rte_mbuf, val: u64);
    fn dpdk_shim_get_mbuf_tx_offload(m: *const rte_mbuf) -> u64;

    // Mbuf length/offset field accessors — same pattern as tx_offload
    fn dpdk_shim_get_mbuf_pkt_len(m: *const rte_mbuf) -> u32;
    fn dpdk_shim_set_mbuf_pkt_len(m: *mut rte_mbuf, len: u32);
    fn dpdk_shim_get_mbuf_data_len(m: *const rte_mbuf) -> u16;
    fn dpdk_shim_set_mbuf_data_len(m: *mut rte_mbuf, len: u16);
    fn dpdk_shim_get_mbuf_data_off(m: *const rte_mbuf) -> u16;
    fn dpdk_shim_set_mbuf_data_off(m: *mut rte_mbuf, off: u16);
    fn dpdk_shim_get_mbuf_buf_len(m: *const rte_mbuf) -> u16;
}

// ---------------------------------------------------------------------------
// Public Rust wrappers — same signatures as the stubs so callers are
// identical regardless of which cfg path is active.
// ---------------------------------------------------------------------------

#[inline]
pub unsafe fn rte_eth_rx_burst(
    port_id: u16,
    queue_id: u16,
    rx_pkts: *mut *mut rte_mbuf,
    nb_pkts: u16,
) -> u16 {
    dpdk_shim_rte_eth_rx_burst(port_id, queue_id, rx_pkts, nb_pkts)
}

#[inline]
pub unsafe fn rte_eth_tx_burst(
    port_id: u16,
    queue_id: u16,
    tx_pkts: *mut *mut rte_mbuf,
    nb_pkts: u16,
) -> u16 {
    dpdk_shim_rte_eth_tx_burst(port_id, queue_id, tx_pkts, nb_pkts)
}

#[inline]
pub unsafe fn rte_pktmbuf_alloc(mp: *mut rte_mempool) -> *mut rte_mbuf {
    dpdk_shim_rte_pktmbuf_alloc(mp)
}

#[inline]
pub unsafe fn rte_pktmbuf_alloc_bulk(
    pool: *mut rte_mempool,
    mbufs: *mut *mut rte_mbuf,
    count: libc::c_uint,
) -> libc::c_int {
    dpdk_shim_rte_pktmbuf_alloc_bulk(pool, mbufs, count)
}

#[inline]
pub unsafe fn rte_pktmbuf_free(m: *mut rte_mbuf) {
    dpdk_shim_rte_pktmbuf_free(m)
}

#[inline]
pub unsafe fn rte_mempool_full(mp: *const rte_mempool) -> libc::c_int {
    dpdk_shim_rte_mempool_full(mp)
}

#[inline]
pub unsafe fn rte_mempool_empty(mp: *const rte_mempool) -> libc::c_int {
    dpdk_shim_rte_mempool_empty(mp)
}

#[inline]
pub unsafe fn rte_errno() -> libc::c_int {
    dpdk_shim_rte_errno()
}

// Mbuf tx_offload field access — the field lives inside an anonymous union
// in rte_mbuf, so we go through C shim wrappers to avoid depending on the
// bindgen-generated union member name.

#[inline]
pub unsafe fn mbuf_set_tx_offload(m: *mut rte_mbuf, val: u64) {
    dpdk_shim_set_mbuf_tx_offload(m, val)
}

#[inline]
pub unsafe fn mbuf_get_tx_offload(m: *const rte_mbuf) -> u64 {
    dpdk_shim_get_mbuf_tx_offload(m)
}

// Mbuf length/offset field access — same anonymous-union pattern as tx_offload.

#[inline]
pub unsafe fn mbuf_get_pkt_len(m: *const rte_mbuf) -> u32 {
    dpdk_shim_get_mbuf_pkt_len(m)
}

#[inline]
pub unsafe fn mbuf_set_pkt_len(m: *mut rte_mbuf, len: u32) {
    dpdk_shim_set_mbuf_pkt_len(m, len)
}

#[inline]
pub unsafe fn mbuf_get_data_len(m: *const rte_mbuf) -> u16 {
    dpdk_shim_get_mbuf_data_len(m)
}

#[inline]
pub unsafe fn mbuf_set_data_len(m: *mut rte_mbuf, len: u16) {
    dpdk_shim_set_mbuf_data_len(m, len)
}

#[inline]
pub unsafe fn mbuf_get_data_off(m: *const rte_mbuf) -> u16 {
    dpdk_shim_get_mbuf_data_off(m)
}

#[inline]
pub unsafe fn mbuf_set_data_off(m: *mut rte_mbuf, off: u16) {
    dpdk_shim_set_mbuf_data_off(m, off)
}

#[inline]
pub unsafe fn mbuf_get_buf_len(m: *const rte_mbuf) -> u16 {
    dpdk_shim_get_mbuf_buf_len(m)
}

// ---------------------------------------------------------------------------
// Constants — C preprocessor macros that bindgen cannot capture.
// Values match DPDK 22.11 (RTE_BIT64 definitions in rte_ethdev.h).
// ---------------------------------------------------------------------------

pub const RTE_ETH_RX_OFFLOAD_VLAN_STRIP: u64 = 1 << 0;
pub const RTE_ETH_RX_OFFLOAD_IPV4_CKSUM: u64 = 1 << 1;
pub const RTE_ETH_RX_OFFLOAD_UDP_CKSUM: u64 = 1 << 2;
pub const RTE_ETH_RX_OFFLOAD_TCP_CKSUM: u64 = 1 << 3;

pub const RTE_ETH_TX_OFFLOAD_VLAN_INSERT: u64 = 1 << 0;
pub const RTE_ETH_TX_OFFLOAD_IPV4_CKSUM: u64 = 1 << 1;
pub const RTE_ETH_TX_OFFLOAD_UDP_CKSUM: u64 = 1 << 2;
pub const RTE_ETH_TX_OFFLOAD_TCP_CKSUM: u64 = 1 << 3;

pub const SOCKET_ID_ANY: libc::c_int = -1;

// Mbuf TX offload flags (set by application, consumed by NIC).
// These are #define macros in rte_mbuf_core.h that bindgen cannot capture.
pub const RTE_MBUF_F_TX_IPV4: u64 = 1 << 55;
pub const RTE_MBUF_F_TX_IP_CKSUM: u64 = 1 << 54;
pub const RTE_MBUF_F_TX_UDP_CKSUM: u64 = 3 << 52;

// Mbuf TX VLAN offload flag: tells the NIC to insert a VLAN tag from mbuf.vlan_tci
pub const RTE_MBUF_F_TX_VLAN: u64 = 1 << 57;

// Mbuf RX VLAN offload flags (set by NIC when it strips the VLAN tag)
pub const RTE_MBUF_F_RX_VLAN: u64 = 1 << 0;
pub const RTE_MBUF_F_RX_VLAN_STRIPPED: u64 = 1 << 6;

// Mbuf RX offload flags (set by NIC, consumed by application).
// These are #define macros in rte_mbuf_core.h that bindgen cannot capture.
pub const RTE_MBUF_F_RX_IP_CKSUM_MASK: u64 = (1 << 4) | (1 << 7);
pub const RTE_MBUF_F_RX_IP_CKSUM_GOOD: u64 = 1 << 7;
pub const RTE_MBUF_F_RX_IP_CKSUM_BAD: u64 = 1 << 4;
pub const RTE_MBUF_F_RX_IP_CKSUM_UNKNOWN: u64 = 0;

pub const RTE_MBUF_F_RX_L4_CKSUM_MASK: u64 = (1 << 3) | (1 << 8);
pub const RTE_MBUF_F_RX_L4_CKSUM_GOOD: u64 = 1 << 8;
pub const RTE_MBUF_F_RX_L4_CKSUM_BAD: u64 = 1 << 3;
pub const RTE_MBUF_F_RX_L4_CKSUM_UNKNOWN: u64 = 0;
