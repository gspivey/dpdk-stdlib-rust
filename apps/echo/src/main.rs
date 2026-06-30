use clap::Parser;
use std::io;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ---- Only difference from plain-echo: import dpdk_udp instead of std::net ----
use dpdk_udp::UdpSocket;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// Install `signal_handler` for SIGTERM and SIGINT via `sigaction`.
///
/// The old `libc::signal(sig, fn as *const () as sighandler_t)` cast is
/// brittle on some toolchains: with certain codegen paths the resulting
/// `usize` doesn't match the actual function address, leaving the default
/// handler in place, which means SIGTERM terminates the process without
/// running any destructors — and in particular without running
/// `PerfReporter::drop`, so the one-shot `[NIC-FINAL]` log line the perf
/// harness relies on is never emitted. `sigaction` takes a typed
/// `sa_sigaction`/`sa_handler` field and avoids the cast entirely.
fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = MaybeUninit::zeroed().assume_init();
        sa.sa_sigaction = signal_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

#[derive(Parser)]
#[command(name = "echo")]
#[command(about = "UDP echo server using dpdk-udp (DPDK-accelerated drop-in for std::net)")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,

    /// Performance reporting interval in seconds (0 = disabled)
    #[arg(long, default_value_t = 0)]
    perf_interval: u64,

    /// Gateway MAC (AA:BB:CC:DD:EE:FF) for AWS VPC (L3-routed). For an IPv6
    /// bind this seeds the NDP neighbor cache (paired with `--peer-ip`) so the
    /// first echo reply is routed via the gateway instead of broadcasting.
    /// Inbound traffic also auto-learns the gateway MAC, so this only guards
    /// the first-packet race.
    #[arg(long)]
    gateway_mac: Option<String>,

    /// Peer IPv6 address to associate with `--gateway-mac` in the NDP cache
    /// (e.g. the traffic generator's address). Ignored without `--gateway-mac`
    /// or on an IPv4 bind.
    #[arg(long)]
    peer_ip: Option<String>,
}

/// Build a `host:port` string valid for both IPv4 and IPv6 literals.
/// IPv6 literals must be wrapped in brackets: `[2001:db8::1]:9000`. A bare
/// `format!("{}:{}", ip, port)` produces an unparsable `2001:db8::1:9000`.
fn join_addr(ip: &str, port: u16) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{}]:{}", ip, port)
    } else {
        format!("{}:{}", ip, port)
    }
}

/// Parse a colon-separated MAC string (`AA:BB:CC:DD:EE:FF`) into 6 octets.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(mac)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_signal_handlers();

    let args = Args::parse();
    let bind_addr = join_addr(&args.ip, args.port);

    let socket = UdpSocket::bind(&bind_addr)?;
    let rt = socket.routing_table();
    eprintln!("echo listening on {} (MTU={}, max_udp_payload={})",
        socket.local_addr()?, rt.mtu(), rt.max_udp_payload());

    // For an IPv6 bind in an AWS VPC, seed the NDP cache with the gateway MAC
    // for the peer so the first echo reply is routed via the gateway instead of
    // broadcasting (which the VPC drops). RX-learning handles the steady state.
    if let (Some(mac_str), Some(peer_str)) = (args.gateway_mac.as_deref(), args.peer_ip.as_deref()) {
        match (parse_mac(mac_str), peer_str.parse::<std::net::Ipv6Addr>()) {
            (Some(mac), Ok(peer)) => {
                socket.add_ndp_entry(peer, mac);
                eprintln!("seeded NDP: {} -> {}", peer, mac_str);
            }
            (None, _) => eprintln!("warning: invalid --gateway-mac '{}', skipping NDP seed", mac_str),
            (_, Err(_)) => eprintln!("warning: --peer-ip '{}' is not an IPv6 address, skipping NDP seed", peer_str),
        }
    }

    if args.perf_interval > 0 {
        socket.enable_perf_reporting(Duration::from_secs(args.perf_interval))?;
    }

    socket.set_read_timeout(Some(Duration::from_millis(500)))?;

    let mut buf = [0u8; 10000];
    while !SHUTDOWN.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                let _ = socket.send_to(&buf[..len], src);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("recv error: {}", e);
                break;
            }
        }
    }

    println!("Shutting down gracefully...");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{join_addr, parse_mac};
    use std::net::ToSocketAddrs;

    #[test]
    fn join_addr_brackets_ipv6_literal() {
        assert_eq!(join_addr("2001:db8::1", 9000), "[2001:db8::1]:9000");
        assert_eq!(join_addr("10.0.1.5", 9000), "10.0.1.5:9000");
        assert_eq!(join_addr("0.0.0.0", 9000), "0.0.0.0:9000");
        // An already-bracketed literal is not double-wrapped.
        assert_eq!(join_addr("[2001:db8::1]", 9000), "[2001:db8::1]:9000");
        // Both families must produce something ToSocketAddrs can parse.
        assert!(join_addr("2001:db8::1", 9000).to_socket_addrs().is_ok());
        assert!(join_addr("10.0.1.5", 9000).to_socket_addrs().is_ok());
    }

    #[test]
    fn parse_mac_roundtrip() {
        assert_eq!(parse_mac("de:ad:be:ef:00:01"), Some([0xde, 0xad, 0xbe, 0xef, 0x00, 0x01]));
        assert_eq!(parse_mac("00:00:00:00:00:00"), Some([0u8; 6]));
        assert_eq!(parse_mac("not-a-mac"), None);
        assert_eq!(parse_mac("de:ad:be:ef:00"), None); // too few octets
        assert_eq!(parse_mac("zz:ad:be:ef:00:01"), None); // invalid hex
    }
}
