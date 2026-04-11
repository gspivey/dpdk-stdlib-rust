//! Drop-in replacement for `std::net::UdpSocket`
//!
//! This module provides a `UdpSocket` type that has the exact same API as
//! `std::net::UdpSocket`, but uses DPDK for packet I/O when the `dpdk`
//! feature is enabled.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

/// A UDP socket compatible with `std::net::UdpSocket`
///
/// This type provides the same API as the standard library's UdpSocket,
/// allowing it to be used as a drop-in replacement. When compiled with
/// the `dpdk` feature, it will attempt to use DPDK for acceleration,
/// falling back to standard sockets if DPDK is unavailable.
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_tokio::compat::net::UdpSocket;
///
/// let socket = UdpSocket::bind("127.0.0.1:8080")?;
/// let mut buf = [0; 1024];
/// let (amt, src) = socket.recv_from(&mut buf)?;
/// socket.send_to(&buf[..amt], src)?;
/// ```
pub struct UdpSocket {
    inner: UdpSocketInner,
}

enum UdpSocketInner {
    Std(std::net::UdpSocket),
    #[cfg(feature = "dpdk")]
    Dpdk(dpdk_udp::UdpSocket),
}

impl UdpSocket {
    /// Creates a UDP socket from the given address.
    ///
    /// The address type can be any implementor of [`ToSocketAddrs`] trait.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use dpdk_tokio::compat::net::UdpSocket;
    ///
    /// let socket = UdpSocket::bind("127.0.0.1:0")?;
    /// ```
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses found"))?;

        #[cfg(feature = "dpdk")]
        {
            // Skip DPDK entirely when linked against the stub backend — a stub
            // socket would bind successfully but never produce/consume any
            // packets, so loopback I/O would silently hang.
            if !dpdk_udp::is_stub() {
                // Try DPDK first
                match dpdk_udp::UdpSocket::bind(addr) {
                    Ok(socket) => {
                        return Ok(UdpSocket {
                            inner: UdpSocketInner::Dpdk(socket),
                        });
                    }
                    Err(e) => {
                        eprintln!("DPDK bind failed ({}), falling back to std", e);
                    }
                }
            }
        }

        // Fallback to standard socket
        let socket = std::net::UdpSocket::bind(addr)?;
        Ok(UdpSocket {
            inner: UdpSocketInner::Std(socket),
        })
    }

    /// Receives a single datagram message on the socket.
    ///
    /// On success, returns the number of bytes read and the origin.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.recv_from(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.recv_from(buf),
        }
    }

    /// Like `recv_from`, except that it receives into a slice of buffers.
    #[cfg(unix)]
    pub fn recv_from_vectored(&self, _bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<(usize, SocketAddr)> {
        // This is a newer API - provide basic implementation
        Err(io::Error::new(io::ErrorKind::Unsupported, "vectored I/O not supported"))
    }

    /// Receives a single datagram message on the socket, without removing it from the queue.
    pub fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.peek_from(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => {
                // DPDK doesn't support peek - fall back to error
                Err(io::Error::new(io::ErrorKind::Unsupported, "peek not supported with DPDK"))
            }
        }
    }

    /// Sends data on the socket to the given address.
    ///
    /// On success, returns the number of bytes written.
    pub fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses found"))?;

        match &self.inner {
            UdpSocketInner::Std(s) => s.send_to(buf, addr),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.send_to(buf, addr),
        }
    }

    /// Returns the socket address of the remote peer this socket was connected to.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.peer_addr(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.peer_addr(),
        }
    }

    /// Returns the socket address that this socket was created from.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.local_addr(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.local_addr(),
        }
    }

    /// Creates a new independently owned handle to the underlying socket.
    pub fn try_clone(&self) -> io::Result<UdpSocket> {
        match &self.inner {
            UdpSocketInner::Std(s) => Ok(UdpSocket {
                inner: UdpSocketInner::Std(s.try_clone()?),
            }),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "DPDK sockets cannot be cloned"))
            }
        }
    }

    /// Sets the read timeout to the timeout specified.
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_read_timeout(dur),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => {
                // DPDK uses polling, timeout not directly applicable
                Ok(())
            }
        }
    }

    /// Sets the write timeout to the timeout specified.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_write_timeout(dur),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()),
        }
    }

    /// Returns the read timeout of this socket.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.read_timeout(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(None),
        }
    }

    /// Returns the write timeout of this socket.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.write_timeout(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(None),
        }
    }

    /// Sets the value of the `SO_BROADCAST` option for this socket.
    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_broadcast(broadcast),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()), // DPDK handles this differently
        }
    }

    /// Gets the value of the `SO_BROADCAST` option for this socket.
    pub fn broadcast(&self) -> io::Result<bool> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.broadcast(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(false),
        }
    }

    /// Sets the value of the `IP_MULTICAST_LOOP` option for this socket.
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_multicast_loop_v4(multicast_loop_v4),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()),
        }
    }

    /// Gets the value of the `IP_MULTICAST_LOOP` option for this socket.
    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.multicast_loop_v4(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(false),
        }
    }

    /// Sets the value of the `IP_MULTICAST_TTL` option for this socket.
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_multicast_ttl_v4(multicast_ttl_v4),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()),
        }
    }

    /// Gets the value of the `IP_MULTICAST_TTL` option for this socket.
    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.multicast_ttl_v4(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(1),
        }
    }

    /// Sets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_multicast_loop_v6(multicast_loop_v6),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()),
        }
    }

    /// Gets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.multicast_loop_v6(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(false),
        }
    }

    /// Sets the value of the `IP_TTL` option for this socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_ttl(ttl),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()),
        }
    }

    /// Gets the value of the `IP_TTL` option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.ttl(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(64),
        }
    }

    /// Executes an operation of the `IP_ADD_MEMBERSHIP` type.
    pub fn join_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.join_multicast_v4(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "multicast not supported with DPDK"))
            }
        }
    }

    /// Executes an operation of the `IPV6_ADD_MEMBERSHIP` type.
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.join_multicast_v6(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "multicast not supported with DPDK"))
            }
        }
    }

    /// Executes an operation of the `IP_DROP_MEMBERSHIP` type.
    pub fn leave_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.leave_multicast_v4(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()),
        }
    }

    /// Executes an operation of the `IPV6_DROP_MEMBERSHIP` type.
    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.leave_multicast_v6(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(()),
        }
    }

    /// Gets the value of the `SO_ERROR` option on this socket.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.take_error(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => Ok(None),
        }
    }

    /// Connects this UDP socket to a remote address.
    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses found"))?;

        match &self.inner {
            UdpSocketInner::Std(s) => s.connect(addr),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.connect(addr),
        }
    }

    /// Sends data on the socket to the remote address to which it is connected.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.send(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.send(buf),
        }
    }

    /// Receives a single datagram message on the socket from the remote address
    /// to which it is connected.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.recv(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.recv(buf),
        }
    }

    /// Receives single datagram on the socket from the remote address to which
    /// it is connected, without removing the message from input queue.
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.peek(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "peek not supported with DPDK"))
            }
        }
    }

    /// Moves this UDP socket into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Std(s) => s.set_nonblocking(nonblocking),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_s) => {
                // DPDK is always non-blocking by nature
                let _ = nonblocking;
                Ok(())
            }
        }
    }

    /// Returns the backend being used by this socket.
    ///
    /// This is an extension method not present in `std::net::UdpSocket`.
    pub fn backend(&self) -> &'static str {
        match &self.inner {
            UdpSocketInner::Std(_) => "std",
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => "dpdk",
        }
    }
}

impl std::fmt::Debug for UdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let addr = self.local_addr().ok();
        f.debug_struct("UdpSocket")
            .field("backend", &self.backend())
            .field("addr", &addr)
            .finish()
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match &self.inner {
            UdpSocketInner::Std(s) => std::os::unix::io::AsRawFd::as_raw_fd(s),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => -1, // DPDK doesn't use file descriptors
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bind_and_local_addr() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        assert_eq!(addr.ip(), std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn test_send_recv() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();

        // Send from client to server
        let msg = b"hello";
        client.send_to(msg, server_addr).unwrap();

        // Receive on server
        let mut buf = [0u8; 1024];
        let (len, from) = server.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..len], msg);
        assert_eq!(from, client.local_addr().unwrap());
    }

    #[test]
    fn test_backend() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Without dpdk feature, should be "std"
        #[cfg(not(feature = "dpdk"))]
        assert_eq!(socket.backend(), "std");
    }
}
