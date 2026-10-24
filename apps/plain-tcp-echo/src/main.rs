//! Minimal TCP echo server using std::net::TcpListener/TcpStream.
//!
//! This is the kernel-path performance baseline for TCP benchmarks.
//! Each accepted connection is handled in a dedicated thread, echoing
//! received data back until the client closes.

use clap::Parser;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

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
#[command(name = "plain-tcp-echo")]
#[command(about = "Minimal TCP echo server using std::net (kernel baseline)")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,
}

fn handle_client(mut stream: TcpStream) {
    let mut buf = [0u8; 65536];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stream.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::ConnectionReset => break,
            Err(_) => break,
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    install_signal_handlers();

    let bind_addr = format!("{}:{}", args.ip, args.port);
    let listener = TcpListener::bind(&bind_addr)?;
    listener.set_nonblocking(true)?;

    eprintln!("plain-tcp-echo listening on {}", listener.local_addr()?);

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            eprintln!("shutting down");
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false)?;
                thread::spawn(move || handle_client(stream));
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}
