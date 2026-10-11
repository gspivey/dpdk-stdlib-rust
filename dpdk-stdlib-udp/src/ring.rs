//! Lock-free ring buffers for multi-core pipeline communication.
//!
//! Two ring types for the multi-core topology:
//! - [`SpscRing`]: Single-Producer Single-Consumer (RX core → Worker, Worker → TX)
//! - [`MpscRing`]: Multi-Producer Single-Consumer (N workers → app recv_from)
//!
//! Both use cache-line padding to prevent false sharing and acquire/release
//! semantics for correct cross-core visibility.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU64, Ordering};

// ============================================================================
// Cache-line padding
// ============================================================================

/// Aligns the inner value to a 64-byte cache line to prevent false sharing.
#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    fn new(value: T) -> Self {
        Self { value }
    }
}

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

// ============================================================================
// SpscRing — Single-Producer Single-Consumer
// ============================================================================

/// A lock-free Single-Producer Single-Consumer ring buffer.
///
/// Fast path uses only relaxed loads with acquire/release fences —
/// no CAS or read-modify-write atomics needed.
///
/// Capacity must be a power of 2 (enforced by constructor).
pub struct SpscRing<T> {
    head: CachePadded<AtomicU64>, // written by producer
    tail: CachePadded<AtomicU64>, // written by consumer
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    capacity: u64, // power of 2, used as mask (capacity - 1)
}

// SAFETY: The SPSC contract guarantees that head is only written by the producer
// and tail is only written by the consumer. The slots between tail..head are
// owned by the producer (for writing) or consumer (for reading) exclusively.
unsafe impl<T: Send> Send for SpscRing<T> {}
unsafe impl<T: Send> Sync for SpscRing<T> {}

impl<T> SpscRing<T> {
    /// Create a new SPSC ring with the given capacity.
    ///
    /// Capacity is rounded up to the next power of 2 (minimum 2).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two() as u64;
        let mut slots = Vec::with_capacity(capacity as usize);
        for _ in 0..capacity {
            slots.push(UnsafeCell::new(MaybeUninit::uninit()));
        }
        Self {
            head: CachePadded::new(AtomicU64::new(0)),
            tail: CachePadded::new(AtomicU64::new(0)),
            slots: slots.into_boxed_slice(),
            capacity,
        }
    }

    /// Returns the usable capacity of the ring.
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    #[inline]
    fn mask(&self) -> u64 {
        self.capacity - 1
    }

    /// Try to enqueue a single item. Returns `Err(item)` if the ring is full.
    #[inline]
    pub fn enqueue(&self, item: T) -> Result<(), T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        if head - tail >= self.capacity {
            return Err(item);
        }

        let slot = (head & self.mask()) as usize;
        // SAFETY: We are the sole producer. The slot at `head` is not readable
        // by the consumer until we publish the new head below.
        unsafe {
            (*self.slots[slot].get()).write(item);
        }

        // Release fence ensures the slot write is visible before head advances.
        self.head.store(head + 1, Ordering::Release);
        Ok(())
    }

    /// Try to dequeue a single item. Returns `None` if the ring is empty.
    #[inline]
    pub fn dequeue(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        if tail >= head {
            return None;
        }

        let slot = (tail & self.mask()) as usize;
        // SAFETY: We are the sole consumer. The slot at `tail` was written by
        // the producer and is visible due to the Acquire load of head above.
        let item = unsafe { (*self.slots[slot].get()).assume_init_read() };

        // Release fence ensures we've finished reading before tail advances.
        self.tail.store(tail + 1, Ordering::Release);
        Some(item)
    }

    /// Dequeue up to `max` items into a `Vec`.
    pub fn dequeue_batch(&self, max: usize) -> Vec<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        let available = (head - tail) as usize;
        let count = available.min(max);

        let mut result = Vec::with_capacity(count);
        for i in 0..count {
            let slot = ((tail + i as u64) & self.mask()) as usize;
            // SAFETY: These slots are in the valid range [tail, head) and
            // were written by the producer.
            let item = unsafe { (*self.slots[slot].get()).assume_init_read() };
            result.push(item);
        }

        self.tail.store(tail + count as u64, Ordering::Release);
        result
    }

    /// Returns the number of items currently in the ring.
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        (head - tail) as usize
    }

    /// Returns true if the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns true if the ring is full.
    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity as usize
    }
}

impl<T> Drop for SpscRing<T> {
    fn drop(&mut self) {
        // Drop any remaining items in the ring.
        let head = *self.head.get_mut();
        let tail = *self.tail.get_mut();
        for i in tail..head {
            let slot = (i & self.mask()) as usize;
            unsafe {
                (*self.slots[slot].get()).assume_init_drop();
            }
        }
    }
}

// ============================================================================
// MpscRing — Multi-Producer Single-Consumer
// ============================================================================

/// A lock-free Multi-Producer Single-Consumer ring buffer.
///
/// Uses a two-phase commit for producers:
/// 1. **Claim**: CAS on `head` to reserve a slot
/// 2. **Publish**: Write the item, then mark the slot as committed
///
/// The consumer reads slots only after they are committed.
///
/// Capacity must be a power of 2 (enforced by constructor).
pub struct MpscRing<T> {
    head: CachePadded<AtomicU64>,      // CAS'd by producers to claim slots
    tail: CachePadded<AtomicU64>,      // advanced by consumer
    slots: Box<[MpscSlot<T>]>,
    capacity: u64,
}

/// Each slot has a sequence number to track the two-phase commit state.
///
/// The sequence progresses:
/// - `seq == slot_index`: slot is empty, ready for a producer to claim
/// - `seq == slot_index + 1`: slot is filled, ready for consumer to read
struct MpscSlot<T> {
    seq: AtomicU64,
    data: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Send> Send for MpscRing<T> {}
unsafe impl<T: Send> Sync for MpscRing<T> {}

impl<T> MpscRing<T> {
    /// Create a new MPSC ring with the given capacity.
    ///
    /// Capacity is rounded up to the next power of 2 (minimum 2).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two() as u64;
        let mut slots = Vec::with_capacity(capacity as usize);
        for i in 0..capacity {
            slots.push(MpscSlot {
                seq: AtomicU64::new(i),
                data: UnsafeCell::new(MaybeUninit::uninit()),
            });
        }
        Self {
            head: CachePadded::new(AtomicU64::new(0)),
            tail: CachePadded::new(AtomicU64::new(0)),
            slots: slots.into_boxed_slice(),
            capacity,
        }
    }

    /// Returns the usable capacity of the ring.
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    #[inline]
    fn mask(&self) -> u64 {
        self.capacity - 1
    }

    /// Try to enqueue a single item. Returns `Err(item)` if the ring is full.
    ///
    /// Thread-safe: multiple producers can call this concurrently.
    #[inline]
    pub fn enqueue(&self, item: T) -> Result<(), T> {
        let mut head = self.head.load(Ordering::Relaxed);

        loop {
            let slot_idx = (head & self.mask()) as usize;
            let slot = &self.slots[slot_idx];
            let seq = slot.seq.load(Ordering::Acquire);

            let diff = seq as i64 - head as i64;

            if diff == 0 {
                // Slot is ready for us — try to claim it
                match self.head.compare_exchange_weak(
                    head,
                    head + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // We claimed slot `head`. Write the data and publish.
                        unsafe {
                            (*slot.data.get()).write(item);
                        }
                        // Mark slot as filled (seq = head + 1)
                        slot.seq.store(head + 1, Ordering::Release);
                        return Ok(());
                    }
                    Err(actual) => {
                        // Another producer claimed it first — retry with updated head
                        head = actual;
                    }
                }
            } else if diff < 0 {
                // Ring is full — consumer hasn't caught up
                return Err(item);
            } else {
                // Another producer claimed this slot but hasn't published yet.
                // Reload head and try the next slot.
                head = self.head.load(Ordering::Relaxed);
            }
        }
    }

    /// Try to dequeue a single item. Returns `None` if the ring is empty
    /// or if the next slot hasn't been published yet.
    ///
    /// Only one thread may call this (the single consumer).
    #[inline]
    pub fn dequeue(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let slot_idx = (tail & self.mask()) as usize;
        let slot = &self.slots[slot_idx];
        let seq = slot.seq.load(Ordering::Acquire);

        // The slot is ready to read when seq == tail + 1
        if seq != tail + 1 {
            return None;
        }

        let item = unsafe { (*slot.data.get()).assume_init_read() };

        // Reset the slot sequence for reuse: seq = tail + capacity
        // (this makes it "empty" for a future producer at head == tail + capacity)
        slot.seq.store(tail + self.capacity, Ordering::Release);
        self.tail.store(tail + 1, Ordering::Release);

        Some(item)
    }

    /// Dequeue up to `max` items.
    ///
    /// Only one thread may call this (the single consumer).
    pub fn dequeue_batch(&self, max: usize) -> Vec<T> {
        let mut result = Vec::with_capacity(max);
        for _ in 0..max {
            match self.dequeue() {
                Some(item) => result.push(item),
                None => break,
            }
        }
        result
    }

    /// Returns an approximate count of items in the ring.
    ///
    /// This is inherently racy for MPSC — producers may be mid-commit.
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        (head - tail) as usize
    }

    /// Returns true if the ring appears empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for MpscRing<T> {
    fn drop(&mut self) {
        // Drain remaining items
        while self.dequeue().is_some() {}
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

    // ========================================================================
    // A3: SpscRing unit tests
    // ========================================================================

    #[test]
    fn spsc_capacity_rounds_up_to_power_of_two() {
        let ring: SpscRing<u32> = SpscRing::new(3);
        assert_eq!(ring.capacity(), 4);

        let ring: SpscRing<u32> = SpscRing::new(1);
        assert_eq!(ring.capacity(), 2); // minimum is 2

        let ring: SpscRing<u32> = SpscRing::new(8);
        assert_eq!(ring.capacity(), 8);
    }

    #[test]
    fn spsc_empty_on_creation() {
        let ring: SpscRing<u32> = SpscRing::new(4);
        assert!(ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.dequeue(), None);
    }

    #[test]
    fn spsc_enqueue_dequeue_single() {
        let ring: SpscRing<u32> = SpscRing::new(4);
        assert!(ring.enqueue(42).is_ok());
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.dequeue(), Some(42));
        assert!(ring.is_empty());
    }

    #[test]
    fn spsc_fill_to_capacity() {
        let ring: SpscRing<u32> = SpscRing::new(4);
        for i in 0..4 {
            assert!(ring.enqueue(i).is_ok());
        }
        assert!(ring.is_full());
        assert_eq!(ring.len(), 4);

        // Should fail when full
        assert_eq!(ring.enqueue(99), Err(99));

        // Dequeue all
        for i in 0..4 {
            assert_eq!(ring.dequeue(), Some(i));
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn spsc_wraparound() {
        let ring: SpscRing<u32> = SpscRing::new(4);

        // Fill and drain multiple times to force wraparound
        for round in 0..3 {
            let base = round * 4;
            for i in 0..4 {
                assert!(ring.enqueue(base + i).is_ok());
            }
            for i in 0..4 {
                assert_eq!(ring.dequeue(), Some(base + i));
            }
        }
    }

    #[test]
    fn spsc_dequeue_batch() {
        let ring: SpscRing<u32> = SpscRing::new(8);
        for i in 0..5 {
            ring.enqueue(i).unwrap();
        }

        let batch = ring.dequeue_batch(3);
        assert_eq!(batch, vec![0, 1, 2]);
        assert_eq!(ring.len(), 2);

        let batch = ring.dequeue_batch(10); // ask for more than available
        assert_eq!(batch, vec![3, 4]);
        assert!(ring.is_empty());
    }

    #[test]
    fn spsc_dequeue_batch_empty() {
        let ring: SpscRing<u32> = SpscRing::new(4);
        let batch = ring.dequeue_batch(10);
        assert!(batch.is_empty());
    }

    #[test]
    fn spsc_interleaved_enqueue_dequeue() {
        let ring: SpscRing<u32> = SpscRing::new(2);
        assert!(ring.enqueue(1).is_ok());
        assert!(ring.enqueue(2).is_ok());
        assert!(ring.enqueue(3).is_err()); // full

        assert_eq!(ring.dequeue(), Some(1));
        assert!(ring.enqueue(3).is_ok()); // slot freed

        assert_eq!(ring.dequeue(), Some(2));
        assert_eq!(ring.dequeue(), Some(3));
        assert_eq!(ring.dequeue(), None);
    }

    #[test]
    fn spsc_drops_remaining_items() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        #[derive(Debug)]
        struct DropCounter;
        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }

        DROP_COUNT.store(0, Ordering::Relaxed);
        {
            let ring: SpscRing<DropCounter> = SpscRing::new(4);
            ring.enqueue(DropCounter).unwrap();
            ring.enqueue(DropCounter).unwrap();
            ring.enqueue(DropCounter).unwrap();
            // drop ring with 3 items still inside
        }
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn spsc_cross_thread() {
        let ring = Arc::new(SpscRing::new(1024));
        let count = 100_000u64;

        let producer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                for i in 0..count {
                    while ring.enqueue(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        let consumer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                let mut next = 0u64;
                while next < count {
                    if let Some(val) = ring.dequeue() {
                        assert_eq!(val, next, "out-of-order at {next}");
                        next += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        producer.join().unwrap();
        consumer.join().unwrap();
        assert!(ring.is_empty());
    }

    // ========================================================================
    // A4: MpscRing unit tests
    // ========================================================================

    #[test]
    fn mpsc_capacity_rounds_up() {
        let ring: MpscRing<u32> = MpscRing::new(5);
        assert_eq!(ring.capacity(), 8);
    }

    #[test]
    fn mpsc_empty_on_creation() {
        let ring: MpscRing<u32> = MpscRing::new(4);
        assert!(ring.is_empty());
        assert_eq!(ring.dequeue(), None);
    }

    #[test]
    fn mpsc_single_producer_single_consumer() {
        let ring: MpscRing<u32> = MpscRing::new(4);
        for i in 0..4 {
            assert!(ring.enqueue(i).is_ok());
        }
        // Should be full
        assert_eq!(ring.enqueue(99), Err(99));

        for i in 0..4 {
            assert_eq!(ring.dequeue(), Some(i));
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn mpsc_wraparound() {
        let ring: MpscRing<u32> = MpscRing::new(4);
        for round in 0..3 {
            let base = round * 4;
            for i in 0..4 {
                ring.enqueue(base + i).unwrap();
            }
            for i in 0..4 {
                assert_eq!(ring.dequeue(), Some(base + i));
            }
        }
    }

    #[test]
    fn mpsc_dequeue_batch() {
        let ring: MpscRing<u32> = MpscRing::new(8);
        for i in 0..5 {
            ring.enqueue(i).unwrap();
        }
        let batch = ring.dequeue_batch(3);
        assert_eq!(batch, vec![0, 1, 2]);

        let batch = ring.dequeue_batch(10);
        assert_eq!(batch, vec![3, 4]);
        assert!(ring.is_empty());
    }

    #[test]
    fn mpsc_multi_producer_stress() {
        let ring = Arc::new(MpscRing::new(4096));
        let num_producers = 4;
        let items_per_producer = 25_000u64;
        let total = num_producers * items_per_producer;

        let mut producers = Vec::new();
        for p in 0..num_producers {
            let ring = Arc::clone(&ring);
            producers.push(thread::spawn(move || {
                for i in 0..items_per_producer {
                    // Encode producer id in upper bits, sequence in lower bits
                    let val = (p as u64) << 32 | i;
                    while ring.enqueue(val).is_err() {
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        let consumer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                let mut per_producer = vec![0u64; num_producers as usize];
                let mut received = 0u64;
                while received < total {
                    if let Some(val) = ring.dequeue() {
                        let producer = (val >> 32) as usize;
                        let seq = val & 0xFFFF_FFFF;
                        // Within each producer, items must arrive in order
                        assert_eq!(
                            seq, per_producer[producer],
                            "producer {producer} out-of-order: expected {}, got {seq}",
                            per_producer[producer]
                        );
                        per_producer[producer] += 1;
                        received += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
                per_producer
            })
        };

        for p in producers {
            p.join().unwrap();
        }

        let counts = consumer.join().unwrap();
        for (p, count) in counts.iter().enumerate() {
            assert_eq!(
                *count, items_per_producer,
                "producer {p} sent {items_per_producer} but consumer got {count}"
            );
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn mpsc_ordering_within_single_producer() {
        // Even with multiple producers, items from the same producer
        // must be observed in FIFO order by the consumer.
        let ring = Arc::new(MpscRing::new(256));
        let count = 10_000u64;

        let producer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                for i in 0..count {
                    while ring.enqueue(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        let consumer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                let mut next = 0u64;
                while next < count {
                    if let Some(val) = ring.dequeue() {
                        assert_eq!(val, next);
                        next += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        producer.join().unwrap();
        consumer.join().unwrap();
    }

    // ========================================================================
    // A4b: Fan-out test — 1 producer, N SPSC consumers
    // Models: RX core distributes frames round-robin to worker SPSC rings
    // ========================================================================

    #[test]
    fn fanout_one_producer_multiple_spsc_consumers() {
        let num_consumers = 4;
        let items_per_consumer = 10_000u64;
        let total = num_consumers * items_per_consumer;

        // Create one SPSC ring per consumer (models RX core → worker topology)
        let rings: Vec<Arc<SpscRing<u64>>> = (0..num_consumers)
            .map(|_| Arc::new(SpscRing::new(1024)))
            .collect();

        // Producer: round-robin distribute to consumer rings
        let producer = {
            let rings: Vec<Arc<SpscRing<u64>>> = rings.iter().map(Arc::clone).collect();
            thread::spawn(move || {
                for i in 0..total {
                    let target = (i % num_consumers) as usize;
                    while rings[target].enqueue(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        // Consumers: each drains its own SPSC ring
        let mut consumers = Vec::new();
        for (c, ring) in rings.into_iter().enumerate() {
            consumers.push(thread::spawn(move || {
                let mut received = Vec::with_capacity(items_per_consumer as usize);
                while received.len() < items_per_consumer as usize {
                    if let Some(val) = ring.dequeue() {
                        received.push(val);
                    } else {
                        std::hint::spin_loop();
                    }
                }
                // Verify ordering: each consumer should get i, i+N, i+2N, ...
                for (idx, val) in received.iter().enumerate() {
                    let expected = c as u64 + idx as u64 * num_consumers;
                    assert_eq!(
                        *val, expected,
                        "consumer {c}, item {idx}: expected {expected}, got {val}"
                    );
                }
                received.len()
            }));
        }

        producer.join().unwrap();
        for consumer in consumers {
            let count = consumer.join().unwrap();
            assert_eq!(count, items_per_consumer as usize);
        }
    }

    #[test]
    fn fanout_with_mpsc_aggregation() {
        // Full pipeline test: 1 producer → N SPSC rings → N consumers → 1 MPSC ring
        // Models: RX core → SPSC → Workers → MPSC → app recv_from
        let num_workers = 4;
        let items_per_worker = 5_000u64;
        let total = num_workers * items_per_worker;

        let spsc_rings: Vec<Arc<SpscRing<u64>>> = (0..num_workers)
            .map(|_| Arc::new(SpscRing::new(512)))
            .collect();
        let mpsc_ring = Arc::new(MpscRing::new(4096));

        // Producer: RX core distributes round-robin
        let producer = {
            let rings: Vec<Arc<SpscRing<u64>>> = spsc_rings.iter().map(Arc::clone).collect();
            thread::spawn(move || {
                for i in 0..total {
                    let target = (i % num_workers) as usize;
                    while rings[target].enqueue(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        // Workers: drain SPSC, process, enqueue to MPSC
        let mut workers = Vec::new();
        for (w, spsc) in spsc_rings.into_iter().enumerate() {
            let mpsc = Arc::clone(&mpsc_ring);
            workers.push(thread::spawn(move || {
                let mut processed = 0u64;
                while processed < items_per_worker {
                    if let Some(val) = spsc.dequeue() {
                        // "Process" the item (in real code: protocol handling)
                        let processed_val = val * 10 + w as u64;
                        while mpsc.enqueue(processed_val).is_err() {
                            std::hint::spin_loop();
                        }
                        processed += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // App consumer: dequeue from MPSC (simulates recv_from)
        let app = {
            let mpsc = Arc::clone(&mpsc_ring);
            thread::spawn(move || {
                let mut received = 0u64;
                while received < total {
                    if mpsc.dequeue().is_some() {
                        received += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
                received
            })
        };

        producer.join().unwrap();
        for w in workers {
            w.join().unwrap();
        }
        let total_received = app.join().unwrap();
        assert_eq!(total_received, total);
    }
}
