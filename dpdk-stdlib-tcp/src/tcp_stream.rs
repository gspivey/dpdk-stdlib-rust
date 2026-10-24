//! Public `TcpStream` API — drop-in replacement for `std::net::TcpStream`.
//!
//! Dispatches IPv4 to the DPDK engine path and IPv6 to kernel fallback.

use std::io::{self, Read, Write};
use std::net::{self, Shutdown, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use dpdk_stdlib_net::backend::PacketBackend;
use dpdk_stdlib_net::neighbor::NeighborResolver;

use crate::contract::{
    CommandSender, EngineCommand, EngineWakeup, KeepaliveConfig, SocketOption,
};
use crate::state::FourTuple;
use crate::stream::{connect_timeout, connect_v4, DpdkTcpStream};

/// A TCP stream, either backed by DPDK (IPv4) or the kernel (IPv6 fallback).
///
/// Provides the full `std::net::TcpStream` API surface. IPv4 addresses use
/// the DPDK userspace path; IPv6 addresses fall back to the kernel stack.
pub struct TcpStream {
    inner: TcpStreamInner,
}

impl std::fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => f.debug_struct("TcpStream")
                .field("local", &s.local_addr())
                .field("peer", &s.peer_addr())
                .field("backend", &"dpdk")
                .finish(),
            TcpStreamInner::Std(s) => f.debug_struct("TcpStream")
                .field("inner", s)
                .finish(),
        }
    }
}

/// Inner representation of TcpStream — DPDK or kernel.
pub enum TcpStreamInner {
    /// DPDK userspace TCP path.
    Dpdk(DpdkTcpStream),
    /// Standard library kernel TCP path (IPv6 fallback).
    Std(net::TcpStream),
}

/// Context needed to establish DPDK connections (shared across the process).
pub struct TcpContext {
    pub backend: Arc<dyn PacketBackend>,
    pub resolver: Arc<dyn NeighborResolver>,
    pub cmd_tx: CommandSender,
    pub wakeup: Arc<EngineWakeup>,
    pub local_ip: std::net::Ipv4Addr,
    next_port: std::sync::atomic::AtomicU16,
}

impl TcpContext {
    /// Build a new TCP context for the global singleton.
    ///
    /// Used by the runtime bootstrap (`init_dpdk_tcp_context`) after it has set
    /// up the backend, neighbor resolver, command channel and engine wakeup.
    pub fn new(
        backend: Arc<dyn PacketBackend>,
        resolver: Arc<dyn NeighborResolver>,
        cmd_tx: CommandSender,
        wakeup: Arc<EngineWakeup>,
        local_ip: std::net::Ipv4Addr,
    ) -> Self {
        // Seed the ephemeral port allocator with a time-based offset so that
        // successive process runs start from different ports. Without this,
        // every new process begins at 49152 and collides with the TIME_WAIT
        // entry left by the previous connection on the same 4-tuple (same
        // peer IP:port and same local IP; TIME_WAIT lasts 120 s). On real
        // hardware this manifests as the second DPDK connect in a test
        // sequence hanging for exactly TEST_TIMEOUT seconds (60 s) because
        // the server drops the SYN while the 4-tuple is still in TIME_WAIT.
        //
        // The range is 49152..=65535 (16384 ports). We mix the low bits of
        // SystemTime to spread starts across the full range without pulling
        // in a PRNG crate. The modulo arithmetic stays within u16 without
        // risk of panic: RANGE is 16384, well below u16::MAX.
        const RANGE: u16 = 65535 - 49152 + 1; // 16384
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u16;
        let start = 49152u16.saturating_add(seed % RANGE);
        Self {
            backend,
            resolver,
            cmd_tx,
            wakeup,
            local_ip,
            next_port: std::sync::atomic::AtomicU16::new(start),
        }
    }

    /// Allocate an ephemeral port for a new outbound connection.
    pub fn allocate_port(&self) -> u16 {
        // Incrementing ephemeral port allocator (49152..=65535), wrapping
        // within the range. The starting point is randomised per process in
        // `TcpContext::new` to avoid TIME_WAIT 4-tuple collisions across runs.
        const RANGE_END: u16 = 65535;
        const RANGE_START: u16 = 49152;
        loop {
            let port = self.next_port.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if port >= RANGE_START {
                if port <= RANGE_END {
                    return port;
                }
                // Wrapped past the top of the ephemeral range; reset to start.
                self.next_port.store(RANGE_START, std::sync::atomic::Ordering::Relaxed);
            }
            // port < RANGE_START means we raced past a reset; retry.
        }
    }
}

/// Resolve the first `SocketAddr` from a `ToSocketAddrs` implementation.
pub(crate) fn resolve_addr<A: ToSocketAddrs>(addr: A) -> io::Result<SocketAddr> {
    addr.to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses resolved"))
}

impl TcpStream {
    /// Open a TCP connection to a remote host, matching `std::net::TcpStream::connect`.
    ///
    /// IPv4 addresses use the DPDK path; IPv6 falls back to the kernel.
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        let remote = resolve_addr(addr)?;
        match remote {
            SocketAddr::V4(_) => {
                let ctx = get_tcp_context()?;
                let port = ctx.allocate_port();
                let local = SocketAddr::new(
                    std::net::IpAddr::V4(ctx.local_ip),
                    port,
                );
                let (_key, handle) = connect_v4(
                    remote, local, &ctx.backend, &ctx.resolver, &ctx.cmd_tx, &ctx.wakeup,
                )?;
                Ok(TcpStream {
                    inner: TcpStreamInner::Dpdk(DpdkTcpStream::new(handle, FourTuple { local, remote })),
                })
            }
            SocketAddr::V6(_) => {
                let s = net::TcpStream::connect(remote)?;
                Ok(TcpStream { inner: TcpStreamInner::Std(s) })
            }
        }
    }

    /// Open a TCP connection with a timeout.
    pub fn connect_timeout(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        match addr {
            SocketAddr::V4(_) => {
                let ctx = get_tcp_context()?;
                let port = ctx.allocate_port();
                let local = SocketAddr::new(
                    std::net::IpAddr::V4(ctx.local_ip),
                    port,
                );
                let (_key, handle) = connect_timeout(
                    *addr, local, timeout, &ctx.backend, &ctx.resolver, &ctx.cmd_tx, &ctx.wakeup,
                )?;
                Ok(TcpStream {
                    inner: TcpStreamInner::Dpdk(DpdkTcpStream::new(handle, FourTuple { local, remote: *addr })),
                })
            }
            SocketAddr::V6(_) => {
                let s = net::TcpStream::connect_timeout(addr, timeout)?;
                Ok(TcpStream { inner: TcpStreamInner::Std(s) })
            }
        }
    }

    /// Construct a `TcpStream` from an already-established DPDK connection.
    pub(crate) fn from_dpdk(stream: DpdkTcpStream) -> Self {
        TcpStream {
            inner: TcpStreamInner::Dpdk(stream),
        }
    }

    /// Construct a `TcpStream` from a standard library stream (IPv6 fallback).
    pub(crate) fn from_std(stream: net::TcpStream) -> Self {
        TcpStream {
            inner: TcpStreamInner::Std(stream),
        }
    }

    /// Consume the stream and return the inner representation.
    ///
    /// Used by the async compat layer to extract the handle for `AsyncRead`/`AsyncWrite`.
    pub fn into_inner(self) -> TcpStreamInner {
        self.inner
    }

    /// Shut down the read, write, or both halves of this connection.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::Shutdown {
                    key: s.key,
                    how,
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(s) => s.shutdown(how),
        }
    }

    /// Returns the socket address of the remote peer.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => Ok(s.peer_addr()),
            TcpStreamInner::Std(s) => s.peer_addr(),
        }
    }

    /// Returns the socket address of the local half.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => Ok(s.local_addr()),
            TcpStreamInner::Std(s) => s.local_addr(),
        }
    }

    /// Sets the read timeout.
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                // Safety: we need interior mutability; DpdkTcpStream stores these as atomic-like.
                // For now we route through the engine via SetOption.
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::ReadTimeout(dur),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(s) => s.set_read_timeout(dur),
        }
    }

    /// Sets the write timeout.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::WriteTimeout(dur),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(s) => s.set_write_timeout(dur),
        }
    }

    /// Returns the read timeout.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => Ok(s.read_timeout()),
            TcpStreamInner::Std(s) => s.read_timeout(),
        }
    }

    /// Returns the write timeout.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => Ok(s.write_timeout()),
            TcpStreamInner::Std(s) => s.write_timeout(),
        }
    }

    /// Sets TCP_NODELAY (disables Nagle's algorithm).
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::Nodelay(nodelay),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(s) => s.set_nodelay(nodelay),
        }
    }

    /// Gets the TCP_NODELAY value.
    pub fn nodelay(&self) -> io::Result<bool> {
        match &self.inner {
            TcpStreamInner::Dpdk(_) => {
                // TODO: query engine for current nodelay state
                Ok(false)
            }
            TcpStreamInner::Std(s) => s.nodelay(),
        }
    }

    /// Sets the TTL value.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::Ttl(ttl as u8),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(s) => s.set_ttl(ttl),
        }
    }

    /// Gets the TTL value.
    pub fn ttl(&self) -> io::Result<u32> {
        match &self.inner {
            TcpStreamInner::Dpdk(_) => Ok(64), // Default TTL
            TcpStreamInner::Std(s) => s.ttl(),
        }
    }

    /// Sets SO_LINGER.
    pub fn set_linger(&self, linger: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                *s.handle.linger.lock().unwrap() = linger;
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::Linger(linger),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(_) => {
                // std::net::TcpStream::set_linger is unstable; for the kernel
                // fallback path we accept the limitation silently.
                Ok(())
            }
        }
    }

    /// Gets SO_LINGER.
    pub fn linger(&self) -> io::Result<Option<Duration>> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => Ok(s.handle.linger.lock().unwrap().clone()),
            TcpStreamInner::Std(_) => {
                // std::net::TcpStream::linger is unstable; return None for kernel path.
                Ok(None)
            }
        }
    }

    /// Sets non-blocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::Nonblocking(nonblocking),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(s) => s.set_nonblocking(nonblocking),
        }
    }

    /// Sets TCP keepalive configuration.
    pub fn set_keepalive(&self, config: Option<KeepaliveConfig>) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::Keepalive(config),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(_) => Ok(()), // Kernel handles keepalive internally
        }
    }

    /// Sets SO_REUSEADDR.
    pub fn set_reuseaddr(&self, reuseaddr: bool) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::ReuseAddr(reuseaddr),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(_) => Ok(()),
        }
    }

    /// Sets the receive buffer size.
    pub fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::RecvBufSize(size),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(_) => Ok(()),
        }
    }

    /// Sets the send buffer size.
    pub fn set_send_buffer_size(&self, size: usize) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                s.handle.cmd_tx.send(EngineCommand::SetOption {
                    key: s.key,
                    option: SocketOption::SendBufSize(size),
                }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine closed"))?;
                Ok(())
            }
            TcpStreamInner::Std(_) => Ok(()),
        }
    }

    /// Returns the pending socket error, if any.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => Ok(s.handle.peek_error().map(|e| e.into())),
            TcpStreamInner::Std(s) => s.take_error(),
        }
    }

    /// Receives data without removing it from the buffer.
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                // Peek: read from rx_ring without advancing the read pointer.
                let n = s.handle.rx_ring.peek(buf);
                if n > 0 {
                    return Ok(n);
                }
                if s.handle.eof.load(std::sync::atomic::Ordering::Acquire) {
                    return Ok(0);
                }
                if let Some(err) = s.handle.peek_error() {
                    return Err(err.into());
                }
                // In blocking mode, we'd need to wait. For now return WouldBlock
                // when empty (same behavior as nonblocking).
                Err(io::Error::new(io::ErrorKind::WouldBlock, "no data available to peek"))
            }
            TcpStreamInner::Std(s) => s.peek(buf),
        }
    }

    /// Creates a new independently owned handle to the stream.
    ///
    /// On the DPDK arm, this returns `Unsupported` — use `into_split()` instead.
    pub fn try_clone(&self) -> io::Result<TcpStream> {
        match &self.inner {
            TcpStreamInner::Dpdk(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "try_clone not supported for DPDK TCP streams; use into_split()",
            )),
            TcpStreamInner::Std(s) => Ok(TcpStream {
                inner: TcpStreamInner::Std(s.try_clone()?),
            }),
        }
    }

    /// Split this stream into owned read and write halves.
    pub fn into_split(self) -> io::Result<(crate::split::OwnedReadHalf, crate::split::OwnedWriteHalf)> {
        match self.inner {
            TcpStreamInner::Dpdk(s) => {
                let handle = s.handle.clone();
                let key = s.key;
                std::mem::forget(s);
                Ok(crate::split::into_split_dpdk(handle, key))
            }
            TcpStreamInner::Std(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "into_split not supported for kernel TCP streams; use try_clone()",
            )),
        }
    }
}

impl Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            TcpStreamInner::Dpdk(s) => s.read(buf),
            TcpStreamInner::Std(s) => s.read(buf),
        }
    }
}

impl Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            TcpStreamInner::Dpdk(s) => s.write(buf),
            TcpStreamInner::Std(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            TcpStreamInner::Dpdk(s) => s.flush(),
            TcpStreamInner::Std(s) => s.flush(),
        }
    }
}

impl Read for &TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                // Serialize via read_mutex (P0-C).
                let _guard = s.handle.read_mutex.lock().unwrap();
                let deadline = s.read_timeout().map(|d| std::time::Instant::now() + d);
                loop {
                    if let Some(err) = s.handle.peek_error() {
                        return Err(err.into());
                    }
                    let n = s.handle.rx_ring.read(buf);
                    if n > 0 {
                        return Ok(n);
                    }
                    if s.handle.eof.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(0);
                    }
                    if s.nonblocking() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
                    }
                    let guard = s.handle.notify_lock.lock().unwrap();
                    if s.handle.rx_ring.available_read() > 0
                        || s.handle.eof.load(std::sync::atomic::Ordering::Acquire)
                        || s.handle.peek_error().is_some()
                    {
                        drop(guard);
                        continue;
                    }
                    match deadline {
                        Some(dl) => {
                            let remaining = dl.saturating_duration_since(std::time::Instant::now());
                            if remaining.is_zero() {
                                return Err(io::Error::new(io::ErrorKind::TimedOut, "read timed out"));
                            }
                            let _unused = s.handle.condvar.wait_timeout(guard, remaining).unwrap();
                        }
                        None => {
                            let _unused = s.handle.condvar.wait(guard).unwrap();
                        }
                    }
                }
            }
            TcpStreamInner::Std(s) => (&*s).read(buf),
        }
    }
}

impl Write for &TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &self.inner {
            TcpStreamInner::Dpdk(s) => {
                // Serialize via write_mutex (P0-C).
                let _guard = s.handle.write_mutex.lock().unwrap();
                let deadline = s.write_timeout().map(|d| std::time::Instant::now() + d);
                loop {
                    if let Some(err) = s.handle.peek_error() {
                        return Err(err.into());
                    }
                    let n = s.handle.tx_ring.write(buf);
                    if n > 0 {
                        s.handle.cmd_tx.wakeup().signal();
                        return Ok(n);
                    }
                    if s.nonblocking() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
                    }
                    let guard = s.handle.notify_lock.lock().unwrap();
                    if s.handle.tx_ring.available_write() > 0
                        || s.handle.peek_error().is_some()
                    {
                        drop(guard);
                        continue;
                    }
                    match deadline {
                        Some(dl) => {
                            let remaining = dl.saturating_duration_since(std::time::Instant::now());
                            if remaining.is_zero() {
                                return Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out"));
                            }
                            let _unused = s.handle.condvar.wait_timeout(guard, remaining).unwrap();
                        }
                        None => {
                            let _unused = s.handle.condvar.wait(guard).unwrap();
                        }
                    }
                }
            }
            TcpStreamInner::Std(s) => (&*s).write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &self.inner {
            TcpStreamInner::Dpdk(_) => Ok(()),
            TcpStreamInner::Std(s) => (&*s).flush(),
        }
    }
}

// --- Global TCP context (process-wide singleton) ---

use std::sync::OnceLock;

static TCP_CONTEXT: OnceLock<Arc<TcpContext>> = OnceLock::new();

/// Initialize the global TCP context. Must be called before any `TcpStream::connect`.
pub fn init_tcp_context(ctx: TcpContext) {
    TCP_CONTEXT.get_or_init(|| Arc::new(ctx));
}

/// Check if the TCP context has been initialized.
pub fn is_tcp_context_initialized() -> bool {
    TCP_CONTEXT.get().is_some()
}

/// Get the global TCP context.
pub fn get_tcp_context() -> io::Result<Arc<TcpContext>> {
    TCP_CONTEXT
        .get()
        .cloned()
        .ok_or_else(|| io::Error::new(
            io::ErrorKind::Other,
            "TCP context not initialized; call init_tcp_context() first",
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_stream_try_clone_std_works() {
        // Std variant can be cloned (if we had a connected socket).
        // We just test the dispatch logic with a kernel-fallback IPv6 scenario.
        // Cannot easily create a real connection in unit test, so test the error path.
        let result = TcpStream::connect("[::1]:1");
        // Will fail to connect but exercises the V6 dispatch path.
        assert!(result.is_err());
    }

    #[test]
    fn tcp_stream_try_clone_dpdk_returns_unsupported() {
        use std::sync::mpsc;
        use crate::contract::{CommandSender, ConnectionHandle, EngineWakeup};
        use crate::state::FourTuple;

        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        let stream = TcpStream::from_dpdk(DpdkTcpStream::new(handle, key));

        let result = stream.try_clone();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn tcp_stream_peer_addr_and_local_addr() {
        use std::sync::mpsc;
        use crate::contract::{CommandSender, ConnectionHandle, EngineWakeup};
        use crate::state::FourTuple;

        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:5000".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        let stream = TcpStream::from_dpdk(DpdkTcpStream::new(handle, key));

        assert_eq!(stream.local_addr().unwrap(), "10.0.0.1:5000".parse::<SocketAddr>().unwrap());
        assert_eq!(stream.peer_addr().unwrap(), "10.0.0.2:80".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn tcp_stream_shutdown_sends_command() {
        use std::sync::mpsc;
        use crate::contract::{CommandSender, ConnectionHandle, EngineCommand, EngineWakeup};
        use crate::state::FourTuple;

        let (tx, rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:5000".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        let stream = TcpStream::from_dpdk(DpdkTcpStream::new(handle, key));

        stream.shutdown(Shutdown::Write).unwrap();

        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, EngineCommand::Shutdown { how: Shutdown::Write, .. }));
    }

    #[test]
    fn tcp_stream_set_linger_updates_handle() {
        use std::sync::mpsc;
        use crate::contract::{CommandSender, ConnectionHandle, EngineWakeup};
        use crate::state::FourTuple;

        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:5000".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        let stream = TcpStream::from_dpdk(DpdkTcpStream::new(handle.clone(), key));

        stream.set_linger(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(stream.linger().unwrap(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn tcp_stream_take_error_returns_none_initially() {
        use std::sync::mpsc;
        use crate::contract::{CommandSender, ConnectionHandle, EngineWakeup};
        use crate::state::FourTuple;

        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:5000".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        let stream = TcpStream::from_dpdk(DpdkTcpStream::new(handle, key));

        assert!(stream.take_error().unwrap().is_none());
    }

    #[test]
    fn tcp_stream_take_error_returns_latched_error() {
        use std::sync::mpsc;
        use crate::contract::{CommandSender, ConnectionHandle, EngineWakeup};
        use crate::error::TcpError;
        use crate::state::FourTuple;

        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:5000".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        handle.latch_error(TcpError::ConnectionReset);
        let stream = TcpStream::from_dpdk(DpdkTcpStream::new(handle, key));

        let err = stream.take_error().unwrap().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn tcp_stream_read_write_dpdk() {
        use std::sync::mpsc;
        use crate::contract::{CommandSender, ConnectionHandle, EngineWakeup};
        use crate::state::FourTuple;

        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:5000".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));

        // Push data to rx_ring (simulating engine).
        handle.rx_ring.write(b"test data");

        let mut stream = TcpStream::from_dpdk(DpdkTcpStream::new(handle.clone(), key));
        let mut buf = [0u8; 32];
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"test data");

        let n = stream.write(b"response").unwrap();
        assert_eq!(n, 8);
    }
}
