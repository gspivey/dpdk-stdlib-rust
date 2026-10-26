//! Async UDP Echo Server with DPDK Support
//!
//! This example demonstrates how to use dpdk-tokio to create an async UDP server
//! that can transparently use either DPDK acceleration or standard Tokio networking.
//!
//! # Modes
//!
//! The server supports two per-packet echo patterns, selected with `--mode`:
//!
//! * `fair` (default) — receive a datagram and send the same bytes back to the
//!   source address within the **same task**: no `tokio::spawn`, no `Semaphore`,
//!   and no `tokio::time::timeout` per packet. This measures the library's
//!   fair-path ceiling (`apps/echo`-style) on the async runtime.
//! * `concurrent` — the per-packet "spawn" pattern a drop-in `tokio::net::UdpSocket`
//!   user naturally writes: copy the payload, acquire a `Semaphore` permit,
//!   `tokio::spawn` a handler, and send under a `tokio::time::timeout`. It echoes
//!   the **exact received bytes** (it does not transform the payload). `--workers`
//!   (the permit count) and `--timeout` apply only in this mode.
//!
//! # Usage
//!
//! Standard Tokio mode (default backend auto-detect):
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
//! cargo run -p tokio-echo -- --ip 0.0.0.0 --port 9000 --mode concurrent --workers 4
//! ```

use clap::{Parser, ValueEnum};
use dpdk_tokio::{AsyncUdpSocket, SocketConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Per-packet echo pattern.
///
/// `clap`'s `ValueEnum` derive makes `--mode` accept exactly `fair` or
/// `concurrent` and reject any other value with a non-zero exit and a clear
/// error message (Req 1.3) — the rejection happens during `Args::parse()`,
/// before the tokio runtime does any work.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
enum Mode {
    /// Receive then send the same bytes in a single task (no spawn/semaphore/timeout).
    Fair,
    /// Per-packet spawn pattern: copy + permit + spawn + timeout, echoing exact bytes.
    Concurrent,
}

impl Mode {
    /// Stable label for the startup line the perf/DUT logs record (Req 1.6).
    fn as_str(self) -> &'static str {
        match self {
            Mode::Fair => "fair",
            Mode::Concurrent => "concurrent",
        }
    }
}

#[derive(Parser, Clone, Debug)]
#[command(name = "tokio-echo")]
#[command(about = "Async UDP Echo Server with DPDK support")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Per-packet echo pattern: `fair` (default) or `concurrent`.
    #[arg(long, value_enum, default_value_t = Mode::Fair)]
    mode: Mode,

    /// Number of concurrent request handlers (concurrent mode only).
    #[arg(long, default_value_t = 100)]
    workers: usize,

    /// Enable verbose logging
    #[arg(long, short)]
    verbose: bool,

    /// Request timeout in milliseconds (concurrent mode only).
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

    /// Opt-in idle backoff for the DPDK recv path, in milliseconds
    /// (flag absent = OFF, the default 0.3.0 busy-spin behaviour).
    ///
    /// PR 1 only parses this flag and prints it at startup; it has **no
    /// behavioural effect** yet. The `set_idle_backoff` method it will drive
    /// lands in PR 5, so PR 1 deliberately does not reference it.
    #[arg(long)]
    idle_backoff_ms: Option<u64>,
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

/// `fair` echo: send the received bytes straight back from the current task.
///
/// No `to_vec`, no `tokio::spawn`, no `Semaphore`, no `tokio::time::timeout`
/// (Req 1.1). When `verbose` is false this performs **no** `from_utf8_lossy`
/// or `format!` on the payload, so the echoed bytes equal the received bytes
/// exactly (Req 1.4). The optional `from_utf8_lossy` logging is gated behind
/// `verbose` (Req 1.5).
async fn echo_fair(
    socket: &dyn AsyncUdpSocket,
    payload: &[u8],
    from_addr: SocketAddr,
    stats: &Stats,
    verbose: bool,
) {
    if verbose {
        let msg = String::from_utf8_lossy(payload);
        println!("[RECV] {} bytes from {}: {}", payload.len(), from_addr, msg);
    }
    match socket.send_to(payload, from_addr).await {
        Ok(n) => {
            stats.record_send(n);
            if verbose {
                println!("[SEND] {} bytes to {}", n, from_addr);
            }
        }
        Err(e) => {
            stats.record_error();
            if verbose {
                eprintln!("[ERROR] Send failed to {}: {}", from_addr, e);
            }
        }
    }
}

/// `concurrent` echo handler body: the per-packet "spawn" workload.
///
/// Echoes the **exact received bytes** (`data`) — it does not `format!` or
/// otherwise transform the payload (Req 1.2) — and sends under a
/// `tokio::time::timeout`. `verbose` gates all logging (Req 1.4/1.5).
///
/// The caller is responsible for the `Semaphore` permit and the `tokio::spawn`;
/// this function is the future that runs inside the spawned task.
async fn echo_concurrent_handler(
    socket: Arc<dyn AsyncUdpSocket>,
    data: Vec<u8>,
    from_addr: SocketAddr,
    stats: Arc<Stats>,
    timeout_ms: u64,
    verbose: bool,
) {
    let send_result = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        socket.send_to(&data, from_addr),
    )
    .await;

    match send_result {
        Ok(Ok(bytes_sent)) => {
            stats.record_send(bytes_sent);
            if verbose {
                println!("[SEND] {} bytes to {}", bytes_sent, from_addr);
            }
        }
        Ok(Err(e)) => {
            stats.record_error();
            if verbose {
                eprintln!("[ERROR] Send failed to {}: {}", from_addr, e);
            }
        }
        Err(_) => {
            stats.record_error();
            if verbose {
                eprintln!("[TIMEOUT] Send to {} timed out", from_addr);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `--mode` is parsed here; an unknown value exits non-zero with a clear
    // message before any runtime work happens (Req 1.3).
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
    // mode=<fair|concurrent> is the one-line identifier the perf/DUT logs
    // record so the harness knows which pattern produced the numbers (Req 1.6).
    println!("mode={}", args.mode.as_str());
    if args.mode == Mode::Concurrent {
        println!("Max concurrent handlers: {}", args.workers);
        println!("Request timeout: {}ms", args.timeout);
    }
    // PR 1 parses --idle-backoff-ms and only reports it; the backoff itself is
    // wired in PR 5 (it needs set_idle_backoff, which does not exist yet).
    match args.idle_backoff_ms {
        Some(ms) => println!("Idle backoff: {}ms (parsed; no effect until a later release)", ms),
        None => println!("Idle backoff: off"),
    }

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

    // Semaphore for limiting concurrent handlers (concurrent mode only).
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

        match args.mode {
            Mode::Fair => {
                // Fair: echo the received bytes directly, in this task. No
                // allocation, no spawn, no semaphore, no timeout (Req 1.1).
                echo_fair(socket.as_ref(), &buf[..len], from_addr, &stats, args.verbose).await;
            }
            Mode::Concurrent => {
                // Concurrent: the E2 "spawn" pattern. Copy the payload, take a
                // permit to bound concurrency, and spawn a handler that echoes
                // the exact bytes under a timeout (Req 1.2).
                let data = buf[..len].to_vec();

                if args.verbose {
                    let msg = String::from_utf8_lossy(&data);
                    println!("[RECV] {} bytes from {}: {}", len, from_addr, msg);
                }

                let socket_clone = socket.clone();
                let stats_clone = stats.clone();
                let verbose = args.verbose;
                let timeout_ms = args.timeout;

                // Acquire semaphore permit (limits concurrency).
                let permit = semaphore.clone().acquire_owned().await?;

                tokio::spawn(async move {
                    let _permit = permit; // Hold permit until done.
                    echo_concurrent_handler(
                        socket_clone,
                        data,
                        from_addr,
                        stats_clone,
                        timeout_ms,
                        verbose,
                    )
                    .await;
                });
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use dpdk_tokio::socket::TokioUdpSocket;

    #[test]
    fn mode_defaults_to_fair() {
        let args = Args::parse_from(["tokio-echo"]);
        assert_eq!(args.mode, Mode::Fair);
    }

    #[test]
    fn mode_accepts_fair_and_concurrent() {
        assert_eq!(
            Args::parse_from(["tokio-echo", "--mode", "fair"]).mode,
            Mode::Fair
        );
        assert_eq!(
            Args::parse_from(["tokio-echo", "--mode", "concurrent"]).mode,
            Mode::Concurrent
        );
    }

    #[test]
    fn mode_rejects_unknown_value() {
        // try_parse_from mirrors what Args::parse() does at startup; an unknown
        // --mode must be an Err (which main() surfaces as a non-zero exit),
        // not a silent default (Req 1.3).
        let err = Args::try_parse_from(["tokio-echo", "--mode", "bogus"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn idle_backoff_ms_is_optional_and_parsed() {
        assert_eq!(Args::parse_from(["tokio-echo"]).idle_backoff_ms, None);
        assert_eq!(
            Args::parse_from(["tokio-echo", "--idle-backoff-ms", "5"]).idle_backoff_ms,
            Some(5)
        );
    }

    #[test]
    fn existing_flags_still_parse() {
        let args = Args::parse_from([
            "tokio-echo",
            "--ip",
            "127.0.0.1",
            "--port",
            "19001",
            "--workers",
            "8",
            "--timeout",
            "1234",
            "--force-tokio",
            "--stats",
            "--stats-interval",
            "3",
            "--perf-interval",
            "2",
            "--verbose",
        ]);
        assert_eq!(args.ip, "127.0.0.1");
        assert_eq!(args.port, 19001);
        assert_eq!(args.workers, 8);
        assert_eq!(args.timeout, 1234);
        assert!(args.force_tokio);
        assert!(args.stats);
        assert_eq!(args.stats_interval, 3);
        assert_eq!(args.perf_interval, 2);
        assert!(args.verbose);
    }

    #[test]
    fn clap_definition_is_valid() {
        // Catches duplicate/ambiguous arg definitions at test time.
        Args::command().debug_assert();
    }

    /// Bind a loopback `TokioUdpSocket` (the `--force-tokio` backend) and a
    /// loopback client, returning (server, client, server_addr).
    async fn loopback_pair() -> (TokioUdpSocket, tokio::net::UdpSocket, SocketAddr) {
        let server = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server_addr).await.unwrap();
        (server, client, server_addr)
    }

    /// Fair mode echoes the exact received bytes over loopback on the tokio
    /// backend (Req 1.1, 1.4). The payload includes non-UTF-8 bytes to prove
    /// no `format!`/`from_utf8_lossy` transform happens on the hot path.
    #[tokio::test]
    async fn fair_echoes_exact_bytes_over_loopback() {
        let (server, client, _addr) = loopback_pair().await;
        let stats = Stats::new();

        let payload: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF, b'h', b'i'];
        client.send(payload).await.unwrap();

        // Server side: one receive + fair echo.
        let mut buf = [0u8; 2048];
        let (len, from) = server.recv_from(&mut buf).await.unwrap();
        echo_fair(&server, &buf[..len], from, &stats, false).await;

        // Client reads the echo back; it must be byte-identical.
        let mut echoed = [0u8; 2048];
        let n = client.recv(&mut echoed).await.unwrap();
        assert_eq!(&echoed[..n], payload, "fair must echo exact bytes");
        assert_eq!(stats.packets_sent.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Concurrent mode echoes the exact received bytes (no `echo: ` prefix)
    /// over loopback on the tokio backend (Req 1.2). This is the regression
    /// guard for the old `format!("echo: {}", ..)` behaviour.
    #[tokio::test]
    async fn concurrent_echoes_exact_bytes_no_prefix() {
        let (server, client, _addr) = loopback_pair().await;
        let server = Arc::new(server) as Arc<dyn AsyncUdpSocket>;
        let stats = Arc::new(Stats::new());

        let payload = b"payload-without-prefix".to_vec();
        client.send(&payload).await.unwrap();

        let mut buf = [0u8; 2048];
        let (len, from) = server.recv_from(&mut buf).await.unwrap();
        let data = buf[..len].to_vec();
        // Run the concurrent handler body directly (the spawn + permit wrapper
        // in main() adds nothing observable to the echoed bytes).
        echo_concurrent_handler(server.clone(), data, from, stats.clone(), 5000, false).await;

        let mut echoed = [0u8; 2048];
        let n = client.recv(&mut echoed).await.unwrap();
        assert_eq!(
            &echoed[..n],
            &payload[..],
            "concurrent must echo exact bytes, with no 'echo: ' prefix"
        );
        // Explicitly assert the old prefixed form is NOT produced.
        assert!(
            !echoed[..n].starts_with(b"echo: "),
            "concurrent must not prepend 'echo: '"
        );
        assert_eq!(stats.packets_sent.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
