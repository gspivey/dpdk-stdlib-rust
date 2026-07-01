//! Sync TCP echo server using dpdk-stdlib-tcp.
//!
//! Accepts connections and echoes received data back. Supports graceful
//! shutdown via SIGTERM/SIGINT.

use clap::Parser;
use dpdk_stdlib_tcp::{init_dpdk_tcp_context, DpdkTcpRuntimeConfig, TcpListener, TcpStream};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

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
#[command(name = "tcp-echo")]
#[command(about = "TCP echo server using dpdk-stdlib-tcp (DPDK-accelerated)")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Gateway MAC (AA:BB:CC:DD:EE:FF) for AWS VPC (L3-routed). Required on EC2:
    /// all outbound frames use this as the Ethernet destination.
    #[arg(long)]
    gateway_mac: Option<String>,

    /// Explicit DPDK EAL arguments (space-separated). Overrides DPDK_EAL_ARGS.
    #[arg(long)]
    eal_args: Option<String>,

    /// Performance reporting interval in seconds (0 = disabled). Reserved.
    #[arg(long, default_value_t = 0)]
    perf_interval: u64,
}

/// Parse a colon-separated MAC string (`AA:BB:CC:DD:EE:FF`) into 6 octets.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(mac)
}

fn handle_client(mut stream: TcpStream) {
    let peer = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    eprintln!("accepted connection from {}", peer);

    let mut buf = [0u8; 4096];
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                if let Err(e) = stream.write_all(&buf[..n]) {
                    eprintln!("write error to {}: {}", peer, e);
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("read error from {}: {}", peer, e);
                break;
            }
        }
    }
    eprintln!("connection closed: {}", peer);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_signal_handlers();

    let args = Args::parse();
    let bind_addr = format!("{}:{}", args.ip, args.port);

    // Stand up the DPDK TCP runtime (EAL + backend + engine driver) before bind.
    let gateway_mac = match args.gateway_mac.as_deref() {
        Some(s) => Some(parse_mac(s).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid --gateway-mac: {s}"))
        })?),
        None => None,
    };
    init_dpdk_tcp_context(DpdkTcpRuntimeConfig {
        port_id: 0,
        local_ip: args.ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED),
        gateway_mac,
        eal_args: args
            .eal_args
            .as_deref()
            .map(|s| s.split_whitespace().map(String::from).collect()),
        mtu: 9001,
    })?;
    if args.perf_interval > 0 {
        eprintln!(
            "perf reporting requested every {}s (not yet implemented for TCP)",
            args.perf_interval
        );
    }

    let listener = TcpListener::bind(&bind_addr)?;
    eprintln!("tcp-echo listening on {}", listener.local_addr()?);

    // Set a short accept timeout so we can check the shutdown flag
    listener.set_ttl(64)?;

    for stream in listener.incoming() {
        if SHUTDOWN.load(Ordering::Relaxed) {
            break;
        }
        match stream {
            Ok(stream) => {
                stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                thread::spawn(move || handle_client(stream));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("accept error: {}", e);
                if SHUTDOWN.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    }

    eprintln!("Shutting down gracefully...");
    Ok(())
}
