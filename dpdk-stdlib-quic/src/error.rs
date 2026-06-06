//! Error types for the DPDK QUIC provider.

use thiserror::Error;

/// Errors produced by the DPDK QUIC provider.
#[derive(Debug, Error)]
pub enum DpdkQuicError {
    #[error("DPDK initialization failed: {0}")]
    DpdkInit(String),

    #[error("Backend creation failed: {0}")]
    BackendInit(#[from] std::io::Error),

    #[error("Address family not supported: IPv6 requires a separate provider")]
    UnsupportedAddressFamily,

    #[error("Port bind failed: {0}")]
    BindFailed(String),

    #[error("Event loop terminated unexpectedly: {0}")]
    EventLoopCrash(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}

    #[test]
    fn error_is_send_and_sync() {
        _assert_send::<DpdkQuicError>();
        _assert_sync::<DpdkQuicError>();
    }

    #[test]
    fn error_is_static() {
        fn _assert_static<T: 'static>() {}
        _assert_static::<DpdkQuicError>();
    }

    #[test]
    fn error_display() {
        let e = DpdkQuicError::UnsupportedAddressFamily;
        assert!(e.to_string().contains("IPv6"));
    }
}
