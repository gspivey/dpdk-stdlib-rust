/*
 * DPDK shim: non-inline wrappers for static inline DPDK functions.
 *
 * bindgen cannot generate Rust bindings for static inline functions defined
 * in DPDK headers.  This file calls through to those inline functions from
 * regular (non-inline, externally visible) wrappers so the linker can
 * resolve them from Rust.
 */

#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_mempool.h>
#include <rte_errno.h>

/* ---- Packet I/O (rte_ethdev.h) ---- */

uint16_t dpdk_shim_rte_eth_rx_burst(uint16_t port_id, uint16_t queue_id,
                                     struct rte_mbuf **rx_pkts,
                                     const uint16_t nb_pkts) {
    return rte_eth_rx_burst(port_id, queue_id, rx_pkts, nb_pkts);
}

uint16_t dpdk_shim_rte_eth_tx_burst(uint16_t port_id, uint16_t queue_id,
                                     struct rte_mbuf **tx_pkts,
                                     const uint16_t nb_pkts) {
    return rte_eth_tx_burst(port_id, queue_id, tx_pkts, nb_pkts);
}

/* ---- Mbuf allocation / free (rte_mbuf.h) ---- */

struct rte_mbuf *dpdk_shim_rte_pktmbuf_alloc(struct rte_mempool *mp) {
    return rte_pktmbuf_alloc(mp);
}

int dpdk_shim_rte_pktmbuf_alloc_bulk(struct rte_mempool *pool,
                                      struct rte_mbuf **mbufs,
                                      unsigned int count) {
    return rte_pktmbuf_alloc_bulk(pool, mbufs, count);
}

void dpdk_shim_rte_pktmbuf_free(struct rte_mbuf *m) {
    rte_pktmbuf_free(m);
}

/* ---- Mempool queries (rte_mempool.h) ---- */

int dpdk_shim_rte_mempool_full(const struct rte_mempool *mp) {
    return rte_mempool_full(mp);
}

int dpdk_shim_rte_mempool_empty(const struct rte_mempool *mp) {
    return rte_mempool_empty(mp);
}

/* ---- Errno (rte_errno.h) ---- */

int dpdk_shim_rte_errno(void) {
    return rte_errno;
}
