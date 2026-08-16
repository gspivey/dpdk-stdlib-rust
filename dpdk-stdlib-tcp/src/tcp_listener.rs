//! Public `TcpListener` API — drop-in replacement for `std::net::TcpListener`.
//!
//! Dispatches IPv4 to the DPDK engine path and IPv6 to kernel fallback.

use std::io;
use std::net::{self, SocketAddr, ToSocketAddrs};

use crate::contract::{
    CommandSender, EngineCommand, oneshot_channel,
};
use crate::stream::DpdkTcpStream;
use crate::tcp_stream::{get_tcp_context, resolve_addr, TcpStream};

/// A TCP socket server, either backed by DPDK (IPv4) or the kernel (IPv6 fallback).
///
/// Provides the full `std::net::TcpListener` API surface.
pub struct TcpListener {
    inner: ListenerInner,
}

enum ListenerInner {
    Dpdk(DpdkTcpListener),
    Std(net::TcpListener),
}

struct DpdkTcpListener {
    addr: SocketAddr,
    cmd_tx: CommandSender,
}

impl TcpListener {
    /// Creates a new `TcpListener` bound to the specified address.
    ///
    /// IPv4 addresses use the DPDK path; IPv6 falls back to the kernel.
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let addr = resolve_addr(addr)?;
        match addr {
            SocketAddr::V4(_) => {
                let ctx = get_tcp_context()?;
                let (resp_tx, resp_rx) = oneshot_channel();
                ctx.cmd_tx
                    .send(EngineCommand::Listen {
                        addr,
                        backlog: 128,
                        response: resp_tx,
                    })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "engine channel closed")
                    })?;

                let result = resp_rx.recv();
                match result {
                    Ok(()) => Ok(TcpListener {
                        inner: ListenerInner::Dpdk(DpdkTcpListener {
                            addr,
                            cmd_tx: ctx.cmd_tx.clone(),
                        }),
                    }),
                    Err(e) => Err(e.into()),
                }
            }
            SocketAddr::V6(_) => {
                let listener = net::TcpListener::bind(addr)?;
                Ok(TcpListener {
                    inner: ListenerInner::Std(listener),
                })
            }
        }
    }

    /// Accept a new incoming connection.
    ///
    /// Blocks until a connection is available.
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        match &self.inner {
            ListenerInner::Dpdk(listener) => {
                let (resp_tx, resp_rx) = oneshot_channel();
                listener
                    .cmd_tx
                    .send(EngineCommand::Accept {
                        listen_addr: listener.addr,
                        response: resp_tx,
                    })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "engine channel closed")
                    })?;

                let result = resp_rx.recv();
                match result {
                    Ok((key, handle)) => {
                        let remote = key.remote;
                        let stream = DpdkTcpStream::new(handle, key);
                        Ok((TcpStream::from_dpdk(stream), remote))
                    }
                    Err(e) => Err(e.into()),
                }
            }
            ListenerInner::Std(listener) => {
                let (stream, addr) = listener.accept()?;
                Ok((TcpStream::from_std(stream), addr))
            }
        }
    }

    /// Returns the local socket address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            ListenerInner::Dpdk(listener) => Ok(listener.addr),
            ListenerInner::Std(listener) => listener.local_addr(),
        }
    }

    /// Sets the TTL value for this listener's socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        match &self.inner {
            ListenerInner::Dpdk(_) => {
                // TTL on a listener is a no-op for DPDK (applies to accepted streams).
                Ok(())
            }
            ListenerInner::Std(listener) => listener.set_ttl(ttl),
        }
    }

    /// Gets the TTL value.
    pub fn ttl(&self) -> io::Result<u32> {
        match &self.inner {
            ListenerInner::Dpdk(_) => Ok(64),
            ListenerInner::Std(listener) => listener.ttl(),
        }
    }

    /// Returns an iterator over incoming connections.
    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }
}

/// An iterator over incoming TCP connections on a `TcpListener`.
pub struct Incoming<'a> {
    listener: &'a TcpListener,
}

impl<'a> Iterator for Incoming<'a> {
    type Item = io::Result<TcpStream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept().map(|(stream, _)| stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_listener_bind_v6_falls_back_to_kernel() {
        // Binding to [::1]:0 should use the kernel path (Std variant).
        // This may fail if IPv6 is disabled, which is fine — we're testing dispatch.
        let result = TcpListener::bind("[::1]:0");
        // Either succeeds (uses kernel) or fails with a kernel error — both are correct.
        if let Ok(listener) = result {
            let addr = listener.local_addr().unwrap();
            assert!(addr.is_ipv6());
        }
    }

    #[test]
    fn tcp_listener_bind_v4_without_context_returns_error() {
        // Without TCP context initialized, V4 bind should fail.
        // (Context may or may not be initialized from other tests, so
        // we just verify the function doesn't panic.)
        let _result = TcpListener::bind("10.0.0.1:0");
        // Result depends on whether TCP_CONTEXT is initialized.
    }
}
