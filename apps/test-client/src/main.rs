use clap::Parser;
use dpdk_tokio::compat::tokio::UdpSocket;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "test-client")]
#[command(about = "UDP test client for DPDK echo server")]
struct Args {
    /// Target IP address
    #[arg(long, default_value = "10.0.0.2")]
    target: String,

    /// Target port
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Message to send (ignored if --payload-size is set)
    #[arg(long, default_value = "hello dpdk")]
    message: String,

    /// Send a binary payload of this many bytes instead of --message
    #[arg(long)]
    payload_size: Option<usize>,

    /// Number of packets to send
    #[arg(long, default_value_t = 1)]
    count: u32,

    /// Delay between packets (ms)
    #[arg(long, default_value_t = 1000)]
    delay: u64,

    /// Local IP address to bind to (default: 0.0.0.0)
    #[arg(long)]
    bind_ip: Option<String>,

}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let bind_addr = args.bind_ip
        .as_deref()
        .map(|ip| format!("{}:0", ip))
        .unwrap_or_else(|| "0.0.0.0:0".to_string());

    // Build payload: either fixed-size binary or the message string
    let base_payload: Vec<u8> = if let Some(size) = args.payload_size {
        // Repeating 'J' pattern for jumbo frame testing
        vec![b'J'; size]
    } else {
        Vec::new() // will use message per-packet below
    };

    println!("UDP Test Client");
    println!("Target: {}:{}", args.target, args.port);
    println!("Bind address: {}", bind_addr);
    if args.payload_size.is_some() {
        println!("Payload size: {} bytes", base_payload.len());
    } else {
        println!("Message: '{}'", args.message);
    }
    println!("Count: {}", args.count);

    let socket = UdpSocket::bind(&bind_addr).await?;
    println!("Backend: {}", socket.backend());

    let target_addr = format!("{}:{}", args.target, args.port);

    println!("Sending packets...");

    for i in 1..=args.count {
        let payload: Vec<u8> = if args.payload_size.is_some() {
            base_payload.clone()
        } else {
            format!("{} #{}", args.message, i).into_bytes()
        };

        let bytes_sent = socket.send_to(&payload, &target_addr).await?;
        if args.payload_size.is_some() {
            println!("Sent {} bytes (binary payload)", bytes_sent);
        } else {
            println!("Sent {} bytes: '{}'", bytes_sent, String::from_utf8_lossy(&payload));
        }

        let mut buf = [0u8; 10000];
        match tokio::time::timeout(Duration::from_millis(5000), socket.recv_from(&mut buf)).await {
            Ok(Ok((bytes_received, from_addr))) => {
                if args.payload_size.is_some() {
                    // For binary payloads, verify size match instead of printing content
                    let match_status = if bytes_received == payload.len() { "OK" } else { "MISMATCH" };
                    println!("Received {} bytes from {} (expected {}, {})",
                        bytes_received, from_addr, payload.len(), match_status);
                } else {
                    let response = String::from_utf8_lossy(&buf[..bytes_received]);
                    println!("Received {} bytes from {}: '{}'", bytes_received, from_addr, response);
                }
            }
            Ok(Err(e)) => {
                eprintln!("Error receiving response: {}", e);
            }
            Err(_) => {
                eprintln!("Timeout waiting for response");
            }
        }

        if i < args.count {
            tokio::time::sleep(Duration::from_millis(args.delay)).await;
        }
    }

    println!("Test complete");
    Ok(())
}
