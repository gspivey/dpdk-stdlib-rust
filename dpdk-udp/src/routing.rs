//! Subnet-aware routing table for next-hop MAC resolution.
//!
//! Determines whether a destination IP is on the local subnet (ARP directly for
//! the peer) or requires forwarding through a gateway (ARP for the gateway IP).
//!
//! When no routing configuration is provided, the behavior matches the previous
//! default: all ARP resolution targets the destination IP directly, and the
//! fallback `dst_mac` (broadcast or user-set) is used when ARP has no entry.
//! This preserves backward compatibility with AWS VPC deployments where the
//! gateway MAC is typically seeded into the ARP cache at startup.

use std::net::Ipv4Addr;

/// Describes where to send the next ARP request for a given destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextHop {
    /// Destination is on the local subnet — ARP for the peer's IP directly.
    Direct(Ipv4Addr),
    /// Destination is off-subnet — ARP for the gateway's IP instead.
    Gateway(Ipv4Addr),
}

/// A single route entry: a destination prefix + next-hop gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    /// Network address (host bits must be zero).
    pub network: Ipv4Addr,
    /// Prefix length (0–32).
    pub prefix_len: u8,
    /// Gateway IP to forward matching traffic to.
    pub gateway: Ipv4Addr,
}

impl RouteEntry {
    pub fn new(network: Ipv4Addr, prefix_len: u8, gateway: Ipv4Addr) -> Self {
        debug_assert!(prefix_len <= 32);
        Self { network, prefix_len, gateway }
    }

    /// Returns the subnet mask as a `u32` (e.g. prefix_len=24 → 0xFFFFFF00).
    fn mask(&self) -> u32 {
        if self.prefix_len == 0 {
            0
        } else {
            !0u32 << (32 - self.prefix_len)
        }
    }

    /// Check if `addr` falls within this route's prefix.
    pub fn matches(&self, addr: Ipv4Addr) -> bool {
        let addr_bits = u32::from(addr);
        let net_bits = u32::from(self.network);
        let mask = self.mask();
        (addr_bits & mask) == (net_bits & mask)
    }
}

/// Network configuration for a single interface.
///
/// This tells the routing table which subnet the local interface belongs to,
/// and optionally provides a default gateway and static routes.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// Local IP address of the interface.
    pub local_ip: Ipv4Addr,
    /// Subnet prefix length (e.g. 24 for a /24 network).
    pub prefix_len: u8,
    /// Default gateway IP (used for traffic that doesn't match any route or the local subnet).
    pub default_gateway: Option<Ipv4Addr>,
    /// Static routes (checked before the default gateway, longest-prefix-match).
    pub static_routes: Vec<RouteEntry>,
    /// Interface MTU in bytes (default 1500). Affects `MAX_UDP_PAYLOAD`.
    pub mtu: u16,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            local_ip: Ipv4Addr::UNSPECIFIED,
            prefix_len: 0,
            default_gateway: None,
            static_routes: Vec::new(),
            mtu: 1500,
        }
    }
}

impl NetworkConfig {
    /// Create a network config for the given local IP and prefix length.
    pub fn new(local_ip: Ipv4Addr, prefix_len: u8) -> Self {
        Self {
            local_ip,
            prefix_len,
            ..Default::default()
        }
    }

    /// Set the default gateway.
    pub fn with_gateway(mut self, gateway: Ipv4Addr) -> Self {
        self.default_gateway = Some(gateway);
        self
    }

    /// Add a static route.
    pub fn with_route(mut self, network: Ipv4Addr, prefix_len: u8, gateway: Ipv4Addr) -> Self {
        self.static_routes.push(RouteEntry::new(network, prefix_len, gateway));
        self
    }

    /// Set the interface MTU.
    pub fn with_mtu(mut self, mtu: u16) -> Self {
        self.mtu = mtu;
        self
    }

    /// Compute the subnet mask as a `u32`.
    fn local_mask(&self) -> u32 {
        if self.prefix_len == 0 {
            0
        } else {
            !0u32 << (32 - self.prefix_len)
        }
    }

    /// Check if `addr` is on the local subnet.
    pub fn is_local(&self, addr: Ipv4Addr) -> bool {
        if self.prefix_len == 0 {
            // No subnet configured — can't determine locality.
            return false;
        }
        let mask = self.local_mask();
        let local = u32::from(self.local_ip);
        let remote = u32::from(addr);
        (local & mask) == (remote & mask)
    }

    /// Maximum UDP payload given this interface's MTU.
    /// MTU covers IP + UDP + payload (no Ethernet header).
    pub fn max_udp_payload(&self) -> usize {
        // MTU = IP header + UDP header + payload
        // payload = MTU - 20 (IPv4) - 8 (UDP)
        (self.mtu as usize).saturating_sub(28)
    }
}

/// Routing table that determines the next-hop IP for a given destination.
///
/// Resolution order:
/// 1. Broadcast addresses → `NextHop::Direct` (no gateway needed).
/// 2. Local subnet → `NextHop::Direct` (ARP for peer).
/// 3. Longest-prefix-match in static routes → `NextHop::Gateway`.
/// 4. Default gateway → `NextHop::Gateway`.
/// 5. No match → `NextHop::Direct` (fall through, legacy behavior).
#[derive(Debug, Clone)]
pub struct RoutingTable {
    config: Option<NetworkConfig>,
}

impl RoutingTable {
    /// Create a routing table with no configuration (legacy/passthrough mode).
    ///
    /// All destinations resolve to `NextHop::Direct(dst_ip)`, preserving
    /// the pre-routing behavior where ARP targets the destination directly.
    pub fn new() -> Self {
        Self { config: None }
    }

    /// Create a routing table from a network configuration.
    pub fn with_config(config: NetworkConfig) -> Self {
        Self { config: Some(config) }
    }

    /// Returns true if routing is configured (not in legacy passthrough mode).
    pub fn is_configured(&self) -> bool {
        self.config.is_some()
    }

    /// Look up the next-hop for `dst_ip`.
    pub fn lookup(&self, dst_ip: Ipv4Addr) -> NextHop {
        // Broadcast is always direct.
        if dst_ip == Ipv4Addr::BROADCAST {
            return NextHop::Direct(dst_ip);
        }

        let config = match &self.config {
            Some(c) => c,
            // No routing config — legacy passthrough.
            None => return NextHop::Direct(dst_ip),
        };

        // Link-local (169.254.0.0/16) is always direct.
        let octets = dst_ip.octets();
        if octets[0] == 169 && octets[1] == 254 {
            return NextHop::Direct(dst_ip);
        }

        // Multicast (224.0.0.0/4) is always direct.
        if octets[0] >= 224 && octets[0] <= 239 {
            return NextHop::Direct(dst_ip);
        }

        // Local subnet — ARP for peer directly.
        if config.is_local(dst_ip) {
            return NextHop::Direct(dst_ip);
        }

        // Subnet-directed broadcast (e.g. 10.0.1.255 for 10.0.1.0/24).
        if config.prefix_len > 0 && config.prefix_len < 32 {
            let mask = config.local_mask();
            let network = u32::from(config.local_ip) & mask;
            let host_bits = u32::from(dst_ip) & !mask;
            if (u32::from(dst_ip) & mask) == network && host_bits == !mask {
                return NextHop::Direct(dst_ip);
            }
        }

        // Static routes — longest prefix match.
        let mut best_match: Option<(u8, Ipv4Addr)> = None;
        for route in &config.static_routes {
            if route.matches(dst_ip) {
                match best_match {
                    Some((best_len, _)) if route.prefix_len <= best_len => {}
                    _ => best_match = Some((route.prefix_len, route.gateway)),
                }
            }
        }
        if let Some((_, gw)) = best_match {
            return NextHop::Gateway(gw);
        }

        // Default gateway.
        if let Some(gw) = config.default_gateway {
            return NextHop::Gateway(gw);
        }

        // No route — fall back to direct (best-effort, will likely fail at ARP).
        NextHop::Direct(dst_ip)
    }

    /// Get the configured MTU, or the default 1500.
    pub fn mtu(&self) -> u16 {
        self.config.as_ref().map_or(1500, |c| c.mtu)
    }

    /// Get the maximum UDP payload for the configured MTU.
    pub fn max_udp_payload(&self) -> usize {
        self.config.as_ref().map_or(1472, |c| c.max_udp_payload())
    }
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- RouteEntry tests ---

    #[test]
    fn test_route_entry_matches_exact() {
        let route = RouteEntry::new(
            Ipv4Addr::new(10, 0, 1, 0), 24,
            Ipv4Addr::new(10, 0, 0, 1),
        );
        assert!(route.matches(Ipv4Addr::new(10, 0, 1, 50)));
        assert!(route.matches(Ipv4Addr::new(10, 0, 1, 255)));
        assert!(!route.matches(Ipv4Addr::new(10, 0, 2, 1)));
    }

    #[test]
    fn test_route_entry_matches_wide_prefix() {
        let route = RouteEntry::new(
            Ipv4Addr::new(172, 16, 0, 0), 12,
            Ipv4Addr::new(172, 16, 0, 1),
        );
        assert!(route.matches(Ipv4Addr::new(172, 16, 5, 10)));
        assert!(route.matches(Ipv4Addr::new(172, 31, 255, 255)));
        assert!(!route.matches(Ipv4Addr::new(172, 32, 0, 1)));
    }

    #[test]
    fn test_route_entry_host_route() {
        let route = RouteEntry::new(
            Ipv4Addr::new(10, 0, 1, 99), 32,
            Ipv4Addr::new(10, 0, 0, 1),
        );
        assert!(route.matches(Ipv4Addr::new(10, 0, 1, 99)));
        assert!(!route.matches(Ipv4Addr::new(10, 0, 1, 100)));
    }

    #[test]
    fn test_route_entry_default_route() {
        let route = RouteEntry::new(
            Ipv4Addr::new(0, 0, 0, 0), 0,
            Ipv4Addr::new(10, 0, 0, 1),
        );
        // /0 matches everything
        assert!(route.matches(Ipv4Addr::new(1, 2, 3, 4)));
        assert!(route.matches(Ipv4Addr::new(192, 168, 1, 1)));
    }

    // --- NetworkConfig tests ---

    #[test]
    fn test_is_local_same_subnet() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24);
        assert!(config.is_local(Ipv4Addr::new(10, 0, 1, 20)));
        assert!(config.is_local(Ipv4Addr::new(10, 0, 1, 254)));
        assert!(!config.is_local(Ipv4Addr::new(10, 0, 2, 1)));
    }

    #[test]
    fn test_is_local_wide_subnet() {
        let config = NetworkConfig::new(Ipv4Addr::new(192, 168, 0, 1), 16);
        assert!(config.is_local(Ipv4Addr::new(192, 168, 255, 255)));
        assert!(!config.is_local(Ipv4Addr::new(192, 169, 0, 1)));
    }

    #[test]
    fn test_is_local_no_prefix() {
        // prefix_len=0 means unconfigured — nothing is "local"
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 0);
        assert!(!config.is_local(Ipv4Addr::new(10, 0, 1, 10)));
    }

    #[test]
    fn test_max_udp_payload_default() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 0, 1), 24);
        assert_eq!(config.max_udp_payload(), 1472); // 1500 - 20 - 8
    }

    #[test]
    fn test_max_udp_payload_jumbo() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 0, 1), 24)
            .with_mtu(9001);
        assert_eq!(config.max_udp_payload(), 8973); // 9001 - 28
    }

    // --- RoutingTable tests ---

    #[test]
    fn test_unconfigured_always_direct() {
        let table = RoutingTable::new();
        assert_eq!(
            table.lookup(Ipv4Addr::new(8, 8, 8, 8)),
            NextHop::Direct(Ipv4Addr::new(8, 8, 8, 8))
        );
        assert!(!table.is_configured());
    }

    #[test]
    fn test_local_subnet_is_direct() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 1, 20)),
            NextHop::Direct(Ipv4Addr::new(10, 0, 1, 20))
        );
    }

    #[test]
    fn test_cross_subnet_uses_gateway() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 2, 50)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 1))
        );
    }

    #[test]
    fn test_internet_uses_gateway() {
        let config = NetworkConfig::new(Ipv4Addr::new(192, 168, 1, 100), 24)
            .with_gateway(Ipv4Addr::new(192, 168, 1, 1));
        let table = RoutingTable::with_config(config);

        assert_eq!(
            table.lookup(Ipv4Addr::new(8, 8, 8, 8)),
            NextHop::Gateway(Ipv4Addr::new(192, 168, 1, 1))
        );
    }

    #[test]
    fn test_broadcast_is_always_direct() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        assert_eq!(
            table.lookup(Ipv4Addr::BROADCAST),
            NextHop::Direct(Ipv4Addr::BROADCAST)
        );
    }

    #[test]
    fn test_link_local_is_direct() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        assert_eq!(
            table.lookup(Ipv4Addr::new(169, 254, 1, 1)),
            NextHop::Direct(Ipv4Addr::new(169, 254, 1, 1))
        );
    }

    #[test]
    fn test_multicast_is_direct() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        assert_eq!(
            table.lookup(Ipv4Addr::new(224, 0, 0, 1)),
            NextHop::Direct(Ipv4Addr::new(224, 0, 0, 1))
        );
    }

    #[test]
    fn test_subnet_directed_broadcast_is_direct() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        // 10.0.1.255 is the broadcast for 10.0.1.0/24
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 1, 255)),
            NextHop::Direct(Ipv4Addr::new(10, 0, 1, 255))
        );
    }

    #[test]
    fn test_static_route_overrides_gateway() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
            .with_route(
                Ipv4Addr::new(172, 16, 0, 0), 16,
                Ipv4Addr::new(10, 0, 1, 254),
            );
        let table = RoutingTable::with_config(config);

        // Traffic to 172.16.x.x uses the static route's gateway
        assert_eq!(
            table.lookup(Ipv4Addr::new(172, 16, 5, 10)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 254))
        );

        // Other cross-subnet traffic uses default gateway
        assert_eq!(
            table.lookup(Ipv4Addr::new(8, 8, 8, 8)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 1))
        );
    }

    #[test]
    fn test_longest_prefix_match() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
            .with_route(
                Ipv4Addr::new(172, 16, 0, 0), 16,
                Ipv4Addr::new(10, 0, 1, 100), // wider route
            )
            .with_route(
                Ipv4Addr::new(172, 16, 5, 0), 24,
                Ipv4Addr::new(10, 0, 1, 200), // more specific route
            );
        let table = RoutingTable::with_config(config);

        // 172.16.5.10 matches both /16 and /24 — /24 wins (longest prefix)
        assert_eq!(
            table.lookup(Ipv4Addr::new(172, 16, 5, 10)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 200))
        );

        // 172.16.6.10 only matches /16
        assert_eq!(
            table.lookup(Ipv4Addr::new(172, 16, 6, 10)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 100))
        );
    }

    #[test]
    fn test_no_gateway_falls_through_direct() {
        // Config with subnet but no gateway
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24);
        let table = RoutingTable::with_config(config);

        // Local subnet is direct
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 1, 20)),
            NextHop::Direct(Ipv4Addr::new(10, 0, 1, 20))
        );

        // Cross-subnet falls through to direct (no gateway configured)
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 2, 1)),
            NextHop::Direct(Ipv4Addr::new(10, 0, 2, 1))
        );
    }

    #[test]
    fn test_mtu_default() {
        let table = RoutingTable::new();
        assert_eq!(table.mtu(), 1500);
        assert_eq!(table.max_udp_payload(), 1472);
    }

    #[test]
    fn test_mtu_custom() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 0, 1), 24)
            .with_mtu(9001);
        let table = RoutingTable::with_config(config);
        assert_eq!(table.mtu(), 9001);
        assert_eq!(table.max_udp_payload(), 8973);
    }

    // --- Builder/config chaining tests ---

    #[test]
    fn test_network_config_builder_chain() {
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
            .with_route(Ipv4Addr::new(172, 16, 0, 0), 16, Ipv4Addr::new(10, 0, 1, 254))
            .with_mtu(9001);

        assert_eq!(config.local_ip, Ipv4Addr::new(10, 0, 1, 10));
        assert_eq!(config.prefix_len, 24);
        assert_eq!(config.default_gateway, Some(Ipv4Addr::new(10, 0, 1, 1)));
        assert_eq!(config.static_routes.len(), 1);
        assert_eq!(config.mtu, 9001);
    }

    // --- Real-world scenario tests ---

    #[test]
    fn test_bare_metal_datacenter_scenario() {
        // Typical bare-metal: 10.0.1.0/24, gateway at .1, peer on different subnet
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 50), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        // Same rack (same subnet) — direct ARP
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 1, 51)),
            NextHop::Direct(Ipv4Addr::new(10, 0, 1, 51))
        );

        // Different rack (different subnet) — via gateway
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 2, 50)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 1))
        );
    }

    #[test]
    fn test_aws_vpc_scenario() {
        // AWS VPC: 10.0.1.0/24 subnet, gateway at 10.0.1.1
        // Both sender and receiver are on different subnets
        let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 100), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1));
        let table = RoutingTable::with_config(config);

        // Peer on different subnet (10.0.2.x) → gateway
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 2, 100)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 1))
        );
    }

    #[test]
    fn test_home_network_scenario() {
        // Home network: 192.168.1.0/24, router at .1
        let config = NetworkConfig::new(Ipv4Addr::new(192, 168, 1, 100), 24)
            .with_gateway(Ipv4Addr::new(192, 168, 1, 1));
        let table = RoutingTable::with_config(config);

        // LAN peer — direct
        assert_eq!(
            table.lookup(Ipv4Addr::new(192, 168, 1, 200)),
            NextHop::Direct(Ipv4Addr::new(192, 168, 1, 200))
        );

        // Internet — via gateway
        assert_eq!(
            table.lookup(Ipv4Addr::new(1, 1, 1, 1)),
            NextHop::Gateway(Ipv4Addr::new(192, 168, 1, 1))
        );
    }
}
