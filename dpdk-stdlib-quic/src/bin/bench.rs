//! Two-way QUIC benchmark: stock s2n-quic vs native DPDK provider.
//!
//! CLI: quic-bench --provider=stock|native-dpdk --duration=<secs> --streams=<n> --payload-size=<bytes>
//!
//! Both providers run the same workload: client opens N streams, sends
//! payload_size bytes on each, server echoes. Reports throughput (Gbps),
//! PPS, handshake latency P50/P99, and provider stats counters.
//!
//! Compiles in stub mode (won't produce real traffic without DPDK).

use dpdk_stdlib_quic::DpdkProvider;
use dpdk_udp::{PacketBackend, RxReadiness};
use s2n_quic::{client::Connect, Client, Server};
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// --- CLI argument parsing (no external dep) ---

#[derive(Debug, Clone)]
struct Args {
    provider: ProviderKind,
    duration_secs: u64,
    streams: usize,
    payload_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Stock,
    NativeDpdk,
}

fn parse_args() -> Args {
    let mut provider = ProviderKind::Stock;
    let mut duration_secs = 10u64;
    let mut streams = 1usize;
    let mut payload_size = 1_048_576usize; // 1 MB

    for arg in std::env::args().skip(1) {
        if let Some(val) = arg.strip_prefix("--provider=") {
            provider = match val {
                "stock" => ProviderKind::Stock,
                "native-dpdk" => ProviderKind::NativeDpdk,
                _ => {
                    eprintln!("Unknown provider: {val}. Use 'stock' or 'native-dpdk'.");
                    std::process::exit(1);
                }
            };
        } else if let Some(val) = arg.strip_prefix("--duration=") {
            duration_secs = val.parse().unwrap_or_else(|_| {
                eprintln!("Invalid --duration value: {val}");
                std::process::exit(1);
            });
        } else if let Some(val) = arg.strip_prefix("--streams=") {
            streams = val.parse().unwrap_or_else(|_| {
                eprintln!("Invalid --streams value: {val}");
                std::process::exit(1);
            });
        } else if let Some(val) = arg.strip_prefix("--payload-size=") {
            payload_size = val.parse().unwrap_or_else(|_| {
                eprintln!("Invalid --payload-size value: {val}");
                std::process::exit(1);
            });
        } else {
            eprintln!("Unknown argument: {arg}");
            eprintln!("Usage: quic-bench --provider=stock|native-dpdk --duration=<secs> --streams=<n> --payload-size=<bytes>");
            std::process::exit(1);
        }
    }

    Args {
        provider,
        duration_secs,
        streams,
        payload_size,
    }
}

// --- Paired loopback backend for native-dpdk provider ---

struct PairedLoopback {
    rx_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    tx_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    mac: [u8; 6],
    promiscuous: AtomicBool,
    allmulticast: AtomicBool,
}

impl PairedLoopback {
    fn new_pair() -> (Arc<Self>, Arc<Self>) {
        let q1 = Arc::new(Mutex::new(VecDeque::new()));
        let q2 = Arc::new(Mutex::new(VecDeque::new()));

        let a = Arc::new(Self {
            rx_queue: Arc::clone(&q1),
            tx_queue: Arc::clone(&q2),
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            promiscuous: AtomicBool::new(false),
            allmulticast: AtomicBool::new(false),
        });

        let b = Arc::new(Self {
            rx_queue: q2,
            tx_queue: q1,
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            promiscuous: AtomicBool::new(false),
            allmulticast: AtomicBool::new(false),
        });

        (a, b)
    }
}

impl PacketBackend for PairedLoopback {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.tx_queue.lock().unwrap().push_back(frame.to_vec());
        Ok(frame.len())
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut q = self.rx_queue.lock().unwrap();
        let n = max_frames.min(q.len());
        Ok(q.drain(..n).collect())
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn backend_name(&self) -> &'static str {
        "paired-loopback"
    }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        self.promiscuous.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_promiscuous(&self) -> bool {
        self.promiscuous.load(Ordering::Relaxed)
    }

    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        self.allmulticast.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_allmulticast(&self) -> bool {
        self.allmulticast.load(Ordering::Relaxed)
    }

    fn rx_readiness(&self) -> RxReadiness {
        RxReadiness::PollOnly
    }
}

// --- TLS configuration ---

fn generate_tls_pair() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen cert generation failed");
    (cert.cert.pem(), cert.key_pair.serialize_pem())
}

// --- Benchmark results ---

#[derive(Debug)]
struct BenchResult {
    total_bytes: u64,
    elapsed: Duration,
    handshake_latencies_us: Vec<u64>,
    provider_stats: Option<dpdk_stdlib_quic::stats::StatsSnapshot>,
}

impl BenchResult {
    fn throughput_gbps(&self) -> f64 {
        let bits = self.total_bytes as f64 * 8.0;
        bits / self.elapsed.as_secs_f64() / 1_000_000_000.0
    }

    fn pps(&self) -> f64 {
        // Approximate: each 1472-byte UDP payload is one packet
        let packets = self.total_bytes as f64 / 1472.0;
        packets / self.elapsed.as_secs_f64()
    }

    fn handshake_p50_us(&self) -> u64 {
        percentile(&self.handshake_latencies_us, 50)
    }

    fn handshake_p99_us(&self) -> u64 {
        percentile(&self.handshake_latencies_us, 99)
    }
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (pct * sorted.len() / 100).min(sorted.len() - 1);
    sorted[idx]
}

// --- Stock provider benchmark ---

async fn run_stock_benchmark(args: &Args) -> BenchResult {
    let (cert_pem, key_pem) = generate_tls_pair();
    let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut server = Server::builder()
        .with_tls((cert_pem.as_str(), key_pem.as_str()))
        .unwrap()
        .with_io(server_addr)
        .unwrap()
        .start()
        .unwrap();

    let server_port = server.local_addr().unwrap().port();
    let actual_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().unwrap();

    let client = Client::builder()
        .with_tls(cert_pem.as_str())
        .unwrap()
        .with_io("0.0.0.0:0".parse::<SocketAddr>().unwrap())
        .unwrap()
        .start()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);
    let payload = vec![0xABu8; args.payload_size];
    let mut total_bytes = 0u64;
    let mut handshake_latencies = Vec::new();
    let streams_per_conn = args.streams;
    let start = Instant::now();

    // Server task: accept connections and echo streams
    let server_task = tokio::spawn(async move {
        while let Some(mut conn) = server.accept().await {
            tokio::spawn(async move {
                while let Ok(Some(mut stream)) = conn.accept_bidirectional_stream().await {
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        while let Ok(Some(chunk)) = stream.receive().await {
                            buf.extend_from_slice(&chunk);
                        }
                        let _ = stream.send(bytes::Bytes::from(buf)).await;
                        let _ = stream.finish();
                    });
                }
            });
        }
    });

    // Client: repeatedly connect and stream until deadline
    while Instant::now() < deadline {
        let hs_start = Instant::now();
        let connect = Connect::new(actual_addr).with_server_name("localhost");
        let mut connection = match client.connect(connect).await {
            Ok(c) => c,
            Err(_) => break,
        };
        let hs_elapsed = hs_start.elapsed();
        handshake_latencies.push(hs_elapsed.as_micros() as u64);

        let mut stream_handles = Vec::new();
        for _ in 0..streams_per_conn {
            let mut stream = match connection.open_bidirectional_stream().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let payload_clone = payload.clone();
            stream_handles.push(tokio::spawn(async move {
                let _ = stream
                    .send(bytes::Bytes::from(payload_clone))
                    .await;
                let _ = stream.finish();
                let mut received = 0u64;
                while let Ok(Some(chunk)) = stream.receive().await {
                    received += chunk.len() as u64;
                }
                received
            }));
        }

        for h in stream_handles {
            if let Ok(bytes) = h.await {
                total_bytes += bytes;
            }
        }
    }

    let elapsed = start.elapsed();
    server_task.abort();

    handshake_latencies.sort_unstable();

    BenchResult {
        total_bytes,
        elapsed,
        handshake_latencies_us: handshake_latencies,
        provider_stats: None,
    }
}

// --- Native DPDK provider benchmark ---

async fn run_native_dpdk_benchmark(args: &Args) -> BenchResult {
    let (server_backend, client_backend) = PairedLoopback::new_pair();
    let (cert_pem, key_pem) = generate_tls_pair();

    let server_addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
    let client_addr: SocketAddr = "10.0.0.2:5000".parse().unwrap();

    let server_mac = server_backend.mac_address();
    let client_mac = client_backend.mac_address();

    let (server_provider, mut server_handle) = DpdkProvider::builder()
        .with_addr(server_addr)
        .with_gateway_mac(client_mac)
        .with_backend(server_backend as Arc<dyn PacketBackend>)
        .build();

    let (client_provider, mut client_handle) = DpdkProvider::builder()
        .with_addr(client_addr)
        .with_gateway_mac(server_mac)
        .with_backend(client_backend as Arc<dyn PacketBackend>)
        .build();

    let mut server = Server::builder()
        .with_tls((cert_pem.as_str(), key_pem.as_str()))
        .unwrap()
        .with_io(server_provider)
        .unwrap()
        .start()
        .unwrap();

    let client = Client::builder()
        .with_tls(cert_pem.as_str())
        .unwrap()
        .with_io(client_provider)
        .unwrap()
        .start()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);
    let payload = vec![0xABu8; args.payload_size];
    let mut total_bytes = 0u64;
    let mut handshake_latencies = Vec::new();
    let streams_per_conn = args.streams;
    let start = Instant::now();

    // Server task
    let server_task = tokio::spawn(async move {
        while let Some(mut conn) = server.accept().await {
            tokio::spawn(async move {
                while let Ok(Some(mut stream)) = conn.accept_bidirectional_stream().await {
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        while let Ok(Some(chunk)) = stream.receive().await {
                            buf.extend_from_slice(&chunk);
                        }
                        let _ = stream.send(bytes::Bytes::from(buf)).await;
                        let _ = stream.finish();
                    });
                }
            });
        }
    });

    // Client: connect and stream until deadline
    while Instant::now() < deadline {
        let hs_start = Instant::now();
        let connect = Connect::new(server_addr).with_server_name("localhost");
        let mut connection = match client.connect(connect).await {
            Ok(c) => c,
            Err(_) => break,
        };
        let hs_elapsed = hs_start.elapsed();
        handshake_latencies.push(hs_elapsed.as_micros() as u64);

        let mut stream_handles = Vec::new();
        for _ in 0..streams_per_conn {
            let mut stream = match connection.open_bidirectional_stream().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let payload_clone = payload.clone();
            stream_handles.push(tokio::spawn(async move {
                let _ = stream
                    .send(bytes::Bytes::from(payload_clone))
                    .await;
                let _ = stream.finish();
                let mut received = 0u64;
                while let Ok(Some(chunk)) = stream.receive().await {
                    received += chunk.len() as u64;
                }
                received
            }));
        }

        for h in stream_handles {
            if let Ok(bytes) = h.await {
                total_bytes += bytes;
            }
        }
    }

    let elapsed = start.elapsed();
    server_task.abort();

    let stats = client_handle.stats();
    server_handle.shutdown();
    client_handle.shutdown();

    handshake_latencies.sort_unstable();

    BenchResult {
        total_bytes,
        elapsed,
        handshake_latencies_us: handshake_latencies,
        provider_stats: Some(stats),
    }
}

// --- Report ---

fn print_report(args: &Args, result: &BenchResult) {
    println!("=== QUIC Benchmark Results ===");
    println!("Provider:      {:?}", args.provider);
    println!("Duration:      {} s", args.duration_secs);
    println!("Streams/conn:  {}", args.streams);
    println!("Payload size:  {} bytes", args.payload_size);
    println!();
    println!("--- Throughput ---");
    println!("Total bytes:   {}", result.total_bytes);
    println!("Elapsed:       {:.3} s", result.elapsed.as_secs_f64());
    println!("Throughput:    {:.4} Gbps", result.throughput_gbps());
    println!("PPS (approx):  {:.0}", result.pps());
    println!();
    println!("--- Handshake Latency ---");
    println!("Connections:   {}", result.handshake_latencies_us.len());
    println!("P50:           {} µs", result.handshake_p50_us());
    println!("P99:           {} µs", result.handshake_p99_us());

    if let Some(ref stats) = result.provider_stats {
        println!();
        println!("--- Provider Stats ---");
        println!("RX burst calls:       {}", stats.rx_burst_calls);
        println!("TX burst calls:       {}", stats.tx_burst_calls);
        println!("Datagrams received:   {}", stats.datagrams_received);
        println!("Datagrams transmitted:{}", stats.datagrams_transmitted);
        println!("RX drops:             {}", stats.rx_drops);
        println!("TX drops:             {}", stats.tx_drops);
        println!("Timer wakeups:        {}", stats.timer_wakeups);
    }
}

// --- Main ---

#[tokio::main]
async fn main() {
    let args = parse_args();

    println!("Starting QUIC benchmark: provider={:?}, duration={}s, streams={}, payload={}B",
        args.provider, args.duration_secs, args.streams, args.payload_size);
    println!();

    let result = match args.provider {
        ProviderKind::Stock => run_stock_benchmark(&args).await,
        ProviderKind::NativeDpdk => run_native_dpdk_benchmark(&args).await,
    };

    print_report(&args, &result);
}
