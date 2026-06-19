//! Owned split halves for `TcpStream`.
//!
//! `into_split()` splits a `TcpStream` into independently-owned read and write halves.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::contract::{ConnectionHandle, EngineCommand};
use crate::state::FourTuple;

/// Owned read half of a `TcpStream`, produced by `into_split()`.
pub struct OwnedReadHalf {
    handle: Arc<ConnectionHandle>,
    key: FourTuple,
}

/// Owned write half of a `TcpStream`, produced by `into_split()`.
pub struct OwnedWriteHalf {
    handle: Arc<ConnectionHandle>,
    key: FourTuple,
}

impl Read for OwnedReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let _guard = self.handle.read_mutex.lock().unwrap();
        loop {
            if let Some(err) = self.handle.peek_error() {
                return Err(err.into());
            }
            let n = self.handle.rx_ring.read(buf);
            if n > 0 {
                return Ok(n);
            }
            if self.handle.eof.load(Ordering::Acquire) {
                return Ok(0);
            }
            let guard = self.handle.notify_lock.lock().unwrap();
            if self.handle.rx_ring.available_read() > 0
                || self.handle.eof.load(Ordering::Acquire)
                || self.handle.peek_error().is_some()
            {
                drop(guard);
                continue;
            }
            let _unused = self.handle.condvar.wait(guard).unwrap();
        }
    }
}

impl Write for OwnedWriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _guard = self.handle.write_mutex.lock().unwrap();
        loop {
            if let Some(err) = self.handle.peek_error() {
                return Err(err.into());
            }
            let n = self.handle.tx_ring.write(buf);
            if n > 0 {
                self.handle.cmd_tx.wakeup().signal();
                return Ok(n);
            }
            let guard = self.handle.notify_lock.lock().unwrap();
            if self.handle.tx_ring.available_write() > 0
                || self.handle.peek_error().is_some()
            {
                drop(guard);
                continue;
            }
            let _unused = self.handle.condvar.wait(guard).unwrap();
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for OwnedReadHalf {
    fn drop(&mut self) {
        if self.handle.app_refcount.fetch_sub(1, Ordering::AcqRel) == 1 {
            let linger = self.handle.linger.lock().unwrap().clone();
            let _ = self.handle.cmd_tx.send(EngineCommand::Close {
                key: self.key,
                linger,
            });
        }
    }
}

impl Drop for OwnedWriteHalf {
    fn drop(&mut self) {
        let _ = self.handle.cmd_tx.send(EngineCommand::Shutdown {
            key: self.key,
            how: Shutdown::Write,
        });
        if self.handle.app_refcount.fetch_sub(1, Ordering::AcqRel) == 1 {
            let linger = self.handle.linger.lock().unwrap().clone();
            let _ = self.handle.cmd_tx.send(EngineCommand::Close {
                key: self.key,
                linger,
            });
        }
    }
}

/// Split a DPDK connection handle into owned read/write halves.
pub(crate) fn into_split_dpdk(
    handle: Arc<ConnectionHandle>,
    key: FourTuple,
) -> (OwnedReadHalf, OwnedWriteHalf) {
    handle.app_refcount.store(2, Ordering::Release);
    (
        OwnedReadHalf { handle: handle.clone(), key },
        OwnedWriteHalf { handle, key },
    )
}
