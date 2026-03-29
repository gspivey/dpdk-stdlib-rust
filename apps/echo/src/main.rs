use clap::Parser;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ---- Only difference from plain-echo: import dpdk_udp instead of std::net ----
use dpdk_udp::UdpSocket;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
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
    unsafe {
        libc::signal(libc::SIGTERM, signal_handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, signal_handler as *const () as libc::sighandler_t);
    }

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
