//! Sync TCP echo server using dpdk-stdlib-tcp.
//!
//! Accepts connections and echoes received data back. Supports graceful
//! shutdown via SIGTERM/SIGINT.

use clap::Parser;
use dpdk_stdlib_tcp::{TcpListener, TcpStream};
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
