//! Zero-copy frame pool for multi-core pipeline.
//!
//! Replaces per-packet `Vec<u8>` heap allocations with a pre-allocated slab of
//! fixed-size frame buffers. Frames are passed by index (`FrameRef`) through
//! SPSC rings instead of by value, eliminating all allocator traffic on the
//! RX→Worker hot path.
//!
//! The pool is single-allocation: one contiguous `Box<[u8]>` of `capacity × frame_size`
//! bytes, with a lock-free SPSC free list of available indices.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Maximum Ethernet frame size (MTU 1500 + 14 Ethernet header + 4 FCS).
/// Rounded up to next power of 2 for alignment.
const DEFAULT_FRAME_SIZE: usize = 2048;

/// Default pool capacity (number of frame slots).
const DEFAULT_POOL_CAPACITY: usize = 16384;

/// A lightweight reference to a frame in the pool.
///
/// Only 8 bytes — fits in a single atomic and can be passed through SPSC rings
/// with minimal overhead. The `pool_idx` indexes into the `FramePool`, and `len`
/// records the actual frame length (the slot is always `frame_size` bytes, but
/// only the first `len` bytes contain valid data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct FrameRef {
    /// Index into the FramePool's buffer.
    pub pool_idx: u32,
    /// Actual frame length in bytes.
    pub len: u16,
    /// Padding for alignment.
    _pad: u16,
}

impl FrameRef {
    /// Create a new FrameRef.
    #[inline]
    pub fn new(pool_idx: u32, len: u16) -> Self {
        Self { pool_idx, len, _pad: 0 }
    }
}

/// Pre-allocated pool of fixed-size frame buffers.
///
/// Frames are allocated by index and freed back to the pool. The pool uses a
/// lock-free free list (SPSC ring of `u32` indices) for O(1) alloc/free.
///
/// # Safety
///
/// The pool buffer is accessed via `UnsafeCell` because multiple threads may
/// hold references to different frame slots concurrently. The safety invariant
/// is maintained by the allocation protocol: only the holder of a `FrameRef`
/// may access that slot, and they must free it when done.
pub struct FramePool {
    /// Contiguous allocation: capacity × frame_size bytes.
    buffer: UnsafeCell<Box<[u8]>>,
    /// Size of each frame slot in bytes.
    frame_size: usize,
    /// Total number of frame slots.
    capacity: usize,
    /// Free list: ring buffer of available frame indices.
    /// Uses atomic head/tail for lock-free SPSC access.
    free_head: AtomicU64,
    free_tail: AtomicU64,
    free_list: Box<[AtomicU32]>,
    free_capacity: u64,
}

// SAFETY: FramePool is designed for concurrent access. The buffer is accessed
// via frame indices, and the allocation protocol ensures exclusive access to
// each slot. The free list uses atomics for thread-safe alloc/free.
unsafe impl Send for FramePool {}
unsafe impl Sync for FramePool {}

impl FramePool {
    /// Create a new frame pool with the given capacity and frame size.
    pub fn new(capacity: usize, frame_size: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        let frame_size = frame_size.max(64);

        // Allocate the contiguous buffer
        let buffer = vec![0u8; capacity * frame_size].into_boxed_slice();

        // Initialize the free list with all indices
        let free_cap = capacity.next_power_of_two();
        let free_list: Vec<AtomicU32> = (0..free_cap)
            .map(|i| AtomicU32::new(if i < capacity { i as u32 } else { u32::MAX }))
            .collect();

        Self {
            buffer: UnsafeCell::new(buffer),
            frame_size,
            capacity,
            free_head: AtomicU64::new(capacity as u64),
            free_tail: AtomicU64::new(0),
            free_list: free_list.into_boxed_slice(),
            free_capacity: free_cap as u64,
        }
    }

    /// Create a pool with default settings (16384 slots, 2048 bytes each).
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_POOL_CAPACITY, DEFAULT_FRAME_SIZE)
    }

    /// Returns the frame size (slot size in bytes).
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Returns the total capacity (number of frame slots).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline]
    fn free_mask(&self) -> u64 {
        self.free_capacity - 1
    }

    /// Allocate a frame index from the pool.
    ///
    /// Returns `None` if the pool is exhausted (all frames are in use).
    #[inline]
    pub fn alloc(&self) -> Option<u32> {
        let tail = self.free_tail.load(Ordering::Relaxed);
        let head = self.free_head.load(Ordering::Acquire);

        if tail >= head {
            return None; // pool exhausted
        }

        let slot = (tail & self.free_mask()) as usize;
        let idx = self.free_list[slot].load(Ordering::Relaxed);

        self.free_tail.store(tail + 1, Ordering::Release);
        Some(idx)
    }

    /// Return a frame index to the pool.
    ///
    /// # Panics
    ///
    /// Panics if `idx >= capacity` (debug builds only).
    #[inline]
    pub fn free(&self, idx: u32) {
        debug_assert!((idx as usize) < self.capacity, "frame index out of bounds");

        let head = self.free_head.load(Ordering::Relaxed);
        let slot = (head & self.free_mask()) as usize;
        self.free_list[slot].store(idx, Ordering::Relaxed);
        self.free_head.store(head + 1, Ordering::Release);
    }

    /// Get a mutable slice to the frame data at the given index.
    ///
    /// # Safety
    ///
    /// The caller must hold exclusive access to this frame index (i.e., it was
    /// obtained via `alloc()` and has not been freed). Multiple callers must not
    /// access the same frame index concurrently.
    #[inline]
    pub unsafe fn frame_mut(&self, idx: u32) -> &mut [u8] {
        let offset = idx as usize * self.frame_size;
        let buf = &mut *self.buffer.get();
        &mut buf[offset..offset + self.frame_size]
    }

    /// Get an immutable slice to the frame data at the given index.
    ///
    /// # Safety
    ///
    /// The caller must hold a valid reference to this frame index (obtained via
    /// `alloc()`, not yet freed).
    #[inline]
    pub unsafe fn frame(&self, idx: u32) -> &[u8] {
        let offset = idx as usize * self.frame_size;
        let buf = &*self.buffer.get();
        &buf[offset..offset + self.frame_size]
    }

    /// Allocate a frame and copy data into it, returning a FrameRef.
    ///
    /// Returns `None` if the pool is exhausted.
    #[inline]
    pub fn alloc_copy(&self, data: &[u8]) -> Option<FrameRef> {
        let idx = self.alloc()?;
        let len = data.len().min(self.frame_size);
        // SAFETY: We just allocated this index, so we have exclusive access.
        unsafe {
            let slot = self.frame_mut(idx);
            slot[..len].copy_from_slice(&data[..len]);
        }
        Some(FrameRef::new(idx, len as u16))
    }

    /// Get frame data referenced by a FrameRef.
    ///
    /// # Safety
    ///
    /// The FrameRef must be valid (obtained from this pool, not yet freed).
    #[inline]
    pub unsafe fn get_frame_data(&self, frame_ref: &FrameRef) -> &[u8] {
        let offset = frame_ref.pool_idx as usize * self.frame_size;
        let buf = &*self.buffer.get();
        &buf[offset..offset + frame_ref.len as usize]
    }

    /// Returns the number of available (free) frames.
    pub fn available(&self) -> usize {
        let head = self.free_head.load(Ordering::Acquire);
        let tail = self.free_tail.load(Ordering::Acquire);
        (head - tail) as usize
    }

    /// Returns the number of frames currently in use.
    pub fn in_use(&self) -> usize {
        self.capacity - self.available()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn frame_ref_size() {
        assert_eq!(std::mem::size_of::<FrameRef>(), 8);
    }

    #[test]
    fn pool_basic_alloc_free() {
        let pool = FramePool::new(4, 128);
        assert_eq!(pool.capacity(), 4);
        assert_eq!(pool.frame_size(), 128);
        assert_eq!(pool.available(), 4);
        assert_eq!(pool.in_use(), 0);

        // Allocate all frames
        let mut indices = Vec::new();
        for _ in 0..4 {
            let idx = pool.alloc().expect("should alloc");
            indices.push(idx);
        }

        assert_eq!(pool.available(), 0);
        assert_eq!(pool.in_use(), 4);

        // Pool exhausted
        assert!(pool.alloc().is_none());

        // Free all frames
        for idx in indices {
            pool.free(idx);
        }

        assert_eq!(pool.available(), 4);
        assert_eq!(pool.in_use(), 0);
    }

    #[test]
    fn pool_write_read() {
        let pool = FramePool::new(4, 64);
        let idx = pool.alloc().unwrap();

        unsafe {
            let slot = pool.frame_mut(idx);
            slot[..5].copy_from_slice(b"hello");

            let data = pool.frame(idx);
            assert_eq!(&data[..5], b"hello");
        }

        pool.free(idx);
    }

    #[test]
    fn pool_alloc_copy() {
        let pool = FramePool::new(4, 128);
        let data = b"test frame data";

        let frame_ref = pool.alloc_copy(data).unwrap();
        assert_eq!(frame_ref.len, data.len() as u16);

        unsafe {
            let read_back = pool.get_frame_data(&frame_ref);
            assert_eq!(read_back, data);
        }

        pool.free(frame_ref.pool_idx);
    }

    #[test]
    fn pool_exhaust_and_refill() {
        let pool = FramePool::new(2, 64);

        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        assert!(pool.alloc().is_none());

        pool.free(a);
        let c = pool.alloc().unwrap();
        assert_eq!(c, a); // reuses the freed index

        pool.free(b);
        pool.free(c);
    }

    #[test]
    fn pool_cycle_many_times() {
        let pool = FramePool::new(8, 64);

        // Alloc/free many times to verify wraparound works
        for round in 0..100 {
            let idx = pool.alloc().expect(&format!("alloc failed on round {}", round));
            unsafe {
                let slot = pool.frame_mut(idx);
                slot[0] = round as u8;
            }
            pool.free(idx);
        }
    }

    #[test]
    fn pool_producer_consumer() {
        // Simulate RX thread allocating frames and worker thread freeing them
        let pool = Arc::new(FramePool::new(1024, 128));
        let ring = Arc::new(crate::ring::SpscRing::<FrameRef>::new(1024));
        let count = 50_000u32;

        let producer = {
            let pool = Arc::clone(&pool);
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                for i in 0..count {
                    // Allocate, write data, enqueue ref
                    loop {
                        if let Some(idx) = pool.alloc() {
                            unsafe {
                                let slot = pool.frame_mut(idx);
                                slot[..4].copy_from_slice(&i.to_le_bytes());
                            }
                            let fr = FrameRef::new(idx, 4);
                            while ring.enqueue(fr).is_err() {
                                std::hint::spin_loop();
                            }
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            })
        };

        let consumer = {
            let pool = Arc::clone(&pool);
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                let mut next = 0u32;
                while next < count {
                    if let Some(fr) = ring.dequeue() {
                        unsafe {
                            let data = pool.get_frame_data(&fr);
                            let val = u32::from_le_bytes(data[..4].try_into().unwrap());
                            assert_eq!(val, next, "data mismatch");
                        }
                        pool.free(fr.pool_idx);
                        next += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        producer.join().unwrap();
        consumer.join().unwrap();

        // All frames should be back in the pool
        assert_eq!(pool.available(), 1024);
    }

    #[test]
    fn pool_with_defaults() {
        let pool = FramePool::with_defaults();
        assert_eq!(pool.capacity(), DEFAULT_POOL_CAPACITY);
        assert_eq!(pool.frame_size(), DEFAULT_FRAME_SIZE);
        assert_eq!(pool.available(), DEFAULT_POOL_CAPACITY);
    }

    #[test]
    fn frame_ref_new() {
        let fr = FrameRef::new(42, 1500);
        assert_eq!(fr.pool_idx, 42);
        assert_eq!(fr.len, 1500);
    }
}
