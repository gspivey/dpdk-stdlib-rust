/*
 * DPDK wrapper header for bindgen
 *
 * This file includes the necessary DPDK headers for generating Rust FFI bindings.
 * Only include headers that are needed for the dpdk-sys crate.
 */

#ifndef DPDK_WRAPPER_H
#define DPDK_WRAPPER_H

/* Core EAL (Environment Abstraction Layer) */
#include <rte_eal.h>
#include <rte_errno.h>
#include <rte_lcore.h>
#include <rte_launch.h>
#include <rte_cycles.h>
#include <rte_debug.h>

/* Memory management */
#include <rte_memory.h>
#include <rte_memzone.h>
#include <rte_mempool.h>
#include <rte_malloc.h>

/* Packet buffers (mbufs) */
#include <rte_mbuf.h>
#include <rte_mbuf_core.h>

/* Ethernet device management */
#include <rte_ethdev.h>
#include <rte_eth_ctrl.h>

/* Networking */
#include <rte_ether.h>
#include <rte_ip.h>
#include <rte_udp.h>
#include <rte_tcp.h>
#include <rte_arp.h>

/* Ring buffers */
#include <rte_ring.h>

/* Flow classification */
#include <rte_flow.h>

#endif /* DPDK_WRAPPER_H */
