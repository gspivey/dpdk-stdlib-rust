//! Drop-in replacement for `tokio::net::UdpSocket`
//!
//! This module provides a `UdpSocket` type that has the exact same async API as
//! `tokio::net::UdpSocket`, but uses DPDK for packet I/O when the `dpdk`
//! feature is enabled.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::task::{Context, Poll};

/// A UDP socket compatible with `tokio::net::UdpSocket`
///
/// This type provides the same async API as Tokio's UdpSocket,
/// allowing it to be used as a drop-in replacement. When compiled with
/// the `dpdk` feature, it will attempt to use DPDK for acceleration,
/// falling back to Tokio sockets if DPDK is unavailable.
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_tokio::compat::tokio::UdpSocket;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let socket = UdpSocket::bind("127.0.0.1:8080").await?;
///     let mut buf = [0; 1024];
///     let (amt, src) = socket.recv_from(&mut buf).await?;
///     socket.send_to(&buf[..amt], src).await?;
///     Ok(())
/// }
/// ```
pub struct UdpSocket {
    inner: UdpSocketInner,
}

enum UdpSocketInner {
    Tokio(::tokio::net::UdpSocket),
    #[cfg(feature = "dpdk")]
    Dpdk(DpdkAsyncSocket),
}

/// DPDK socket wrapper using std::sync::Mutex for fast direct-call hot path.
///
/// The underlying operations are CPU-only (no kernel I/O), so holding a
/// std::sync::Mutex briefly is safe in async context. This eliminates the
/// ~20μs spawn_blocking overhead and per-packet buf.to_vec() allocations.
#[cfg(feature = "dpdk")]
struct DpdkAsyncSocket {
    socket: std::sync::Arc<std::sync::Mutex<dpdk_udp::UdpSocket>>,
    local_addr: SocketAddr,
}

impl UdpSocket {
    /// Creates a UDP socket from the given address.
    ///
    /// Binding with a port number of 0 will request that the OS assigns a port.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use dpdk_tokio::compat::tokio::UdpSocket;
    ///
    /// let socket = UdpSocket::bind("127.0.0.1:0").await?;
    /// ```
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses found"))?;

        #[cfg(feature = "dpdk")]
        {
            // Skip DPDK entirely when linked against the stub backend — a stub
            // socket would bind successfully but never produce/consume any
            // packets, so loopback I/O would silently hang.
            if !dpdk_udp::is_stub() {
                // Try DPDK first (blocking init in spawn_blocking)
                let dpdk_result = ::tokio::task::spawn_blocking(move || {
                    dpdk_udp::UdpSocket::bind(addr)
                }).await;

                match dpdk_result {
                    Ok(Ok(socket)) => {
                        let local_addr = socket.local_addr()?;
                        return Ok(UdpSocket {
                            inner: UdpSocketInner::Dpdk(DpdkAsyncSocket {
                                socket: std::sync::Arc::new(std::sync::Mutex::new(socket)),
                                local_addr,
                            }),
                        });
                    }
                    Ok(Err(e)) => {
                        eprintln!("DPDK bind failed ({}), falling back to tokio", e);
                    }
                    Err(e) => {
                        eprintln!("DPDK spawn_blocking failed ({}), falling back to tokio", e);
                    }
                }
            }
        }

        // Fallback to Tokio socket
        let socket = ::tokio::net::UdpSocket::bind(addr).await?;
        Ok(UdpSocket {
            inner: UdpSocketInner::Tokio(socket),
        })
    }

    /// Creates new UdpSocket from a previously bound std::net::UdpSocket.
    ///
    /// This is useful when you need more control over socket options.
    pub fn from_std(socket: std::net::UdpSocket) -> io::Result<UdpSocket> {
        socket.set_nonblocking(true)?;
        let tokio_socket = ::tokio::net::UdpSocket::from_std(socket)?;
        Ok(UdpSocket {
            inner: UdpSocketInner::Tokio(tokio_socket),
        })
    }

    /// Turns a UdpSocket into a std::net::UdpSocket.
    pub fn into_std(self) -> io::Result<std::net::UdpSocket> {
        match self.inner {
            UdpSocketInner::Tokio(s) => s.into_std(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "DPDK socket cannot be converted to std"))
            }
        }
    }

    /// Returns the local address that this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.local_addr(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => Ok(s.local_addr),
        }
    }

    /// Returns the socket address of the remote peer this socket was connected to.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.peer_addr(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                s.socket.lock().unwrap().peer_addr()
            }
        }
    }

    /// Connects the UDP socket to a remote address.
    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses found"))?;

        match &self.inner {
            UdpSocketInner::Tokio(s) => s.connect(addr).await,
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                let socket = s.socket.clone();
                ::tokio::task::spawn_blocking(move || {
                    socket.lock().unwrap().connect(addr)
                }).await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            }
        }
    }

    /// Sends data on the socket to the remote address that the socket is connected to.
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.send(buf).await,
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                // Direct call — send is CPU-only on connected socket.
                s.socket.lock().unwrap().send(buf)
            }
        }
    }

    /// Receives a single datagram message on the socket from the remote address to
    /// which it is connected.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.recv(buf).await,
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                // Non-blocking poll loop using try_recv_from.
                loop {
                    match s.socket.lock().unwrap().try_recv_from(buf)? {
                        Some((len, _addr)) => return Ok(len),
                        None => ::tokio::task::yield_now().await,
                    }
                }
            }
        }
    }

    /// Sends data on the socket to the given address.
    pub async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], target: A) -> io::Result<usize> {
        let addr = target.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses found"))?;

        match &self.inner {
            UdpSocketInner::Tokio(s) => s.send_to(buf, addr).await,
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                // Direct call — send_to is CPU-only (frame build + backend.send_frame).
                s.socket.lock().unwrap().send_to(buf, addr)
            }
        }
    }

    /// Receives a single datagram message on the socket.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.recv_from(buf).await,
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                // Non-blocking poll loop using try_recv_from — no spawn_blocking,
                // no buf.to_vec(), no 1-second timeout.
                loop {
                    match s.socket.lock().unwrap().try_recv_from(buf)? {
                        Some(result) => return Ok(result),
                        None => ::tokio::task::yield_now().await,
                    }
                }
            }
        }
    }

    /// Attempts to receive a single datagram on the socket.
    ///
    /// This method will poll the socket for readiness before attempting to receive.
    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ::tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.poll_recv_from(cx, buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                // DPDK doesn't integrate with tokio's reactor
                // Return pending and suggest using recv_from().await instead
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "poll_recv_from not supported with DPDK, use recv_from().await",
                )))
            }
        }
    }

    /// Attempts to send data on the socket to a given address.
    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.poll_send_to(cx, buf, target),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "poll_send_to not supported with DPDK, use send_to().await",
                )))
            }
        }
    }

    /// Attempts to receive a single datagram on the socket from the connected address.
    pub fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ::tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.poll_recv(cx, buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "poll_recv not supported with DPDK, use recv().await",
                )))
            }
        }
    }

    /// Attempts to send data on the socket to the connected address.
    pub fn poll_send(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.poll_send(cx, buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "poll_send not supported with DPDK, use send().await",
                )))
            }
        }
    }

    /// Tries to receive a single datagram message on the socket.
    ///
    /// Returns `io::ErrorKind::WouldBlock` if the socket is not ready.
    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.try_recv_from(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                let socket = s.socket.try_lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "socket locked"))?;
                match socket.try_recv_from(buf)? {
                    Some(result) => Ok(result),
                    None => Err(io::Error::new(io::ErrorKind::WouldBlock, "no data available")),
                }
            }
        }
    }

    /// Tries to send data on the socket to the given address.
    ///
    /// Returns `io::ErrorKind::WouldBlock` if the socket is not ready.
    pub fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.try_send_to(buf, target),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                let socket = s.socket.try_lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "socket locked"))?;
                socket.send_to(buf, target)
            }
        }
    }

    /// Tries to receive a single datagram on the socket from the connected address.
    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.try_recv(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                let socket = s.socket.try_lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "socket locked"))?;
                match socket.try_recv_from(buf)? {
                    Some((len, _addr)) => Ok(len),
                    None => Err(io::Error::new(io::ErrorKind::WouldBlock, "no data available")),
                }
            }
        }
    }

    /// Tries to send data on the socket to the connected address.
    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.try_send(buf),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => {
                let socket = s.socket.try_lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "socket locked"))?;
                socket.send(buf)
            }
        }
    }

    /// Waits for the socket to become readable.
    pub async fn readable(&self) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.readable().await,
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                // DPDK is always "ready" - it just polls
                Ok(())
            }
        }
    }

    /// Waits for the socket to become writable.
    pub async fn writable(&self) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.writable().await,
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                // DPDK is always "ready" - it just polls
                Ok(())
            }
        }
    }

    /// Sets the value of the `SO_BROADCAST` option for this socket.
    pub fn set_broadcast(&self, on: bool) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.set_broadcast(on),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(()),
        }
    }

    /// Gets the value of the `SO_BROADCAST` option for this socket.
    pub fn broadcast(&self) -> io::Result<bool> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.broadcast(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(false),
        }
    }

    /// Sets the value of the `IP_TTL` option for this socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.set_ttl(ttl),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(()),
        }
    }

    /// Gets the value of the `IP_TTL` option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.ttl(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(64),
        }
    }

    /// Executes an operation of the `IP_ADD_MEMBERSHIP` type.
    pub fn join_multicast_v4(&self, multiaddr: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.join_multicast_v4(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "multicast not supported with DPDK"))
            }
        }
    }

    /// Executes an operation of the `IPV6_ADD_MEMBERSHIP` type.
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.join_multicast_v6(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "multicast not supported with DPDK"))
            }
        }
    }

    /// Executes an operation of the `IP_DROP_MEMBERSHIP` type.
    pub fn leave_multicast_v4(&self, multiaddr: Ipv4Addr, interface: Ipv4Addr) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.leave_multicast_v4(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(()),
        }
    }

    /// Executes an operation of the `IPV6_DROP_MEMBERSHIP` type.
    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.leave_multicast_v6(multiaddr, interface),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(()),
        }
    }

    /// Gets the value of the `IP_MULTICAST_LOOP` option for this socket.
    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.multicast_loop_v4(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(false),
        }
    }

    /// Sets the value of the `IP_MULTICAST_LOOP` option for this socket.
    pub fn set_multicast_loop_v4(&self, on: bool) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.set_multicast_loop_v4(on),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(()),
        }
    }

    /// Gets the value of the `IP_MULTICAST_TTL` option for this socket.
    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.multicast_ttl_v4(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(1),
        }
    }

    /// Sets the value of the `IP_MULTICAST_TTL` option for this socket.
    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.set_multicast_ttl_v4(ttl),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(()),
        }
    }

    /// Gets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.multicast_loop_v6(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(false),
        }
    }

    /// Sets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    pub fn set_multicast_loop_v6(&self, on: bool) -> io::Result<()> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.set_multicast_loop_v6(on),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(_) => Ok(()),
        }
    }

    /// Gets the value of the `SO_ERROR` option on this socket.
    ///
    /// Returns the first pending ICMP error (Destination Unreachable, Time
    /// Exceeded, etc.) and removes it from the queue, or `Ok(None)` if no
    /// errors are pending.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        match &self.inner {
            UdpSocketInner::Tokio(s) => s.take_error(),
            #[cfg(feature = "dpdk")]
            UdpSocketInner::Dpdk(s) => s.socket.lock().unwrap().take_error(),
        }
    }

    /// Returns the backend being used by this socket.
    ///
    /// This is an extension method not present in `tokio::net::UdpSocket`.
    pub fn backend(&self) -> &'static str {
        match &self.inner {
            UdpSocketInner::Tokio(_) => "tokio",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[::tokio::test]
    async fn test_bind_and_local_addr() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        assert_eq!(addr.ip(), std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[::tokio::test]
    async fn test_send_recv() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send from client to server
        let msg = b"hello async";
        client.send_to(msg, server_addr).await.unwrap();

        // Receive on server
        let mut buf = [0u8; 1024];
        let (len, from) = server.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], msg);
        assert_eq!(from, client.local_addr().unwrap());
    }

    #[::tokio::test]
    async fn test_backend() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Without dpdk feature, should be "tokio"
        #[cfg(not(feature = "dpdk"))]
        assert_eq!(socket.backend(), "tokio");
    }
}
