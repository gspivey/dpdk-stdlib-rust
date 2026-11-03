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

#[test]
fn auto_trait_set_matches_0_3_0() {
    use static_assertions::assert_impl_all;
    use std::panic::{RefUnwindSafe, UnwindSafe};
    assert_impl_all!(DpdkUdpSocket: Send, Sync, Unpin);
    assert_impl_all!(CompatSocket: Send, Sync, Unpin, UnwindSafe, RefUnwindSafe);
}
