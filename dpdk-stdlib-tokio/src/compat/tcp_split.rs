//! Owned split halves for async `TcpStream`.
//!
//! `OwnedReadHalf` implements `AsyncRead`, `OwnedWriteHalf` implements `AsyncWrite`.
//! Shutdown-on-drop semantics for the write half.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(feature = "dpdk")]
use std::net::Shutdown;
#[cfg(feature = "dpdk")]
use std::sync::atomic::Ordering;
#[cfg(feature = "dpdk")]
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Owned read half of an async `TcpStream`, produced by `into_split()`.
pub struct OwnedReadHalf {
    inner: ReadHalfInner,
}

enum ReadHalfInner {
    #[cfg(feature = "dpdk")]
    Dpdk {
        handle: Arc<dpdk_stdlib_tcp::contract::ConnectionHandle>,
        key: dpdk_stdlib_tcp::state::FourTuple,
    },
    Tokio(::tokio::net::tcp::OwnedReadHalf),
}

/// Owned write half of an async `TcpStream`, produced by `into_split()`.
pub struct OwnedWriteHalf {
    inner: WriteHalfInner,
}

enum WriteHalfInner {
    #[cfg(feature = "dpdk")]
    Dpdk {
        handle: Arc<dpdk_stdlib_tcp::contract::ConnectionHandle>,
        key: dpdk_stdlib_tcp::state::FourTuple,
    },
    Tokio(::tokio::net::tcp::OwnedWriteHalf),
}

impl OwnedReadHalf {
    #[cfg(feature = "dpdk")]
    pub(crate) fn new_dpdk(
        handle: Arc<dpdk_stdlib_tcp::contract::ConnectionHandle>,
        key: dpdk_stdlib_tcp::state::FourTuple,
    ) -> Self {
        Self {
            inner: ReadHalfInner::Dpdk { handle, key },
        }
    }

    pub(crate) fn new_tokio(inner: ::tokio::net::tcp::OwnedReadHalf) -> Self {
        Self {
            inner: ReadHalfInner::Tokio(inner),
        }
    }
}

impl OwnedWriteHalf {
    #[cfg(feature = "dpdk")]
    pub(crate) fn new_dpdk(
        handle: Arc<dpdk_stdlib_tcp::contract::ConnectionHandle>,
        key: dpdk_stdlib_tcp::state::FourTuple,
    ) -> Self {
        Self {
            inner: WriteHalfInner::Dpdk { handle, key },
        }
    }

    pub(crate) fn new_tokio(inner: ::tokio::net::tcp::OwnedWriteHalf) -> Self {
        Self {
            inner: WriteHalfInner::Tokio(inner),
        }
    }
}

impl AsyncRead for OwnedReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // SAFETY: OwnedReadHalf is Unpin (no self-referential fields)
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            ReadHalfInner::Dpdk { handle, .. } => {
                super::tcp::poll_read_dpdk(handle, cx, buf)
            }
            ReadHalfInner::Tokio(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            WriteHalfInner::Dpdk { handle, .. } => {
                super::tcp::poll_write_dpdk(handle, cx, data)
            }
            WriteHalfInner::Tokio(s) => Pin::new(s).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            WriteHalfInner::Dpdk { .. } => Poll::Ready(Ok(())),
            WriteHalfInner::Tokio(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.inner {
            #[cfg(feature = "dpdk")]
            WriteHalfInner::Dpdk { handle, key } => {
                let _ = handle.cmd_tx.send(
                    dpdk_stdlib_tcp::contract::EngineCommand::Shutdown {
                        key: *key,
                        how: Shutdown::Write,
                    },
                );
                Poll::Ready(Ok(()))
            }
            WriteHalfInner::Tokio(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(feature = "dpdk")]
impl Drop for OwnedReadHalf {
    fn drop(&mut self) {
        if let ReadHalfInner::Dpdk { handle, key } = &self.inner {
            if handle.app_refcount.fetch_sub(1, Ordering::AcqRel) == 1 {
                let linger = handle.linger.lock().unwrap().clone();
                let _ = handle.cmd_tx.send(
                    dpdk_stdlib_tcp::contract::EngineCommand::Close {
                        key: *key,
                        linger,
                    },
                );
            }
        }
    }
}

#[cfg(feature = "dpdk")]
impl Drop for OwnedWriteHalf {
    fn drop(&mut self) {
        if let WriteHalfInner::Dpdk { handle, key } = &self.inner {
            let _ = handle.cmd_tx.send(
                dpdk_stdlib_tcp::contract::EngineCommand::Shutdown {
                    key: *key,
                    how: Shutdown::Write,
                },
            );
            if handle.app_refcount.fetch_sub(1, Ordering::AcqRel) == 1 {
                let linger = handle.linger.lock().unwrap().clone();
                let _ = handle.cmd_tx.send(
                    dpdk_stdlib_tcp::contract::EngineCommand::Close {
                        key: *key,
                        linger,
                    },
                );
            }
        }
    }
}
