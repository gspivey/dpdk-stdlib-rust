use clap::Parser;
use std::net::UdpSocket;

#[derive(Parser)]
#[command(name = "plain-echo")]
#[command(about = "Minimal UDP echo server using std::net (performance baseline)")]
struct Args {
    /// IP address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    ip: String,

    /// Port to bind to
    #[arg(long, default_value_t = 9000)]
    port: u16,
}

/// Build a `host:port` string valid for both IPv4 and IPv6 literals.
/// IPv6 literals must be wrapped in brackets: `[2001:db8::1]:9000`.
fn join_addr(ip: &str, port: u16) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{}]:{}", ip, port)
    } else {
        format!("{}:{}", ip, port)
    }
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let bind_addr = join_addr(&args.ip, args.port);

    let socket = UdpSocket::bind(&bind_addr)?;
    eprintln!("plain-echo listening on {}", socket.local_addr()?);

    let mut buf = [0u8; 10000];
    loop {
        let (len, src) = socket.recv_from(&mut buf)?;
        socket.send_to(&buf[..len], src)?;
    }
}
