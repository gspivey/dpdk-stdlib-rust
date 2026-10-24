//! Native DPDK I/O provider for s2n-quic.
//!
//! This crate implements `s2n_quic::provider::io::Provider` using DPDK
//! kernel-bypass packet I/O, enabling high-performance QUIC transport
//! without Tokio runtime involvement in the I/O path.
//!
//! The provider owns an s2n-quic endpoint and drives it from a dedicated
//! event loop thread calling rx_burst/tx_burst directly against the
//! `PacketBackend` trait.
//!
//! ## Version Pin
//!
//! This crate pins `s2n-quic = "=1.81.0"` and `s2n-quic-core = "=0.81.0"`
//! because the native provider implements lower-level s2n-quic types
//! (rx::Queue, tx::Queue, Message, Header, Handle) that are not stable
//! across s2n-quic releases.

pub mod clock;
pub mod ecn;
pub mod error;
pub mod event_loop;
pub mod frame;
pub mod loopback;
pub mod path_handle;
pub mod provider;
pub mod rx;
pub mod stats;
pub mod tx;

pub use dpdk_udp::BackendConfig;
pub use error::DpdkQuicError;
pub use provider::{DpdkProvider, ProviderBuilder};
pub use rx::{parse_to_rx_datagram, DpdkRxQueue};
pub use stats::ProviderHandle;
pub use tx::DpdkTxQueue;
