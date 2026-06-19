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

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_tcp_error() -> impl Strategy<Value = (TcpError, io::ErrorKind)> {
        prop_oneof![
            Just((TcpError::ConnectionRefused, io::ErrorKind::ConnectionRefused)),
            Just((TcpError::ConnectionReset, io::ErrorKind::ConnectionReset)),
            Just((TcpError::ConnectionAborted, io::ErrorKind::ConnectionAborted)),
            Just((TcpError::BrokenPipe, io::ErrorKind::BrokenPipe)),
            Just((TcpError::NotConnected, io::ErrorKind::NotConnected)),
            Just((TcpError::TimedOut, io::ErrorKind::TimedOut)),
            Just((TcpError::AddrInUse, io::ErrorKind::AddrInUse)),
            Just((TcpError::AddrNotAvailable, io::ErrorKind::AddrNotAvailable)),
            ".*".prop_map(|s| (TcpError::InvalidPacket(s), io::ErrorKind::InvalidData)),
            ".*".prop_map(|s| (TcpError::ResourceLimit(s), io::ErrorKind::Other)),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        /// Property 19: Every TcpError variant maps to the correct io::ErrorKind.
        #[test]
        fn tcp_error_to_io_error_mapping((tcp_err, expected_kind) in arb_tcp_error()) {
            let io_err: io::Error = tcp_err.into();
            prop_assert_eq!(io_err.kind(), expected_kind);
        }
    }
}
