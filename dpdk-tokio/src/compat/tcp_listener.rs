//! Async TCP listener — drop-in replacement for `tokio::net::TcpListener`.
//!
//! DPDK-first for IPv4, tokio fallback for IPv6.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};

use crate::compat::tcp::{TcpStream, TcpStreamInner};
#[cfg(feature = "dpdk")]
use crate::compat::tcp::DpdkAsyncTcpStream;

/// Async TCP listener — drop-in replacement for `tokio::net::TcpListener`.
pub struct TcpListener {
    inner: TcpListenerInner,
}

enum TcpListenerInner {
    #[cfg(feature = "dpdk")]
    Dpdk(DpdkAsyncTcpListener),
    Tokio(::tokio::net::TcpListener),
}

#[cfg(feature = "dpdk")]
struct DpdkAsyncTcpListener {
    addr: SocketAddr,
    cmd_tx: dpdk_stdlib_tcp::contract::CommandSender,
}

impl TcpListener {
    /// Bind an async TCP listener. DPDK-first for IPv4, tokio fallback for IPv6.
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses resolved"))?;

        match addr {
            #[cfg(feature = "dpdk")]
            SocketAddr::V4(_) => {
                if dpdk_stdlib_tcp::is_tcp_context_initialized() {
                    let cmd_tx = {
                        let ctx = dpdk_stdlib_tcp::get_tcp_context()
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                        ctx.cmd_tx.clone()
                    };

                    let (resp_tx, resp_rx) = dpdk_stdlib_tcp::contract::oneshot_channel();
                    cmd_tx
                        .send(dpdk_stdlib_tcp::contract::EngineCommand::Listen {
                            addr,
                            backlog: 128,
                            response: resp_tx,
                        })
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "engine channel closed")
                        })?;

                    // Block in spawn_blocking to avoid blocking the async runtime.
                    let result = ::tokio::task::spawn_blocking(move || resp_rx.recv())
                        .await
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                    match result {
                        Ok(()) => Ok(TcpListener {
                            inner: TcpListenerInner::Dpdk(DpdkAsyncTcpListener {
                                addr,
                                cmd_tx,
                            }),
                        }),
                        Err(e) => Err(e.into()),
                    }
                } else {
                    let listener = ::tokio::net::TcpListener::bind(addr).await?;
                    Ok(TcpListener {
                        inner: TcpListenerInner::Tokio(listener),
                    })
                }
            }
            _ => {
                let listener = ::tokio::net::TcpListener::bind(addr).await?;
                Ok(TcpListener {
                    inner: TcpListenerInner::Tokio(listener),
                })
            }
        }
    }

    /// Accept a new incoming TCP connection.
    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        match &self.inner {
            #[cfg(feature = "dpdk")]
            TcpListenerInner::Dpdk(listener) => {
                let (resp_tx, resp_rx) = dpdk_stdlib_tcp::contract::oneshot_channel();
                listener
                    .cmd_tx
                    .send(dpdk_stdlib_tcp::contract::EngineCommand::Accept {
                        listen_addr: listener.addr,
                        response: resp_tx,
                    })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "engine channel closed")
                    })?;

                let result = ::tokio::task::spawn_blocking(move || resp_rx.recv())
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                match result {
                    Ok((key, handle)) => {
                        let peer = key.remote;
                        let stream = TcpStream {
                            inner: TcpStreamInner::Dpdk(DpdkAsyncTcpStream {
                                handle,
                                key,
                            }),
                        };
                        Ok((stream, peer))
                    }
                    Err(e) => Err(e.into()),
                }
            }
            TcpListenerInner::Tokio(listener) => {
                let (stream, addr) = listener.accept().await?;
                Ok((
                    TcpStream {
                        inner: TcpStreamInner::Tokio(stream),
                    },
                    addr,
                ))
            }
        }
    }

    /// Returns the local address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            #[cfg(feature = "dpdk")]
            TcpListenerInner::Dpdk(l) => Ok(l.addr),
            TcpListenerInner::Tokio(l) => l.local_addr(),
        }
    }
}
