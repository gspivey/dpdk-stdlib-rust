//! Memory buffer management for DPDK packets
//!
//! The mbuf (memory buffer) is the fundamental data structure for packet handling
//! in DPDK. This module provides safe Rust wrappers around DPDK's mbuf operations.

use crate::error::{DpdkError, DpdkResult};
use std::ffi::CString;
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

    /// Get the buffer length (total available space)
    pub fn buf_len(&self) -> u16 {
        unsafe { (*self.raw.as_ptr()).buf_len }
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

    /// Get the ol_flags field (offload flags)
    pub fn ol_flags(&self) -> u64 {
        unsafe { (*self.raw.as_ptr()).ol_flags }
    }

    /// Set the ol_flags field (offload flags)
    pub fn set_ol_flags(&mut self, flags: u64) {
        unsafe {
            (*self.raw.as_ptr()).ol_flags = flags;
        }
    }

    /// Get the VLAN Tag Control Information (TCI) field.
    ///
    /// On RX, the NIC populates this when `RTE_MBUF_F_RX_VLAN_STRIPPED` is set
    /// in ol_flags. On TX, the application sets this and the NIC inserts the
    /// VLAN tag when `RTE_MBUF_F_TX_VLAN` is set.
    pub fn vlan_tci(&self) -> u16 {
        unsafe { (*self.raw.as_ptr()).vlan_tci }
    }

    /// Set the VLAN TCI field for hardware VLAN insertion on TX.
    ///
    /// Must be combined with `set_ol_flags(RTE_MBUF_F_TX_VLAN)` for the NIC
    /// to actually insert the VLAN tag.
    pub fn set_vlan_tci(&mut self, tci: u16) {
        unsafe {
            (*self.raw.as_ptr()).vlan_tci = tci;
        }
    }

    /// Set TX offload lengths (l2_len, l3_len, l4_len) in the tx_offload bitfield.
    ///
    /// DPDK encodes these as: l2_len (bits 0-6, 7 bits), l3_len (bits 7-15, 9 bits),
    /// l4_len (bits 16-23, 8 bits).
    ///
    /// Uses a C shim wrapper because `tx_offload` lives inside an anonymous union
    /// in the real DPDK `rte_mbuf` struct, which bindgen represents as
    /// `__bindgen_anon_N.tx_offload` rather than a direct field.
    pub fn set_tx_offload(&mut self, l2_len: u8, l3_len: u16, l4_len: u8) {
        let tx_offload = (l2_len as u64)
            | ((l3_len as u64) << 7)
            | ((l4_len as u64) << 16);
        unsafe {
            dpdk_sys::mbuf_set_tx_offload(self.raw.as_ptr(), tx_offload);
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

/// Default number of mbufs in a pool
pub const DEFAULT_POOL_SIZE: u32 = 8192;

/// Default per-core cache size
pub const DEFAULT_CACHE_SIZE: u32 = 256;

/// Default data room size (2KB + headroom)
pub const DEFAULT_DATA_ROOM_SIZE: u16 = dpdk_sys::RTE_MBUF_DEFAULT_BUF_SIZE as u16;

/// Maximum length for mempool names
pub const MAX_MEMPOOL_NAME_LEN: usize = 32;

/// Configuration for creating a mempool
#[derive(Debug, Clone)]
pub struct MempoolConfig {
    /// Number of elements in the pool
    pub n: u32,
    /// Per-core cache size (0 to disable caching)
    pub cache_size: u32,
    /// Size of data buffer in each mbuf
    pub data_room_size: u16,
    /// NUMA socket ID (-1 for any socket)
    pub socket_id: i32,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            n: DEFAULT_POOL_SIZE,
            cache_size: DEFAULT_CACHE_SIZE,
            data_room_size: DEFAULT_DATA_ROOM_SIZE,
            socket_id: dpdk_sys::SOCKET_ID_ANY,
        }
    }
}

impl MempoolConfig {
    /// Create a new mempool configuration with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the pool size
    pub fn with_size(mut self, n: u32) -> Self {
        self.n = n;
        self
    }

    /// Set the cache size
    pub fn with_cache_size(mut self, cache_size: u32) -> Self {
        self.cache_size = cache_size;
        self
    }

    /// Set the data room size
    pub fn with_data_room_size(mut self, size: u16) -> Self {
        self.data_room_size = size;
        self
    }

    /// Set the NUMA socket ID
    pub fn with_socket_id(mut self, socket_id: i32) -> Self {
        self.socket_id = socket_id;
        self
    }
}

impl Mempool {
    /// Create a new mempool for packet buffers
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this mempool (max 32 characters)
    /// * `n` - Number of elements in the pool
    /// * `cache_size` - Per-core cache size (0 to disable)
    /// * `data_room_size` - Size of data buffer in each mbuf
    /// * `socket_id` - NUMA socket ID (-1 for any)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The name is empty or too long
    /// - The name contains null bytes
    /// - DPDK mempool creation fails (e.g., out of memory, invalid parameters)
    pub fn create(
        name: &str,
        n: u32,
        cache_size: u32,
        data_room_size: u16,
        socket_id: i32,
    ) -> DpdkResult<Self> {
        // Validate name
        if name.is_empty() {
            return Err(DpdkError::InvalidName("mempool name cannot be empty".to_string()));
        }
        if name.len() > MAX_MEMPOOL_NAME_LEN {
            return Err(DpdkError::InvalidName(format!(
                "mempool name too long (max {} characters)",
                MAX_MEMPOOL_NAME_LEN
            )));
        }

        // Convert name to C string
        let c_name = CString::new(name).map_err(|_| {
            DpdkError::InvalidName("mempool name contains null bytes".to_string())
        })?;

        // Create the mempool using DPDK
        let ptr = unsafe {
            dpdk_sys::rte_pktmbuf_pool_create(
                c_name.as_ptr(),
                n,
                cache_size,
                0, // priv_size - typically 0 for standard packet pools
                data_room_size,
                socket_id,
            )
        };

        NonNull::new(ptr)
            .map(|raw| Self {
                raw,
                name: name.to_string(),
            })
            .ok_or_else(|| {
                // Get the DPDK errno for more detailed error message
                let errno = unsafe { dpdk_sys::rte_errno() };
                DpdkError::MempoolCreateFailed(format!(
                    "rte_pktmbuf_pool_create failed for '{}' (errno: {})",
                    name, errno
                ))
            })
    }

    /// Create a new mempool with configuration
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this mempool
    /// * `config` - Mempool configuration
    pub fn create_with_config(name: &str, config: &MempoolConfig) -> DpdkResult<Self> {
        Self::create(
            name,
            config.n,
            config.cache_size,
            config.data_room_size,
            config.socket_id,
        )
    }

    /// Create a mempool with default configuration
    pub fn create_default(name: &str) -> DpdkResult<Self> {
        Self::create_with_config(name, &MempoolConfig::default())
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

    /// Allocate multiple mbufs from this pool in a batch
    ///
    /// This is more efficient than calling `alloc()` multiple times.
    /// Returns Ok with a vector of mbufs, or Err if allocation fails.
    pub fn alloc_bulk(&self, count: usize) -> DpdkResult<Vec<Mbuf>> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut ptrs: Vec<*mut dpdk_sys::rte_mbuf> =
            vec![std::ptr::null_mut(); count];

        let ret = unsafe {
            dpdk_sys::rte_pktmbuf_alloc_bulk(
                self.raw.as_ptr(),
                ptrs.as_mut_ptr(),
                count as u32,
            )
        };

        if ret != 0 {
            return Err(DpdkError::MemoryAllocationFailed);
        }

        let mbufs: Vec<Mbuf> = ptrs
            .into_iter()
            .filter_map(|ptr| unsafe { Mbuf::from_raw(ptr) })
            .collect();

        if mbufs.len() != count {
            // Some allocations failed, free the ones we got and return error
            // (mbufs will be dropped automatically)
            return Err(DpdkError::MemoryAllocationFailed);
        }

        Ok(mbufs)
    }

    /// Get the number of free elements in the pool
    pub fn available_count(&self) -> u32 {
        unsafe { dpdk_sys::rte_mempool_avail_count(self.raw.as_ptr()) }
    }

    /// Get the number of elements in use
    pub fn in_use_count(&self) -> u32 {
        unsafe { dpdk_sys::rte_mempool_in_use_count(self.raw.as_ptr()) }
    }

    /// Check if the pool is full (all elements available)
    pub fn is_full(&self) -> bool {
        unsafe { dpdk_sys::rte_mempool_full(self.raw.as_ptr()) != 0 }
    }

    /// Check if the pool is empty (no elements available)
    pub fn is_empty(&self) -> bool {
        unsafe { dpdk_sys::rte_mempool_empty(self.raw.as_ptr()) != 0 }
    }

    /// Get the raw pointer to the mempool
    pub fn as_raw(&self) -> *mut dpdk_sys::rte_mempool {
        self.raw.as_ptr()
    }
}

impl Drop for Mempool {
    fn drop(&mut self) {
        unsafe {
            dpdk_sys::rte_mempool_free(self.raw.as_ptr());
        }
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

    // ========================================================================
    // MbufBuilder Tests
    // ========================================================================

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

    #[test]
    fn test_mbuf_builder_empty() {
        let frame = MbufBuilder::new().build();
        assert!(frame.is_empty());
    }

    #[test]
    fn test_mbuf_builder_payload_only() {
        let payload = b"hello world";
        let frame = MbufBuilder::new().payload(payload).build();
        assert_eq!(frame.len(), payload.len());
        assert_eq!(&frame, payload);
    }

    #[test]
    fn test_mbuf_builder_ethernet_only() {
        let frame = MbufBuilder::new()
            .ethernet([0x01, 0x02, 0x03, 0x04, 0x05, 0x06], [0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f], 0x0800)
            .build();

        // Ethernet header is 14 bytes: 6 dst + 6 src + 2 ethertype
        assert_eq!(frame.len(), 14);

        // Verify dst MAC
        assert_eq!(&frame[0..6], &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        // Verify src MAC
        assert_eq!(&frame[6..12], &[0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]);
        // Verify ethertype (0x0800 = IPv4)
        assert_eq!(&frame[12..14], &[0x08, 0x00]);
    }

    // ========================================================================
    // MempoolConfig Tests
    // ========================================================================

    #[test]
    fn test_mempool_config_default() {
        let config = MempoolConfig::default();
        assert_eq!(config.n, DEFAULT_POOL_SIZE);
        assert_eq!(config.cache_size, DEFAULT_CACHE_SIZE);
        assert_eq!(config.data_room_size, DEFAULT_DATA_ROOM_SIZE);
        assert_eq!(config.socket_id, dpdk_sys::SOCKET_ID_ANY);
    }

    #[test]
    fn test_mempool_config_builder() {
        let config = MempoolConfig::new()
            .with_size(4096)
            .with_cache_size(128)
            .with_data_room_size(2048)
            .with_socket_id(0);

        assert_eq!(config.n, 4096);
        assert_eq!(config.cache_size, 128);
        assert_eq!(config.data_room_size, 2048);
        assert_eq!(config.socket_id, 0);
    }

    #[test]
    fn test_mempool_config_chaining() {
        // Test that builder methods can be chained
        let config = MempoolConfig::new()
            .with_size(1024)
            .with_cache_size(64);

        assert_eq!(config.n, 1024);
        assert_eq!(config.cache_size, 64);
        // Other fields should retain defaults
        assert_eq!(config.data_room_size, DEFAULT_DATA_ROOM_SIZE);
    }

    // ========================================================================
    // Mempool Creation Tests
    // ========================================================================

    #[test]
    fn test_mempool_create() {
        let pool = Mempool::create("test_pool", 128, 32, 2048, -1);
        assert!(pool.is_ok());
        let pool = pool.unwrap();
        assert_eq!(pool.name(), "test_pool");
    }

    #[test]
    fn test_mempool_create_with_config() {
        let config = MempoolConfig::new().with_size(256).with_cache_size(64);
        let pool = Mempool::create_with_config("config_pool", &config);
        assert!(pool.is_ok());
        let pool = pool.unwrap();
        assert_eq!(pool.name(), "config_pool");
    }

    #[test]
    fn test_mempool_create_default() {
        let pool = Mempool::create_default("default_pool");
        assert!(pool.is_ok());
        let pool = pool.unwrap();
        assert_eq!(pool.name(), "default_pool");
    }

    #[test]
    fn test_mempool_create_empty_name() {
        let result = Mempool::create("", 128, 32, 2048, -1);
        assert!(result.is_err());
        match result {
            Err(DpdkError::InvalidName(msg)) => {
                assert!(msg.contains("empty"));
            }
            _ => panic!("Expected InvalidName error"),
        }
    }

    #[test]
    fn test_mempool_create_name_too_long() {
        let long_name = "a".repeat(MAX_MEMPOOL_NAME_LEN + 1);
        let result = Mempool::create(&long_name, 128, 32, 2048, -1);
        assert!(result.is_err());
        match result {
            Err(DpdkError::InvalidName(msg)) => {
                assert!(msg.contains("too long"));
            }
            _ => panic!("Expected InvalidName error"),
        }
    }

    #[test]
    fn test_mempool_create_name_with_null() {
        let result = Mempool::create("test\0pool", 128, 32, 2048, -1);
        assert!(result.is_err());
        match result {
            Err(DpdkError::InvalidName(msg)) => {
                assert!(msg.contains("null"));
            }
            _ => panic!("Expected InvalidName error"),
        }
    }

    #[test]
    fn test_mempool_name_max_length() {
        let max_name = "a".repeat(MAX_MEMPOOL_NAME_LEN);
        let result = Mempool::create(&max_name, 128, 32, 2048, -1);
        assert!(result.is_ok());
    }

    // ========================================================================
    // Mempool Operations Tests
    // ========================================================================

    #[test]
    fn test_mempool_alloc() {
        let pool = Mempool::create("alloc_pool", 128, 32, 2048, -1).unwrap();
        let mbuf = pool.alloc();
        assert!(mbuf.is_ok());
    }

    #[test]
    fn test_mempool_alloc_bulk_zero() {
        let pool = Mempool::create("bulk_zero_pool", 128, 32, 2048, -1).unwrap();
        let mbufs = pool.alloc_bulk(0);
        assert!(mbufs.is_ok());
        assert!(mbufs.unwrap().is_empty());
    }

    #[test]
    fn test_mempool_alloc_bulk() {
        let pool = Mempool::create("bulk_pool", 128, 32, 2048, -1).unwrap();
        let mbufs = pool.alloc_bulk(4);
        assert!(mbufs.is_ok());
        let mbufs = mbufs.unwrap();
        assert_eq!(mbufs.len(), 4);
    }

    #[test]
    fn test_mempool_available_count() {
        let pool = Mempool::create("avail_pool", 128, 32, 2048, -1).unwrap();
        let count = pool.available_count();
        // With stubs, initial count should be the pool size
        assert!(count > 0);
    }

    #[test]
    fn test_mempool_in_use_count() {
        let pool = Mempool::create("inuse_pool", 128, 32, 2048, -1).unwrap();
        let count = pool.in_use_count();
        // Initially nothing should be in use
        assert_eq!(count, 0);
    }

    #[test]
    fn test_mempool_is_full() {
        let pool = Mempool::create("full_pool", 128, 32, 2048, -1).unwrap();
        // Initially pool should be full (all elements available)
        assert!(pool.is_full());
    }

    #[test]
    fn test_mempool_is_empty() {
        let pool = Mempool::create("empty_pool", 128, 32, 2048, -1).unwrap();
        // Initially pool should not be empty
        assert!(!pool.is_empty());
    }

    #[test]
    fn test_mempool_as_raw() {
        let pool = Mempool::create("raw_pool", 128, 32, 2048, -1).unwrap();
        let raw = pool.as_raw();
        assert!(!raw.is_null());
    }

    // ========================================================================
    // Mbuf Tests (with stubs)
    // ========================================================================

    #[test]
    fn test_mbuf_data_offset() {
        let pool = Mempool::create("offset_pool", 128, 32, 2048, -1).unwrap();
        let mbuf = pool.alloc().unwrap();
        // Data offset should be set (typically RTE_PKTMBUF_HEADROOM)
        let offset = mbuf.data_offset();
        assert!(offset >= 0);
    }

    #[test]
    fn test_mbuf_packet_len() {
        let pool = Mempool::create("pktlen_pool", 128, 32, 2048, -1).unwrap();
        let mbuf = pool.alloc().unwrap();
        // Initially packet length should be 0
        assert_eq!(mbuf.packet_len(), 0);
    }

    #[test]
    fn test_mbuf_data_len() {
        let pool = Mempool::create("datalen_pool", 128, 32, 2048, -1).unwrap();
        let mbuf = pool.alloc().unwrap();
        // Initially data length should be 0
        assert_eq!(mbuf.data_len(), 0);
    }

    #[test]
    fn test_mbuf_set_lengths() {
        let pool = Mempool::create("setlen_pool", 128, 32, 2048, -1).unwrap();
        let mut mbuf = pool.alloc().unwrap();

        mbuf.set_data_len(100);
        assert_eq!(mbuf.data_len(), 100);

        mbuf.set_packet_len(100);
        assert_eq!(mbuf.packet_len(), 100);
    }

    #[test]
    fn test_mbuf_into_raw() {
        let pool = Mempool::create("intoraw_pool", 128, 32, 2048, -1).unwrap();
        let mbuf = pool.alloc().unwrap();
        let raw = mbuf.into_raw();
        assert!(!raw.is_null());

        // Manually free to avoid leak (in real code DPDK would handle this)
        unsafe {
            dpdk_sys::rte_pktmbuf_free(raw);
        }
    }

    #[test]
    fn test_mbuf_from_raw_null() {
        let result = unsafe { Mbuf::from_raw(std::ptr::null_mut()) };
        assert!(result.is_none());
    }
}
