//! Lock-free single-producer/single-consumer byte ring buffer.
//!
//! Power-of-2 capacity. Head/tail are byte offsets (wrapping naturally via mask).
//! Memory ordering: producer stores head with Release; consumer loads head with Acquire.

use std::sync::atomic::{AtomicUsize, Ordering};

/// SPSC byte-stream ring buffer for TCP rx/tx paths.
pub struct SpscByteRing {
    buf: Box<[u8]>,
    /// Write position (producer advances with Release).
    head: AtomicUsize,
    /// Read position (consumer advances with Release).
    tail: AtomicUsize,
    /// Always a power of 2.
    capacity: usize,
}

impl SpscByteRing {
    /// Create a new ring with capacity rounded up to the next power of 2.
    /// Minimum effective capacity is 1 (rounds up from 0).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1).next_power_of_two();
        Self {
            buf: vec![0u8; capacity].into_boxed_slice(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            capacity,
        }
    }

    /// Write bytes into the ring. Returns the number of bytes actually written
    /// (may be less than `data.len()` if the ring is full, may be 0).
    pub fn write(&self, data: &[u8]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let free = self.capacity - (head.wrapping_sub(tail));
        let n = data.len().min(free);
        if n == 0 {
            return 0;
        }

        let mask = self.capacity - 1;
        let start = head & mask;
        let first_chunk = n.min(self.capacity - start);

        // Safety: we own the producer side; no other writer exists.
        let buf = self.buf.as_ptr() as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), buf.add(start), first_chunk);
            if first_chunk < n {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr().add(first_chunk),
                    buf,
                    n - first_chunk,
                );
            }
        }

        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Read bytes from the ring. Returns the number of bytes actually read
    /// (may be less than `buf.len()` if the ring is empty, may be 0).
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let available = head.wrapping_sub(tail);
        let n = buf.len().min(available);
        if n == 0 {
            return 0;
        }

        let mask = self.capacity - 1;
        let start = tail & mask;
        let first_chunk = n.min(self.capacity - start);

        // Safety: we own the consumer side; no other reader exists.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.buf.as_ptr().add(start),
                buf.as_mut_ptr(),
                first_chunk,
            );
            if first_chunk < n {
                std::ptr::copy_nonoverlapping(
                    self.buf.as_ptr(),
                    buf.as_mut_ptr().add(first_chunk),
                    n - first_chunk,
                );
            }
        }

        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Number of bytes available to read.
    pub fn available_read(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }

    /// Number of bytes available to write.
    pub fn available_write(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        self.capacity - head.wrapping_sub(tail)
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.available_read() == 0
    }

    /// Peek at available bytes without advancing the read pointer.
    /// Returns the number of bytes copied into `buf`.
    pub fn peek(&self, buf: &mut [u8]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let available = head.wrapping_sub(tail);
        let n = buf.len().min(available);
        if n == 0 {
            return 0;
        }

        let mask = self.capacity - 1;
        let start = tail & mask;
        let first_chunk = n.min(self.capacity - start);

        unsafe {
            std::ptr::copy_nonoverlapping(
                self.buf.as_ptr().add(start),
                buf.as_mut_ptr(),
                first_chunk,
            );
            if first_chunk < n {
                std::ptr::copy_nonoverlapping(
                    self.buf.as_ptr(),
                    buf.as_mut_ptr().add(first_chunk),
                    n - first_chunk,
                );
            }
        }
        n
    }
}

// Safety: SpscByteRing is Send+Sync because atomic operations guard head/tail,
// and the SPSC contract ensures only one thread writes and one thread reads.
unsafe impl Send for SpscByteRing {}
unsafe impl Sync for SpscByteRing {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_rounds_up() {
        let ring = SpscByteRing::new(5);
        assert_eq!(ring.capacity, 8);
        let ring = SpscByteRing::new(8);
        assert_eq!(ring.capacity, 8);
        let ring = SpscByteRing::new(0);
        assert_eq!(ring.capacity, 1);
    }

    #[test]
    fn write_read_basic() {
        let ring = SpscByteRing::new(16);
        let written = ring.write(b"hello");
        assert_eq!(written, 5);
        assert_eq!(ring.available_read(), 5);

        let mut buf = [0u8; 16];
        let read = ring.read(&mut buf);
        assert_eq!(read, 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn write_full() {
        let ring = SpscByteRing::new(4); // capacity = 4
        let written = ring.write(b"abcd");
        assert_eq!(written, 4);
        assert_eq!(ring.available_write(), 0);

        let written = ring.write(b"e");
        assert_eq!(written, 0);
    }

    #[test]
    fn read_empty() {
        let ring = SpscByteRing::new(4);
        let mut buf = [0u8; 4];
        let read = ring.read(&mut buf);
        assert_eq!(read, 0);
        assert!(ring.is_empty());
    }

    #[test]
    fn wrap_around() {
        let ring = SpscByteRing::new(4); // capacity = 4
        // Fill and drain partially to advance head/tail past the boundary
        ring.write(b"ab");
        let mut buf = [0u8; 2];
        ring.read(&mut buf);
        assert_eq!(&buf, b"ab");

        // Now write data that wraps
        let written = ring.write(b"cdef");
        assert_eq!(written, 4);
        let mut buf = [0u8; 4];
        let read = ring.read(&mut buf);
        assert_eq!(read, 4);
        assert_eq!(&buf, b"cdef");
    }

    #[test]
    fn partial_write() {
        let ring = SpscByteRing::new(4);
        ring.write(b"ab");
        let written = ring.write(b"cdef");
        assert_eq!(written, 2); // only 2 bytes free
    }

    #[test]
    fn partial_read() {
        let ring = SpscByteRing::new(16);
        ring.write(b"hello");
        let mut buf = [0u8; 3];
        let read = ring.read(&mut buf);
        assert_eq!(read, 3);
        assert_eq!(&buf, b"hel");
        assert_eq!(ring.available_read(), 2);
    }

    #[test]
    fn data_integrity_many_iterations() {
        let ring = SpscByteRing::new(8);
        let mut read_buf = [0u8; 8];
        for i in 0u8..200 {
            let data = [i, i.wrapping_add(1), i.wrapping_add(2)];
            let w = ring.write(&data);
            assert_eq!(w, 3);
            let r = ring.read(&mut read_buf[..3]);
            assert_eq!(r, 3);
            assert_eq!(&read_buf[..3], &data);
        }
    }

    #[test]
    fn concurrent_single_producer_single_consumer() {
        use std::sync::Arc;
        use std::thread;

        let ring = Arc::new(SpscByteRing::new(64));
        let ring_w = ring.clone();
        let ring_r = ring.clone();

        let total: usize = 1000;

        let writer = thread::spawn(move || {
            let mut sent = 0usize;
            let mut val = 0u8;
            while sent < total {
                let chunk: Vec<u8> = (0..4).map(|j| val.wrapping_add(j)).collect();
                let n = ring_w.write(&chunk);
                if n > 0 {
                    sent += n;
                    val = val.wrapping_add(n as u8);
                } else {
                    thread::yield_now();
                }
            }
        });

        let reader = thread::spawn(move || {
            let mut received = 0usize;
            let mut expected = 0u8;
            let mut buf = [0u8; 4];
            while received < total {
                let n = ring_r.read(&mut buf);
                for b in &buf[..n] {
                    assert_eq!(*b, expected, "mismatch at byte {received}");
                    expected = expected.wrapping_add(1);
                    received += 1;
                }
                if n == 0 {
                    thread::yield_now();
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        /// Property 24: SPSC ring data integrity — arbitrary bytes pushed in chunks
        /// must come back in the same order with no loss or duplication.
        #[test]
        fn spsc_data_integrity(
            data in proptest::collection::vec(any::<u8>(), 1..2048),
            chunk_size in 1usize..64,
        ) {
            let ring = SpscByteRing::new(128);
            let mut output = Vec::with_capacity(data.len());
            let mut written = 0;
            let mut read_buf = [0u8; 64];

            while output.len() < data.len() {
                // Write a chunk
                if written < data.len() {
                    let end = (written + chunk_size).min(data.len());
                    let n = ring.write(&data[written..end]);
                    written += n;
                }
                // Read what's available
                let n = ring.read(&mut read_buf);
                output.extend_from_slice(&read_buf[..n]);
            }

            prop_assert_eq!(&output, &data);
        }
    }
}

#[cfg(test)]
mod miri_tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    /// SPSC miri serialization: validates sequential byte ordering under miri's
    /// memory model checking.
    /// Run under miri: cargo +nightly miri test spsc_miri_serialization
    #[test]
    fn spsc_miri_serialization() {
        let ring = Arc::new(SpscByteRing::new(32));
        let ring_w = ring.clone();
        let ring_r = ring.clone();
        let total: usize = 128;

        let writer = thread::spawn(move || {
            let mut sent = 0usize;
            while sent < total {
                let byte = (sent & 0xFF) as u8;
                let n = ring_w.write(&[byte]);
                if n > 0 {
                    sent += 1;
                } else {
                    thread::yield_now();
                }
            }
        });

        let reader = thread::spawn(move || {
            let mut received = 0usize;
            let mut buf = [0u8; 1];
            while received < total {
                let n = ring_r.read(&mut buf);
                if n > 0 {
                    assert_eq!(buf[0], (received & 0xFF) as u8, "mismatch at {received}");
                    received += 1;
                } else {
                    thread::yield_now();
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }

    /// Verifies that write_mutex serializes concurrent writes from multiple
    /// references, producing non-interleaved output.
    /// Run under miri: cargo +nightly miri test write_mutex_serialization
    #[test]
    fn write_mutex_serialization() {
        use std::sync::Mutex;

        let ring = Arc::new(SpscByteRing::new(256));
        let write_mutex = Arc::new(Mutex::new(()));
        let ring1 = ring.clone();
        let ring2 = ring.clone();
        let wm1 = write_mutex.clone();
        let wm2 = write_mutex.clone();

        let t1 = thread::spawn(move || {
            for _ in 0..8 {
                let _guard = wm1.lock().unwrap();
                ring1.write(&[0xAA; 4]);
            }
        });

        let t2 = thread::spawn(move || {
            for _ in 0..8 {
                let _guard = wm2.lock().unwrap();
                ring2.write(&[0xBB; 4]);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // Read all data and verify no interleaving within 4-byte chunks.
        let mut buf = [0u8; 256];
        let total = ring.read(&mut buf);
        let chunks: Vec<&[u8]> = buf[..total].chunks(4).collect();
        for chunk in chunks {
            // Each 4-byte chunk must be all-AA or all-BB (no interleaving).
            assert!(
                chunk.iter().all(|&b| b == 0xAA) || chunk.iter().all(|&b| b == 0xBB),
                "interleaved write detected: {chunk:?}"
            );
        }
    }
}
