//! Memory buffer management for DPDK packets
//!
//! The mbuf (memory buffer) is the fundamental data structure for packet handling
//! in DPDK. This module provides safe Rust wrappers around DPDK's mbuf operations.

use crate::error::{DpdkError, DpdkResult};
use std::ptr::NonNull;

/// A memory buffer for packet data
///
/// Mbufs are the basic unit for carrying packet data in DPDK.
/// They are allocated from memory pools and contain both metadata
/// and actual packet data.
pub struct Mbuf {
    raw: NonNull<dpdk_sys::rte_mbuf>,
}

// Safety: Mbufs can be sent between threads as long as only one thread
// accesses them at a time (which is enforced by ownership)
unsafe impl Send for Mbuf {}

impl Mbuf {
    /// Create a new Mbuf from a raw pointer
    ///
    /// # Safety
    ///
    /// The caller must ensure:
    /// - The pointer is valid and properly aligned
    /// - The mbuf was allocated from a valid mempool
    /// - Ownership is being transferred (the raw pointer should not be freed elsewhere)
    pub unsafe fn from_raw(ptr: *mut dpdk_sys::rte_mbuf) -> Option<Self> {
        NonNull::new(ptr).map(|raw| Self { raw })
    }

    /// Get the raw pointer to the underlying mbuf
    pub fn as_raw(&self) -> *mut dpdk_sys::rte_mbuf {
        self.raw.as_ptr()
    }

    /// Consume self and return the raw pointer
    ///
    /// After calling this, the caller is responsible for freeing the mbuf
    pub fn into_raw(self) -> *mut dpdk_sys::rte_mbuf {
        let ptr = self.raw.as_ptr();
        std::mem::forget(self); // Don't run Drop
        ptr
    }

    /// Get the data offset within the buffer
    pub fn data_offset(&self) -> u16 {
        unsafe { (*self.raw.as_ptr()).data_off }
    }

    /// Get the total packet length
    pub fn packet_len(&self) -> u32 {
        unsafe { (*self.raw.as_ptr()).pkt_len }
    }

    /// Get the data length in this segment
    pub fn data_len(&self) -> u16 {
        unsafe { (*self.raw.as_ptr()).data_len }
    }

    /// Get a slice to the packet data
    ///
    /// Returns None if the buffer address is null
    pub fn data(&self) -> Option<&[u8]> {
        unsafe {
            let mbuf = self.raw.as_ptr();
            let buf_addr = (*mbuf).buf_addr;
            if buf_addr.is_null() {
                return None;
            }
            let data_ptr = (buf_addr as *const u8).add((*mbuf).data_off as usize);
            Some(std::slice::from_raw_parts(data_ptr, (*mbuf).data_len as usize))
        }
    }

    /// Get a mutable slice to the packet data
    ///
    /// Returns None if the buffer address is null
    pub fn data_mut(&mut self) -> Option<&mut [u8]> {
        unsafe {
            let mbuf = self.raw.as_ptr();
            let buf_addr = (*mbuf).buf_addr;
            if buf_addr.is_null() {
                return None;
            }
            let data_ptr = (buf_addr as *mut u8).add((*mbuf).data_off as usize);
            Some(std::slice::from_raw_parts_mut(data_ptr, (*mbuf).data_len as usize))
        }
    }

    /// Set the data length
    pub fn set_data_len(&mut self, len: u16) {
        unsafe {
            (*self.raw.as_ptr()).data_len = len;
        }
    }

    /// Set the packet length
    pub fn set_packet_len(&mut self, len: u32) {
        unsafe {
            (*self.raw.as_ptr()).pkt_len = len;
        }
    }
}

impl Drop for Mbuf {
    fn drop(&mut self) {
        unsafe {
            dpdk_sys::rte_pktmbuf_free(self.raw.as_ptr());
        }
    }
}

/// Memory pool for packet buffers
///
/// A mempool is a fixed-size pool of mbufs. All packet buffers in DPDK
/// must be allocated from a mempool.
pub struct Mempool {
    raw: NonNull<dpdk_sys::rte_mempool>,
    name: String,
}

// Safety: Mempools are thread-safe in DPDK
unsafe impl Send for Mempool {}
unsafe impl Sync for Mempool {}

impl Mempool {
    /// Create a new mempool (placeholder for actual DPDK mempool creation)
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this mempool
    /// * `n` - Number of elements in the pool
    /// * `cache_size` - Per-core cache size (0 to disable)
    /// * `data_room_size` - Size of data buffer in each mbuf
    /// * `socket_id` - NUMA socket ID (-1 for any)
    pub fn create(
        name: &str,
        _n: u32,
        _cache_size: u32,
        _data_room_size: u16,
        _socket_id: i32,
    ) -> DpdkResult<Self> {
        // In a real implementation, this would call rte_pktmbuf_pool_create
        // For now, return a placeholder
        Ok(Self {
            raw: NonNull::dangling(),
            name: name.to_string(),
        })
    }

    /// Get the name of this mempool
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Allocate an mbuf from this pool
    pub fn alloc(&self) -> DpdkResult<Mbuf> {
        unsafe {
            let ptr = dpdk_sys::rte_pktmbuf_alloc(self.raw.as_ptr());
            Mbuf::from_raw(ptr).ok_or(DpdkError::MemoryAllocationFailed)
        }
    }

    /// Get the raw pointer to the mempool
    pub fn as_raw(&self) -> *mut dpdk_sys::rte_mempool {
        self.raw.as_ptr()
    }
}

/// Builder for creating mbufs with specific content
pub struct MbufBuilder {
    data: Vec<u8>,
}

impl MbufBuilder {
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    /// Add ethernet header
    pub fn ethernet(mut self, dst_mac: [u8; 6], src_mac: [u8; 6], ethertype: u16) -> Self {
        self.data.extend_from_slice(&dst_mac);
        self.data.extend_from_slice(&src_mac);
        self.data.extend_from_slice(&ethertype.to_be_bytes());
        self
    }

    /// Add IPv4 header (simplified)
    pub fn ipv4(mut self, src: [u8; 4], dst: [u8; 4], protocol: u8, payload_len: u16) -> Self {
        let total_len = 20 + payload_len;
        self.data.push(0x45); // Version + IHL
        self.data.push(0x00); // DSCP + ECN
        self.data.extend_from_slice(&total_len.to_be_bytes());
        self.data.extend_from_slice(&[0, 0]); // Identification
        self.data.extend_from_slice(&[0, 0]); // Flags + Fragment offset
        self.data.push(64); // TTL
        self.data.push(protocol);
        self.data.extend_from_slice(&[0, 0]); // Checksum (to be calculated)
        self.data.extend_from_slice(&src);
        self.data.extend_from_slice(&dst);
        self
    }

    /// Add UDP header
    pub fn udp(mut self, src_port: u16, dst_port: u16, payload_len: u16) -> Self {
        let udp_len = 8 + payload_len;
        self.data.extend_from_slice(&src_port.to_be_bytes());
        self.data.extend_from_slice(&dst_port.to_be_bytes());
        self.data.extend_from_slice(&udp_len.to_be_bytes());
        self.data.extend_from_slice(&[0, 0]); // Checksum
        self
    }

    /// Add payload data
    pub fn payload(mut self, data: &[u8]) -> Self {
        self.data.extend_from_slice(data);
        self
    }

    /// Build into a byte vector (for use with synthetic testing)
    pub fn build(self) -> Vec<u8> {
        self.data
    }
}

impl Default for MbufBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mbuf_builder() {
        let frame = MbufBuilder::new()
            .ethernet([0xff; 6], [0x00; 6], 0x0800)
            .ipv4([192, 168, 1, 1], [192, 168, 1, 2], 17, 16)
            .udp(12345, 9000, 8)
            .payload(b"test")
            .build();

        // Ethernet: 14 bytes, IP: 20 bytes, UDP: 8 bytes, payload: 4 bytes
        assert_eq!(frame.len(), 14 + 20 + 8 + 4);
    }
}
