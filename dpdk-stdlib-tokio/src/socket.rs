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
// Performance design: no outer mutex (dpdk_udp::UdpSocket synchronizes internally
// with per-direction and per-queue guards) and direct calls (not spawn_blocking)
// for the hot path. This eliminates three
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
// The internal guards are held only briefly and never across an await, so
// calling them from async context is safe.
#[cfg(feature = "dpdk")]
pub struct DpdkUdpSocket {
    inner: Arc<dpdk_udp::UdpSocket>,
    local_addr: SocketAddr,
    connected_addr: Arc<Mutex<Option<SocketAddr>>>,
    idle_backoff: crate::idle_backoff::IdleBackoff,
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
            inner: Arc::new(socket),
            connected_addr: Arc::new(Mutex::new(None)),
            idle_backoff: crate::idle_backoff::IdleBackoff::new(),
        })
    }

    /// Wrap an already-constructed `dpdk_udp::UdpSocket` (e.g. one built with
    /// `UdpSocket::bind_with_backend` over a mock backend).
    ///
    /// Test helper; not part of the documented API.
    #[doc(hidden)]
    pub fn from_udp_socket(socket: dpdk_udp::UdpSocket) -> io::Result<Self> {
        Ok(Self {
            local_addr: socket.local_addr()?,
            inner: Arc::new(socket),
            connected_addr: Arc::new(Mutex::new(None)),
            idle_backoff: crate::idle_backoff::IdleBackoff::new(),
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

    /// Opt in to (or out of) the receive-side idle backoff. **Default: off.**
    ///
    /// `Some(d)` makes `recv_from`/`recv` keep spinning with `yield_now` while packets
    /// flow, and once the socket has seen **no packet for `max(d, 2 ms)`** (values below
    /// 2 ms are treated as 2 ms), escalate to `tokio::time::sleep` in fixed ~1 ms steps.
    /// The idle clock resets the moment a packet arrives, so a loaded receiver never
    /// sleeps and throughput is unaffected. `None` restores the default 0.3.0 behaviour
    /// (`try_recv_from` + `yield_now`, no timer).
    ///
    /// **Latency tradeoff:** tokio's timer resolution is ~1 ms, so the first packet
    /// after an idle period can be delayed by up to ~1-2 ms. Sub-millisecond precision
    /// is not claimed.
    ///
    /// **Requires a time-enabled runtime** (`enable_time` / `enable_all`;
    /// `#[tokio::main]` provides it). On a runtime built without the time driver the
    /// backoff panics exactly like any other `tokio::time` call once it reaches the
    /// sleep stage. Callers who never opt in are unaffected.
    pub fn set_idle_backoff(&self, idle: Option<Duration>) {
        self.idle_backoff.set(idle);
    }

    /// The idle threshold configured with [`set_idle_backoff`](Self::set_idle_backoff),
    /// exactly as passed (before the 2 ms clamp), or `None` when the backoff is off.
    pub fn idle_backoff(&self) -> Option<Duration> {
        self.idle_backoff.get()
    }
}

#[cfg(feature = "dpdk")]
#[async_trait]
impl AsyncUdpSocket for DpdkUdpSocket {
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // try_recv_from does a single non-blocking poll — no spawn_blocking. The socket
        // synchronizes internally (per-direction + queue guards). With the idle backoff
        // off (default) this is exactly the 0.3.0 spin: poll + yield_now.
        crate::idle_backoff::recv_with_backoff(&self.idle_backoff, || self.inner.try_recv_from(buf))
            .await
    }

    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        // Direct call — send_to is CPU-only (frame build + backend.send_frame).
        // No spawn_blocking, no buf.to_vec().
        self.inner.send_to(buf, addr)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    async fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        // connect may trigger ARP resolution (brief block on first call).
        // Use spawn_blocking for safety since ARP waits for a reply.
        let socket = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            socket.connect(addr)
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
        self.inner.send(buf)
    }

    fn backend_name(&self) -> &'static str {
        "dpdk"
    }

    async fn enable_perf_reporting(&self, interval: Duration) -> io::Result<()> {
        // enable_perf_reporting spawns a thread internally — brief call.
        self.inner.enable_perf_reporting(interval)
    }

    async fn disable_perf_reporting(&self) {
        // disable_perf_reporting joins the reporter thread (blocking).
        // Use spawn_blocking so we don't block the async runtime.
        let socket = self.inner.clone();
        let _ = tokio::task::spawn_blocking(move || {
            socket.disable_perf_reporting();
        }).await;
    }

    async fn recv_drops(&self) -> RecvDropsSnapshot {
        let stats = self.inner.recv_drops();
        RecvDropsSnapshot {
            packets: stats.packets,
            bytes: stats.bytes,
        }
    }
}
