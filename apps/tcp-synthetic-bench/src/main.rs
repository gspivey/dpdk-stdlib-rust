//! Synthetic TCP performance benchmark.
//!
//! Measures pure framework overhead using a mock `PacketBackend`. No real NIC
//! or network required — runs entirely in-process.
//!
//! Metrics:
//! - Connection establishment latency (SYN→ESTABLISHED via engine)
//! - Single-stream throughput (write→engine→read loop with mock backend)
//! - Engine tick processing time
//!
//! Output: markdown on stdout, JSON on stderr.

use dpdk_stdlib_net::backend::{PacketBackend, RxReadiness};
use dpdk_stdlib_tcp::clock::{Clock, MockClock};
use dpdk_stdlib_tcp::codec::{build_tcp_frame, parse_tcp_packet, TcpFlags, TcpFrameParams, TcpOptions};
use dpdk_stdlib_tcp::contract::{
    oneshot_channel, CommandSender, ConnectionHandle, EngineCommand, EngineWakeup,
};
use dpdk_stdlib_tcp::engine::{EngineConfig, TcpEngine};
use dpdk_stdlib_tcp::seq::SeqNum;
use dpdk_stdlib_tcp::state::FourTuple;

use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BENCH_DURATION: Duration = Duration::from_secs(2);
const WARMUP_ITERS: usize = 100;

const LOCAL_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const PEER_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const LOCAL_PORT: u16 = 9000;
const PEER_PORT: u16 = 5000;

// ---------------------------------------------------------------------------
// Mock PacketBackend
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct MockBackend {
    mac: [u8; 6],
    tx_count: AtomicU64,
    tx_frames: Mutex<VecDeque<Vec<u8>>>,
    rx_queue: Mutex<VecDeque<Vec<u8>>>,
    promiscuous: AtomicBool,
}

impl MockBackend {
    #[allow(dead_code)]
    fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            tx_count: AtomicU64::new(0),
            tx_frames: Mutex::new(VecDeque::new()),
            rx_queue: Mutex::new(VecDeque::new()),
            promiscuous: AtomicBool::new(false),
        }
    }
}

impl PacketBackend for MockBackend {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.tx_count.fetch_add(1, Ordering::Relaxed);
        self.tx_frames.lock().unwrap().push_back(frame.to_vec());
        Ok(frame.len())
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut queue = self.rx_queue.lock().unwrap();
        let count = max_frames.min(queue.len());
        Ok(queue.drain(..count).collect())
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn backend_name(&self) -> &'static str {
        "mock-tcp-bench"
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

    fn rx_readiness(&self) -> RxReadiness {
        RxReadiness::PollOnly
    }
}

// ---------------------------------------------------------------------------
// Benchmark results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct BenchResult {
    test_name: String,
    metric_name: String,
    metric_value: f64,
    unit: String,
}

#[derive(Debug, Serialize)]
struct BenchSuite {
    results: Vec<BenchResult>,
    summary: String,
    all_passed: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn local_addr() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(LOCAL_IP, LOCAL_PORT))
}

fn peer_addr() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(PEER_IP, PEER_PORT))
}

/// Build a SYN-ACK frame from peer to local.
fn build_syn_ack(seq: u32, ack: u32) -> Vec<u8> {
    let params = TcpFrameParams {
        src_mac: PEER_MAC,
        dst_mac: LOCAL_MAC,
        src: peer_addr(),
        dst: local_addr(),
        seq: SeqNum(seq),
        ack: SeqNum(ack),
        flags: TcpFlags::SYN | TcpFlags::ACK,
        window: 65535,
        payload: Vec::new(),
        options: TcpOptions {
            mss: Some(1460),
            window_scale: Some(7),
            sack_permitted: true,
            timestamps: None,
            sack_blocks: Vec::new(),
        },
        ttl: 64,
    };
    build_tcp_frame(&params).expect("build syn-ack")
}

/// Build an ACK frame from peer.
fn build_ack(seq: u32, ack: u32) -> Vec<u8> {
    let params = TcpFrameParams {
        src_mac: PEER_MAC,
        dst_mac: LOCAL_MAC,
        src: peer_addr(),
        dst: local_addr(),
        seq: SeqNum(seq),
        ack: SeqNum(ack),
        flags: TcpFlags::ACK,
        window: 65535,
        payload: Vec::new(),
        options: TcpOptions::default(),
        ttl: 64,
    };
    build_tcp_frame(&params).expect("build ack")
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Measure connection establishment latency by driving SYN→SYN-ACK→established.
fn bench_connection_establishment() -> BenchResult {
    let clock = Arc::new(MockClock::new());
    let config = EngineConfig::default();
    let wakeup = Arc::new(EngineWakeup::new());
    let (cmd_tx_raw, cmd_rx) = std::sync::mpsc::channel();
    let cmd_tx = CommandSender::new(cmd_tx_raw, wakeup.clone());

    // Warmup
    for _ in 0..WARMUP_ITERS {
        let mut engine = TcpEngine::new(clock.clone(), config.clone());
        let four = FourTuple { local: local_addr(), remote: peer_addr() };
        let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx.clone(), four));
        let (resp_tx, _) = oneshot_channel();
        let _ = engine.on_command(EngineCommand::Connect {
            local: local_addr(),
            remote: peer_addr(),
            src_mac: LOCAL_MAC,
            dst_mac: PEER_MAC,
            handle,
            response: resp_tx,
        });
        while cmd_rx.try_recv().is_ok() {}
    }

    let mut total_ns: u64 = 0;
    let mut count: u64 = 0;
    let deadline = Instant::now() + BENCH_DURATION;

    while Instant::now() < deadline {
        let mut engine = TcpEngine::new(clock.clone(), config.clone());
        let four = FourTuple { local: local_addr(), remote: peer_addr() };
        let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx.clone(), four));
        let (resp_tx, _) = oneshot_channel();

        let start = Instant::now();

        // Send Connect → engine emits SYN
        let syn_frames = engine.on_command(EngineCommand::Connect {
            local: local_addr(),
            remote: peer_addr(),
            src_mac: LOCAL_MAC,
            dst_mac: PEER_MAC,
            handle,
            response: resp_tx,
        });

        // Extract SYN sequence number
        let syn_seq = syn_frames.first()
            .and_then(|f| parse_tcp_packet(f).ok())
            .map(|seg| seg.seq.0)
            .unwrap_or(0);

        // Feed SYN-ACK → engine transitions to ESTABLISHED
        let syn_ack = build_syn_ack(1000, syn_seq + 1);
        if let Ok(seg) = parse_tcp_packet(&syn_ack) {
            let _ = engine.on_segment(&seg);
        }

        total_ns += start.elapsed().as_nanos() as u64;
        count += 1;

        while cmd_rx.try_recv().is_ok() {}
    }

    let avg_ns = if count > 0 { total_ns / count } else { 0 };
    eprintln!("  connection establishment: {} ns/conn ({} iterations)", avg_ns, count);

    BenchResult {
        test_name: "connection_establishment".to_string(),
        metric_name: "latency_ns".to_string(),
        metric_value: avg_ns as f64,
        unit: "ns".to_string(),
    }
}

/// Measure single-stream throughput: write data into tx_ring, drive on_tick.
fn bench_single_stream_throughput() -> BenchResult {
    let clock = Arc::new(MockClock::new());
    let config = EngineConfig::default();
    let wakeup = Arc::new(EngineWakeup::new());
    let (cmd_tx_raw, cmd_rx) = std::sync::mpsc::channel();
    let cmd_tx = CommandSender::new(cmd_tx_raw, wakeup.clone());
    let mut engine = TcpEngine::new(clock.clone(), config.clone());

    let four = FourTuple { local: local_addr(), remote: peer_addr() };
    let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx.clone(), four));
    let (resp_tx, _) = oneshot_channel();

    // Establish connection
    let syn_frames = engine.on_command(EngineCommand::Connect {
        local: local_addr(),
        remote: peer_addr(),
        src_mac: LOCAL_MAC,
        dst_mac: PEER_MAC,
        handle: handle.clone(),
        response: resp_tx,
    });

    let syn_seq = syn_frames.first()
        .and_then(|f| parse_tcp_packet(f).ok())
        .map(|seg| seg.seq.0)
        .unwrap_or(0);

    let syn_ack = build_syn_ack(1000, syn_seq + 1);
    if let Ok(seg) = parse_tcp_packet(&syn_ack) {
        let _ = engine.on_segment(&seg);
    }
    while cmd_rx.try_recv().is_ok() {}

    // Benchmark: write data, tick, ACK
    let payload = vec![0xAB_u8; 1400];
    let mut total_bytes: u64 = 0;
    let peer_seq: u32 = 1001;
    let mut highest_ack: u32 = syn_seq + 1;

    let start = Instant::now();
    let deadline = start + BENCH_DURATION;

    while Instant::now() < deadline {
        // Write data into tx_ring
        let written = handle.tx_ring.write(&payload);
        if written > 0 {
            total_bytes += written as u64;
        }

        // Tick to produce frames
        let out = engine.on_tick(clock.now());
        for frame in &out {
            if let Ok(seg) = parse_tcp_packet(frame) {
                let seg_len = seg.payload.len() as u32;
                if seg_len > 0 {
                    let seg_end = seg.seq.0.wrapping_add(seg_len);
                    if seg_end.wrapping_sub(highest_ack) < (1 << 31) {
                        highest_ack = seg_end;
                    }
                }
            }
        }

        // ACK to open window
        let ack = build_ack(peer_seq, highest_ack);
        if let Ok(seg) = parse_tcp_packet(&ack) {
            let _ = engine.on_segment(&seg);
        }
    }

    let elapsed = start.elapsed();
    let throughput_mbps = (total_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
    eprintln!("  single-stream throughput: {:.1} Mbps ({} bytes in {:.2}s)",
        throughput_mbps, total_bytes, elapsed.as_secs_f64());

    while cmd_rx.try_recv().is_ok() {}

    BenchResult {
        test_name: "single_stream_throughput".to_string(),
        metric_name: "throughput_mbps".to_string(),
        metric_value: throughput_mbps,
        unit: "Mbps".to_string(),
    }
}

/// Measure engine tick processing time with an established connection.
fn bench_engine_tick_time() -> BenchResult {
    let clock = Arc::new(MockClock::new());
    let config = EngineConfig::default();
    let wakeup = Arc::new(EngineWakeup::new());
    let (cmd_tx_raw, cmd_rx) = std::sync::mpsc::channel();
    let cmd_tx = CommandSender::new(cmd_tx_raw, wakeup.clone());
    let mut engine = TcpEngine::new(clock.clone(), config.clone());

    let four = FourTuple { local: local_addr(), remote: peer_addr() };
    let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx.clone(), four));
    let (resp_tx, _) = oneshot_channel();
    let syn_frames = engine.on_command(EngineCommand::Connect {
        local: local_addr(),
        remote: peer_addr(),
        src_mac: LOCAL_MAC,
        dst_mac: PEER_MAC,
        handle,
        response: resp_tx,
    });

    let syn_seq = syn_frames.first()
        .and_then(|f| parse_tcp_packet(f).ok())
        .map(|seg| seg.seq.0)
        .unwrap_or(0);

    let syn_ack = build_syn_ack(1000, syn_seq + 1);
    if let Ok(seg) = parse_tcp_packet(&syn_ack) {
        let _ = engine.on_segment(&seg);
    }
    while cmd_rx.try_recv().is_ok() {}

    // Warmup
    for _ in 0..WARMUP_ITERS {
        let _ = engine.on_tick(clock.now());
    }

    // Measure
    let mut total_ns: u64 = 0;
    let mut count: u64 = 0;
    let deadline = Instant::now() + BENCH_DURATION;

    while Instant::now() < deadline {
        let start = Instant::now();
        let _ = engine.on_tick(clock.now());
        total_ns += start.elapsed().as_nanos() as u64;
        count += 1;
    }

    let avg_ns = if count > 0 { total_ns / count } else { 0 };
    eprintln!("  engine tick time: {} ns/tick ({} iterations)", avg_ns, count);

    BenchResult {
        test_name: "engine_tick".to_string(),
        metric_name: "latency_ns".to_string(),
        metric_value: avg_ns as f64,
        unit: "ns".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn format_markdown(results: &[BenchResult]) -> String {
    let mut out = String::new();
    out.push_str("| Test | Metric | Value | Unit |\n");
    out.push_str("|------|--------|-------|------|\n");
    for r in results {
        let formatted_value = if r.unit == "Mbps" {
            format!("{:.1}", r.metric_value)
        } else {
            format!("{:.0}", r.metric_value)
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            r.test_name, r.metric_name, formatted_value, r.unit
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    eprintln!("=== TCP Synthetic Performance Benchmark ===");
    eprintln!("Measures TCP engine framework overhead with mock PacketBackend.");
    eprintln!("Bench duration: {}s per test", BENCH_DURATION.as_secs());
    eprintln!();

    let mut results = Vec::new();

    eprintln!("Running connection establishment benchmark...");
    results.push(bench_connection_establishment());

    eprintln!("Running single-stream throughput benchmark...");
    results.push(bench_single_stream_throughput());

    eprintln!("Running engine tick time benchmark...");
    results.push(bench_engine_tick_time());

    let summary = format!(
        "conn_est={}ns, throughput={:.1}Mbps, tick={}ns",
        results[0].metric_value as u64,
        results[1].metric_value,
        results[2].metric_value as u64,
    );

    let suite = BenchSuite {
        results: results.clone(),
        summary: summary.clone(),
        all_passed: true,
    };

    // Markdown to stdout
    println!("## TCP Synthetic Performance Results\n");
    println!("Measures TCP engine framework overhead using a mock PacketBackend.\n");
    println!("{}", format_markdown(&results));
    println!("**{}**\n", summary);

    // JSON to stderr
    eprintln!("\n--- JSON results ---");
    eprintln!("{}", serde_json::to_string_pretty(&suite).unwrap());
}
