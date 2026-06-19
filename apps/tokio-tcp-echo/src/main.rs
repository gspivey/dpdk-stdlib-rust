//! Async TCP echo server using dpdk-tokio compat layer.
//!
//! Demonstrates transparent DPDK-accelerated TCP via the tokio-compatible API.

use clap::Parser;
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
