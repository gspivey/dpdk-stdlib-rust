//! H3 safety check: can safe code reach `TxBuffer` from two threads at once?
//!
//! `dpdk_udp::UdpSocket` is `Sync`, so `Arc<UdpSocket>` shared between threads
//! compiles. `send_to` borrows the shared `TxBuffer` (an `UnsafeCell<Vec<u8>>`
//! with `unsafe impl Sync`). Debug builds carry an in-use assertion; if it
//! fires here, the outer Mutex in dpdk-stdlib-tokio is load-bearing and
//! removing it would be a data race in release builds.
//!
//! Run in a debug build against the stub backend:
//!   cargo run --bin race_check

use dpdk::port::MacAddress;
use dpdk_udp::UdpSocket;
use std::net::Ipv4Addr;
use std::sync::Arc;

fn main() {
    let sock = Arc::new(UdpSocket::bind("10.0.0.1:9000").expect("stub bind"));
    let peer = Ipv4Addr::new(10, 0, 0, 2);
    sock.add_arp_entry(peer, MacAddress::new([0x02, 0, 0, 0, 0, 2]));

    let threads: Vec<_> = (0..4)
        .map(|i| {
            let s = sock.clone();
            std::thread::spawn(move || {
                let payload = vec![b'a' + i as u8; 1200];
                for _ in 0..200_000 {
                    let _ = s.send_to(&payload, (peer, 40000));
                }
            })
        })
        .collect();

    let mut panicked = 0;
    for t in threads {
        if let Err(e) = t.join() {
            panicked += 1;
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            println!("sender thread panicked: {msg}");
        }
    }
    if panicked > 0 {
        println!("RESULT: concurrent send_to on a shared UdpSocket hit the TxBuffer in-use assertion ({panicked}/4 threads)");
        std::process::exit(1);
    }
    println!("RESULT: no assertion fired (4 threads x 200k send_to)");
}
