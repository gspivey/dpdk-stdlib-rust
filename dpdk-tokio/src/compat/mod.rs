//! Compatibility layer for drop-in replacement of standard socket types
//!
//! This module provides socket types that are API-compatible with their
//! standard library and Tokio counterparts. Simply change your imports
//! to get DPDK acceleration with zero code changes.
//!
//! # Usage
//!
//! ## For `std::net::UdpSocket` replacement:
//!
//! ```rust,ignore
//! // Before:
//! // use std::net::UdpSocket;
//!
//! // After:
//! use dpdk_tokio::compat::net::UdpSocket;
//!
//! fn main() -> std::io::Result<()> {
//!     let socket = UdpSocket::bind("0.0.0.0:9000")?;
//!     let mut buf = [0u8; 1024];
//!     let (len, addr) = socket.recv_from(&mut buf)?;
//!     socket.send_to(&buf[..len], addr)?;
//!     Ok(())
//! }
//! ```
//!
//! ## For `tokio::net::UdpSocket` replacement:
//!
//! ```rust,ignore
//! // Before:
//! // use tokio::net::UdpSocket;
//!
//! // After:
//! use dpdk_tokio::compat::tokio::UdpSocket;
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let socket = UdpSocket::bind("0.0.0.0:9000").await?;
//!     let mut buf = [0u8; 1024];
//!     let (len, addr) = socket.recv_from(&mut buf).await?;
//!     socket.send_to(&buf[..len], addr).await?;
//!     Ok(())
//! }
//! ```
//!
//! # Backend Selection
//!
//! The backend is selected at compile time via feature flags:
//!
//! - Default (no features): Uses standard library / Tokio sockets
//! - `dpdk` feature: Uses DPDK-accelerated sockets with automatic fallback

pub mod net;
pub mod tokio;
pub mod tcp;
pub mod tcp_listener;
pub mod tcp_split;

// Re-export for convenience
pub use net::UdpSocket as StdUdpSocket;
pub use tokio::UdpSocket as TokioUdpSocket;
pub use tcp::TcpStream;
pub use tcp_listener::TcpListener;
pub use tcp_split::{OwnedReadHalf, OwnedWriteHalf};
