//! Provider and builder for the native DPDK QUIC I/O provider.

use crate::event_loop::{event_loop_with_tx_queue, LoopConfig};
use crate::path_handle::DpdkPathHandle;
use crate::stats::{ProviderHandle, ProviderStats, SharedThread};
use crate::tx::DpdkTxQueue;
use dpdk_udp::{BackendConfig, BackendType, DpdkBackend, IcmpHandler, PacketBackend};
use s2n_quic_core::endpoint::Endpoint;
use s2n_quic_core::inet::{IpV4Address, SocketAddress};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::error::DpdkQuicError;

/// Configuration for the DPDK QUIC provider.
pub struct ProviderConfig {
    pub bind_addr: SocketAddr,
    pub eal_args: Option<Vec<String>>,
    pub backend_config: BackendConfig,
    pub gateway_mac: Option<[u8; 6]>,
    pub max_rx_burst: usize,
    pub max_tx_burst: usize,
    pub backend_override: Option<Arc<dyn PacketBackend>>,
}

/// The native DPDK I/O provider for s2n-quic.
///
/// Implements `s2n_quic::provider::io::Provider` to own and drive
/// an s2n-quic endpoint from a dedicated event loop thread.
pub struct DpdkProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) stats: Arc<ProviderStats>,
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) thread: SharedThread,
}

/// Builder for constructing a `DpdkProvider` and its `ProviderHandle`.
pub struct ProviderBuilder {
    bind_addr: SocketAddr,
    eal_args: Option<Vec<String>>,
    backend_config: BackendConfig,
    gateway_mac: Option<[u8; 6]>,
    max_rx_burst: usize,
    max_tx_burst: usize,
    backend_override: Option<Arc<dyn PacketBackend>>,
}

impl ProviderBuilder {
    pub fn new() -> Self {
        Self {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            eal_args: None,
            backend_config: BackendConfig::default(),
            gateway_mac: None,
            max_rx_burst: 32,
            max_tx_burst: 32,
            backend_override: None,
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

    pub fn with_backend_config(mut self, config: BackendConfig) -> Self {
        self.backend_config = config;
        self
    }

    /// Select a real DPDK NIC by port id. Sets the backend type to `Dpdk` so
    /// `start()` builds a real-NIC backend via `DpdkBackend::new_real_nic`
    /// (which probes PCI for the vfio-bound ENI — unlike the `--no-pci` path).
    pub fn with_dpdk_port(mut self, port_id: u16) -> Self {
        self.backend_config.backend_type = BackendType::Dpdk;
        self.backend_config.dpdk_port_id = port_id;
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

    /// Inject a pre-created backend for testing (e.g. LoopbackBackend).
    ///
    /// When set, `start()` uses this backend instead of creating one
    /// from the `BackendConfig`.
    pub fn with_backend(mut self, backend: Arc<dyn PacketBackend>) -> Self {
        self.backend_override = Some(backend);
        self
    }

    /// Build the provider and its control handle.
    ///
    /// Both `Arc`s (stats + shutdown) are created here so the handle
    /// can observe stats even before `start()` is called.
    pub fn build(self) -> (DpdkProvider, ProviderHandle) {
        let stats = Arc::new(ProviderStats::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread: SharedThread = Arc::new(Mutex::new(None));

        let provider = DpdkProvider {
            config: ProviderConfig {
                bind_addr: self.bind_addr,
                eal_args: self.eal_args,
                backend_config: self.backend_config,
                gateway_mac: self.gateway_mac,
                max_rx_burst: self.max_rx_burst,
                max_tx_burst: self.max_tx_burst,
                backend_override: self.backend_override,
            },
            stats: Arc::clone(&stats),
            shutdown: Arc::clone(&shutdown),
            thread: Arc::clone(&thread),
        };

        let handle = ProviderHandle {
            stats,
            shutdown,
            thread,
        };

        (provider, handle)
    }
}

impl DpdkProvider {
    pub fn builder() -> ProviderBuilder {
        ProviderBuilder::new()
    }
}

/// Resolve gateway MAC: use explicit value if set, otherwise read kernel ARP cache.
fn resolve_gateway_mac(explicit: Option<[u8; 6]>, _local_ip: Ipv4Addr) -> [u8; 6] {
    if let Some(mac) = explicit {
        return mac;
    }
    // Kernel ARP cache fallback: read /proc/net/route for the default gateway IP,
    // then look up that IP in /proc/net/arp.
    if let Some(gw_ip) = read_default_gateway() {
        if let Some(mac) = lookup_arp_entry(gw_ip) {
            return mac;
        }
    }
    // Stub/test environments without /proc return zeros.
    [0u8; 6]
}

/// Read the default gateway IP from /proc/net/route.
fn read_default_gateway() -> Option<Ipv4Addr> {
    let content = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        // Destination == 00000000 means default route
        if fields[1] == "00000000" {
            // Gateway is in hex, little-endian u32
            let gw_hex = u32::from_str_radix(fields[2], 16).ok()?;
            return Some(Ipv4Addr::from(gw_hex.to_be()));
        }
    }
    None
}

/// Look up a MAC address in /proc/net/arp for the given IP.
fn lookup_arp_entry(ip: Ipv4Addr) -> Option<[u8; 6]> {
    let content = std::fs::read_to_string("/proc/net/arp").ok()?;
    let ip_str = ip.to_string();
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        if fields[0] == ip_str && fields[2] != "0x0" {
            let mac_parts: Vec<u8> = fields[3]
                .split(':')
                .filter_map(|s| u8::from_str_radix(s, 16).ok())
                .collect();
            if mac_parts.len() == 6 {
                return Some([
                    mac_parts[0],
                    mac_parts[1],
                    mac_parts[2],
                    mac_parts[3],
                    mac_parts[4],
                    mac_parts[5],
                ]);
            }
        }
    }
    None
}

impl s2n_quic::provider::io::Provider for DpdkProvider {
    type PathHandle = DpdkPathHandle;
    type Error = DpdkQuicError;

    fn start<E: Endpoint<PathHandle = Self::PathHandle>>(
        self,
        endpoint: E,
    ) -> Result<SocketAddress, Self::Error> {
        // 1. Reject IPv6 bind address
        let local_ip = match self.config.bind_addr.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => return Err(DpdkQuicError::UnsupportedAddressFamily),
        };

        // 2. Initialize backend.
        // An injected backend (loopback bench / tests) takes precedence. Otherwise,
        // for a real NIC we use DpdkBackend::new_real_nic — NOT create_backend /
        // DpdkBackend::new, which init EAL with `--no-pci` and therefore never probe
        // the vfio-pci ENI (a null device). new_real_nic honors `eal_args`, which
        // were previously stored but never reached EAL.
        let backend: Arc<dyn PacketBackend> = match self.config.backend_override {
            Some(b) => b,
            None => match self.config.backend_config.backend_type {
                BackendType::Dpdk | BackendType::Auto => Arc::new(DpdkBackend::new_real_nic(
                    self.config.backend_config.dpdk_port_id,
                    self.config.eal_args.as_deref(),
                )?) as Arc<dyn PacketBackend>,
                _ => dpdk_udp::create_backend(&self.config.backend_config)?,
            },
        };

        // 3. Resolve gateway MAC
        let gateway_mac = resolve_gateway_mac(self.config.gateway_mac, local_ip);

        // 4. Determine bound address
        let bind_addr = self.config.bind_addr;
        let src_mac = backend.mac_address();

        // 5. Create IcmpHandler
        let icmp_handler = IcmpHandler::new(src_mac, local_ip);

        // 6. Build TxQueue with resolved gateway MAC
        let tx_queue = DpdkTxQueue::new(bind_addr, self.config.max_tx_burst, src_mac, gateway_mac);

        // 7. Prepare loop config
        let loop_config = LoopConfig {
            max_rx_burst: self.config.max_rx_burst,
            max_tx_burst: self.config.max_tx_burst,
            ..Default::default()
        };

        // 8. Clone arcs for the thread
        let shutdown = Arc::clone(&self.shutdown);
        let stats = Arc::clone(&self.stats);
        let thread_slot = Arc::clone(&self.thread);

        // 9. Spawn event loop thread
        let handle = std::thread::Builder::new()
            .name("dpdk-quic-io".into())
            .spawn(move || {
                event_loop_with_tx_queue(
                    endpoint, backend, bind_addr, loop_config, shutdown, stats, icmp_handler,
                    tx_queue,
                );
            })
            .map_err(|e| DpdkQuicError::EventLoopCrash(e.to_string()))?;

        // Store thread handle so ProviderHandle::shutdown() can join it
        *thread_slot.lock().unwrap() = Some(handle);

        // 10. Return bound SocketAddress
        let socket_addr = SocketAddress::IpV4(
            IpV4Address::from(local_ip.octets()).with_port(bind_addr.port()),
        );
        Ok(socket_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_quic::provider::io::Provider;
    use serial_test::serial;

    #[test]
    fn builder_defaults() {
        let (provider, _handle) = DpdkProvider::builder().build();
        assert_eq!(provider.config.bind_addr, "0.0.0.0:0".parse().unwrap());
        assert_eq!(provider.config.max_rx_burst, 32);
        assert_eq!(provider.config.max_tx_burst, 32);
        assert!(provider.config.gateway_mac.is_none());
        assert!(provider.config.eal_args.is_none());
    }

    #[test]
    fn builder_with_all_options() {
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let (provider, _handle) = DpdkProvider::builder()
            .with_addr(addr)
            .with_eal_args(vec!["--no-huge".into()])
            .with_gateway_mac(mac)
            .with_rx_burst(64)
            .with_tx_burst(64)
            .with_backend_config(BackendConfig::default())
            .build();

        assert_eq!(provider.config.bind_addr, addr);
        assert_eq!(provider.config.gateway_mac, Some(mac));
        assert_eq!(provider.config.max_rx_burst, 64);
        assert_eq!(provider.config.max_tx_burst, 64);
        assert_eq!(
            provider.config.eal_args,
            Some(vec!["--no-huge".to_string()])
        );
    }

    #[test]
    fn build_creates_shared_arcs() {
        let (_provider, mut handle) = DpdkProvider::builder().build();
        let snap = handle.stats();
        assert_eq!(snap.rx_burst_calls, 0);
        assert_eq!(snap.datagrams_received, 0);
        // Shutdown on non-started provider is clean
        handle.shutdown();
    }

    #[test]
    fn resolve_gateway_mac_explicit() {
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let resolved = super::resolve_gateway_mac(Some(mac), Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(resolved, mac);
    }

    #[test]
    fn resolve_gateway_mac_fallback_no_panic() {
        let resolved = super::resolve_gateway_mac(None, Ipv4Addr::new(10, 0, 0, 1));
        let _ = resolved;
    }

    #[test]
    #[serial]
    fn provider_start_stub_mode() {
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let (provider, mut handle) = DpdkProvider::builder()
            .with_addr(addr)
            .with_gateway_mac([0xaa; 6])
            .build();

        let endpoint = crate::event_loop::tests::make_mock_endpoint(Some(2));

        let result = provider.start(endpoint);
        assert!(result.is_ok());
        let bound = result.unwrap();
        assert!(matches!(bound, SocketAddress::IpV4(_)));

        std::thread::sleep(std::time::Duration::from_millis(10));
        handle.shutdown();
    }

    #[test]
    fn provider_ipv6_rejected() {
        let addr: SocketAddr = "[::1]:4433".parse().unwrap();
        let (provider, _handle) = DpdkProvider::builder().with_addr(addr).build();

        let endpoint = crate::event_loop::tests::make_mock_endpoint(Some(0));

        let result = provider.start(endpoint);
        assert!(matches!(
            result.unwrap_err(),
            DpdkQuicError::UnsupportedAddressFamily
        ));
    }

    #[test]
    #[serial]
    fn provider_handle_shutdown_joins_thread() {
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let (provider, mut handle) = DpdkProvider::builder()
            .with_addr(addr)
            .with_gateway_mac([0xaa; 6])
            .build();

        // Endpoint that never auto-closes
        let endpoint = crate::event_loop::tests::make_mock_endpoint(None);

        let result = provider.start(endpoint);
        assert!(result.is_ok());

        // Thread should be running
        std::thread::sleep(std::time::Duration::from_millis(5));
        let snap = handle.stats();
        assert!(snap.timer_wakeups > 0);

        // Shutdown joins the thread
        handle.shutdown();
        // After shutdown, no more increments
        let snap_after = handle.stats();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let snap_later = handle.stats();
        assert_eq!(snap_after.timer_wakeups, snap_later.timer_wakeups);
    }

    // With no backend override and a Dpdk/Auto backend type, start() must route
    // through DpdkBackend::new_real_nic and reach the event loop. This also proves
    // eal_args (previously dead config) flows through without breaking start().
    #[test]
    #[serial]
    fn provider_start_real_backend_path_with_eal_args() {
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let (provider, mut handle) = DpdkProvider::builder()
            .with_addr(addr)
            .with_dpdk_port(0)
            .with_eal_args(vec!["dpdk-quic".into(), "--no-huge".into()])
            .with_gateway_mac([0xaa; 6])
            .build();

        let endpoint = crate::event_loop::tests::make_mock_endpoint(None);
        let result = provider.start(endpoint);
        assert!(result.is_ok(), "real-backend start failed: {result:?}");

        std::thread::sleep(std::time::Duration::from_millis(5));
        let snap = handle.stats();
        assert!(snap.timer_wakeups > 0, "event loop did not tick");
        handle.shutdown();
    }
}
