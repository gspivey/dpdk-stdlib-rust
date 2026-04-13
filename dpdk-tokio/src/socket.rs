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
//
// Performance design: uses std::sync::Mutex (not tokio::sync::Mutex) and
// direct calls (not spawn_blocking) for the hot path. This eliminates three
// sources of overhead that previously capped throughput at ~50K ops/sec:
//
//   1. spawn_blocking dispatch (~10-20μs per call)
//   2. tokio::sync::Mutex async locking overhead
//   3. buf.to_vec() heap allocation on every send/recv
//
// The underlying dpdk_udp::UdpSocket operations are CPU-only (no kernel I/O):
//   - send_to: builds frame + calls backend.send_frame (non-blocking)
//   - try_recv_from: single poll of backend.recv_frames (non-blocking)
//
// Holding a std::sync::Mutex briefly for these operations is safe in async
// context because the critical section is short and never awaits.
#[cfg(feature = "dpdk")]
pub struct DpdkUdpSocket {
    inner: Arc<std::sync::Mutex<dpdk_udp::UdpSocket>>,
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
            inner: Arc::new(std::sync::Mutex::new(socket)),
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
        // Spin-poll up to RECV_SPIN_COUNT times before yielding to the Tokio
        // scheduler. This mirrors how real Tokio's reactor keeps the task
        // on-CPU while the fd is readable — since DPDK has no fd, we spin
        // instead. At high packet rates (>350K pps) this prevents NIC ring
        // overflow that occurs when yield_now() puts us at the back of the
        // scheduler queue after every empty poll.
        const RECV_SPIN_COUNT: u32 = 64;
        loop {
            for _ in 0..RECV_SPIN_COUNT {
                // try_recv_from does a single non-blocking poll.
                // Scope the lock so MutexGuard is dropped before the await point.
                let result = self.inner.lock().unwrap().try_recv_from(buf)?;
                if let Some(r) = result {
                    return Ok(r);
                }
                std::hint::spin_loop();
            }
            // Yield after RECV_SPIN_COUNT empty polls to let other Tokio tasks run.
            tokio::task::yield_now().await;
        }
    }

    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        // Direct call — send_to is CPU-only (frame build + backend.send_frame).
        // No spawn_blocking, no buf.to_vec().
        self.inner.lock().unwrap().send_to(buf, addr)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    async fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        // connect may trigger ARP resolution (brief block on first call).
        // Use spawn_blocking for safety since ARP waits for a reply.
        let socket = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            socket.lock().unwrap().connect(addr)
        }).await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

        let mut connected = self.connected_addr.lock().await;
        *connected = Some(addr);
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let (len, _addr) = self.recv_from(buf).await?;
        Ok(len)
    }

    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        // Direct call — send is CPU-only on connected socket.
        self.inner.lock().unwrap().send(buf)
    }

    fn backend_name(&self) -> &'static str {
        "dpdk"
    }

    async fn enable_perf_reporting(&self, interval: Duration) -> io::Result<()> {
        // enable_perf_reporting spawns a thread internally — brief call.
        self.inner.lock().unwrap().enable_perf_reporting(interval)
    }

    async fn disable_perf_reporting(&self) {
        // disable_perf_reporting joins the reporter thread (blocking).
        // Use spawn_blocking so we don't block the async runtime.
        let socket = self.inner.clone();
        let _ = tokio::task::spawn_blocking(move || {
            socket.lock().unwrap().disable_perf_reporting();
        }).await;
    }

    async fn recv_drops(&self) -> RecvDropsSnapshot {
        let socket = self.inner.lock().unwrap();
        let stats = socket.recv_drops();
        RecvDropsSnapshot {
            packets: stats.packets,
            bytes: stats.bytes,
        }
    }
}
