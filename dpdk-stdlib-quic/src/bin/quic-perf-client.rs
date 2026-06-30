//! Real-NIC QUIC sustained-throughput client using the native DPDK provider.
//!
//! Unlike `quic-test-client` (single request/response correctness check), this
//! drives N bidirectional streams continuously for a fixed duration against a
//! `quic-echo-server --throughput`, then prints one machine-parseable line:
//!
//!   PERF_RESULT gbps=.. bytes=.. elapsed_s=.. hs_us=.. rx_bursts=.. tx_bursts=.. rx_drops=.. tx_drops=..
//!
//! Usage:
//!   quic-perf-client --server-ip 10.0.1.100 --port 4433 --bind-ip 10.0.1.50 \
//!       --gateway-mac aa:bb:cc:dd:ee:ff --cert-pem /path/to/cert.pem \
//!       --duration 30 --streams 8 --payload-size 65536 [--eal-args "..."] [--dpdk-port 0]
//!
//! NOTE: this is goodput measured request/response per stream (one payload in
//! flight per stream at a time), parallelised across `--streams`. Full pipelining
//! and TX batching/GSO are future optimisations; the backend also allocates and
//! copies one mbuf per datagram, which bounds line rate.

use dpdk_stdlib_quic::DpdkProvider;
use s2n_quic::client::Connect;
use s2n_quic::Client;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

fn parse_mac(s: &str) -> [u8; 6] {
    let parts: Vec<u8> = s
        .split(':')
        .map(|p| u8::from_str_radix(p, 16).expect("invalid MAC byte"))
        .collect();
    assert_eq!(parts.len(), 6, "MAC must have 6 octets");
    [parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]]
}

/// Throughput in Gbps from bytes transferred over `secs` seconds.
fn throughput_gbps(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    (bytes as f64 * 8.0) / secs / 1_000_000_000.0
}

#[tokio::main]
async fn main() {
    let mut server_ip = String::new();
    let mut port: u16 = 4433;
    let mut bind_ip = String::from("0.0.0.0");
    let mut gateway_mac: Option<[u8; 6]> = None;
    let mut cert_pem_path = String::new();
    let mut duration_secs: u64 = 10;
    let mut streams: usize = 1;
    let mut payload_size: usize = 65536;
    let mut eal_args: Option<Vec<String>> = None;
    let mut dpdk_port: u16 = 0;

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
            "--duration" => {
                i += 1;
                duration_secs = args[i].parse().expect("invalid duration");
            }
            "--streams" => {
                i += 1;
                streams = args[i].parse().expect("invalid streams");
            }
            "--payload-size" => {
                i += 1;
                payload_size = args[i].parse().expect("invalid payload-size");
            }
            "--eal-args" => {
                i += 1;
                eal_args = Some(args[i].split_whitespace().map(String::from).collect());
            }
            "--dpdk-port" => {
                i += 1;
                dpdk_port = args[i].parse().expect("invalid dpdk-port");
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
    // In an AWS VPC (L3-routed) the gateway MAC is the only valid L2 next hop;
    // without it the provider would broadcast and the VPC would drop the frames.
    if gateway_mac.is_none() {
        eprintln!("Required: --gateway-mac (AWS VPC is L3-routed)");
        std::process::exit(1);
    }

    let cert_pem = std::fs::read_to_string(&cert_pem_path).expect("failed to read cert PEM");
    let server_addr: SocketAddr = format!("{server_ip}:{port}")
        .parse()
        .expect("invalid server addr");
    let bind_addr: SocketAddr = format!("{bind_ip}:0").parse().expect("invalid bind addr");

    // Build the DPDK provider (real NIC via with_dpdk_port -> new_real_nic).
    let mut builder = DpdkProvider::builder()
        .with_addr(bind_addr)
        .with_dpdk_port(dpdk_port)
        .with_gateway_mac(gateway_mac.unwrap());
    if let Some(args) = eal_args {
        builder = builder.with_eal_args(args);
    }
    let (provider, mut handle) = builder.build();

    let client = Client::builder()
        .with_tls(cert_pem.as_str())
        .unwrap()
        .with_io(provider)
        .unwrap()
        .start()
        .unwrap();

    eprintln!("QUIC perf client connecting to {server_addr} (streams={streams}, payload={payload_size}B, duration={duration_secs}s)...");

    let hs_start = Instant::now();
    let connect = Connect::new(server_addr).with_server_name("localhost");
    let mut connection = client.connect(connect).await.unwrap_or_else(|e| {
        eprintln!("FAIL: Connection failed: {e}");
        std::process::exit(1);
    });
    let hs_us = hs_start.elapsed().as_micros() as u64;
    eprintln!("HANDSHAKE_OK latency_us={hs_us}");

    let start = Instant::now();
    let deadline = start + Duration::from_secs(duration_secs);

    // Open all streams up front, then drive each in its own task until the deadline.
    let mut tasks = Vec::new();
    for _ in 0..streams {
        let mut stream = match connection.open_bidirectional_stream().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("WARN: open stream failed: {e}");
                break;
            }
        };
        let payload = bytes::Bytes::from(vec![0xABu8; payload_size]);
        tasks.push(tokio::spawn(async move {
            let mut recv_total: u64 = 0;
            while Instant::now() < deadline {
                if stream.send(payload.clone()).await.is_err() {
                    break;
                }
                // Drain one payload's worth of echoed bytes (goodput).
                let mut got = 0usize;
                while got < payload_size {
                    match stream.receive().await {
                        Ok(Some(chunk)) => {
                            got += chunk.len();
                            recv_total += chunk.len() as u64;
                        }
                        _ => return recv_total,
                    }
                }
            }
            let _ = stream.finish();
            recv_total
        }));
    }

    let mut total_bytes = 0u64;
    for t in tasks {
        if let Ok(b) = t.await {
            total_bytes += b;
        }
    }
    let elapsed = start.elapsed();

    let stats = handle.stats();
    drop(connection);
    handle.shutdown();

    let gbps = throughput_gbps(total_bytes, elapsed.as_secs_f64());
    println!(
        "PERF_RESULT gbps={:.4} bytes={} elapsed_s={:.3} hs_us={} rx_bursts={} tx_bursts={} rx_drops={} tx_drops={}",
        gbps,
        total_bytes,
        elapsed.as_secs_f64(),
        hs_us,
        stats.rx_burst_calls,
        stats.tx_burst_calls,
        stats.rx_drops,
        stats.tx_drops,
    );
}

#[cfg(test)]
mod tests {
    use super::throughput_gbps;

    #[test]
    fn gbps_math() {
        // 1e9 bytes in 8 s = 1 Gbps
        assert!((throughput_gbps(1_000_000_000, 8.0) - 1.0).abs() < 1e-9);
        // zero bytes / zero time are well-defined and don't panic
        assert_eq!(throughput_gbps(0, 5.0), 0.0);
        assert_eq!(throughput_gbps(1_000, 0.0), 0.0);
    }
}
