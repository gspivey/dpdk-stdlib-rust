//! Socket implementations for different backends

use crate::AsyncUdpSocket;
#[cfg(feature = "dpdk")]
use crate::{RecvDropsSnapshot, SocketConfig};
use async_trait::async_trait;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(feature = "dpdk")]
use std::time::Duration;
use tokio::sync::Mutex;

/// Type alias for boxed async UDP sockets
pub type BoxedAsyncUdpSocket = Box<dyn AsyncUdpSocket>;

/// Tokio-based async UDP socket implementation
pub struct TokioUdpSocket {
    inner: Arc<tokio::net::UdpSocket>,
    connected_addr: Arc<Mutex<Option<SocketAddr>>>,
}

impl TokioUdpSocket {
    /// Bind to the given address
    pub async fn bind<A: tokio::net::ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        Ok(Self {
            inner: Arc::new(socket),
            connected_addr: Arc::new(Mutex::new(None)),
        })
    }

    /// Get a reference to the inner Tokio socket
    pub fn inner(&self) -> &tokio::net::UdpSocket {
        &self.inner
    }
}

#[async_trait]
impl AsyncUdpSocket for TokioUdpSocket {
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf).await
    }

    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.inner.send_to(buf, addr).await
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    async fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        self.inner.connect(addr).await?;
        let mut connected = self.connected_addr.lock().await;
        *connected = Some(addr);
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf).await
    }

    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send(buf).await
    }

    fn backend_name(&self) -> &'static str {
        "tokio"
    }
}

// DPDK-based async UDP socket implementation
#[cfg(feature = "dpdk")]
pub struct DpdkUdpSocket {
    inner: Arc<Mutex<dpdk_udp::UdpSocket>>,
    local_addr: SocketAddr,
    connected_addr: Arc<Mutex<Option<SocketAddr>>>,
}

#[cfg(feature = "dpdk")]
impl DpdkUdpSocket {
    /// Bind to the given address
    pub async fn bind<A: std::net::ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let addr = addr.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;

        // DPDK initialization is blocking, run in spawn_blocking
        let socket = tokio::task::spawn_blocking(move || {
            dpdk_udp::UdpSocket::bind(addr)
        }).await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

        Ok(Self {
            local_addr: socket.local_addr()?,
            inner: Arc::new(Mutex::new(socket)),
            connected_addr: Arc::new(Mutex::new(None)),
        })
    }

    /// Bind with custom configuration
    pub async fn bind_with_config<A: std::net::ToSocketAddrs>(
        addr: A,
        _config: &SocketConfig,
    ) -> io::Result<Self> {
        // For now, just use default bind
        // Future: pass EAL args from config
        Self::bind(addr).await
    }
}

#[cfg(feature = "dpdk")]
#[async_trait]
impl AsyncUdpSocket for DpdkUdpSocket {
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            let socket = self.inner.clone();
            let mut buf_owned = buf.to_vec();

            let result = tokio::task::spawn_blocking(move || {
                let socket = socket.blocking_lock();
                let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                let res = socket.recv_from(&mut buf_owned).map(|(len, addr)| (len, addr, buf_owned));
                let _ = socket.set_read_timeout(None);
                res
            }).await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            match result {
                Ok((len, addr, received_buf)) => {
                    buf[..len].copy_from_slice(&received_buf[..len]);
                    return Ok((len, addr));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let socket = self.inner.clone();
        let buf_owned = buf.to_vec();

        tokio::task::spawn_blocking(move || {
            let socket = socket.blocking_lock();
            socket.send_to(&buf_owned, addr)
        }).await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    async fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        let socket = self.inner.clone();

        tokio::task::spawn_blocking(move || {
            let mut socket = socket.blocking_lock();
            socket.connect(addr)
        }).await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

        let mut connected = self.connected_addr.lock().await;
        *connected = Some(addr);
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let socket = self.inner.clone();
            let mut buf_owned = buf.to_vec();

            let result = tokio::task::spawn_blocking(move || {
                let socket = socket.blocking_lock();
                let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                let res = socket.recv(&mut buf_owned).map(|len| (len, buf_owned));
                let _ = socket.set_read_timeout(None);
                res
            }).await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            match result {
                Ok((len, received_buf)) => {
                    buf[..len].copy_from_slice(&received_buf[..len]);
                    return Ok(len);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let socket = self.inner.clone();
        let buf_owned = buf.to_vec();

        tokio::task::spawn_blocking(move || {
            let socket = socket.blocking_lock();
            socket.send(&buf_owned)
        }).await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }

    fn backend_name(&self) -> &'static str {
        "dpdk"
    }

    async fn enable_perf_reporting(&self, interval: Duration) -> io::Result<()> {
        let socket = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let socket = socket.blocking_lock();
            socket.enable_perf_reporting(interval)
        }).await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }

    async fn disable_perf_reporting(&self) {
        let socket = self.inner.clone();
        // spawn_blocking because disable_perf_reporting joins the reporter
        // thread (blocking), and we must not block the async runtime thread.
        // The final `[NIC-FINAL]` stderr line is emitted synchronously as
        // part of PerfReporter::drop() inside this call.
        let _ = tokio::task::spawn_blocking(move || {
            let socket = socket.blocking_lock();
            socket.disable_perf_reporting();
        }).await;
    }

    async fn recv_drops(&self) -> RecvDropsSnapshot {
        let socket = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let socket = socket.blocking_lock();
            let stats = socket.recv_drops();
            RecvDropsSnapshot {
                packets: stats.packets,
                bytes: stats.bytes,
            }
        }).await
            .unwrap_or_default()
    }
}
