use clap::Parser;
use std::io;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ---- Only difference from plain-echo: import dpdk_udp instead of std::net ----
use dpdk_udp::UdpSocket;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// Install `signal_handler` for SIGTERM and SIGINT via `sigaction`.
///
/// The old `libc::signal(sig, fn as *const () as sighandler_t)` cast is
/// brittle on some toolchains: with certain codegen paths the resulting
/// `usize` doesn't match the actual function address, leaving the default
/// handler in place, which means SIGTERM terminates the process without
/// running any destructors — and in particular without running
/// `PerfReporter::drop`, so the one-shot `[NIC-FINAL]` log line the perf
/// harness relies on is never emitted. `sigaction` takes a typed
/// `sa_sigaction`/`sa_handler` field and avoids the cast entirely.
fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = MaybeUninit::zeroed().assume_init();
        sa.sa_sigaction = signal_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

#[derive(Parser)]
#[command(name = "echo")]
#[command(about = "UDP echo server using dpdk-udp (DPDK-accelerated drop-in for std::net)")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Performance reporting interval in seconds (0 = disabled)
    #[arg(long, default_value_t = 0)]
    perf_interval: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_signal_handlers();

    let args = Args::parse();
    let bind_addr = format!("{}:{}", args.ip, args.port);

    let socket = UdpSocket::bind(&bind_addr)?;
    let rt = socket.routing_table();
    eprintln!("echo listening on {} (MTU={}, max_udp_payload={})",
        socket.local_addr()?, rt.mtu(), rt.max_udp_payload());

    if args.perf_interval > 0 {
        socket.enable_perf_reporting(Duration::from_secs(args.perf_interval))?;
    }

    socket.set_read_timeout(Some(Duration::from_millis(500)))?;

    let mut buf = [0u8; 10000];
    while !SHUTDOWN.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                let _ = socket.send_to(&buf[..len], src);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("recv error: {}", e);
                break;
            }
        }
    }

    println!("Shutting down gracefully...");
    Ok(())
}
