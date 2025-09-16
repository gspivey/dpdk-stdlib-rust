use clap::Parser;
use tokio::net::UdpSocket;
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
    
    /// Message to send
    #[arg(long, default_value = "hello dpdk")]
    message: String,
    
    /// Number of packets to send
    #[arg(long, default_value_t = 1)]
    count: u32,
    
    /// Delay between packets (ms)
    #[arg(long, default_value_t = 1000)]
    delay: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    println!("UDP Test Client");
    println!("Target: {}:{}", args.target, args.port);
    println!("Message: '{}'", args.message);
    println!("Count: {}", args.count);
    
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let target_addr = format!("{}:{}", args.target, args.port);
    
    println!("Sending packets...");
    
    for i in 1..=args.count {
        let message = format!("{} #{}", args.message, i);
        
        let bytes_sent = socket.send_to(message.as_bytes(), &target_addr).await?;
        println!("Sent {} bytes: '{}'", bytes_sent, message);
        
        let mut buf = [0u8; 1024];
        match tokio::time::timeout(Duration::from_millis(5000), socket.recv_from(&mut buf)).await {
            Ok(Ok((bytes_received, from_addr))) => {
                let response = String::from_utf8_lossy(&buf[..bytes_received]);
                println!("Received {} bytes from {}: '{}'", bytes_received, from_addr, response);
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
