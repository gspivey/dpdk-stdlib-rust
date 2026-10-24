//! Async TCP echo server using dpdk-tokio compat layer.
//!
//! Demonstrates transparent DPDK-accelerated TCP via the tokio-compatible API.

use clap::Parser;
use dpdk_stdlib_tcp::{init_dpdk_tcp_context, DpdkTcpRuntimeConfig};
use dpdk_tokio::compat::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(name = "tokio-tcp-echo")]
#[command(about = "Async TCP echo server using dpdk-tokio (DPDK-accelerated)")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Gateway MAC (AA:BB:CC:DD:EE:FF) for AWS VPC (L3-routed). Required on EC2.
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

async fn handle_client(mut stream: TcpStream, peer: std::net::SocketAddr) {
    eprintln!("accepted connection from {}", peer);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = stream.write_all(&buf[..n]).await {
                    eprintln!("write error to {}: {}", peer, e);
                    break;
                }
            }
            Err(e) => {
                eprintln!("read error from {}: {}", peer, e);
                break;
            }
        }
    }
    eprintln!("connection closed: {}", peer);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let bind_addr = format!("{}:{}", args.ip, args.port);

    // Stand up the shared DPDK TCP runtime (EAL + backend + engine driver) before
    // bind; the compat listener reads the same process-wide context.
    let gateway_mac = match args.gateway_mac.as_deref() {
        Some(s) => Some(parse_mac(s).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid --gateway-mac: {s}"),
            )
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

    let listener = TcpListener::bind(&bind_addr).await?;
    eprintln!("tokio-tcp-echo listening on {}", bind_addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(handle_client(stream, peer));
            }
            Err(e) => {
                eprintln!("accept error: {}", e);
            }
        }
    }
}
