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

/* ---- Mbuf offload field access (rte_mbuf.h) ---- */
/* tx_offload lives inside an anonymous union in rte_mbuf, so bindgen
   generates it as __bindgen_anon_N.tx_offload.  These shims let Rust
   access it portably without depending on the bindgen-generated name. */

void dpdk_shim_set_mbuf_tx_offload(struct rte_mbuf *m, uint64_t val) {
    m->tx_offload = val;
}

uint64_t dpdk_shim_get_mbuf_tx_offload(const struct rte_mbuf *m) {
    return m->tx_offload;
}

/* ---- Mbuf length/offset field access (rte_mbuf.h) ---- */
/* pkt_len, data_len, data_off and buf_len live inside anonymous unions
   in rte_mbuf — same pattern as tx_offload above. */

uint32_t dpdk_shim_get_mbuf_pkt_len(const struct rte_mbuf *m) {
    return m->pkt_len;
}

void dpdk_shim_set_mbuf_pkt_len(struct rte_mbuf *m, uint32_t len) {
    m->pkt_len = len;
}

uint16_t dpdk_shim_get_mbuf_data_len(const struct rte_mbuf *m) {
    return m->data_len;
}

void dpdk_shim_set_mbuf_data_len(struct rte_mbuf *m, uint16_t len) {
    m->data_len = len;
}

uint16_t dpdk_shim_get_mbuf_data_off(const struct rte_mbuf *m) {
    return m->data_off;
}

void dpdk_shim_set_mbuf_data_off(struct rte_mbuf *m, uint16_t off) {
    m->data_off = off;
}

uint16_t dpdk_shim_get_mbuf_buf_len(const struct rte_mbuf *m) {
    return m->buf_len;
}

/* ---- Errno (rte_errno.h) ---- */

int dpdk_shim_rte_errno(void) {
    return rte_errno;
}
