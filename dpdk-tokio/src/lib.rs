//! # dpdk-tokio
//!
//! Async Tokio integration for DPDK networking. Provides a unified async interface
//! that works with both standard Tokio networking and DPDK-accelerated sockets.
//!
//! ## Features
//!
//! - `dpdk` - Enable DPDK socket support (requires dpdk-udp crate)
//!
//! ## Usage
//!
//! ```rust,ignore
//! use dpdk_tokio::{AsyncUdpSocket, bind_udp};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Automatically selects DPDK if available, falls back to Tokio
//!     let socket = bind_udp("0.0.0.0:9000").await?;
//!
//!     let mut buf = [0u8; 1024];
//!     let (len, addr) = socket.recv_from(&mut buf).await?;
//!     socket.send_to(&buf[..len], addr).await?;
//!
//!     Ok(())
//! }
//! ```

use std::io;
use std::net::SocketAddr;
use async_trait::async_trait;
use thiserror::Error;

pub mod socket;
pub mod runtime;
pub mod compat;

pub use socket::{TokioUdpSocket, BoxedAsyncUdpSocket};
#[cfg(feature = "dpdk")]
pub use socket::DpdkUdpSocket;

/// Errors that can occur in dpdk-tokio
#[derive(Error, Debug)]
pub enum DpdkTokioError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("DPDK initialization failed: {0}")]
    DpdkInit(String),

    #[error("Socket bind failed: {0}")]
    BindFailed(String),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("Operation timeout")]
    Timeout,

    #[error("Channel closed")]
    ChannelClosed,
}

pub type Result<T> = std::result::Result<T, DpdkTokioError>;

/// Unified async UDP socket trait that abstracts over different backends
#[async_trait]
pub trait AsyncUdpSocket: Send + Sync {
    /// Receives a single datagram message on the socket
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>;

    /// Sends data on the socket to the given address
    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize>;

    /// Returns the local address that this socket is bound to
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Connects this UDP socket to a remote address
    async fn connect(&self, addr: SocketAddr) -> io::Result<()>;

    /// Receives a single datagram from the connected address
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Sends data to the connected address
    async fn send(&self, buf: &[u8]) -> io::Result<usize>;

    /// Returns the name of the backend (for logging/debugging)
    fn backend_name(&self) -> &'static str;
}

/// Socket configuration options
#[derive(Debug, Clone)]
pub struct SocketConfig {
    /// Prefer DPDK if available
    pub prefer_dpdk: bool,
    /// DPDK EAL arguments
    pub dpdk_args: Vec<String>,
    /// Receive buffer size
    pub recv_buffer_size: Option<usize>,
    /// Send buffer size
    pub send_buffer_size: Option<usize>,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            prefer_dpdk: true,
            dpdk_args: vec!["-l".into(), "0".into(), "-n".into(), "4".into()],
            recv_buffer_size: None,
            send_buffer_size: None,
        }
    }
}

/// Bind a UDP socket with automatic backend selection
///
/// If the `dpdk` feature is enabled and DPDK initialization succeeds,
/// returns a DPDK-accelerated socket. Otherwise falls back to Tokio.
pub async fn bind_udp<A: std::net::ToSocketAddrs>(addr: A) -> io::Result<BoxedAsyncUdpSocket> {
    bind_udp_with_config(addr, SocketConfig::default()).await
}

/// Bind a UDP socket with custom configuration
#[allow(unused_variables)]
pub async fn bind_udp_with_config<A: std::net::ToSocketAddrs>(
    addr: A,
    config: SocketConfig,
) -> io::Result<BoxedAsyncUdpSocket> {
    let addr = addr.to_socket_addrs()?.next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid address"))?;

    #[cfg(feature = "dpdk")]
    if config.prefer_dpdk {
        match DpdkUdpSocket::bind_with_config(addr, &config).await {
            Ok(socket) => {
                return Ok(Box::new(socket));
            }
            Err(e) => {
                eprintln!("DPDK init failed ({}), falling back to Tokio", e);
            }
        }
    }

    // Fallback to Tokio
    let socket = TokioUdpSocket::bind(addr).await?;
    Ok(Box::new(socket))
}

/// Macro for creating an async UDP socket with automatic backend selection
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_tokio::dpdk_socket;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let socket = dpdk_socket!("0.0.0.0:9000").await?;
///     println!("Using backend: {}", socket.backend_name());
///     Ok(())
/// }
/// ```
#[macro_export]
macro_rules! dpdk_socket {
    ($addr:expr) => {
        $crate::bind_udp($addr)
    };
    ($addr:expr, $config:expr) => {
        $crate::bind_udp_with_config($addr, $config)
    };
}

/// Macro for running an async function with DPDK-aware Tokio runtime
///
/// This sets up the Tokio runtime with configuration optimized for DPDK workloads.
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_tokio::dpdk_main;
///
/// dpdk_main! {
///     async fn run() -> Result<(), Box<dyn std::error::Error>> {
///         let socket = dpdk_tokio::bind_udp("0.0.0.0:9000").await?;
///         // ... use socket
///         Ok(())
///     }
/// }
/// ```
#[macro_export]
macro_rules! dpdk_main {
    (async fn $name:ident() -> $ret:ty $body:block) => {
        fn main() -> $ret {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1) // Single worker for DPDK affinity
                .build()
                .expect("Failed to create Tokio runtime");

            runtime.block_on(async $body)
        }
    };
}

/// Macro to try DPDK first, fall back to Tokio, with compile-time feature detection
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_tokio::try_dpdk_async;
///
/// async fn create_socket() -> std::io::Result<Box<dyn AsyncUdpSocket>> {
///     try_dpdk_async!("0.0.0.0:9000")
/// }
/// ```
#[macro_export]
macro_rules! try_dpdk_async {
    ($addr:expr) => {{
        #[cfg(feature = "dpdk")]
        {
            match $crate::socket::DpdkUdpSocket::bind($addr).await {
                Ok(socket) => {
                    println!("Using DPDK backend");
                    Ok(Box::new(socket) as $crate::BoxedAsyncUdpSocket)
                }
                Err(_) => {
                    println!("DPDK unavailable, using Tokio backend");
                    let socket = $crate::socket::TokioUdpSocket::bind($addr).await?;
                    Ok(Box::new(socket) as $crate::BoxedAsyncUdpSocket)
                }
            }
        }
        #[cfg(not(feature = "dpdk"))]
        {
            println!("Using Tokio backend (DPDK feature not enabled)");
            let socket = $crate::socket::TokioUdpSocket::bind($addr).await?;
            Ok(Box::new(socket) as $crate::BoxedAsyncUdpSocket)
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tokio_socket_bind() {
        let socket = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(socket.backend_name(), "tokio");
        assert!(socket.local_addr().is_ok());
    }

    #[tokio::test]
    async fn test_bind_udp_fallback() {
        // Without DPDK feature, should use Tokio
        let socket = bind_udp("127.0.0.1:0").await.unwrap();
        // The backend name depends on feature flags
        assert!(!socket.backend_name().is_empty());
    }
}
