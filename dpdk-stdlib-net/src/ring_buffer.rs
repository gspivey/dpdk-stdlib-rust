//! PACKET_MMAP ring buffer for zero-copy packet I/O
//!
//! This module implements Linux PACKET_MMAP ring buffers for use with AF_PACKET
//! sockets. PACKET_MMAP provides zero-copy packet transmission and reception by
//! sharing a memory-mapped ring buffer between kernel and userspace.
//!
//! ## Architecture
//!
//! The ring buffer consists of a contiguous mmap'd region divided into fixed-size
//! frames. Each frame contains a `tpacket2_hdr` header followed by packet data.
//!
//! ### RX Ring
//! 1. Kernel writes received packets into frames with status `TP_STATUS_KERNEL`
//! 2. When a packet arrives, kernel sets status to `TP_STATUS_USER`
//! 3. Userspace reads the packet data
//! 4. Userspace sets status back to `TP_STATUS_KERNEL` to return the frame
//!
//! ### TX Ring
//! 1. Userspace writes packet data into a frame with status `TP_STATUS_AVAILABLE`
//! 2. Userspace sets status to `TP_STATUS_SEND_REQUEST`
//! 3. Calling `sendto()` triggers kernel to transmit pending frames
//! 4. Kernel sets status back to `TP_STATUS_AVAILABLE` after transmission
//!
//! ## Zero-Copy Benefits
//!
//! - No `copy_to_user` / `copy_from_user` for packet data
//! - Batch processing via ring buffer polling
//! - Reduced system call overhead (one `sendto()` for multiple packets)

use std::io;
use std::ptr;

/// TPACKET version 2 (used for ring buffer setup)
pub const TPACKET_V2: i32 = 1;

// Ring buffer frame status flags
/// Frame is available for kernel to fill (RX) or available for userspace to fill (TX)
pub const TP_STATUS_KERNEL: u32 = 0;
/// Frame contains a received packet ready for userspace to read
pub const TP_STATUS_USER: u32 = 1;
/// Frame is ready to be sent by the kernel
pub const TP_STATUS_SEND_REQUEST: u32 = 1;
/// Frame has been sent by the kernel
pub const TP_STATUS_AVAILABLE: u32 = 0;

// Socket option constants
/// SOL_PACKET level for setsockopt
pub const SOL_PACKET: i32 = 263;
/// Set PACKET_RX_RING
pub const PACKET_RX_RING: i32 = 5;
/// Set PACKET_TX_RING
pub const PACKET_TX_RING: i32 = 13;
/// Set PACKET_VERSION
pub const PACKET_VERSION: i32 = 10;

/// Ring buffer request structure (maps to `tpacket_req` in the kernel)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TpacketReq {
    /// Size of each block (must be page-aligned, power of 2)
    pub tp_block_size: u32,
    /// Number of blocks in the ring
    pub tp_block_nr: u32,
    /// Size of each frame (must divide block_size evenly)
    pub tp_frame_size: u32,
    /// Total number of frames (block_nr * block_size / frame_size)
    pub tp_frame_nr: u32,
}

/// TPACKET2 header structure (maps to `tpacket2_hdr` in the kernel)
///
/// This header precedes each packet in the ring buffer and contains
/// metadata about the packet.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Tpacket2Hdr {
    /// Frame status (TP_STATUS_*)
    pub tp_status: u32,
    /// Packet length (wire length)
    pub tp_len: u32,
    /// Captured length (may be less than tp_len due to snaplen)
    pub tp_snaplen: u32,
    /// Offset from start of frame to MAC header
    pub tp_mac: u16,
    /// Offset from start of frame to network header
    pub tp_net: u16,
    /// Timestamp seconds
    pub tp_sec: u32,
    /// Timestamp nanoseconds
    pub tp_nsec: u32,
    /// VLAN TCI (tag control information)
    pub tp_vlan_tci: u16,
    /// VLAN TPID (tag protocol identifier)
    pub tp_vlan_tpid: u16,
    /// Padding
    pub tp_padding: [u8; 4],
}

/// Size of the tpacket2_hdr structure
pub const TPACKET2_HDRLEN: usize = std::mem::size_of::<Tpacket2Hdr>();

/// Alignment for packet data within a frame
pub const TPACKET_ALIGNMENT: usize = 16;

/// Align a value up to TPACKET_ALIGNMENT
pub const fn tpacket_align(x: usize) -> usize {
    (x + TPACKET_ALIGNMENT - 1) & !(TPACKET_ALIGNMENT - 1)
}

/// Configuration for a ring buffer
#[derive(Debug, Clone)]
pub struct RingConfig {
    /// Size of each frame in the ring buffer
    pub frame_size: usize,
    /// Number of frames in the ring buffer
    pub frame_count: u32,
}

impl Default for RingConfig {
    fn default() -> Self {
        Self {
            frame_size: 2048,
            frame_count: 256,
        }
    }
}

impl RingConfig {
    /// Calculate the block size (must be page-aligned, power of 2)
    pub fn block_size(&self) -> u32 {
        let page_size = 4096usize;
        // Block size must be a power of 2 and >= frame_size
        let mut block_size = page_size;
        while block_size < self.frame_size {
            block_size *= 2;
        }
        block_size as u32
    }

    /// Calculate the number of blocks needed
    pub fn block_count(&self) -> u32 {
        let frames_per_block = self.block_size() as usize / self.frame_size;
        if frames_per_block == 0 {
            self.frame_count
        } else {
            (self.frame_count as usize + frames_per_block - 1) as u32 / frames_per_block as u32
        }
    }

    /// Calculate the total mmap size
    pub fn total_size(&self) -> usize {
        self.block_size() as usize * self.block_count() as usize
    }

    /// Build a TpacketReq for this configuration
    pub fn to_tpacket_req(&self) -> TpacketReq {
        let block_size = self.block_size();
        let frames_per_block = block_size as usize / self.frame_size;
        let block_count = self.block_count();
        let frame_count = block_count as usize * frames_per_block;

        TpacketReq {
            tp_block_size: block_size,
            tp_block_nr: block_count,
            tp_frame_size: self.frame_size as u32,
            tp_frame_nr: frame_count as u32,
        }
    }
}

/// A memory-mapped ring buffer for packet I/O.
///
/// This struct manages the mmap'd memory region and provides methods to
/// access individual frames for reading and writing.
pub struct MmapRing {
    /// Pointer to the mmap'd region
    mmap_ptr: *mut u8,
    /// Total size of the mmap'd region
    mmap_size: usize,
    /// Size of each frame
    frame_size: usize,
    /// Total number of frames
    frame_count: usize,
    /// Current frame index for sequential access
    current_frame: usize,
}

// Safety: The mmap region is thread-safe when properly synchronized by the caller
unsafe impl Send for MmapRing {}
unsafe impl Sync for MmapRing {}

impl MmapRing {
    /// Create a new ring buffer by mmap'ing memory for the given socket.
    ///
    /// # Safety
    /// The socket fd must be valid and must have had the appropriate
    /// PACKET_RX_RING or PACKET_TX_RING configured via setsockopt.
    pub unsafe fn new(fd: i32, config: &RingConfig, offset: usize) -> io::Result<Self> {
        let total_size = config.total_size();
        let req = config.to_tpacket_req();

        let ptr = libc::mmap(
            ptr::null_mut(),
            total_size + offset,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );

        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            mmap_ptr: (ptr as *mut u8).add(offset),
            mmap_size: total_size,
            frame_size: config.frame_size,
            frame_count: req.tp_frame_nr as usize,
            current_frame: 0,
        })
    }

    /// Create a ring buffer from raw parts (for testing).
    ///
    /// # Safety
    /// The caller must ensure the pointer is valid and the memory is properly allocated.
    #[cfg(test)]
    pub unsafe fn from_raw(ptr: *mut u8, frame_size: usize, frame_count: usize) -> Self {
        Self {
            mmap_ptr: ptr,
            mmap_size: frame_size * frame_count,
            frame_size,
            frame_count,
            current_frame: 0,
        }
    }

    /// Get a pointer to the frame header at the given index.
    fn frame_header(&self, index: usize) -> *mut Tpacket2Hdr {
        let offset = index * self.frame_size;
        unsafe { self.mmap_ptr.add(offset) as *mut Tpacket2Hdr }
    }

    /// Get the status of a frame.
    pub fn frame_status(&self, index: usize) -> u32 {
        let hdr = self.frame_header(index);
        unsafe { (*hdr).tp_status }
    }

    /// Set the status of a frame.
    pub fn set_frame_status(&self, index: usize, status: u32) {
        let hdr = self.frame_header(index);
        unsafe {
            // Use volatile write to ensure the kernel sees the update
            ptr::write_volatile(&mut (*hdr).tp_status, status);
        }
    }

    /// Get a slice to the packet data in the given frame.
    ///
    /// Returns the raw packet data (starting from the MAC header).
    pub fn frame_data(&self, index: usize) -> Option<&[u8]> {
        let hdr = self.frame_header(index);
        unsafe {
            let mac_offset = (*hdr).tp_mac as usize;
            let snaplen = (*hdr).tp_snaplen as usize;
            if mac_offset == 0 || snaplen == 0 {
                return None;
            }
            let data_ptr = (hdr as *const u8).add(mac_offset);
            Some(std::slice::from_raw_parts(data_ptr, snaplen))
        }
    }

    /// Get a mutable slice to write packet data into a frame.
    ///
    /// Returns a buffer starting after the tpacket header, suitable for
    /// writing a complete Ethernet frame.
    pub fn frame_data_mut(&self, index: usize) -> &mut [u8] {
        let hdr = self.frame_header(index);
        let data_offset = tpacket_align(TPACKET2_HDRLEN);
        let available = self.frame_size - data_offset;
        unsafe {
            let data_ptr = (hdr as *mut u8).add(data_offset);
            std::slice::from_raw_parts_mut(data_ptr, available)
        }
    }

    /// Write a frame for transmission and mark it ready.
    ///
    /// Copies the frame data into the TX ring and sets the status to
    /// `TP_STATUS_SEND_REQUEST`.
    pub fn write_tx_frame(&self, frame: &[u8]) -> io::Result<usize> {
        let index = self.current_frame;

        // Check if frame slot is available
        if self.frame_status(index) != TP_STATUS_AVAILABLE {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "TX ring frame not available"));
        }

        let data = self.frame_data_mut(index);
        if frame.len() > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Frame too large: {} > {}", frame.len(), data.len()),
            ));
        }

        // Copy frame data
        data[..frame.len()].copy_from_slice(frame);

        // Set the header fields
        let hdr = self.frame_header(index);
        unsafe {
            (*hdr).tp_len = frame.len() as u32;
            (*hdr).tp_snaplen = frame.len() as u32;
        }

        // Mark as ready to send
        self.set_frame_status(index, TP_STATUS_SEND_REQUEST);

        Ok(frame.len())
    }

    /// Read a received frame from the RX ring.
    ///
    /// Returns the frame data if a frame is available, or None if
    /// no frames are ready.
    pub fn read_rx_frame(&self) -> Option<Vec<u8>> {
        let index = self.current_frame;

        // Check if frame has data
        if self.frame_status(index) & TP_STATUS_USER == 0 {
            return None;
        }

        // Read frame data
        let data = self.frame_data(index)?;
        let frame = data.to_vec();

        // Return frame to kernel
        self.set_frame_status(index, TP_STATUS_KERNEL);

        Some(frame)
    }

    /// Advance to the next frame in the ring.
    pub fn advance(&mut self) {
        self.current_frame = (self.current_frame + 1) % self.frame_count;
    }

    /// Get the current frame index.
    pub fn current_index(&self) -> usize {
        self.current_frame
    }

    /// Get the total number of frames.
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Get the frame size.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Reset the current frame pointer to the beginning.
    pub fn reset(&mut self) {
        self.current_frame = 0;
    }
}

impl Drop for MmapRing {
    fn drop(&mut self) {
        if !self.mmap_ptr.is_null() {
            unsafe {
                libc::munmap(self.mmap_ptr as *mut libc::c_void, self.mmap_size);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_config_defaults() {
        let config = RingConfig::default();
        assert_eq!(config.frame_size, 2048);
        assert_eq!(config.frame_count, 256);
    }

    #[test]
    fn test_ring_config_block_size() {
        let config = RingConfig {
            frame_size: 2048,
            frame_count: 256,
        };
        let block_size = config.block_size();
        // Block size must be a power of 2 and >= frame_size
        assert!(block_size >= 2048);
        assert!(block_size.is_power_of_two());
    }

    #[test]
    fn test_ring_config_to_tpacket_req() {
        let config = RingConfig {
            frame_size: 2048,
            frame_count: 256,
        };
        let req = config.to_tpacket_req();
        assert_eq!(req.tp_frame_size, 2048);
        assert!(req.tp_frame_nr >= 256);
        assert!(req.tp_block_size.is_power_of_two());
        assert_eq!(
            req.tp_frame_nr as usize,
            req.tp_block_nr as usize * req.tp_block_size as usize / req.tp_frame_size as usize
        );
    }

    #[test]
    fn test_tpacket_align() {
        assert_eq!(tpacket_align(0), 0);
        assert_eq!(tpacket_align(1), 16);
        assert_eq!(tpacket_align(15), 16);
        assert_eq!(tpacket_align(16), 16);
        assert_eq!(tpacket_align(17), 32);
        assert_eq!(tpacket_align(TPACKET2_HDRLEN), tpacket_align(TPACKET2_HDRLEN));
    }

    #[test]
    fn test_tpacket2_hdr_size() {
        // tpacket2_hdr should be 32 bytes
        assert_eq!(TPACKET2_HDRLEN, 32);
    }

    #[test]
    fn test_mmap_ring_simulated() {
        // Simulate a ring buffer using heap-allocated memory
        let frame_size = 2048;
        let frame_count = 4;
        let total_size = frame_size * frame_count;
        let layout = std::alloc::Layout::from_size_align(total_size, 16).unwrap();

        unsafe {
            let ptr = std::alloc::alloc_zeroed(layout);
            assert!(!ptr.is_null());

            let mut ring = MmapRing::from_raw(ptr, frame_size, frame_count);

            // Initially all frames should have status 0 (KERNEL/AVAILABLE)
            for i in 0..frame_count {
                assert_eq!(ring.frame_status(i), 0);
            }

            // Test write_tx_frame
            let test_frame = vec![0xffu8; 64];
            let result = ring.write_tx_frame(&test_frame);
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 64);

            // Frame should now be marked as SEND_REQUEST
            assert_eq!(ring.frame_status(0), TP_STATUS_SEND_REQUEST);

            // Advance and write another
            ring.advance();
            let result2 = ring.write_tx_frame(&test_frame);
            assert!(result2.is_ok());

            // Test read_rx_frame - initially no data (status is 0/KERNEL)
            ring.reset();
            // Set status to USER to simulate a received packet
            ring.set_frame_status(0, TP_STATUS_USER);
            // We need to set tp_mac and tp_snaplen for frame_data to work
            let hdr = ring.frame_header(0);
            (*hdr).tp_mac = tpacket_align(TPACKET2_HDRLEN) as u16;
            (*hdr).tp_snaplen = 64;
            // Copy some data at the mac offset
            let data_offset = tpacket_align(TPACKET2_HDRLEN);
            let data_ptr = (hdr as *mut u8).add(data_offset);
            std::ptr::copy_nonoverlapping(test_frame.as_ptr(), data_ptr, 64);

            let received = ring.read_rx_frame();
            assert!(received.is_some());
            assert_eq!(received.unwrap().len(), 64);

            // After reading, status should be back to KERNEL
            assert_eq!(ring.frame_status(0), TP_STATUS_KERNEL);

            // Don't let MmapRing's Drop call munmap on heap memory
            ring.mmap_ptr = ptr::null_mut();
            std::alloc::dealloc(ptr, layout);
        }
    }
}
