use dpdk_udp::{DpdkUdpSocket, UdpHandler};
use clap::Parser;

#[derive(Parser)]
#[command(name = "echo")]
#[command(about = "UDP Echo Server - synthetic or DPDK mode")]
struct Args {
    /// Run in DPDK mode (requires --dpdk-args)
    #[arg(long)]
    dpdk: bool,
    
    /// DPDK EAL arguments (e.g., "-l 0-1 -n 4")
    #[arg(long)]
    dpdk_args: Option<String>,
    
    /// IP address to bind to
    #[arg(long, default_value = "10.0.0.2")]
    ip: String,
    
    /// Port to bind to  
    #[arg(long, default_value_t = 9000)]
    port: u16,
}

struct Echo;
impl UdpHandler for Echo {
    fn on_packet(&self, _src_ip: [u8;4], _src_port: u16, _dst_ip: [u8;4], _dst_port: u16, payload: &[u8]) -> Option<Vec<u8>> {
        println!("Received {} bytes: {:?}", payload.len(), std::str::from_utf8(payload).unwrap_or("invalid utf8"));
        Some(payload.to_vec())
    }
}

fn parse_ip(ip_str: &str) -> [u8; 4] {
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

fn run_synthetic_mode(ip: [u8; 4], port: u16) {
    println!("== Userspace UDP Echo (synthetic mode) ==");
    println!("Listening on {}:{}", 
        format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]), port);
    
    let socket = DpdkUdpSocket::new(ip, port, Box::new(Echo));

    let mut frame = vec![0u8; 14 + 20 + 8 + 15];
    frame[14] = 0x45;
    frame[14+2..14+4].copy_from_slice(&(20+8+15u16).to_be_bytes());
    frame[14+9] = 17;
    frame[14+12..14+16].copy_from_slice(&[10,0,0,1]);
    frame[14+16..14+20].copy_from_slice(&ip);
    frame[14+20..14+22].copy_from_slice(&12345u16.to_be_bytes());
    frame[14+20+2..14+20+4].copy_from_slice(&port.to_be_bytes());
    frame[14+20+4..14+20+6].copy_from_slice(&(8+15u16).to_be_bytes());
    frame[14+20+8..].copy_from_slice(b"hello userspace");

    match socket.parse_and_handle(&frame) {
        Ok(Some(resp)) => {
            println!("Generated {}-byte response frame", resp.len());
            let payload_start = 14 + 20 + 8;
            let payload = &resp[payload_start..];
            println!("Echo response payload: {:?}", std::str::from_utf8(payload).unwrap_or("invalid utf8"));
        }
        Ok(None) => println!("Packet not for this socket"),
        Err(e) => eprintln!("Error: {e:?}"),
    }
}

#[cfg(feature = "dpdk-support")]
fn run_dpdk_mode(dpdk_args: &str, ip: [u8; 4], port: u16) {
    println!("== Userspace UDP Echo (DPDK mode) ==");
    println!("DPDK args: {}", dpdk_args);
    println!("Listening on {}:{}", 
        format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]), port);
    
    let args: Vec<&str> = dpdk_args.split_whitespace().collect();
    
    let _eal = match dpdk::Eal::init(&args) {
        Ok(eal) => {
            println!("DPDK EAL initialized successfully");
            eal
        }
        Err(e) => {
            eprintln!("Failed to initialize DPDK EAL: {:?}", e);
            std::process::exit(1);
        }
    };

    let dpdk_port = match dpdk::Port::new(0) {
        Ok(port) => {
            println!("Initialized DPDK port 0");
            port
        }
        Err(e) => {
            eprintln!("Failed to initialize DPDK port: {:?}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = dpdk_port.start() {
        eprintln!("Failed to start DPDK port: {:?}", e);
        std::process::exit(1);
    }

    println!("DPDK port started, entering packet processing loop...");
    println!("Press Ctrl+C to stop");

    let socket = DpdkUdpSocket::new(ip, port, Box::new(Echo));

    loop {
        match dpdk_port.receive_burst(32) {
            Ok(packets) => {
                if !packets.is_empty() {
                    println!("Received {} packets", packets.len());
                    
                    let mut responses = Vec::new();
                    
                    for packet in packets {
                        match socket.parse_and_handle(&packet) {
                            Ok(Some(response)) => {
                                responses.push(response);
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("Error processing packet: {:?}", e);
                            }
                        }
                    }
                    
                    if !responses.is_empty() {
                        match dpdk_port.send_burst(&responses) {
                            Ok(sent) => {
                                println!("Sent {} response packets", sent);
                            }
                            Err(e) => {
                                eprintln!("Error sending responses: {:?}", e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Error receiving packets: {:?}", e);
                break;
            }
        }
        
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(not(feature = "dpdk-support"))]
fn run_dpdk_mode(_dpdk_args: &str, _ip: [u8; 4], _port: u16) {
    eprintln!("Error: DPDK support not compiled in. Build with --features dpdk-support");
    std::process::exit(1);
}

fn main() {
    let args = Args::parse();
    let ip = parse_ip(&args.ip);
    
    if args.dpdk {
        if let Some(dpdk_args) = args.dpdk_args {
            run_dpdk_mode(&dpdk_args, ip, args.port);
        } else {
            eprintln!("Error: DPDK mode requires --dpdk-args");
            std::process::exit(1);
        }
    } else {
        run_synthetic_mode(ip, args.port);
    }
}
