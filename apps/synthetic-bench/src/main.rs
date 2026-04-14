//! Synthetic performance benchmark: sync DPDK UDP vs Tokio async wrapper.
//!
//! Measures the pure framework overhead by using a mock `PacketBackend` that
//! accepts TX frames into /dev/null and generates synthetic UDP frames for RX.
//! No real NIC or network is needed — this runs entirely in-process.
//!
//! Output: structured JSON + human-readable markdown table suitable for CI
//! comments on pull requests.

use dpdk_udp::{self, PacketBackend, UdpSocket, build_udp_frame};
use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

// Label for the async implementation being benchmarked
const ASYNC_LABEL: &str = "async (std::sync::Mutex + try_recv_from)";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BENCH_DURATION: Duration = Duration::from_secs(2);
const WARMUP_DURATION: Duration = Duration::from_millis(200);
const PAYLOAD_SMALL: usize = 64;
const PAYLOAD_LARGE: usize = 1400;

/// Pre-fill this many frames for RX benchmarks so the recv path never starves.
const RX_PREFILL: usize = 500_000;

/// How many frames to keep in the refill high-water mark during RX bench.
const RX_REFILL_BATCH: usize = 10_000;

// Test network addresses (synthetic — never hit the wire)
const LOCAL_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const PEER_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const LOCAL_PORT: u16 = 9000;
const PEER_PORT: u16 = 9001;

// ---------------------------------------------------------------------------
// SyntheticBackend — mock PacketBackend for benchmarking
// ---------------------------------------------------------------------------

struct SyntheticBackend {
    mac: [u8; 6],
    tx_count: AtomicU64,
    rx_queue: Mutex<VecDeque<Vec<u8>>>,
    promiscuous: AtomicBool,
}

impl SyntheticBackend {
    fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            tx_count: AtomicU64::new(0),
            rx_queue: Mutex::new(VecDeque::new()),
            promiscuous: AtomicBool::new(false),
        }
    }

    fn tx_count(&self) -> u64 {
        self.tx_count.load(Ordering::Relaxed)
    }

    /// Pre-fill the RX queue with valid UDP frames destined for `local_ip:local_port`.
    fn prefill_rx(&self, count: usize, payload_size: usize, local_ip: Ipv4Addr, local_port: u16) {
        let payload = vec![0xABu8; payload_size];
        let frame = build_udp_frame(
            &PEER_MAC,
            &self.mac,
            PEER_IP,
            local_ip,
            PEER_PORT,
            local_port,
            &payload,
            64,
        )
        .expect("build_udp_frame");
        let mut queue = self.rx_queue.lock().unwrap();
        for _ in 0..count {
            queue.push_back(frame.clone());
        }
    }

    fn rx_queue_len(&self) -> usize {
        self.rx_queue.lock().unwrap().len()
    }

    /// Build an ARP reply frame responding to an ARP request.
    fn make_arp_reply(&self, request: &[u8]) -> Vec<u8> {
        // ARP request layout (after 14-byte Ethernet header):
        //   offset 0-1:  hardware type (0x0001)
        //   offset 2-3:  protocol type (0x0800)
        //   offset 4:    hw addr len (6)
        //   offset 5:    proto addr len (4)
        //   offset 6-7:  operation (1=request, 2=reply)
        //   offset 8-13: sender hw addr
        //   offset 14-17: sender proto addr
        //   offset 18-23: target hw addr
        //   offset 24-27: target proto addr
        let mut reply = vec![0u8; 42];

        // Ethernet header: dst=requester MAC, src=our MAC, type=ARP
        reply[0..6].copy_from_slice(&request[6..12]); // dst = sender of request
        reply[6..12].copy_from_slice(&PEER_MAC); // src = our peer MAC
        reply[12..14].copy_from_slice(&[0x08, 0x06]); // EtherType = ARP

        // ARP header
        reply[14..16].copy_from_slice(&[0x00, 0x01]); // HW type: Ethernet
        reply[16..18].copy_from_slice(&[0x08, 0x00]); // Proto type: IPv4
        reply[18] = 6; // HW addr len
        reply[19] = 4; // Proto addr len
        reply[20..22].copy_from_slice(&[0x00, 0x02]); // Operation: Reply

        // Sender = us (the "peer" responding)
        reply[22..28].copy_from_slice(&PEER_MAC);
        reply[28..32].copy_from_slice(&request[38..42]); // target IP from request

        // Target = the requester
        reply[32..38].copy_from_slice(&request[6..12]); // requester's MAC
        reply[38..42].copy_from_slice(&request[28..32]); // requester's IP

        reply
    }
}

impl PacketBackend for SyntheticBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.tx_count.fetch_add(1, Ordering::Relaxed);

        // Auto-reply to ARP requests so the socket's ARP resolution works.
        if frame.len() >= 42 {
            let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
            if ethertype == 0x0806 {
                // ARP
                let arp_op = u16::from_be_bytes([frame[20], frame[21]]);
                if arp_op == 1 {
                    // ARP Request
                    let reply = self.make_arp_reply(frame);
                    self.rx_queue.lock().unwrap().push_back(reply);
                }
            }
        }

        Ok(frame.len())
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut queue = self.rx_queue.lock().unwrap();
        let count = max_frames.min(queue.len());
        if count == 0 {
            return Ok(Vec::new());
        }
        let frames: Vec<_> = queue.drain(..count).collect();
        Ok(frames)
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn backend_name(&self) -> &'static str {
        "synthetic"
    }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        self.promiscuous.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_promiscuous(&self) -> bool {
        self.promiscuous.load(Ordering::Relaxed)
    }

    fn set_allmulticast(&self, _enable: bool) -> io::Result<()> {
        Ok(())
    }

    fn is_allmulticast(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Benchmark harness
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct BenchResult {
    test_name: String,
    payload_bytes: usize,
    sync_pps: u64,
    async_pps: u64,
    ratio: f64,
    sync_ns_per_op: u64,
    async_ns_per_op: u64,
}

#[derive(Debug, Serialize)]
struct BenchSuite {
    results: Vec<BenchResult>,
    summary: String,
    all_passed: bool,
}

/// Run a sync send_to benchmark and return packets-per-second.
fn bench_sync_tx(payload_size: usize) -> u64 {
    let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
    let socket = UdpSocket::bind_with_backend(
        SocketAddr::new(LOCAL_IP.into(), LOCAL_PORT),
        backend.clone(),
    )
    .expect("bind_with_backend");

    let payload = vec![0xCDu8; payload_size];
    let dst = SocketAddr::new(PEER_IP.into(), PEER_PORT);

    // Warmup — first send triggers ARP resolution
    let warmup_end = Instant::now() + WARMUP_DURATION;
    while Instant::now() < warmup_end {
        let _ = socket.send_to(&payload, dst);
    }

    // Reset counter
    backend.tx_count.store(0, Ordering::Relaxed);

    // Timed run
    let start = Instant::now();
    let deadline = start + BENCH_DURATION;
    while Instant::now() < deadline {
        let _ = socket.send_to(&payload, dst);
    }
    let elapsed = start.elapsed();
    let count = backend.tx_count();

    (count as f64 / elapsed.as_secs_f64()) as u64
}

/// Run an async send_to benchmark (new pattern: std::sync::Mutex, direct call) and return PPS.
async fn bench_async_tx(payload_size: usize) -> u64 {
    let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
    let sync_socket = UdpSocket::bind_with_backend(
        SocketAddr::new(LOCAL_IP.into(), LOCAL_PORT),
        backend.clone(),
    )
    .expect("bind_with_backend");

    let payload = vec![0xCDu8; payload_size];
    let dst = SocketAddr::new(PEER_IP.into(), PEER_PORT);

    // Warmup — first send triggers ARP resolution via the sync socket
    let warmup_end = Instant::now() + WARMUP_DURATION;
    while Instant::now() < warmup_end {
        let _ = sync_socket.send_to(&payload, dst);
    }

    // Wrap in std::sync::Mutex — matches the new DpdkUdpSocket pattern
    let socket = Arc::new(std::sync::Mutex::new(sync_socket));

    // Reset counter
    backend.tx_count.store(0, Ordering::Relaxed);

    // Timed run — direct lock + send_to, no spawn_blocking, no buf clone
    let start = Instant::now();
    let deadline = start + BENCH_DURATION;
    while Instant::now() < deadline {
        let _ = socket.lock().unwrap().send_to(&payload, dst);
    }
    let elapsed = start.elapsed();
    let count = backend.tx_count();

    (count as f64 / elapsed.as_secs_f64()) as u64
}

/// Run a sync recv_from benchmark and return packets-per-second.
fn bench_sync_rx(payload_size: usize) -> u64 {
    let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
    let socket = UdpSocket::bind_with_backend(
        SocketAddr::new(LOCAL_IP.into(), LOCAL_PORT),
        backend.clone(),
    )
    .expect("bind_with_backend");

    // Set a short read timeout so recv_from doesn't block forever when queue is empty
    socket
        .set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();

    // Pre-fill RX queue
    backend.prefill_rx(RX_PREFILL, payload_size, LOCAL_IP, LOCAL_PORT);

    let mut buf = [0u8; 2048];

    // Warmup
    let warmup_end = Instant::now() + WARMUP_DURATION;
    while Instant::now() < warmup_end {
        let _ = socket.recv_from(&mut buf);
        // Refill if getting low
        if backend.rx_queue_len() < RX_REFILL_BATCH {
            backend.prefill_rx(RX_REFILL_BATCH, payload_size, LOCAL_IP, LOCAL_PORT);
        }
    }

    let mut count: u64 = 0;

    // Timed run
    let start = Instant::now();
    let deadline = start + BENCH_DURATION;
    while Instant::now() < deadline {
        if socket.recv_from(&mut buf).is_ok() {
            count += 1;
        }
        // Keep queue fed
        if backend.rx_queue_len() < RX_REFILL_BATCH {
            backend.prefill_rx(RX_REFILL_BATCH, payload_size, LOCAL_IP, LOCAL_PORT);
        }
    }
    let elapsed = start.elapsed();

    (count as f64 / elapsed.as_secs_f64()) as u64
}

/// Run an async recv_from benchmark (new pattern: std::sync::Mutex + try_recv_from) and return PPS.
async fn bench_async_rx(payload_size: usize) -> u64 {
    let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
    let sync_socket = UdpSocket::bind_with_backend(
        SocketAddr::new(LOCAL_IP.into(), LOCAL_PORT),
        backend.clone(),
    )
    .expect("bind_with_backend");

    // Pre-fill RX queue
    backend.prefill_rx(RX_PREFILL, payload_size, LOCAL_IP, LOCAL_PORT);

    // Wrap in std::sync::Mutex — matches the new DpdkUdpSocket pattern
    let socket = Arc::new(std::sync::Mutex::new(sync_socket));
    let mut buf = [0u8; 2048];

    // Warmup
    let warmup_end = Instant::now() + WARMUP_DURATION;
    while Instant::now() < warmup_end {
        match socket.lock().unwrap().try_recv_from(&mut buf).unwrap() {
            Some(_) => {}
            None => tokio::task::yield_now().await,
        }
        if backend.rx_queue_len() < RX_REFILL_BATCH {
            backend.prefill_rx(RX_REFILL_BATCH, payload_size, LOCAL_IP, LOCAL_PORT);
        }
    }

    let mut count: u64 = 0;

    // Timed run — direct lock + try_recv_from + yield, no spawn_blocking, no buf clone
    let start = Instant::now();
    let deadline = start + BENCH_DURATION;
    while Instant::now() < deadline {
        match socket.lock().unwrap().try_recv_from(&mut buf).unwrap() {
            Some(_) => count += 1,
            None => tokio::task::yield_now().await,
        }
        if backend.rx_queue_len() < RX_REFILL_BATCH {
            backend.prefill_rx(RX_REFILL_BATCH, payload_size, LOCAL_IP, LOCAL_PORT);
        }
    }
    let elapsed = start.elapsed();

    (count as f64 / elapsed.as_secs_f64()) as u64
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn format_pps(pps: u64) -> String {
    if pps >= 1_000_000 {
        format!("{:.1}M", pps as f64 / 1_000_000.0)
    } else if pps >= 1_000 {
        format!("{:.1}K", pps as f64 / 1_000.0)
    } else {
        format!("{}", pps)
    }
}

fn format_markdown_table(results: &[BenchResult]) -> String {
    let mut out = String::new();
    out.push_str("| Test | Payload | Sync PPS | Async PPS | Ratio (sync/async) | Async ns/op |\n");
    out.push_str("|------|---------|----------|-----------|-------------------|-------------|\n");
    for r in results {
        out.push_str(&format!(
            "| {} | {}B | {} | {} | {:.1}x | {} |\n",
            r.test_name,
            r.payload_bytes,
            format_pps(r.sync_pps),
            format_pps(r.async_pps),
            r.ratio,
            r.async_ns_per_op,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    eprintln!("=== Synthetic UDP Performance Benchmark ===");
    eprintln!(
        "Comparing sync dpdk_udp::UdpSocket vs Tokio async wrapper overhead"
    );
    eprintln!(
        "Bench duration: {}s per test, warmup: {}ms",
        BENCH_DURATION.as_secs(),
        WARMUP_DURATION.as_millis()
    );
    eprintln!();

    let mut results = Vec::new();

    for &payload_size in &[PAYLOAD_SMALL, PAYLOAD_LARGE] {
        // --- TX benchmark ---
        eprintln!(
            "Running TX send_to benchmark ({}B payload)...",
            payload_size
        );

        let sync_tx = bench_sync_tx(payload_size);
        let async_tx = bench_async_tx(payload_size).await;
        let ratio_tx = if async_tx > 0 {
            sync_tx as f64 / async_tx as f64
        } else {
            f64::INFINITY
        };

        eprintln!(
            "  sync: {} pps, async: {} pps, ratio: {:.1}x",
            format_pps(sync_tx),
            format_pps(async_tx),
            ratio_tx
        );

        results.push(BenchResult {
            test_name: "TX send_to".to_string(),
            payload_bytes: payload_size,
            sync_pps: sync_tx,
            async_pps: async_tx,
            ratio: ratio_tx,
            sync_ns_per_op: if sync_tx > 0 {
                1_000_000_000 / sync_tx
            } else {
                0
            },
            async_ns_per_op: if async_tx > 0 {
                1_000_000_000 / async_tx
            } else {
                0
            },
        });

        // --- RX benchmark ---
        eprintln!(
            "Running RX recv_from benchmark ({}B payload)...",
            payload_size
        );

        let sync_rx = bench_sync_rx(payload_size);
        let async_rx = bench_async_rx(payload_size).await;
        let ratio_rx = if async_rx > 0 {
            sync_rx as f64 / async_rx as f64
        } else {
            f64::INFINITY
        };

        eprintln!(
            "  sync: {} pps, async: {} pps, ratio: {:.1}x",
            format_pps(sync_rx),
            format_pps(async_rx),
            ratio_rx
        );

        results.push(BenchResult {
            test_name: "RX recv_from".to_string(),
            payload_bytes: payload_size,
            sync_pps: sync_rx,
            async_pps: async_rx,
            ratio: ratio_rx,
            sync_ns_per_op: if sync_rx > 0 {
                1_000_000_000 / sync_rx
            } else {
                0
            },
            async_ns_per_op: if async_rx > 0 {
                1_000_000_000 / async_rx
            } else {
                0
            },
        });
    }

    // Compute summary
    let worst_ratio = results
        .iter()
        .map(|r| r.ratio)
        .fold(0.0f64, f64::max);
    let avg_ratio =
        results.iter().map(|r| r.ratio).sum::<f64>() / results.len() as f64;

    let all_passed = true; // No regression threshold yet — informational only

    let summary = format!(
        "Avg sync/async ratio: {:.1}x, worst: {:.1}x",
        avg_ratio, worst_ratio
    );

    let suite = BenchSuite {
        results: results.clone(),
        summary: summary.clone(),
        all_passed,
    };

    // Print markdown table to stdout (for CI to capture)
    println!("## Synthetic UDP Performance Results\n");
    println!(
        "Measures framework overhead: sync `dpdk_udp::UdpSocket` vs {}.\n",
        ASYNC_LABEL,
    );
    println!("{}", format_markdown_table(&results));
    println!("**{}**\n", summary);
    if worst_ratio > 5.0 {
        println!(
            "> **Warning:** Async wrapper is {:.0}x slower than sync on the worst case. \
             Investigate framework overhead in the Tokio wrapper.",
            worst_ratio
        );
    } else if worst_ratio < 2.0 {
        println!(
            "> **Good:** Async wrapper is within {:.1}x of sync — minimal framework overhead.",
            worst_ratio
        );
    }

    // Print JSON to stderr for machine consumption
    eprintln!("\n--- JSON results ---");
    eprintln!("{}", serde_json::to_string_pretty(&suite).unwrap());

    if !all_passed {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Tests — lightweight smoke tests to ensure the harness works
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_synthetic_backend_send() {
        let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
        let frame = vec![0u8; 64];
        backend.send_frame(&frame).unwrap();
        assert_eq!(backend.tx_count(), 1);
    }

    #[test]
    fn test_synthetic_backend_arp_reply() {
        let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));

        // Build a minimal ARP request
        let mut arp_req = vec![0u8; 42];
        // Ethernet: dst=broadcast, src=LOCAL_MAC, type=ARP
        arp_req[0..6].copy_from_slice(&[0xff; 6]);
        arp_req[6..12].copy_from_slice(&LOCAL_MAC);
        arp_req[12..14].copy_from_slice(&[0x08, 0x06]);
        // ARP: HW=Ethernet, Proto=IPv4, HWLen=6, ProtoLen=4, Op=Request
        arp_req[14..16].copy_from_slice(&[0x00, 0x01]);
        arp_req[16..18].copy_from_slice(&[0x08, 0x00]);
        arp_req[18] = 6;
        arp_req[19] = 4;
        arp_req[20..22].copy_from_slice(&[0x00, 0x01]); // request
        arp_req[22..28].copy_from_slice(&LOCAL_MAC);
        arp_req[28..32].copy_from_slice(&LOCAL_IP.octets());
        arp_req[32..38].copy_from_slice(&[0x00; 6]);
        arp_req[38..42].copy_from_slice(&PEER_IP.octets());

        backend.send_frame(&arp_req).unwrap();

        // Should have an ARP reply in the rx queue
        let frames = backend.recv_frames(10).unwrap();
        assert_eq!(frames.len(), 1);
        let reply = &frames[0];
        assert_eq!(u16::from_be_bytes([reply[20], reply[21]]), 2); // ARP reply
    }

    #[test]
    fn test_synthetic_backend_prefill_rx() {
        let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
        backend.prefill_rx(100, 64, LOCAL_IP, LOCAL_PORT);
        assert_eq!(backend.rx_queue_len(), 100);

        let frames = backend.recv_frames(50).unwrap();
        assert_eq!(frames.len(), 50);
        assert_eq!(backend.rx_queue_len(), 50);
    }

    #[test]
    fn test_sync_socket_with_synthetic_backend() {
        let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
        let socket = UdpSocket::bind_with_backend(
            SocketAddr::new(LOCAL_IP.into(), LOCAL_PORT),
            backend.clone(),
        )
        .expect("bind");

        let payload = b"hello synthetic";
        let dst = SocketAddr::new(PEER_IP.into(), PEER_PORT);

        // send_to triggers ARP, which the backend auto-replies to
        let sent = socket.send_to(payload, dst).unwrap();
        assert_eq!(sent, payload.len());
        // ARP request + gratuitous ARP + data packet
        assert!(backend.tx_count() >= 1);
    }

    #[test]
    fn test_sync_socket_recv_from_synthetic() {
        let backend = Arc::new(SyntheticBackend::new(LOCAL_MAC));
        let socket = UdpSocket::bind_with_backend(
            SocketAddr::new(LOCAL_IP.into(), LOCAL_PORT),
            backend.clone(),
        )
        .expect("bind");

        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();

        // Pre-fill with one frame
        backend.prefill_rx(1, 64, LOCAL_IP, LOCAL_PORT);

        let mut buf = [0u8; 2048];
        let (len, addr) = socket.recv_from(&mut buf).unwrap();
        assert_eq!(len, 64);
        assert_eq!(addr.ip(), std::net::IpAddr::V4(PEER_IP));
        assert_eq!(addr.port(), PEER_PORT);
    }
}
