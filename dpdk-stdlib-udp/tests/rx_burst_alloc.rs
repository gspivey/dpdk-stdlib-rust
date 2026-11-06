//! Allocation test for the burst RX buffer free-list (tokio-udp-perf Req 5.1-5.3).
//!
//! This lives in its **own test binary** because it installs a counting
//! `#[global_allocator]`. The counter is read immediately before and after the single
//! burst-refill call (`try_recv_from`), so unrelated allocations elsewhere do not count.
//! After warm-up, the N-1 buffered packets of an N-packet burst must not allocate.

use dpdk_stdlib_udp::{build_udp_frame, PacketBackend, RxReadiness, UdpSocket};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct CountingAlloc;
static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

/// Backend that hands out a pre-built burst without allocating inside `recv_frames`.
struct BurstBackend {
    next: Mutex<Vec<Vec<u8>>>,
}

impl PacketBackend for BurstBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        Ok(frame.len())
    }
    fn recv_frames(&self, _max: usize) -> io::Result<Vec<Vec<u8>>> {
        Ok(std::mem::take(&mut *self.next.lock().unwrap()))
    }
    fn mac_address(&self) -> [u8; 6] {
        [0x02, 0, 0, 0, 0, 1]
    }
    fn backend_name(&self) -> &'static str {
        "burst-mock"
    }
    fn set_promiscuous(&self, _e: bool) -> io::Result<()> {
        Ok(())
    }
    fn is_promiscuous(&self) -> bool {
        false
    }
    fn set_allmulticast(&self, _e: bool) -> io::Result<()> {
        Ok(())
    }
    fn is_allmulticast(&self) -> bool {
        false
    }
    fn rx_readiness(&self) -> RxReadiness {
        RxReadiness::PollOnly
    }
}

const N: usize = 16;
const PORT: u16 = 9100;

fn burst(round: u8) -> Vec<Vec<u8>> {
    (0..N)
        .map(|i| {
            let payload = [round, i as u8, 0xAB, 0xCD, 1, 2, 3, 4];
            build_udp_frame(
                &[0xaa; 6],
                &[0x02, 0, 0, 0, 0, 1],
                Ipv4Addr::new(10, 0, 0, 2),
                Ipv4Addr::new(10, 0, 0, 1),
                7000 + i as u16,
                PORT,
                &payload,
                64,
            )
            .unwrap()
        })
        .collect()
}

#[test]
fn buffered_burst_packets_do_not_allocate_after_warmup() {
    let backend = Arc::new(BurstBackend { next: Mutex::new(Vec::new()) });
    let sock = UdpSocket::bind_with_backend(
        SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), PORT),
        backend.clone() as Arc<dyn PacketBackend>,
    )
    .unwrap();
    let mut buf = [0u8; 2048];

    // Warm-up: one burst fills the free-list, then drain it.
    *backend.next.lock().unwrap() = burst(0);
    let (len, _) = sock.try_recv_from(&mut buf).unwrap().expect("first packet");
    assert_eq!(len, 8);
    for _ in 1..N {
        sock.try_recv_from(&mut buf).unwrap().expect("buffered packet");
    }

    // Measured burst. Build frames before the counter window.
    *backend.next.lock().unwrap() = burst(1);
    let before = ALLOCS.load(Ordering::SeqCst);
    let first = sock.try_recv_from(&mut buf).unwrap().expect("first packet");
    let after = ALLOCS.load(Ordering::SeqCst);
    assert_eq!(&buf[..2], &[1, 0]);
    assert_eq!(first.1.port(), 7000);

    let delta = after - before;
    assert_eq!(
        delta, 0,
        "refill of a {N}-packet burst allocated {delta} times; buffered packets must reuse the free-list"
    );

    // Correctness: the buffered packets come back in order with the right payload and source.
    for i in 1..N {
        let (len, src) = sock.try_recv_from(&mut buf).unwrap().expect("buffered packet");
        assert_eq!(len, 8);
        assert_eq!(&buf[..4], &[1, i as u8, 0xAB, 0xCD]);
        assert_eq!(src.port(), 7000 + i as u16);
    }
}
