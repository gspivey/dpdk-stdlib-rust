//! Provider and builder for the native DPDK QUIC I/O provider.

use crate::stats::{ProviderHandle, ProviderStats};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Configuration for the DPDK QUIC provider.
pub struct ProviderConfig {
    pub bind_addr: SocketAddr,
    pub eal_args: Option<Vec<String>>,
    pub gateway_mac: Option<[u8; 6]>,
    pub max_rx_burst: usize,
    pub max_tx_burst: usize,
}

/// The native DPDK I/O provider for s2n-quic.
///
/// Implements `s2n_quic::provider::io::Provider` to own and drive
/// an s2n-quic endpoint from a dedicated event loop thread.
pub struct DpdkProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) stats: Arc<ProviderStats>,
    pub(crate) shutdown: Arc<AtomicBool>,
}

/// Builder for constructing a `DpdkProvider` and its `ProviderHandle`.
pub struct ProviderBuilder {
    bind_addr: SocketAddr,
    eal_args: Option<Vec<String>>,
    gateway_mac: Option<[u8; 6]>,
    max_rx_burst: usize,
    max_tx_burst: usize,
}

impl ProviderBuilder {
    pub fn new() -> Self {
        Self {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            eal_args: None,
            gateway_mac: None,
            max_rx_burst: 32,
            max_tx_burst: 32,
        }
    }

    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    pub fn with_eal_args(mut self, args: Vec<String>) -> Self {
        self.eal_args = Some(args);
        self
    }

    pub fn with_gateway_mac(mut self, mac: [u8; 6]) -> Self {
        self.gateway_mac = Some(mac);
        self
    }

    pub fn with_rx_burst(mut self, max: usize) -> Self {
        self.max_rx_burst = max;
        self
    }

    pub fn with_tx_burst(mut self, max: usize) -> Self {
        self.max_tx_burst = max;
        self
    }

    /// Build the provider and its control handle.
    ///
    /// Both `Arc`s (stats + shutdown) are created here so the handle
    /// can observe stats even before `start()` is called.
    pub fn build(self) -> (DpdkProvider, ProviderHandle) {
        let stats = Arc::new(ProviderStats::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        let provider = DpdkProvider {
            config: ProviderConfig {
                bind_addr: self.bind_addr,
                eal_args: self.eal_args,
                gateway_mac: self.gateway_mac,
                max_rx_burst: self.max_rx_burst,
                max_tx_burst: self.max_tx_burst,
            },
            stats: Arc::clone(&stats),
            shutdown: Arc::clone(&shutdown),
        };

        let handle = ProviderHandle {
            stats,
            shutdown,
            thread: None,
        };

        (provider, handle)
    }
}

impl DpdkProvider {
    pub fn builder() -> ProviderBuilder {
        ProviderBuilder::new()
    }
}
