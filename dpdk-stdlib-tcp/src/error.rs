//! TCP error types.

use std::io;
use thiserror::Error;

/// Errors produced by the TCP stack.
#[derive(Debug, Clone, Error)]
pub enum TcpError {
    #[error("connection refused")]
    ConnectionRefused,
    #[error("connection reset")]
    ConnectionReset,
    #[error("connection aborted")]
    ConnectionAborted,
    #[error("broken pipe")]
    BrokenPipe,
    #[error("not connected")]
    NotConnected,
    #[error("operation timed out")]
    TimedOut,
    #[error("address already in use")]
    AddrInUse,
    #[error("address not available")]
    AddrNotAvailable,
    #[error("invalid packet: {0}")]
    InvalidPacket(String),
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),
}

impl From<TcpError> for io::Error {
    fn from(e: TcpError) -> Self {
        let kind = match &e {
            TcpError::ConnectionRefused => io::ErrorKind::ConnectionRefused,
            TcpError::ConnectionReset => io::ErrorKind::ConnectionReset,
            TcpError::ConnectionAborted => io::ErrorKind::ConnectionAborted,
            TcpError::BrokenPipe => io::ErrorKind::BrokenPipe,
            TcpError::NotConnected => io::ErrorKind::NotConnected,
            TcpError::TimedOut => io::ErrorKind::TimedOut,
            TcpError::AddrInUse => io::ErrorKind::AddrInUse,
            TcpError::AddrNotAvailable => io::ErrorKind::AddrNotAvailable,
            TcpError::InvalidPacket(_) => io::ErrorKind::InvalidData,
            TcpError::ResourceLimit(_) => io::ErrorKind::Other,
        };
        io::Error::new(kind, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_mapping() {
        let cases: Vec<(TcpError, io::ErrorKind)> = vec![
            (TcpError::ConnectionRefused, io::ErrorKind::ConnectionRefused),
            (TcpError::ConnectionReset, io::ErrorKind::ConnectionReset),
            (TcpError::ConnectionAborted, io::ErrorKind::ConnectionAborted),
            (TcpError::BrokenPipe, io::ErrorKind::BrokenPipe),
            (TcpError::NotConnected, io::ErrorKind::NotConnected),
            (TcpError::TimedOut, io::ErrorKind::TimedOut),
            (TcpError::AddrInUse, io::ErrorKind::AddrInUse),
            (TcpError::AddrNotAvailable, io::ErrorKind::AddrNotAvailable),
            (
                TcpError::InvalidPacket("bad".into()),
                io::ErrorKind::InvalidData,
            ),
            (
                TcpError::ResourceLimit("max".into()),
                io::ErrorKind::Other,
            ),
        ];
        for (tcp_err, expected_kind) in cases {
            let io_err: io::Error = tcp_err.into();
            assert_eq!(io_err.kind(), expected_kind);
        }
    }

    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}

    #[test]
    fn error_is_send_sync() {
        _assert_send::<TcpError>();
        _assert_sync::<TcpError>();
    }
}
