//! Pure-kernel (`std::net`) TCP echo client — the "reference" peer for the TCP
//! smoke tiers.
//!
//! It uses the OS kernel TCP stack (NO DPDK), so it exercises our DPDK
//! `tcp-echo` server against a known-good, independent TCP implementation over
//! the real NIC. This is the exact scenario that surfaced the codec padding bug:
//! a standard stack's bare ACKs are NIC-padded to 60 bytes, which our parser
//! must not mistake for payload.
//!
//! Prints `TCP_KERNEL_OK round_trips=N` on full success; on any error/mismatch
//! prints `TCP_KERNEL_FAIL <reason>` and exits non-zero.

use clap::Parser;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "tcp-kernel-client")]
#[command(about = "Pure-kernel std::net TCP echo client (reference peer for TCP smoke tests)")]
struct Args {
    /// Target IP address
    #[arg(long, default_value = "10.0.0.2")]
    target: String,

    /// Target port
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Number of connect → echo → close iterations
    #[arg(long, default_value_t = 5)]
    count: u32,

    /// Payload size in bytes per round-trip
    #[arg(long, default_value_t = 64)]
    payload_size: usize,

    /// Connect/read/write timeout in seconds
    #[arg(long, default_value_t = 10)]
    timeout_secs: u64,
}

/// One connect → send → recv-echo → verify → close cycle over the kernel stack.
fn one_round_trip(addr: &SocketAddr, payload: &[u8], timeout: Duration) -> Result<(), String> {
    let mut stream =
        TcpStream::connect_timeout(addr, timeout).map_err(|e| format!("connect failed: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set_read_timeout failed: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("set_write_timeout failed: {e}"))?;

    stream
        .write_all(payload)
        .map_err(|e| format!("write failed: {e}"))?;

    let mut recv = vec![0u8; payload.len()];
    let mut got = 0;
    while got < payload.len() {
        match stream.read(&mut recv[got..]) {
            Ok(0) => return Err(format!("server closed after {got}/{} bytes", payload.len())),
            Ok(n) => got += n,
            Err(e) => return Err(format!("read failed after {got} bytes: {e}")),
        }
    }
    if recv != payload {
        return Err("echo mismatch (bytes differ)".to_string());
    }
    Ok(())
}

fn main() {
    let args = Args::parse();

    let addr: SocketAddr = match format!("{}:{}", args.target, args.port).parse() {
        Ok(a) => a,
        Err(e) => {
            println!("TCP_KERNEL_FAIL bad address {}:{}: {e}", args.target, args.port);
            std::process::exit(1);
        }
    };
    let timeout = Duration::from_secs(args.timeout_secs);
    let payload: Vec<u8> = (0..args.payload_size).map(|i| (i % 256) as u8).collect();

    println!(
        "tcp-kernel-client: {} round-trips of {}B to {}",
        args.count, args.payload_size, addr
    );
    for i in 0..args.count {
        if let Err(reason) = one_round_trip(&addr, &payload, timeout) {
            println!("TCP_KERNEL_FAIL iteration {}/{}: {reason}", i + 1, args.count);
            std::process::exit(1);
        }
    }
    println!("TCP_KERNEL_OK round_trips={}", args.count);
}
