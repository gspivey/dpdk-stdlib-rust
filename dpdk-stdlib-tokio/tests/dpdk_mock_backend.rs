//! Tokio-layer tests over a loopback `PacketBackend` mock (no DPDK, no network).
//!
//! Covers the outer-mutex removal: `recv_from`/`send_to`/`try_*` go straight to
//! `dpdk_udp::UdpSocket`. Gated on the `dpdk` feature, which is only on when run
//! from the workspace root (`cargo test`), not `cargo test -p dpdk-stdlib-tokio`.
#![cfg(feature = "dpdk")]

use dpdk_stdlib_tokio::compat::tokio::UdpSocket as CompatSocket;
use dpdk_stdlib_tokio::{AsyncUdpSocket, DpdkUdpSocket};
use dpdk_udp::{PacketBackend, RxReadiness, UdpSocket};
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// Frames sent are queued and handed back by `recv_frames` (loopback).
struct LoopbackBackend {
    queue: Mutex<VecDeque<Vec<u8>>>,
}

impl PacketBackend for LoopbackBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.queue.lock().unwrap().push_back(frame.to_vec());
        Ok(frame.len())
    }
    fn recv_frames(&self, max: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut q = self.queue.lock().unwrap();
        let n = max.min(q.len());
        Ok(q.drain(..n).collect())
    }
    fn mac_address(&self) -> [u8; 6] {
        [0x02, 0, 0, 0, 0, 0x01]
    }
    fn backend_name(&self) -> &'static str {
        "loopback-mock"
    }
    fn set_promiscuous(&self, _enable: bool) -> io::Result<()> {
        Ok(())
    }
    fn is_promiscuous(&self) -> bool {
        false
    }
    fn set_allmulticast(&self, _enable: bool) -> io::Result<()> {
        Ok(())
    }
    fn is_allmulticast(&self) -> bool {
        false
    }
    fn rx_readiness(&self) -> RxReadiness {
        RxReadiness::PollOnly
    }
}

fn mock_socket(port: u16) -> UdpSocket {
    let backend = Arc::new(LoopbackBackend { queue: Mutex::new(VecDeque::new()) });
    UdpSocket::bind_with_backend(format!("10.0.0.1:{port}"), backend as Arc<dyn PacketBackend>)
        .expect("bind over mock backend")
}

fn self_addr(port: u16) -> SocketAddr {
    format!("10.0.0.1:{port}").parse().unwrap()
}

#[tokio::test]
async fn dpdk_socket_send_recv_without_outer_mutex() {
    let sock = DpdkUdpSocket::from_udp_socket(mock_socket(7001)).unwrap();
    sock.send_to(b"hello", self_addr(7001)).await.unwrap();
    let mut buf = [0u8; 64];
    let (n, from) = sock.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");
    assert_eq!(from.port(), 7001);
}

#[tokio::test]
async fn compat_socket_async_and_try_paths() {
    let sock = CompatSocket::from_dpdk_socket(mock_socket(7002)).unwrap();

    // Empty queue: non-blocking try path reports WouldBlock.
    let mut buf = [0u8; 64];
    let err = sock.try_recv_from(&mut buf).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

    sock.send_to(b"async", self_addr(7002)).await.unwrap();
    let (n, _) = sock.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"async");

    sock.try_send_to(b"try", self_addr(7002)).unwrap();
    let (n, _) = sock.try_recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"try");
}

#[tokio::test]
async fn compat_socket_concurrent_tasks_share_socket() {
    let sock = Arc::new(CompatSocket::from_dpdk_socket(mock_socket(7003)).unwrap());
    let mut tasks = Vec::new();
    for i in 0..4u8 {
        let s = sock.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..25 {
                s.send_to(&[i; 16], self_addr(7003)).await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let mut buf = [0u8; 64];
    for _ in 0..100 {
        let (n, _) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(n, 16);
    }
}

// ── Opt-in idle backoff (tokio-udp-perf Req 5.4) ────────────────────────────

#[test]
fn idle_backoff_default_off_and_roundtrips() {
    let d = DpdkUdpSocket::from_udp_socket(mock_socket(7010)).unwrap();
    assert_eq!(d.idle_backoff(), None);
    d.set_idle_backoff(Some(std::time::Duration::from_millis(1)));
    // Returned exactly as passed; the 2 ms clamp applies only to the effective threshold.
    assert_eq!(d.idle_backoff(), Some(std::time::Duration::from_millis(1)));
    d.set_idle_backoff(None);
    assert_eq!(d.idle_backoff(), None);

    let c = CompatSocket::from_dpdk_socket(mock_socket(7011)).unwrap();
    assert_eq!(c.idle_backoff(), None);
    c.set_idle_backoff(Some(std::time::Duration::from_millis(5)));
    assert_eq!(c.idle_backoff(), Some(std::time::Duration::from_millis(5)));
}

#[tokio::test]
async fn idle_backoff_kernel_variant_only_stores_the_value() {
    let std_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let c = CompatSocket::from_std(std_sock).unwrap();
    assert_eq!(c.idle_backoff(), None);
    c.set_idle_backoff(Some(std::time::Duration::from_millis(9)));
    assert_eq!(c.idle_backoff(), Some(std::time::Duration::from_millis(9)));
    // Still receives normally through the reactor.
    let addr = c.local_addr().unwrap();
    let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    tx.send_to(b"k", addr).unwrap();
    let mut buf = [0u8; 8];
    let (n, _) = c.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"k");
}

/// A packet that arrives only after the socket went idle (past the 2 ms threshold, so the
/// receiver is in its sleep stage) is still delivered, on a single-threaded runtime where
/// the sender task can only run if the receiver yields/sleeps.
#[tokio::test(flavor = "current_thread")]
async fn idle_backoff_delivers_packet_arriving_after_idle() {
    let sock = Arc::new(CompatSocket::from_dpdk_socket(mock_socket(7012)).unwrap());
    sock.set_idle_backoff(Some(std::time::Duration::from_millis(2)));

    let tx = sock.clone();
    let sender = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        tx.send_to(b"late", self_addr(7012)).await.unwrap();
    });

    let mut buf = [0u8; 16];
    let started = std::time::Instant::now();
    let (n, from) = tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv_from(&mut buf))
        .await
        .expect("recv_from must complete")
        .unwrap();
    assert_eq!(&buf[..n], b"late");
    assert_eq!(from.port(), 7012);
    // First-packet-after-idle latency stays within the documented ~1-2 ms (+ scheduling slack).
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    sender.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn idle_backoff_dpdk_socket_trait_path_delivers_after_idle() {
    let sock = Arc::new(DpdkUdpSocket::from_udp_socket(mock_socket(7013)).unwrap());
    sock.set_idle_backoff(Some(std::time::Duration::from_millis(2)));
    let tx = sock.clone();
    let sender = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        tx.send_to(b"late", self_addr(7013)).await.unwrap();
    });
    let mut buf = [0u8; 16];
    let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv_from(&mut buf))
        .await
        .expect("recv_from must complete")
        .unwrap();
    assert_eq!(&buf[..n], b"late");
    sender.await.unwrap();
}

/// Busy path unchanged: with a packet already queued, recv returns immediately and the
/// enabled backoff never reaches the sleep stage (no time driver needed).
#[test]
fn idle_backoff_busy_path_needs_no_timer() {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap(); // no enable_time()
    rt.block_on(async {
        let sock = CompatSocket::from_dpdk_socket(mock_socket(7014)).unwrap();
        sock.set_idle_backoff(Some(std::time::Duration::from_millis(2)));
        sock.send_to(b"now", self_addr(7014)).await.unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"now");
    });
}

#[test]
fn auto_trait_set_matches_0_3_0() {
    use static_assertions::assert_impl_all;
    use std::panic::{RefUnwindSafe, UnwindSafe};
    assert_impl_all!(DpdkUdpSocket: Send, Sync, Unpin);
    assert_impl_all!(CompatSocket: Send, Sync, Unpin, UnwindSafe, RefUnwindSafe);
}
