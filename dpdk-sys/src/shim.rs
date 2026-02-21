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
