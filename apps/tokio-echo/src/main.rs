//! Async UDP Echo Server with DPDK Support
//!
//! This example demonstrates how to use dpdk-tokio to create an async UDP server
//! that can transparently use either DPDK acceleration or standard Tokio networking.
//!
//! # Usage
//!
//! Standard Tokio mode (default):
//! ```bash
//! cargo run -p tokio-echo
//! ```
//!
//! DPDK-accelerated mode:
//! ```bash
//! cargo run -p tokio-echo --features dpdk
//! ```
//!
//! With custom options:
//! ```bash
//! cargo run -p tokio-echo -- --ip 0.0.0.0 --port 9000 --workers 4
//! ```

use clap::Parser;
use dpdk_tokio::{AsyncUdpSocket, SocketConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

#[derive(Parser, Clone)]
#[command(name = "tokio-echo")]
#[command(about = "Async UDP Echo Server with DPDK support")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Number of concurrent request handlers
    #[arg(long, default_value_t = 100)]
    workers: usize,

    /// Enable verbose logging
    #[arg(long, short)]
    verbose: bool,

    /// Request timeout in milliseconds
    #[arg(long, default_value_t = 5000)]
    timeout: u64,

    /// Force Tokio backend (skip DPDK detection)
    #[arg(long)]
    force_tokio: bool,

    /// Enable statistics reporting
    #[arg(long)]
    stats: bool,

    /// Statistics report interval in seconds
    #[arg(long, default_value_t = 10)]
    stats_interval: u64,

    /// Performance reporting interval in seconds (0 = disabled).
    /// On the DPDK backend this starts the PerfReporter, emitting `[PERF]`
    /// log lines with rx/tx pps, drop counters, and latency percentiles.
    #[arg(long, default_value_t = 0)]
    perf_interval: u64,
}

/// Statistics tracker
struct Stats {
    packets_received: std::sync::atomic::AtomicU64,
    packets_sent: std::sync::atomic::AtomicU64,
    bytes_received: std::sync::atomic::AtomicU64,
    bytes_sent: std::sync::atomic::AtomicU64,
    errors: std::sync::atomic::AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            packets_received: std::sync::atomic::AtomicU64::new(0),
            packets_sent: std::sync::atomic::AtomicU64::new(0),
            bytes_received: std::sync::atomic::AtomicU64::new(0),
            bytes_sent: std::sync::atomic::AtomicU64::new(0),
            errors: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn record_recv(&self, bytes: usize) {
        self.packets_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_send(&self, bytes: usize) {
        self.packets_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn report(&self) -> String {
        format!(
            "rx: {} pkts ({} bytes), tx: {} pkts ({} bytes), errors: {}",
            self.packets_received.load(std::sync::atomic::Ordering::Relaxed),
            self.bytes_received.load(std::sync::atomic::Ordering::Relaxed),
            self.packets_sent.load(std::sync::atomic::Ordering::Relaxed),
            self.bytes_sent.load(std::sync::atomic::Ordering::Relaxed),
            self.errors.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// Build a `host:port` string valid for both IPv4 and IPv6 literals.
/// IPv6 literals must be wrapped in brackets: `[2001:db8::1]:9000`.
fn join_addr(ip: &str, port: u16) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{}]:{}", ip, port)
    } else {
        format!("{}:{}", ip, port)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Create socket with automatic backend selection.
    // join_addr brackets IPv6 literals ([2001:db8::1]:9000) so ToSocketAddrs
    // can parse them — a bare "{}:{}" produces an unparsable v6 string.
    let bind_addr = join_addr(&args.ip, args.port);

    println!("=== Async UDP Echo Server ===");
    println!("Binding to {}", bind_addr);

    let socket: Arc<dyn AsyncUdpSocket> = if args.force_tokio {
        println!("Backend: Tokio (forced)");
        Arc::from(dpdk_tokio::socket::TokioUdpSocket::bind(&bind_addr).await?)
    } else {
        // Use the macro for automatic DPDK detection
        let config = SocketConfig {
            prefer_dpdk: true,
            ..Default::default()
        };
        let boxed = dpdk_tokio::bind_udp_with_config(&bind_addr, config).await?;
        println!("Backend: {}", boxed.backend_name());
        Arc::from(boxed)
    };

    println!("Local address: {}", socket.local_addr()?);
    println!("Max concurrent handlers: {}", args.workers);
    println!("Request timeout: {}ms", args.timeout);

    if args.perf_interval > 0 {
        socket.enable_perf_reporting(Duration::from_secs(args.perf_interval)).await?;
        println!("Perf reporting interval: {}s", args.perf_interval);
    }

    println!();
    println!("Echo server running... (Ctrl+C to stop)");
    println!();

    // Create stats tracker
    let stats = Arc::new(Stats::new());

    // Spawn stats reporter if enabled
    if args.stats {
        let stats_clone = stats.clone();
        let interval = args.stats_interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval));
            loop {
                interval.tick().await;
                println!("[STATS] {}", stats_clone.report());
            }
        });
    }

    // Semaphore for limiting concurrent handlers
    let semaphore = Arc::new(Semaphore::new(args.workers));

    // Install a shutdown signal that fires on SIGTERM / SIGINT. Racing
    // this against recv_from in a tokio::select! lets the main loop exit
    // cleanly on signal, which drops the Arc<dyn AsyncUdpSocket>, which
    // drops the underlying UdpSocket, which drops the PerfReporter — and
    // that Drop impl is what emits the one-shot `[NIC-FINAL]` line the
    // perf harness cross-checks against per-tick [PERF] deltas. Without
    // this, the process is killed by pkill -TERM before any destructor
    // can run and [NIC-FINAL] is never emitted.
    let shutdown = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
            let mut sigint = signal(SignalKind::interrupt())
                .expect("failed to install SIGINT handler");
            tokio::select! {
                _ = sigterm.recv() => {},
                _ = sigint.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    tokio::pin!(shutdown);

    // Main receive loop
    let mut buf = [0u8; 65535];

    loop {
        // Receive a packet, racing against shutdown. On signal we break
        // out of the loop, fall through to the `Shutting down` log, and
        // let main() return so PerfReporter::drop() emits [NIC-FINAL].
        let (len, from_addr) = tokio::select! {
            biased;
            _ = &mut shutdown => {
                println!("Received shutdown signal, stopping echo server...");
                break;
            }
            recv = socket.recv_from(&mut buf) => match recv {
                Ok(result) => result,
                Err(e) => {
                    stats.record_error();
                    if args.verbose {
                        eprintln!("Receive error: {}", e);
                    }
                    continue;
                }
            },
        };

        stats.record_recv(len);

        // Get the data for this packet
        let data = buf[..len].to_vec();

        if args.verbose {
            let msg = String::from_utf8_lossy(&data);
            println!("[RECV] {} bytes from {}: {}", len, from_addr, msg);
        }

        // Clone what we need for the spawned task
        let socket_clone = socket.clone();
        let stats_clone = stats.clone();
        let verbose = args.verbose;
        let timeout_ms = args.timeout;

        // Acquire semaphore permit (limits concurrency)
        let permit = semaphore.clone().acquire_owned().await?;

        // Spawn async handler for response
        tokio::spawn(async move {
            let _permit = permit; // Hold permit until done

            // Create echo response
            let response = format!("echo: {}", String::from_utf8_lossy(&data));

            // Send with timeout
            let send_result = tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                socket_clone.send_to(response.as_bytes(), from_addr),
            )
            .await;

            match send_result {
                Ok(Ok(bytes_sent)) => {
                    stats_clone.record_send(bytes_sent);
                    if verbose {
                        println!("[SEND] {} bytes to {}", bytes_sent, from_addr);
                    }
                }
                Ok(Err(e)) => {
                    stats_clone.record_error();
                    if verbose {
                        eprintln!("[ERROR] Send failed to {}: {}", from_addr, e);
                    }
                }
                Err(_) => {
                    stats_clone.record_error();
                    if verbose {
                        eprintln!("[TIMEOUT] Send to {} timed out", from_addr);
                    }
                }
            }
        });
    }

    // Explicitly stop the PerfReporter before returning. This joins the
    // reporter thread synchronously inside a spawn_blocking and triggers
    // the one-shot `[NIC-FINAL]` stderr line that the perf harness pairs
    // with `[NIC-BASELINE]` for its instrumentation self-check. Doing this
    // explicitly (rather than relying on Drop) removes all timing dependence
    // on Arc refcounts and tokio runtime shutdown ordering.
    if args.perf_interval > 0 {
        socket.disable_perf_reporting().await;
    }
    println!("Shutting down gracefully...");
    Ok(())
}
