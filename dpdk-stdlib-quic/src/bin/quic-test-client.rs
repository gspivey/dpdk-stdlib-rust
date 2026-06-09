//! QUIC test client using the native DPDK provider.
//!
//! Connects to a QUIC echo server, sends data, verifies echo response.
//! Used for EC2 integration testing.
//!
//! Usage:
//!   quic-test-client --server-ip 10.0.1.100 --port 4433 --bind-ip 10.0.1.50 \
//!       --gateway-mac aa:bb:cc:dd:ee:ff --cert-pem /path/to/cert.pem \
//!       --mode handshake|bidir

use dpdk_stdlib_quic::DpdkProvider;
use s2n_quic::client::Connect;
use s2n_quic::Client;
use std::net::SocketAddr;
use std::time::Instant;

fn parse_mac(s: &str) -> [u8; 6] {
    let parts: Vec<u8> = s
        .split(':')
        .map(|p| u8::from_str_radix(p, 16).expect("invalid MAC byte"))
        .collect();
    assert_eq!(parts.len(), 6, "MAC must have 6 octets");
    [parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]]
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Handshake,
    Bidir,
}

#[tokio::main]
async fn main() {
    let mut server_ip = String::new();
    let mut port: u16 = 4433;
    let mut bind_ip = String::from("0.0.0.0");
    let mut gateway_mac: Option<[u8; 6]> = None;
    let mut cert_pem_path = String::new();
    let mut mode = Mode::Handshake;
    let mut payload_size: usize = 1024;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--server-ip" => {
                i += 1;
                server_ip = args[i].clone();
            }
            "--port" => {
                i += 1;
                port = args[i].parse().expect("invalid port");
            }
            "--bind-ip" => {
                i += 1;
                bind_ip = args[i].clone();
            }
            "--gateway-mac" => {
                i += 1;
                gateway_mac = Some(parse_mac(&args[i]));
            }
            "--cert-pem" => {
                i += 1;
                cert_pem_path = args[i].clone();
            }
            "--mode" => {
                i += 1;
                mode = match args[i].as_str() {
                    "handshake" => Mode::Handshake,
                    "bidir" => Mode::Bidir,
                    other => {
                        eprintln!("Unknown mode: {other}. Use 'handshake' or 'bidir'.");
                        std::process::exit(1);
                    }
                };
            }
            "--payload-size" => {
                i += 1;
                payload_size = args[i].parse().expect("invalid payload-size");
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if server_ip.is_empty() || cert_pem_path.is_empty() {
        eprintln!("Required: --server-ip and --cert-pem");
        std::process::exit(1);
    }

    let cert_pem = std::fs::read_to_string(&cert_pem_path).expect("failed to read cert PEM");
    let server_addr: SocketAddr = format!("{server_ip}:{port}").parse().expect("invalid server addr");
    let bind_addr: SocketAddr = format!("{bind_ip}:0").parse().expect("invalid bind addr");

    // Build the DPDK provider
    let mut builder = DpdkProvider::builder().with_addr(bind_addr);
    if let Some(mac) = gateway_mac {
        builder = builder.with_gateway_mac(mac);
    }
    let (provider, mut handle) = builder.build();

    let client = Client::builder()
        .with_tls(cert_pem.as_str())
        .unwrap()
        .with_io(provider)
        .unwrap()
        .start()
        .unwrap();

    eprintln!("QUIC test client started, connecting to {server_addr}...");

    let hs_start = Instant::now();
    let connect = Connect::new(server_addr).with_server_name("localhost");
    let mut connection = client.connect(connect).await.unwrap_or_else(|e| {
        eprintln!("FAIL: Connection failed: {e}");
        std::process::exit(1);
    });
    let hs_elapsed = hs_start.elapsed();

    println!("HANDSHAKE_OK latency_us={}", hs_elapsed.as_micros());

    if mode == Mode::Bidir {
        let payload = vec![0xABu8; payload_size];
        let send_start = Instant::now();

        let mut stream = connection
            .open_bidirectional_stream()
            .await
            .unwrap_or_else(|e| {
                eprintln!("FAIL: Open stream failed: {e}");
                std::process::exit(1);
            });

        stream
            .send(bytes::Bytes::from(payload.clone()))
            .await
            .unwrap_or_else(|e| {
                eprintln!("FAIL: Send failed: {e}");
                std::process::exit(1);
            });
        stream.finish().unwrap_or_else(|e| {
            eprintln!("FAIL: Finish failed: {e}");
            std::process::exit(1);
        });

        let mut received = Vec::new();
        while let Ok(Some(chunk)) = stream.receive().await {
            received.extend_from_slice(&chunk);
        }
        let elapsed = send_start.elapsed();

        if received.len() != payload.len() {
            eprintln!(
                "FAIL: Payload size mismatch: sent={} received={}",
                payload.len(),
                received.len()
            );
            std::process::exit(1);
        }
        if received != payload {
            eprintln!("FAIL: Payload content mismatch");
            std::process::exit(1);
        }

        let throughput_mbps =
            (payload_size as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
        println!(
            "BIDIR_OK payload_bytes={} elapsed_us={} throughput_mbps={:.2}",
            payload_size,
            elapsed.as_micros(),
            throughput_mbps
        );
    }

    // Clean connection close
    drop(connection);
    handle.shutdown();

    println!("TEST_PASSED");
}
