//! Thread-safety tests for `UdpSocket`: concurrent senders/receiver over one `Arc<UdpSocket>`,
//! per-queue guard acquisition, and the non-blocking try entry points.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static_assertions::assert_impl_all!(UdpSocket: Send, Sync, Unpin);

const SENDERS: usize = 4;

/// A `PacketBackend` that validates every UDP frame inside `send_frame` and keeps only
/// counters — storing the frames would cost hundreds of MB in a debug build.
struct ValidatingBackend {
    mac: [u8; 6],
    /// Frames before `arm()` (gratuitous ARP/NA from `bind`) are not counted.
    armed: AtomicBool,
    frames_seen: AtomicU64,
    frames_bad: AtomicU64,
}

impl ValidatingBackend {
    fn new() -> Self {
        Self {
            mac: [0x02, 0, 0, 0, 0, 0x11],
            armed: AtomicBool::new(false),
            frames_seen: AtomicU64::new(0),
            frames_bad: AtomicU64::new(0),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// Payload length for sender `id`.
    fn payload_len(id: u8) -> usize {
        32 + id as usize * 8
    }

    /// True when `payload` is exactly one sender's pattern: `payload_len(id)` bytes of `id`.
    fn intact(payload: &[u8]) -> bool {
        match payload.first() {
            Some(&id) => {
                payload.len() == Self::payload_len(id) && payload.iter().all(|&b| b == id)
            }
            None => false,
        }
    }

    fn validate(frame: &[u8]) -> bool {
        if frame.len() < 14 {
            return false;
        }
        match u16::from_be_bytes([frame[12], frame[13]]) {
            ETH_TYPE_IPV4 => {
                verify_udp_checksum(frame)
                    && parse_udp_packet(frame).map_or(false, |p| Self::intact(&p.payload))
            }
            ipv6::ETH_TYPE_IPV6 => {
                ipv6::verify_udp6_checksum(frame)
                    && ipv6::parse_udp6_packet_ref(frame).map_or(false, |p| Self::intact(p.payload))
            }
            _ => false,
        }
    }
}

impl PacketBackend for ValidatingBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        if self.armed.load(Ordering::Acquire) {
            self.frames_seen.fetch_add(1, Ordering::Relaxed);
            if !Self::validate(frame) {
                self.frames_bad.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(frame.len())
    }
    fn recv_frames(&self, _max: usize) -> io::Result<Vec<Vec<u8>>> {
        Ok(Vec::new())
    }
    fn mac_address(&self) -> [u8; 6] { self.mac }
    fn backend_name(&self) -> &'static str { "validating" }
    fn set_promiscuous(&self, _enable: bool) -> io::Result<()> { Ok(()) }
    fn is_promiscuous(&self) -> bool { false }
    fn set_allmulticast(&self, _enable: bool) -> io::Result<()> { Ok(()) }
    fn is_allmulticast(&self) -> bool { false }
    fn rx_readiness(&self) -> RxReadiness { RxReadiness::PollOnly }
}

/// N senders × `per_sender` `send_to` plus one `try_recv_from` thread on one shared socket.
fn run_stress(socket: Arc<UdpSocket>, backend: &Arc<ValidatingBackend>, dst: SocketAddr, per_sender: u64) {
    backend.arm();
    let done = Arc::new(AtomicBool::new(false));

    let receiver = {
        let socket = Arc::clone(&socket);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while !done.load(Ordering::Acquire) {
                let _ = socket.try_recv_from(&mut buf);
            }
        })
    };

    let senders: Vec<_> = (1..=SENDERS as u8)
        .map(|id| {
            let socket = Arc::clone(&socket);
            std::thread::spawn(move || {
                let payload = vec![id; ValidatingBackend::payload_len(id)];
                for _ in 0..per_sender {
                    socket.send_to(&payload, dst).expect("send_to");
                }
            })
        })
        .collect();
    for s in senders {
        s.join().expect("sender panicked");
    }
    done.store(true, Ordering::Release);
    receiver.join().expect("receiver panicked");

    assert_eq!(backend.frames_bad.load(Ordering::Relaxed), 0, "corrupt or interleaved frame");
    assert_eq!(
        backend.frames_seen.load(Ordering::Relaxed),
        SENDERS as u64 * per_sender,
        "every send_to must reach the backend exactly once"
    );
}

#[test]
fn concurrent_send_to_v4_shares_one_socket_soundly() {
    let backend = Arc::new(ValidatingBackend::new());
    let socket = Arc::new(
        UdpSocket::bind_with_backend("10.77.0.1:9000", backend.clone() as Arc<dyn PacketBackend>).unwrap(),
    );
    let peer = Ipv4Addr::new(10, 77, 0, 2);
    socket.arp_handler.cache.insert(peer, MacAddress::new([0x02, 0, 0, 0, 0, 0x22]));
    run_stress(socket, &backend, SocketAddr::V4(SocketAddrV4::new(peer, 7000)), 100_000);
}

#[test]
fn concurrent_send_to_v6_shares_one_socket_soundly() {
    let backend = Arc::new(ValidatingBackend::new());
    let local: Ipv6Addr = "2001:db8:77::1".parse().unwrap();
    let socket = Arc::new(
        UdpSocket::bind_with_backend(
            SocketAddr::V6(SocketAddrV6::new(local, 9000, 0, 0)),
            backend.clone() as Arc<dyn PacketBackend>,
        )
        .unwrap(),
    );
    let peer: Ipv6Addr = "2001:db8:77::2".parse().unwrap();
    socket.add_ndp_entry(peer, [0x02, 0, 0, 0, 0, 0x22]);
    run_stress(socket, &backend, SocketAddr::V6(SocketAddrV6::new(peer, 7000, 0, 0)), 25_000);
}

/// Fresh resources per test so the guard counters are not shared with other tests.
fn stub_resources() -> Arc<DpdkResources> {
    init_dpdk_resources(0, 1500).expect("stub DPDK init")
}

fn counts(res: &DpdkResources) -> (u64, u64, u64) {
    (
        res.guard_counts.q_rx0.load(Ordering::Relaxed),
        res.guard_counts.q_tx0.load(Ordering::Relaxed),
        res.guard_counts.q_tx_direct.load(Ordering::Relaxed),
    )
}

#[test]
fn pipeline_send_fn_takes_q_tx0() {
    let res = stub_resources();
    let send = DpdkResources::make_send_fn(Arc::clone(&res));
    let _ = send(&[0u8; 64]); // stub tx_burst sends nothing → WouldBlock, after the guard
    assert_eq!(counts(&res), (0, 1, 0));
}

#[test]
fn pipeline_recv_fn_takes_q_rx0() {
    let res = stub_resources();
    let recv = DpdkResources::make_recv_fn(Arc::clone(&res));
    let pool = FramePool::new(8, 2048);
    let refs = recv(32, &pool).expect("recv_fn");
    assert!(refs.is_empty());
    assert_eq!(counts(&res), (1, 0, 0));
}

#[test]
fn pipeline_direct_send_fn_takes_q_tx_direct() {
    let res = stub_resources();
    let send = DpdkResources::make_direct_send_fn(Arc::clone(&res));
    let _ = send(&[0u8; 64], None);
    assert_eq!(counts(&res), (0, 0, 1));
}

/// The RTC `SocketBackend::Dpdk` arm: `send_to`, `try_recv_from` and a timed-out `recv_from`
/// reach `send_frame`/`rx_burst` directly, so each must take its queue guard. The generic mock
/// never takes these guards, so this is the only coverage of the RTC Dpdk-arm guards.
#[test]
fn rtc_dpdk_arm_takes_queue_guards() {
    let res = stub_resources();
    let socket = UdpSocket::bind_v4_on(SocketAddrV4::new(Ipv4Addr::new(10, 88, 0, 1), 9100), Arc::clone(&res))
        .expect("bind");
    let peer = Ipv4Addr::new(10, 88, 0, 2);
    socket.add_arp_entry(peer, MacAddress::new([0x02, 0, 0, 0, 0, 0x22]));

    // bind() announced itself with a gratuitous ARP through send_frame → q_tx0.
    let (rx0, tx0, _) = counts(&res);
    assert_eq!(rx0, 0);
    assert!(tx0 >= 1, "gratuitous ARP must take q_tx0");

    let err = socket.send_to(b"x", SocketAddr::V4(SocketAddrV4::new(peer, 7000))).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock, "stub tx_burst sends nothing");
    assert_eq!(counts(&res).1, tx0 + 1, "send_to takes q_tx0");

    let mut buf = [0u8; 64];
    assert!(socket.try_recv_from(&mut buf).unwrap().is_none());
    assert_eq!(counts(&res).0, 1, "try_recv_from takes q_rx0");

    socket.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
    assert_eq!(socket.recv_from(&mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(counts(&res).0 > 1, "recv_from takes q_rx0");
}

#[test]
fn nonblocking_send_reports_would_block_when_guards_are_held() {
    let backend = Arc::new(ValidatingBackend::new());
    let socket = UdpSocket::bind_with_backend("10.77.1.1:9000", backend.clone() as Arc<dyn PacketBackend>).unwrap();
    let peer = Ipv4Addr::new(10, 77, 1, 2);
    socket.arp_handler.cache.insert(peer, MacAddress::new([0x02, 0, 0, 0, 0, 0x22]));
    let dst = SocketAddr::V4(SocketAddrV4::new(peer, 7000));
    let payload = vec![1u8; ValidatingBackend::payload_len(1)];

    {
        let _held = lock_or_recover(&socket.tx_state);
        let err = socket.try_send_to_nb(&payload, dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }
    assert_eq!(socket.try_send_to_nb(&payload, dst).unwrap(), payload.len());
}

#[test]
fn nonblocking_recv_reports_would_block_when_rx_guard_is_held() {
    let backend = Arc::new(ValidatingBackend::new());
    let socket = UdpSocket::bind_with_backend("10.77.2.1:9000", backend as Arc<dyn PacketBackend>).unwrap();
    let mut buf = [0u8; 64];

    {
        let _held = lock_or_recover(&socket.rx_state);
        let err = socket.try_recv_from_nb(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }
    assert!(socket.try_recv_from_nb(&mut buf).unwrap().is_none());
}

#[test]
fn nonblocking_recv_drops_and_counts_auto_reply_when_q_tx0_is_busy() {
    let res = stub_resources();
    let local = Ipv4Addr::new(10, 88, 1, 1);
    let socket = UdpSocket::bind_v4_on(SocketAddrV4::new(local, 9100), Arc::clone(&res)).expect("bind");
    let request = arp::build_arp_request(&[0x02, 0, 0, 0, 0, 0x33], Ipv4Addr::new(10, 88, 1, 2), local);

    let mut buf = [0u8; 64];
    let mut result = None;
    {
        let _tx_busy = lock_or_recover(&res.q_tx0);
        let mut rx = lock_or_recover(&socket.rx_state);
        socket.process_frame_zerocopy_locked(&mut rx, false, &request, 9100, &mut buf, &mut result, None);
    }
    assert_eq!(socket.auto_reply_drops(), 1, "reply dropped and counted, not blocked on");

    // A blocking receive waits for the guard instead of dropping the reply.
    let mut rx = lock_or_recover(&socket.rx_state);
    socket.process_frame_zerocopy_locked(&mut rx, true, &request, 9100, &mut buf, &mut result, None);
    assert_eq!(socket.auto_reply_drops(), 1);
}

#[test]
fn has_buffered_packets_tracks_queue_contents() {
    let backend = Arc::new(ValidatingBackend::new());
    let socket = UdpSocket::bind_with_backend("10.77.3.1:9000", backend as Arc<dyn PacketBackend>).unwrap();
    let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 77, 3, 2), 7000));

    {
        let mut rx = lock_or_recover(&socket.rx_state);
        socket.rx_enqueue(&mut rx, vec![1, 2, 3], src, 3);
    }
    assert!(socket.has_buffered_packets.load(Ordering::Acquire));

    let mut buf = [0u8; 8];
    assert_eq!(socket.try_recv_from(&mut buf).unwrap().map(|(n, _)| n), Some(3));
    assert!(!socket.has_buffered_packets.load(Ordering::Acquire), "cleared when drained");
}

#[test]
fn poisoned_guard_is_recovered() {
    let m = Arc::new(Mutex::new(5u32));
    let m2 = Arc::clone(&m);
    let _ = std::thread::spawn(move || {
        let _g = m2.lock().unwrap();
        panic!("poison the lock");
    })
    .join();
    assert!(m.is_poisoned());
    assert_eq!(*lock_or_recover(&m), 5);
    assert_eq!(*try_lock_or_recover(&m).expect("recovered"), 5);
}
