//! TCP test client with multiple test modes.
//!
//! Modes:
//! - `handshake`: connect then close (measures connection establishment)
//! - `bidir`: bidirectional data transfer and echo verification
//! - `shutdown`: graceful FIN teardown
//! - `std-parity`: compare dpdk-stdlib-tcp vs std::net::TcpStream byte-for-byte

use clap::Parser;
use dpdk_stdlib_tcp::{init_dpdk_tcp_context, DpdkTcpRuntimeConfig, TcpStream};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "tcp-test-client")]
#[command(about = "TCP test client for DPDK TCP echo server")]
struct Args {
    /// Target IP address
    #[arg(long, default_value = "10.0.0.2")]
    target: String,

    /// Target port
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Test mode: handshake, bidir, shutdown, std-parity
    #[arg(long, default_value = "bidir")]
    mode: String,

    /// Number of iterations
    #[arg(long, default_value_t = 1)]
    count: u32,

    /// Payload size in bytes (for bidir mode)
    #[arg(long, default_value_t = 64)]
    payload_size: usize,

    /// Local source IPv4 for outbound DPDK connections (this client's data-ENI IP).
    #[arg(long, default_value = "0.0.0.0")]
    local_ip: String,

    /// Gateway MAC (AA:BB:CC:DD:EE:FF) for AWS VPC (L3-routed). Required on EC2.
    #[arg(long)]
    gateway_mac: Option<String>,

    /// Explicit DPDK EAL arguments (space-separated). Overrides DPDK_EAL_ARGS.
    #[arg(long)]
    eal_args: Option<String>,
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

fn mode_handshake(target: &str, port: u16, count: u32) -> io::Result<()> {
    println!("Mode: handshake (connect + close)");
    let start = Instant::now();
    for i in 0..count {
        let addr = format!("{}:{}", target, port);
        let stream = TcpStream::connect(&addr)?;
        drop(stream);
        if (i + 1) % 100 == 0 || i + 1 == count {
            println!("  completed {}/{}", i + 1, count);
        }
    }
    let elapsed = start.elapsed();
    let rate = count as f64 / elapsed.as_secs_f64();
    println!(
        "Result: {} connections in {:.2}s ({:.1} conn/s)",
        count,
        elapsed.as_secs_f64(),
        rate
    );
    Ok(())
}

fn mode_bidir(target: &str, port: u16, count: u32, payload_size: usize) -> io::Result<()> {
    println!("Mode: bidir (echo verification, {}B payload)", payload_size);
    let addr = format!("{}:{}", target, port);
    let mut stream = TcpStream::connect(&addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let payload: Vec<u8> = (0..payload_size).map(|i| (i % 256) as u8).collect();
    let mut recv_buf = vec![0u8; payload_size];

    let start = Instant::now();
    for i in 0..count {
        stream.write_all(&payload)?;
        let mut received = 0;
        while received < payload_size {
            let n = stream.read(&mut recv_buf[received..])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "server closed"));
            }
            received += n;
        }
        if recv_buf[..payload_size] != payload[..] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("echo mismatch at iteration {}", i),
            ));
        }
    }
    let elapsed = start.elapsed();
    println!(
        "Result: {} echo round-trips in {:.2}s ({:.1} rtt/s, {:.1} MB/s)",
        count,
        elapsed.as_secs_f64(),
        count as f64 / elapsed.as_secs_f64(),
        (count as f64 * payload_size as f64 * 2.0) / elapsed.as_secs_f64() / 1_000_000.0
    );
    stream.shutdown(Shutdown::Both)?;
    Ok(())
}

fn mode_shutdown(target: &str, port: u16) -> io::Result<()> {
    println!("Mode: shutdown (graceful FIN teardown)");
    let addr = format!("{}:{}", target, port);
    let mut stream = TcpStream::connect(&addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    // Send data, shutdown write, read remaining, verify clean EOF
    let msg = b"shutdown-test-payload";
    stream.write_all(msg)?;
    stream.shutdown(Shutdown::Write)?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break, // Clean EOF
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for server FIN",
                ));
            }
            Err(e) => return Err(e),
        }
    }

    if buf == msg {
        println!("Result: graceful shutdown OK (echoed {} bytes before FIN)", buf.len());
    } else {
        println!("Result: graceful shutdown OK (received {} bytes)", buf.len());
    }
    Ok(())
}

fn mode_std_parity(target: &str, port: u16, payload_size: usize) -> io::Result<()> {
    println!("Mode: std-parity (compare dpdk-stdlib-tcp vs std::net::TcpStream)");
    let addr = format!("{}:{}", target, port);

    let payload: Vec<u8> = (0..payload_size).map(|i| (i % 256) as u8).collect();

    // Test with dpdk-stdlib-tcp
    let mut dpdk_stream = TcpStream::connect(&addr)?;
    dpdk_stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    dpdk_stream.write_all(&payload)?;
    let mut dpdk_recv = vec![0u8; payload_size];
    let mut received = 0;
    while received < payload_size {
        let n = dpdk_stream.read(&mut dpdk_recv[received..])?;
        if n == 0 { break; }
        received += n;
    }
    dpdk_stream.shutdown(Shutdown::Both)?;

    // Test with std::net::TcpStream
    let mut std_stream = std::net::TcpStream::connect(&addr)?;
    std_stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    std_stream.write_all(&payload)?;
    let mut std_recv = vec![0u8; payload_size];
    let mut std_received = 0;
    while std_received < payload_size {
        let n = std_stream.read(&mut std_recv[std_received..])?;
        if n == 0 { break; }
        std_received += n;
    }
    std_stream.shutdown(Shutdown::Both)?;

    // Compare
    if dpdk_recv[..received] == std_recv[..std_received] {
        println!("Result: PASS — byte-for-byte identical ({} bytes)", received);
    } else {
        println!(
            "Result: FAIL — dpdk got {} bytes, std got {} bytes, content differs",
            received, std_received
        );
        std::process::exit(1);
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    println!("TCP Test Client");
    println!("Target: {}:{}", args.target, args.port);

    // Stand up the DPDK TCP runtime before any DPDK connect. The std-parity mode
    // also opens a std::net stream, which is unaffected.
    let gateway_mac = match args.gateway_mac.as_deref() {
        Some(s) => Some(parse_mac(s).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid --gateway-mac: {s}"))
        })?),
        None => None,
    };
    init_dpdk_tcp_context(DpdkTcpRuntimeConfig {
        port_id: 0,
        local_ip: args.local_ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED),
        gateway_mac,
        eal_args: args
            .eal_args
            .as_deref()
            .map(|s| s.split_whitespace().map(String::from).collect()),
        mtu: 9001,
    })?;

    let result = match args.mode.as_str() {
        "handshake" => mode_handshake(&args.target, args.port, args.count),
        "bidir" => mode_bidir(&args.target, args.port, args.count, args.payload_size),
        "shutdown" => mode_shutdown(&args.target, args.port),
        "std-parity" => mode_std_parity(&args.target, args.port, args.payload_size),
        other => {
            eprintln!("Unknown mode: '{}'. Use: handshake, bidir, shutdown, std-parity", other);
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("ERROR: {}", e);
        std::process::exit(1);
    }
    Ok(())
}
