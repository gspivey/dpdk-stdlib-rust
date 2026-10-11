//! Async TCP compat layer — drop-in replacement for `tokio::net::TcpStream`.
//!
//! Implements `AsyncRead` and `AsyncWrite` using register-first-then-recheck
//! pattern with per-TCB `AtomicWaker`. DPDK-first for IPv4, tokio fallback for IPv6.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(feature = "dpdk")]
use std::net::Shutdown;
#[cfg(feature = "dpdk")]
use std::sync::atomic::Ordering;
#[cfg(feature = "dpdk")]
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::compat::tcp_split::{OwnedReadHalf, OwnedWriteHalf};

/// Async TCP stream — drop-in replacement for `tokio::net::TcpStream`.
///
/// IPv4 addresses use the DPDK userspace path; IPv6 falls back to tokio.
pub struct TcpStream {
    pub(crate) inner: TcpStreamInner,
}

pub(crate) enum TcpStreamInner {
    #[cfg(feature = "dpdk")]
    Dpdk(DpdkAsyncTcpStream),
    Tokio(::tokio::net::TcpStream),
}

/// DPDK-backed async TCP stream state.
#[cfg(feature = "dpdk")]
pub(crate) struct DpdkAsyncTcpStream {
    pub(crate) handle: Arc<dpdk_stdlib_tcp::contract::ConnectionHandle>,
    pub(crate) key: dpdk_stdlib_tcp::state::FourTuple,
}

impl TcpStream {
    /// Open an async TCP connection. DPDK-first for IPv4, tokio fallback for IPv6.
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        let remote = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses resolved"))?;

        match remote {
            #[cfg(feature = "dpdk")]
            SocketAddr::V4(_) => {
                if dpdk_stdlib_tcp::is_tcp_context_initialized() {
                    let stream = ::tokio::task::spawn_blocking(move || {
                        dpdk_stdlib_tcp::TcpStream::connect(remote)
                    })
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

                    match stream.into_inner() {
                        dpdk_stdlib_tcp::TcpStreamInner::Dpdk(dpdk_stream) => {
                            let handle = dpdk_stream.handle.clone();
                            let key = dpdk_stream.key;
                            // Forget the DpdkTcpStream to prevent its Drop from
                            // decrementing app_refcount — we take ownership.
                            std::mem::forget(dpdk_stream);
                            Ok(TcpStream {
                                inner: TcpStreamInner::Dpdk(DpdkAsyncTcpStream {
                                    handle,
                                    key,
                                }),
                            })
                        }
                        dpdk_stdlib_tcp::TcpStreamInner::Std(std_stream) => {
                            std_stream.set_nonblocking(true)?;
                            let tokio_stream = ::tokio::net::TcpStream::from_std(std_stream)?;
                            Ok(TcpStream {
                                inner: TcpStreamInner::Tokio(tokio_stream),
                            })
                        }
                    }
                } else {
                    let stream = ::tokio::net::TcpStream::connect(remote).await?;
                    Ok(TcpStream {
                        inner: TcpStreamInner::Tokio(stream),
                    })
                }
            }
            _ => {
                let stream = ::tokio::net::TcpStream::connect(remote).await?;
                Ok(TcpStream {
                    inner: TcpStreamInner::Tokio(stream),
                })
            }
        }
    }

    /// Returns the local address of this stream.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            #[cfg(feature = "dpdk")]
            TcpStreamInner::Dpdk(s) => Ok(s.key.local),
            TcpStreamInner::Tokio(s) => s.local_addr(),
        }
    }

    /// Returns the remote address of this stream.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            #[cfg(feature = "dpdk")]
            TcpStreamInner::Dpdk(s) => Ok(s.key.remote),
            TcpStreamInner::Tokio(s) => s.peer_addr(),
        }
    }

    /// Split this stream into owned read and write halves.
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        match self.inner {
            #[cfg(feature = "dpdk")]
            TcpStreamInner::Dpdk(s) => {
                let handle = s.handle.clone();
                let key = s.key;
                // Set refcount to 2 for the two halves
                handle.app_refcount.store(2, Ordering::Release);
                // Forget s to prevent its Drop from decrementing app_refcount
                std::mem::forget(s);
                (
                    OwnedReadHalf::new_dpdk(handle.clone(), key),
                    OwnedWriteHalf::new_dpdk(handle, key),
                )
            }
            TcpStreamInner::Tokio(s) => {
                let (rh, wh) = s.into_split();
                (
                    OwnedReadHalf::new_tokio(rh),
                    OwnedWriteHalf::new_tokio(wh),
                )
            }
        }
    }
}

// We need mutable access through Pin for the tokio inner
// The DPDK inner doesn't need &mut since all state is in Arc<ConnectionHandle>

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // SAFETY: TcpStream is Unpin (no self-referential fields)
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            TcpStreamInner::Dpdk(dpdk) => poll_read_dpdk(&dpdk.handle, cx, buf),
            TcpStreamInner::Tokio(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            TcpStreamInner::Dpdk(dpdk) => poll_write_dpdk(&dpdk.handle, cx, data),
            TcpStreamInner::Tokio(s) => Pin::new(s).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            TcpStreamInner::Dpdk(_) => Poll::Ready(Ok(())),
            TcpStreamInner::Tokio(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            TcpStreamInner::Dpdk(dpdk) => {
                let _ = dpdk.handle.cmd_tx.send(
                    dpdk_stdlib_tcp::contract::EngineCommand::Shutdown {
                        key: dpdk.key,
                        how: Shutdown::Write,
                    },
                );
                Poll::Ready(Ok(()))
            }
            TcpStreamInner::Tokio(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

// --- Shared poll helpers (used by TcpStream and OwnedReadHalf/OwnedWriteHalf) ---

/// Register-first-then-recheck poll_read for DPDK TCP.
#[cfg(feature = "dpdk")]
pub(crate) fn poll_read_dpdk(
    handle: &Arc<dpdk_stdlib_tcp::contract::ConnectionHandle>,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<()>> {
    // 1. Register waker FIRST (before any state check)
    handle.read_waker.register(cx.waker());

    // 2. Check sticky error
    if let Some(err) = handle.peek_error() {
        return Poll::Ready(Err(err.into()));
    }

    // 3. Try non-blocking read from SpscByteRing
    let unfilled = buf.initialize_unfilled();
    let n = handle.rx_ring.read(unfilled);
    if n > 0 {
        buf.advance(n);
        return Poll::Ready(Ok(()));
    }

    // 4. Check explicit EOF
    if handle.eof.load(Ordering::Acquire) {
        return Poll::Ready(Ok(()));
    }

    // 5. Waker registered, ring empty, not EOF → Pending
    Poll::Pending
}

/// Register-first-then-recheck poll_write for DPDK TCP.
#[cfg(feature = "dpdk")]
pub(crate) fn poll_write_dpdk(
    handle: &Arc<dpdk_stdlib_tcp::contract::ConnectionHandle>,
    cx: &mut Context<'_>,
    data: &[u8],
) -> Poll<io::Result<usize>> {
    // 1. Register waker FIRST
    handle.write_waker.register(cx.waker());

    // 2. Check sticky error
    if let Some(err) = handle.peek_error() {
        return Poll::Ready(Err(err.into()));
    }

    // 3. Try non-blocking write to SpscByteRing
    let n = handle.tx_ring.write(data);
    if n > 0 {
        handle.cmd_tx.wakeup().signal();
        return Poll::Ready(Ok(n));
    }

    // 4. Ring full → Pending (engine will wake after draining)
    Poll::Pending
}

#[cfg(feature = "dpdk")]
impl Drop for DpdkAsyncTcpStream {
    fn drop(&mut self) {
        if self.handle.app_refcount.fetch_sub(1, Ordering::AcqRel) == 1 {
            let linger = self.handle.linger.lock().unwrap().clone();
            let _ = self.handle.cmd_tx.send(
                dpdk_stdlib_tcp::contract::EngineCommand::Close {
                    key: self.key,
                    linger,
                },
            );
        }
    }
}
