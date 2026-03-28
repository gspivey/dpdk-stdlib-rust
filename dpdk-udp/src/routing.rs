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
use std::path::Path;

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

    /// Try to auto-detect routing from the OS for the given local IP.
    ///
    /// Parses `/proc/net/route` and `/proc/net/arp` to discover the local
    /// subnet, prefix length, default gateway, and any ARP entries for the
    /// gateway. Returns a tuple of `(RoutingTable, Vec<ProcArpEntry>)` on
    /// success, or `(RoutingTable::new(), vec![])` if detection fails
    /// (falling back to passthrough mode).
    pub fn auto_detect(local_ip: Ipv4Addr) -> (Self, Vec<ProcArpEntry>) {
        match detect_from_proc(local_ip) {
            Some((config, arp_entries)) => {
                (Self::with_config(config), arp_entries)
            }
            None => (Self::new(), Vec::new()),
        }
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
// OS Auto-Detection (Phase 3)
// ============================================================================

/// An entry parsed from `/proc/net/route`.
#[derive(Debug, Clone)]
struct ProcRouteEntry {
    iface: String,
    destination: u32,  // network-byte-order u32
    gateway: u32,      // network-byte-order u32
    mask: u32,         // network-byte-order u32
}

/// An entry parsed from `/proc/net/arp`.
#[derive(Debug, Clone)]
pub struct ProcArpEntry {
    /// IP address.
    pub ip: Ipv4Addr,
    /// MAC address as 6 bytes.
    pub mac: [u8; 6],
    /// Interface name.
    pub device: String,
}

/// Parse `/proc/net/route` (or a file with the same format).
///
/// Format (tab-separated, first line is header):
/// ```text
/// Iface   Destination Gateway Flags   RefCnt  Use Metric  Mask    MTU Window  IRTT
/// ens5    00000A0A    0100000A ...     00FFFFFF ...
/// ```
/// Destination, Gateway, Mask are hex-encoded little-endian u32 values.
fn parse_proc_route(content: &str) -> Vec<ProcRouteEntry> {
    let mut entries = Vec::new();
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let iface = fields[0].to_string();
        let destination = match u32::from_str_radix(fields[1], 16) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let gateway = match u32::from_str_radix(fields[2], 16) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mask = match u32::from_str_radix(fields[7], 16) {
            Ok(v) => v,
            Err(_) => continue,
        };
        entries.push(ProcRouteEntry { iface, destination, gateway, mask });
    }
    entries
}

/// Parse `/proc/net/arp` (or a file with the same format).
///
/// Format (space-separated, first line is header):
/// ```text
/// IP address       HW type     Flags       HW address            Mask     Device
/// 10.0.1.1         0x1         0x2         02:e0:9f:5d:6a:a0    *        ens5
/// ```
fn parse_proc_arp(content: &str) -> Vec<ProcArpEntry> {
    let mut entries = Vec::new();
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        let ip: Ipv4Addr = match fields[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Parse MAC address (xx:xx:xx:xx:xx:xx)
        let mac_str = fields[3];
        let mac = match parse_mac(mac_str) {
            Some(m) => m,
            None => continue,
        };
        let device = fields[5].to_string();
        entries.push(ProcArpEntry { ip, mac, device });
    }
    entries
}

/// Parse a colon-separated MAC address string into 6 bytes.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(mac)
}

/// Convert a host-byte-order hex u32 from /proc/net/route to an Ipv4Addr.
///
/// `/proc/net/route` prints `%08X` of the kernel's in-memory u32, which is in
/// host byte order. `u32::to_ne_bytes()` gives us back the original 4 bytes,
/// which are the IP octets in network order.
fn le_hex_to_ipv4(val: u32) -> Ipv4Addr {
    Ipv4Addr::from(val.to_ne_bytes())
}

/// Count the number of leading 1-bits in a subnet mask to get prefix length.
///
/// The mask from /proc/net/route is a host-byte-order u32. Convert to network
/// order (big-endian) first so we can count leading ones correctly.
fn mask_to_prefix_len(mask: u32) -> u8 {
    // to_ne_bytes gives the original IP-order bytes, then re-interpret as BE u32
    let bytes = mask.to_ne_bytes();
    let mask_be = u32::from_be_bytes(bytes);
    mask_be.leading_ones() as u8
}

/// Detect routing configuration from the OS for a given local IP.
///
/// Reads `/proc/net/route` to find the interface, subnet, and default gateway.
/// Returns `None` if parsing fails or the IP isn't found in any route.
///
/// This function accepts path overrides for testing with mock data.
pub fn detect_from_os(
    local_ip: Ipv4Addr,
    route_path: &Path,
    arp_path: &Path,
) -> Option<(NetworkConfig, Vec<ProcArpEntry>)> {
    let route_content = std::fs::read_to_string(route_path).ok()?;
    let routes = parse_proc_route(&route_content);
    if routes.is_empty() {
        return None;
    }

    // Find the interface that carries our local IP by matching the local subnet.
    // Also collect the default gateway route.
    let mut local_iface: Option<String> = None;
    let mut local_prefix_len: u8 = 0;
    let mut default_gateway: Option<Ipv4Addr> = None;

    // /proc/net/route stores addresses as host-byte-order u32 printed in hex.
    // On LE machines (x86/ARM), 10.0.1.100 is stored as 0x6401000A.
    // Convert our IP to the same representation for comparison.
    let local_u32 = u32::from_ne_bytes(local_ip.octets());

    for entry in &routes {
        if entry.destination == 0 && entry.mask == 0 && entry.gateway != 0 {
            // Default route (0.0.0.0/0 with a gateway)
            default_gateway = Some(le_hex_to_ipv4(entry.gateway));
        }

        if entry.mask != 0 {
            // Check if local_ip falls within this route's subnet
            if (local_u32 & entry.mask) == (entry.destination & entry.mask) {
                let plen = mask_to_prefix_len(entry.mask);
                // Prefer the most specific match (longest prefix)
                if plen > local_prefix_len {
                    local_prefix_len = plen;
                    local_iface = Some(entry.iface.clone());
                }
            }
        }
    }

    // If we couldn't determine the prefix, fall back
    if local_prefix_len == 0 {
        return None;
    }

    let mut config = NetworkConfig::new(local_ip, local_prefix_len);
    if let Some(gw) = default_gateway {
        config = config.with_gateway(gw);
    }

    // Parse ARP table for gateway/peer MAC entries
    let arp_entries = std::fs::read_to_string(arp_path)
        .ok()
        .map(|content| {
            let all = parse_proc_arp(&content);
            // Filter to entries on our interface
            match &local_iface {
                Some(iface) => all.into_iter().filter(|e| e.device == *iface).collect(),
                None => all,
            }
        })
        .unwrap_or_default();

    Some((config, arp_entries))
}

/// Convenience wrapper that reads from the real `/proc` filesystem.
pub fn detect_from_proc(local_ip: Ipv4Addr) -> Option<(NetworkConfig, Vec<ProcArpEntry>)> {
    detect_from_os(
        local_ip,
        Path::new("/proc/net/route"),
        Path::new("/proc/net/arp"),
    )
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

    // --- Phase 3: /proc parsing tests ---

    #[test]
    fn test_parse_proc_route_basic() {
        let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
ens5\t00000000\t0101000A\t0003\t0\t0\t100\t00000000\t0\t0\t0
ens5\t0001000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
";
        let routes = parse_proc_route(content);
        assert_eq!(routes.len(), 2);

        // First entry: default route via 10.0.1.1
        assert_eq!(routes[0].destination, 0x00000000);
        assert_eq!(routes[0].gateway, 0x0101000A);
        assert_eq!(routes[0].mask, 0x00000000);
        assert_eq!(routes[0].iface, "ens5");

        // Second entry: 10.0.1.0/24 direct
        assert_eq!(routes[1].destination, 0x0001000A);
        assert_eq!(routes[1].mask, 0x00FFFFFF);
    }

    #[test]
    fn test_parse_proc_arp_basic() {
        let content = "\
IP address       HW type     Flags       HW address            Mask     Device
10.0.1.1         0x1         0x2         02:e0:9f:5d:6a:a0     *        ens5
10.0.1.50        0x1         0x2         0a:1b:2c:3d:4e:5f     *        ens5
";
        let entries = parse_proc_arp(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].ip, Ipv4Addr::new(10, 0, 1, 1));
        assert_eq!(entries[0].mac, [0x02, 0xe0, 0x9f, 0x5d, 0x6a, 0xa0]);
        assert_eq!(entries[0].device, "ens5");
        assert_eq!(entries[1].ip, Ipv4Addr::new(10, 0, 1, 50));
        assert_eq!(entries[1].mac, [0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f]);
    }

    #[test]
    fn test_parse_mac() {
        assert_eq!(parse_mac("02:e0:9f:5d:6a:a0"), Some([0x02, 0xe0, 0x9f, 0x5d, 0x6a, 0xa0]));
        assert_eq!(parse_mac("ff:ff:ff:ff:ff:ff"), Some([0xff; 6]));
        assert_eq!(parse_mac("00:00:00:00:00:00"), Some([0x00; 6]));
        assert_eq!(parse_mac("invalid"), None);
        assert_eq!(parse_mac("02:e0:9f:5d:6a"), None); // too short
    }

    #[test]
    fn test_le_hex_to_ipv4() {
        // 0x0101000A in LE = bytes 0A, 00, 01, 01 = 10.0.1.1
        assert_eq!(le_hex_to_ipv4(0x0101000A), Ipv4Addr::new(10, 0, 1, 1));
        // 0x0001000A = 10.0.1.0
        assert_eq!(le_hex_to_ipv4(0x0001000A), Ipv4Addr::new(10, 0, 1, 0));
        // 0x00000000 = 0.0.0.0
        assert_eq!(le_hex_to_ipv4(0x00000000), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn test_mask_to_prefix_len() {
        // 0x00FFFFFF in LE = FF FF FF 00 in BE = /24
        assert_eq!(mask_to_prefix_len(0x00FFFFFF), 24);
        // 0x0000FFFF = FF FF 00 00 in BE = /16
        assert_eq!(mask_to_prefix_len(0x0000FFFF), 16);
        // 0x00000000 = /0
        assert_eq!(mask_to_prefix_len(0x00000000), 0);
        // 0xFFFFFFFF = /32
        assert_eq!(mask_to_prefix_len(0xFFFFFFFF), 32);
    }

    #[test]
    fn test_detect_from_os_aws_vpc() {
        use std::io::Write;

        // Simulate an AWS VPC instance: ens5 on 10.0.1.100/24, gateway 10.0.1.1
        let route_content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
ens5\t00000000\t0101000A\t0003\t0\t0\t100\t00000000\t0\t0\t0
ens5\t0001000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
";
        let arp_content = "\
IP address       HW type     Flags       HW address            Mask     Device
10.0.1.1         0x1         0x2         02:e0:9f:5d:6a:a0     *        ens5
";

        let dir = std::env::temp_dir().join("dpdk_routing_test_aws");
        std::fs::create_dir_all(&dir).unwrap();
        let route_path = dir.join("route");
        let arp_path = dir.join("arp");
        std::fs::write(&route_path, route_content).unwrap();
        std::fs::write(&arp_path, arp_content).unwrap();

        let result = detect_from_os(
            Ipv4Addr::new(10, 0, 1, 100),
            &route_path,
            &arp_path,
        );

        let (config, arp_entries) = result.expect("should detect");
        assert_eq!(config.local_ip, Ipv4Addr::new(10, 0, 1, 100));
        assert_eq!(config.prefix_len, 24);
        assert_eq!(config.default_gateway, Some(Ipv4Addr::new(10, 0, 1, 1)));
        assert_eq!(config.mtu, 1500); // default

        assert_eq!(arp_entries.len(), 1);
        assert_eq!(arp_entries[0].ip, Ipv4Addr::new(10, 0, 1, 1));
        assert_eq!(arp_entries[0].mac, [0x02, 0xe0, 0x9f, 0x5d, 0x6a, 0xa0]);

        // Verify routing behavior
        let table = RoutingTable::with_config(config);
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 1, 50)),
            NextHop::Direct(Ipv4Addr::new(10, 0, 1, 50)) // same subnet
        );
        assert_eq!(
            table.lookup(Ipv4Addr::new(10, 0, 2, 100)),
            NextHop::Gateway(Ipv4Addr::new(10, 0, 1, 1)) // cross-subnet
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_detect_from_os_home_network() {
        let route_content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlan0\t0000A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
";
        let arp_content = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.1.1      0x1         0x2         aa:bb:cc:dd:ee:ff     *        wlan0
";

        let dir = std::env::temp_dir().join("dpdk_routing_test_home");
        std::fs::create_dir_all(&dir).unwrap();
        let route_path = dir.join("route");
        let arp_path = dir.join("arp");
        std::fs::write(&route_path, route_content).unwrap();
        std::fs::write(&arp_path, arp_content).unwrap();

        let result = detect_from_os(
            Ipv4Addr::new(192, 168, 1, 100),  // not in route directly, but in subnet 192.168.0.0/24
            &route_path,
            &arp_path,
        );

        // 192.168.0.0/24 won't match 192.168.1.100 — the route entry is 0x0000A8C0 = 192.168.0.0
        // with mask 0x00FFFFFF = /24. 192.168.1.100 & /24 = 192.168.1.0, not 192.168.0.0.
        // So this should fail to match.
        // Let's use a proper route instead.
        let route_content2 = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
";
        // 0x0001A8C0 in LE = C0 A8 01 00 = 192.168.1.0
        std::fs::write(&route_path, route_content2).unwrap();

        let result = detect_from_os(
            Ipv4Addr::new(192, 168, 1, 100),
            &route_path,
            &arp_path,
        );

        let (config, arp_entries) = result.expect("should detect");
        assert_eq!(config.prefix_len, 24);
        // Gateway: 0x0101A8C0 in LE = C0 A8 01 01 = 192.168.1.1
        assert_eq!(config.default_gateway, Some(Ipv4Addr::new(192, 168, 1, 1)));

        assert_eq!(arp_entries.len(), 1);
        assert_eq!(arp_entries[0].ip, Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(arp_entries[0].mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_detect_from_os_no_route_file() {
        let result = detect_from_os(
            Ipv4Addr::new(10, 0, 1, 100),
            Path::new("/nonexistent/route"),
            Path::new("/nonexistent/arp"),
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_detect_from_os_empty_route() {
        let dir = std::env::temp_dir().join("dpdk_routing_test_empty");
        std::fs::create_dir_all(&dir).unwrap();
        let route_path = dir.join("route");
        let arp_path = dir.join("arp");
        std::fs::write(&route_path, "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n").unwrap();
        std::fs::write(&arp_path, "").unwrap();

        let result = detect_from_os(
            Ipv4Addr::new(10, 0, 1, 100),
            &route_path,
            &arp_path,
        );
        assert!(result.is_none()); // no routes to match

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_detect_from_os_no_matching_subnet() {
        let route_content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
ens5\t00000000\t0101000A\t0003\t0\t0\t100\t00000000\t0\t0\t0
ens5\t0002000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
";
        // Subnet is 10.0.2.0/24 but our IP is 10.0.1.100 — no match
        let dir = std::env::temp_dir().join("dpdk_routing_test_nomatch");
        std::fs::create_dir_all(&dir).unwrap();
        let route_path = dir.join("route");
        let arp_path = dir.join("arp");
        std::fs::write(&route_path, route_content).unwrap();
        std::fs::write(&arp_path, "").unwrap();

        let result = detect_from_os(
            Ipv4Addr::new(10, 0, 1, 100),
            &route_path,
            &arp_path,
        );
        assert!(result.is_none()); // prefix_len stays 0 → fallback

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_detect_multi_interface() {
        // Multiple interfaces — should pick the one matching our IP
        let route_content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
ens5\t00000000\t0101000A\t0003\t0\t0\t100\t00000000\t0\t0\t0
ens5\t0001000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
ens6\t0002000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
";
        let arp_content = "\
IP address       HW type     Flags       HW address            Mask     Device
10.0.1.1         0x1         0x2         02:e0:9f:5d:6a:a0     *        ens5
10.0.2.1         0x1         0x2         aa:bb:cc:dd:ee:ff     *        ens6
";
        let dir = std::env::temp_dir().join("dpdk_routing_test_multi");
        std::fs::create_dir_all(&dir).unwrap();
        let route_path = dir.join("route");
        let arp_path = dir.join("arp");
        std::fs::write(&route_path, route_content).unwrap();
        std::fs::write(&arp_path, arp_content).unwrap();

        // IP 10.0.1.50 matches ens5 subnet
        let (config, arp_entries) = detect_from_os(
            Ipv4Addr::new(10, 0, 1, 50),
            &route_path,
            &arp_path,
        ).expect("should detect");

        assert_eq!(config.prefix_len, 24);
        assert_eq!(config.default_gateway, Some(Ipv4Addr::new(10, 0, 1, 1)));
        // ARP entries should be filtered to ens5 only
        assert_eq!(arp_entries.len(), 1);
        assert_eq!(arp_entries[0].device, "ens5");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_auto_detect_returns_passthrough() {
        // auto_detect with a random IP on a machine without matching /proc entries
        // should fall back to passthrough
        let (table, entries) = RoutingTable::auto_detect(Ipv4Addr::new(203, 0, 113, 1));
        // This may or may not be configured depending on the host OS, but it must not panic.
        // At minimum, the table should be usable.
        let _ = table.lookup(Ipv4Addr::new(8, 8, 8, 8));
        // entries may be empty
        let _ = entries;
    }

    #[test]
    fn test_parse_proc_route_malformed_lines() {
        let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
too_short\t00000000
ens5\tNOTHEX\t0101000A\t0003\t0\t0\t100\t00000000\t0\t0\t0
ens5\t00000000\t0101000A\t0003\t0\t0\t100\t00000000\t0\t0\t0
";
        let routes = parse_proc_route(content);
        // Only the last valid line should parse
        assert_eq!(routes.len(), 1);
    }

    #[test]
    fn test_parse_proc_arp_malformed() {
        let content = "\
IP address       HW type     Flags       HW address            Mask     Device
not_an_ip        0x1         0x2         02:e0:9f:5d:6a:a0     *        ens5
10.0.1.1         0x1         0x2         invalid_mac            *        ens5
10.0.1.2         0x1         0x2         02:aa:bb:cc:dd:ee     *        ens5
";
        let entries = parse_proc_arp(content);
        // Only the last valid entry should parse
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ip, Ipv4Addr::new(10, 0, 1, 2));
    }
}
