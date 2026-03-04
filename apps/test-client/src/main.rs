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

    /// Message to send
    #[arg(long, default_value = "hello dpdk")]
    message: String,

    /// Number of packets to send
    #[arg(long, default_value_t = 1)]
    count: u32,

    /// Delay between packets (ms)
    #[arg(long, default_value_t = 1000)]
    delay: u64,

    /// Local IP address to bind to (default: 0.0.0.0)
    #[arg(long)]
    bind_ip: Option<String>,

    /// Gateway MAC address for AWS VPC DPDK routing (format: xx:xx:xx:xx:xx:xx).
    /// Pre-populates the ARP cache so DPDK sends to the VPC gateway MAC.
    /// See docs/aws-vpc-networking.md.
    #[arg(long)]
    gateway_mac: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let bind_addr = args.bind_ip
        .as_deref()
        .map(|ip| format!("{}:0", ip))
        .unwrap_or_else(|| "0.0.0.0:0".to_string());

    println!("UDP Test Client");
    println!("Target: {}:{}", args.target, args.port);
    println!("Bind address: {}", bind_addr);
    println!("Message: '{}'", args.message);
    println!("Count: {}", args.count);

    let socket = UdpSocket::bind(&bind_addr).await?;
    println!("Backend: {}", socket.backend());

    // Pre-populate ARP cache with gateway MAC for AWS VPC routing.
    // Maps the TARGET IP to the gateway MAC so DPDK sends frames to the
    // VPC virtual router, which does L3 forwarding to the actual destination.
    if let Some(ref gw_mac_str) = args.gateway_mac {
        let parts: Vec<u8> = gw_mac_str.split(':')
            .map(|s| u8::from_str_radix(s, 16).expect("invalid MAC octet"))
            .collect();
        assert_eq!(parts.len(), 6, "gateway MAC must have 6 octets");
        let mac = [parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]];

        let target_ip: std::net::Ipv4Addr = args.target.parse()
            .expect("--target must be a valid IPv4 address when using --gateway-mac");
        println!("Pre-populating ARP: {} -> {}", target_ip, gw_mac_str);
        socket.add_arp_entry(target_ip, mac);
    }

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
