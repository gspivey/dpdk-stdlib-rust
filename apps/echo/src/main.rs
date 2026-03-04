use clap::Parser;
use std::io;
use std::net::SocketAddr;

// Trait that both std::net::UdpSocket and dpdk_udp::UdpSocket implement
trait UdpSocketTrait {
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>;
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

// Implement trait for std::net::UdpSocket
impl UdpSocketTrait for std::net::UdpSocket {
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.recv_from(buf)
    }
    
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.send_to(buf, addr)
    }
    
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr()
    }
}

// Implement trait for dpdk_udp::UdpSocket
#[cfg(feature = "dpdk")]
impl UdpSocketTrait for dpdk_udp::UdpSocket {
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.recv_from(buf)
    }
    
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.send_to(buf, addr)
    }
    
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr()
    }
}

/// Parse a MAC address string (xx:xx:xx:xx:xx:xx) into a 6-byte array.
#[cfg(feature = "dpdk")]
fn parse_mac(mac_str: &str) -> [u8; 6] {
    let parts: Vec<u8> = mac_str.split(':')
        .map(|s| u8::from_str_radix(s, 16).expect("invalid MAC octet"))
        .collect();
    assert_eq!(parts.len(), 6, "MAC address must have 6 octets");
    [parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]]
}

/// Try DPDK first, then fall back to standard networking.
/// If gateway_mac is provided, pre-populate the ARP cache for AWS VPC routing.
fn bind_socket(bind_addr: &str, _gateway_mac: Option<&str>) -> Result<Box<dyn UdpSocketTrait>, Box<dyn std::error::Error>> {
    #[cfg(feature = "dpdk")]
    {
        match dpdk_udp::UdpSocket::bind(bind_addr) {
            Ok(socket) => {
                println!("Using DPDK acceleration");

                // Pre-populate ARP cache with gateway MAC for AWS VPC routing
                if let Some(gw_mac_str) = _gateway_mac {
                    let mac_bytes = parse_mac(gw_mac_str);
                    let mac = dpdk_udp::MacAddress::new(mac_bytes);
                    // Derive gateway IP from bind address (subnet_base + 1)
                    let bind_ip: std::net::Ipv4Addr = bind_addr.split(':').next()
                        .unwrap_or("0.0.0.0").parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
                    let octets = bind_ip.octets();
                    let gateway_ip = std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], 1);
                    println!("Pre-populating ARP: gateway {} -> {}", gateway_ip, gw_mac_str);
                    socket.add_arp_entry(gateway_ip, mac.clone());
                    // Also map all /24 addresses to gateway MAC (VPC routes everything via gateway)
                    // We only need the specific peers, but mapping the gateway itself is sufficient
                    // because send_to_addr resolves via ARP which will send to gateway IP first.
                    // For direct peer-to-peer, we learn src_mac from inbound packets.
                }

                return Ok(Box::new(socket));
            }
            Err(e) => {
                println!("DPDK failed ({}), falling back to standard networking", e);
            }
        }
    }

    // Standard library fallback
    println!("Using standard networking");
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    Ok(Box::new(socket))
}

#[derive(Parser)]
#[command(name = "echo")]
#[command(about = "UDP Echo Server - auto-detects DPDK or uses standard networking")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "127.0.0.1")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Use synthetic packet mode (for protocol testing - developer option)
    #[arg(long, hide = true)]
    synthetic: bool,

    /// Gateway MAC address for AWS VPC DPDK routing (format: xx:xx:xx:xx:xx:xx).
    /// In AWS VPC, all outbound DPDK frames must use the gateway MAC as the
    /// Ethernet destination. See docs/aws-vpc-networking.md.
    #[arg(long)]
    gateway_mac: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    println!("🚀 DPDK-STDLIB Echo Server");
    
    if args.synthetic {
        #[cfg(feature = "dpdk")]
        {
            println!("🔧 Using synthetic packet mode for protocol testing");
            run_synthetic_mode(&args)?;
        }
        #[cfg(not(feature = "dpdk"))]
        {
            println!("❌ Synthetic mode requires DPDK feature. Use: cargo run --features dpdk");
        }
    } else {
        let bind_addr = format!("{}:{}", args.ip, args.port);
        println!("Binding to {}", bind_addr);

        let socket = bind_socket(&bind_addr, args.gateway_mac.as_deref())?;
        run_echo_server(socket)?;
    }
    
    Ok(())
}

// Single echo server function that works with any UdpSocket implementation
fn run_echo_server(socket: Box<dyn UdpSocketTrait>) -> Result<(), Box<dyn std::error::Error>> {
    println!("✅ Socket created successfully!");
    println!("📡 Local address: {}", socket.local_addr()?);
    println!("🔄 Echo server running... (Ctrl+C to stop)");
    
    let mut buf = [0u8; 1024];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((size, from)) => {
                let msg = String::from_utf8_lossy(&buf[..size]);
                println!("📨 Received from {}: {}", from, msg);
                
                // Echo back
                let response = format!("echo: {}", msg);
                match socket.send_to(response.as_bytes(), from) {
                    Ok(sent) => println!("📤 Sent {} bytes back to {}", sent, from),
                    Err(e) => eprintln!("❌ Send error: {}", e),
                }
            }
            Err(e) => {
                eprintln!("❌ Receive error: {}", e);
                break;
            }
        }
    }
    
    Ok(())
}

// Synthetic mode (only available with DPDK feature)
#[cfg(feature = "dpdk")]
use dpdk_udp::{SyntheticUdpSocket, UdpHandler};

#[cfg(feature = "dpdk")]
struct Echo;

#[cfg(feature = "dpdk")]
impl UdpHandler for Echo {
    fn on_packet(&self, _src_ip: [u8;4], _src_port: u16, _dst_ip: [u8;4], _dst_port: u16, payload: &[u8]) -> Option<Vec<u8>> {
        println!("📨 Received {} bytes: {:?}", payload.len(), std::str::from_utf8(payload).unwrap_or("invalid utf8"));
        Some(payload.to_vec())
    }
}

#[cfg(feature = "dpdk")]
fn parse_ip(ip_str: &str) -> [u8; 4] {
    if ip_str == "0.0.0.0" {
        return [0, 0, 0, 0];
    }
    let parts: Vec<&str> = ip_str.split('.').collect();
    if parts.len() != 4 {
        panic!("Invalid IP address format");
    }
    [
        parts[0].parse().expect("Invalid IP octet"),
        parts[1].parse().expect("Invalid IP octet"), 
        parts[2].parse().expect("Invalid IP octet"),
        parts[3].parse().expect("Invalid IP octet"),
    ]
}

#[cfg(feature = "dpdk")]
fn run_synthetic_mode(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    println!("📡 Synthetic packet mode - testing protocol parsing without real networking");
    println!("💡 This tests our UDP parsing logic with fake packets");
    
    let ip = parse_ip(&args.ip);
    let socket = SyntheticUdpSocket::new(ip, args.port, Box::new(Echo));

    // Create a synthetic UDP packet for testing
    let payload = b"hello synthetic";
    let mut frame = vec![0u8; 14 + 20 + 8 + payload.len()];
    
    // Ethernet header (dummy)
    frame[12..14].copy_from_slice(&[0x08, 0x00]); // IPv4
    
    // IP header
    frame[14] = 0x45; // Version + IHL
    frame[14+2..14+4].copy_from_slice(&((20+8+payload.len()) as u16).to_be_bytes()); // Total length
    frame[14+9] = 17; // Protocol (UDP)
    frame[14+12..14+16].copy_from_slice(&[10,0,0,1]); // Source IP
    frame[14+16..14+20].copy_from_slice(&ip); // Dest IP
    
    // UDP header
    frame[14+20..14+22].copy_from_slice(&12345u16.to_be_bytes()); // Source port
    frame[14+20+2..14+20+4].copy_from_slice(&args.port.to_be_bytes()); // Dest port
    frame[14+20+4..14+20+6].copy_from_slice(&((8+payload.len()) as u16).to_be_bytes()); // UDP length
    // Checksum = 0 (skip)
    
    // Payload
    frame[14+20+8..].copy_from_slice(payload);

    match socket.parse_and_handle(&frame) {
        Ok(Some(resp)) => {
            println!("✅ Generated {}-byte response frame", resp.len());
            let payload_start = 14 + 20 + 8;
            let payload = &resp[payload_start..];
            println!("📤 Echo response: {:?}", std::str::from_utf8(payload).unwrap_or("invalid utf8"));
            println!("✅ Protocol parsing works correctly!");
        }
        Ok(None) => println!("❌ Packet not for this socket"),
        Err(e) => eprintln!("❌ Error: {e:?}"),
    }
    
    Ok(())
}
