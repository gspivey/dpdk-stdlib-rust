#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use libc::{c_char, c_int, c_void};

#[repr(C)]
pub struct rte_mbuf {
    pub buf_addr: *mut c_void,
    pub data_off: u16,
    pub pkt_len: u32,
    pub data_len: u16,
}

// Stub implementations that will link successfully
#[no_mangle]
pub extern "C" fn rte_eal_init(_argc: c_int, _argv: *mut *mut c_char) -> c_int { 0 }
#[no_mangle] 
pub extern "C" fn rte_eal_cleanup() -> c_int { 0 }
#[no_mangle]
pub extern "C" fn rte_eth_dev_count_avail() -> u16 { 1 }
#[no_mangle]
pub extern "C" fn rte_eth_dev_configure(_port_id: u16, _nb_rx_q: u16, _nb_tx_q: u16, _eth_conf: *const c_void) -> c_int { 0 }
#[no_mangle]
pub extern "C" fn rte_eth_dev_start(_port_id: u16) -> c_int { 0 }
#[no_mangle]
pub extern "C" fn rte_pktmbuf_alloc(_mp: *mut c_void) -> *mut rte_mbuf { std::ptr::null_mut() }
#[no_mangle]
pub extern "C" fn rte_pktmbuf_free(_m: *mut rte_mbuf) {}
