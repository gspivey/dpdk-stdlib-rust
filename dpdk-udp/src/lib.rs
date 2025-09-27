use std::io;
use std::net::{SocketAddr, ToSocketAddrs};

/// Drop-in replacement for std::net::UdpSocket with DPDK acceleration
pub struct UdpSocket {
    local_addr: SocketAddr,
    connected_addr: Option<SocketAddr>,
}

impl UdpSocket {
    /// Creates a UDP socket from the given address.
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;
        
        // Try to initialize DPDK
        match dpdk::Eal::init(&["-l", "0", "-n", "4"]) {
            Ok(_) => {
                println!("✅ DPDK EAL initialized for {}", addr);
                Ok(UdpSocket { 
                    local_addr: addr,
                    connected_addr: None,
                })
            }
            Err(_) => {
                Err(io::Error::new(io::ErrorKind::Other, "DPDK initialization failed"))
            }
        }
    }

    /// Receives a single datagram message on the socket.
    pub fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // TODO: Implement DPDK packet receive
        todo!("DPDK recv_from implementation")
    }

    /// Sends data on the socket to the given address.
    pub fn send_to<A: ToSocketAddrs>(&self, _buf: &[u8], _addr: A) -> io::Result<usize> {
        // TODO: Implement DPDK packet send
        todo!("DPDK send_to implementation")
    }

    /// Returns the socket address that this socket was created from.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    /// Connects this UDP socket to a remote address.
    pub fn connect<A: ToSocketAddrs>(&mut self, addr: A) -> io::Result<()> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;
        self.connected_addr = Some(addr);
        Ok(())
    }

    /// Receives a single datagram message on the socket from the remote address to which it is connected.
    pub fn recv(&self, _buf: &mut [u8]) -> io::Result<usize> {
        // TODO: Implement DPDK connected recv
        todo!("DPDK recv implementation")
    }

    /// Sends data on the socket to the remote address to which it is connected.
    pub fn send(&self, _buf: &[u8]) -> io::Result<usize> {
        // TODO: Implement DPDK connected send
        todo!("DPDK send implementation")
    }
}

// ============================================================================
// SYNTHETIC TESTING UTILITIES (separate from main API)
// ============================================================================

use thiserror::Error;

#[derive(Error, Debug)]
pub enum UdpError {
    #[error("Invalid packet format")]
    InvalidPacket,
    #[error("Checksum mismatch")]
    ChecksumMismatch,
    #[error("Packet too short: expected at least {expected}, got {actual}")]
    PacketTooShort { expected: usize, actual: usize },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type UdpResult<T> = Result<T, UdpError>;

pub trait UdpHandler {
    fn on_packet(&self, src_ip: [u8;4], src_port: u16, dst_ip: [u8;4], dst_port: u16, payload: &[u8]) -> Option<Vec<u8>>;
}

/// Synthetic packet processor for testing protocol logic without real networking
pub struct SyntheticUdpSocket {
    bind_ip: [u8; 4],
    bind_port: u16,
    handler: Box<dyn UdpHandler>,
}

impl SyntheticUdpSocket {
    pub fn new(bind_ip: [u8; 4], bind_port: u16, handler: Box<dyn UdpHandler>) -> Self {
        Self { bind_ip, bind_port, handler }
    }

    pub fn parse_and_handle(&self, frame: &[u8]) -> UdpResult<Option<Vec<u8>>> {
        if frame.len() < 14 + 20 + 8 {
            return Err(UdpError::PacketTooShort { expected: 42, actual: frame.len() });
        }

        let ip_header = &frame[14..34];
        let udp_header = &frame[34..42];
        let payload = &frame[42..];

        if ip_header[9] != 17 {
            return Ok(None);
        }

        let src_ip = [ip_header[12], ip_header[13], ip_header[14], ip_header[15]];
        let dst_ip = [ip_header[16], ip_header[17], ip_header[18], ip_header[19]];
        let src_port = u16::from_be_bytes([udp_header[0], udp_header[1]]);
        let dst_port = u16::from_be_bytes([udp_header[2], udp_header[3]]);

        if dst_ip != self.bind_ip || dst_port != self.bind_port {
            return Ok(None);
        }

        if let Some(response_payload) = self.handler.on_packet(src_ip, src_port, dst_ip, dst_port, payload) {
            let mut response_frame = vec![0u8; 14 + 20 + 8 + response_payload.len()];
            
            // Ethernet header (swap src/dst)
            response_frame[0..6].copy_from_slice(&frame[6..12]);
            response_frame[6..12].copy_from_slice(&frame[0..6]);
            response_frame[12..14].copy_from_slice(&frame[12..14]);
            
            // IP header
            response_frame[14] = 0x45;
            let total_len = (20 + 8 + response_payload.len()) as u16;
            response_frame[16..18].copy_from_slice(&total_len.to_be_bytes());
            response_frame[23] = 17;
            response_frame[26..30].copy_from_slice(&dst_ip);
            response_frame[30..34].copy_from_slice(&src_ip);
            
            // UDP header
            response_frame[34..36].copy_from_slice(&dst_port.to_be_bytes());
            response_frame[36..38].copy_from_slice(&src_port.to_be_bytes());
            let udp_len = (8 + response_payload.len()) as u16;
            response_frame[38..40].copy_from_slice(&udp_len.to_be_bytes());
            
            // Payload
            response_frame[42..].copy_from_slice(&response_payload);
            
            return Ok(Some(response_frame));
        }

        Ok(None)
    }
}
