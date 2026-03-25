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

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let bind_addr = format!("{}:{}", args.ip, args.port);

    let socket = UdpSocket::bind(&bind_addr)?;
    eprintln!("plain-echo listening on {}", socket.local_addr()?);

    let mut buf = [0u8; 2048];
    loop {
        let (len, src) = socket.recv_from(&mut buf)?;
        socket.send_to(&buf[..len], src)?;
    }
}
