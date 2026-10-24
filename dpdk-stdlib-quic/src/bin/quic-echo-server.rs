//! QUIC echo server using the native DPDK provider.
//!
//! Accepts QUIC connections, echoes bidirectional stream data back to the client.
//! Used for EC2 integration testing.
//!
//! Usage:
//!   quic-echo-server --ip 10.0.1.100 --port 4433 --gateway-mac aa:bb:cc:dd:ee:ff
//!
//! Generates a self-signed TLS certificate and prints it to stdout (PEM)
//! so the client can use it as a trust anchor.

use dpdk_stdlib_quic::DpdkProvider;
use s2n_quic::Server;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn parse_mac(s: &str) -> [u8; 6] {
    let parts: Vec<u8> = s
        .split(':')
        .map(|p| u8::from_str_radix(p, 16).expect("invalid MAC byte"))
        .collect();
    assert_eq!(parts.len(), 6, "MAC must have 6 octets");
    [parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]]
}

#[tokio::main]
async fn main() {
    let mut ip = String::from("0.0.0.0");
    let mut port: u16 = 4433;
    let mut gateway_mac: Option<[u8; 6]> = None;
    let mut eal_args: Option<Vec<String>> = None;
    let mut dpdk_port: u16 = 0;
    let mut throughput = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ip" => {
                i += 1;
                ip = args[i].clone();
            }
            "--port" => {
                i += 1;
                port = args[i].parse().expect("invalid port");
            }
            "--gateway-mac" => {
                i += 1;
                gateway_mac = Some(parse_mac(&args[i]));
            }
            "--eal-args" => {
                i += 1;
                eal_args = Some(args[i].split_whitespace().map(String::from).collect());
            }
            "--dpdk-port" => {
                i += 1;
                dpdk_port = args[i].parse().expect("invalid dpdk-port");
            }
            // Stream-echo each chunk as it arrives (for sustained-throughput
            // clients that keep a stream open) instead of buffering until EOF.
            "--throughput" => {
                throughput = true;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let addr: SocketAddr = format!("{ip}:{port}").parse().expect("invalid address");

    // Generate self-signed TLS cert
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("cert generation failed");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    // Print cert PEM to stdout for client to consume (delimited for parsing)
    println!("---BEGIN CERT PEM---");
    print!("{cert_pem}");
    println!("---END CERT PEM---");

    // Build the DPDK provider
    let mut builder = DpdkProvider::builder()
        .with_addr(addr)
        .with_dpdk_port(dpdk_port);
    if let Some(mac) = gateway_mac {
        builder = builder.with_gateway_mac(mac);
    }
    if let Some(args) = eal_args {
        builder = builder.with_eal_args(args);
    }
    let (provider, mut handle) = builder.build();

    // Set up shutdown signal
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        running_clone.store(false, Ordering::Release);
    });

    // Start the QUIC server
    let mut server = Server::builder()
        .with_tls((cert_pem.as_str(), key_pem.as_str()))
        .unwrap()
        .with_io(provider)
        .unwrap()
        .start()
        .unwrap();

    eprintln!("QUIC echo server listening on {addr}");
    eprintln!("QUIC_SERVER_READY");

    // Accept connections and echo streams
    while let Some(mut conn) = server.accept().await {
        if !running.load(Ordering::Acquire) {
            break;
        }
        tokio::spawn(async move {
            while let Ok(Some(mut stream)) = conn.accept_bidirectional_stream().await {
                tokio::spawn(async move {
                    if throughput {
                        // Streaming echo: bounce each chunk back as it arrives so a
                        // client can keep the stream open and sustain throughput.
                        while let Ok(Some(chunk)) = stream.receive().await {
                            if stream.send(chunk).await.is_err() {
                                break;
                            }
                        }
                        let _ = stream.finish();
                    } else {
                        // Buffer-until-EOF echo (request/response correctness path).
                        let mut buf = Vec::new();
                        while let Ok(Some(chunk)) = stream.receive().await {
                            buf.extend_from_slice(&chunk);
                        }
                        let _ = stream.send(bytes::Bytes::from(buf)).await;
                        let _ = stream.finish();
                    }
                });
            }
        });
    }

    handle.shutdown();
    eprintln!("QUIC echo server shut down");
}
