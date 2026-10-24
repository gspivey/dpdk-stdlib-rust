//! IPv6 address utilities: link-local detection, scope ID parsing, and
//! solicited-node multicast address/MAC derivation.
//!
//! These are prerequisites for NDP (Neighbor Discovery Protocol, IPv6 task 6).
//! Link-local addresses (`fe80::/10`) are used for on-link communication and
//! NDP messages. Solicited-node multicast (`ff02::1:ffXX:XXXX`) is used by NDP
//! Neighbor Solicitation to efficiently resolve link-layer addresses without
//! broadcasting to all nodes.

use std::net::Ipv6Addr;

// ============================================================================
// Link-Local Detection
// ============================================================================

/// Returns `true` if `addr` is an IPv6 link-local unicast address (`fe80::/10`).
///
/// Link-local addresses are used for communication on a single link (subnet)
/// and are not routable. They are required for NDP and are automatically
/// assigned to every IPv6-enabled interface.
pub fn is_link_local(addr: &Ipv6Addr) -> bool {
    let octets = addr.octets();
    // fe80::/10 means the first 10 bits are 1111 1110 10xx xxxx
    // First byte must be 0xFE, second byte top 2 bits must be 0b10 (0x80..0xBF)
    octets[0] == 0xFE && (octets[1] & 0xC0) == 0x80
}

// ============================================================================
// Scope ID Parsing
// ============================================================================

/// An IPv6 address with an optional scope ID (zone identifier).
///
/// RFC 6874 / RFC 4007 define the `%zone_id` suffix for link-local addresses.
/// The scope ID identifies which interface the link-local address is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedIpv6Addr {
    /// The IPv6 address.
    pub addr: Ipv6Addr,
    /// The scope ID (zone identifier), if present. This is typically an
    /// interface name (e.g., `"eth0"`) or numeric index (e.g., `"2"`).
    pub scope_id: Option<String>,
}

/// Parse an IPv6 address string that may contain a `%scope_id` suffix.
///
/// Accepts formats:
/// - `"fe80::1%eth0"` → addr=fe80::1, scope_id=Some("eth0")
/// - `"fe80::1%2"` → addr=fe80::1, scope_id=Some("2")
/// - `"2001:db8::1"` → addr=2001:db8::1, scope_id=None
/// - `"::1"` → addr=::1, scope_id=None
///
/// Returns `None` if the address portion cannot be parsed.
pub fn parse_scoped_address(s: &str) -> Option<ScopedIpv6Addr> {
    match s.find('%') {
        Some(idx) => {
            let addr_str = &s[..idx];
            let scope_str = &s[idx + 1..];
            let addr: Ipv6Addr = addr_str.parse().ok()?;
            if scope_str.is_empty() {
                return None;
            }
            Some(ScopedIpv6Addr {
                addr,
                scope_id: Some(scope_str.to_string()),
            })
        }
        None => {
            let addr: Ipv6Addr = s.parse().ok()?;
            Some(ScopedIpv6Addr {
                addr,
                scope_id: None,
            })
        }
    }
}

// ============================================================================
// Solicited-Node Multicast
// ============================================================================

/// Derive the solicited-node multicast address for a given IPv6 unicast address.
///
/// The solicited-node multicast address is formed by taking the low-order 24 bits
/// of the unicast address and appending them to the prefix `ff02::1:ff00:0/104`.
///
/// Result: `ff02::1:ffXX:XXXX` where XX:XXXX are the low 24 bits of `addr`.
///
/// Used by NDP Neighbor Solicitation (RFC 4861 §7.2.2) to efficiently query
/// for a specific address without disturbing all nodes on the link.
pub fn solicited_node_multicast_addr(addr: &Ipv6Addr) -> Ipv6Addr {
    let octets = addr.octets();
    Ipv6Addr::new(
        0xff02, 0, 0, 0, 0, 1,
        0xff00 | (octets[13] as u16),
        ((octets[14] as u16) << 8) | (octets[15] as u16),
    )
}

/// Derive the Ethernet multicast MAC address for a solicited-node multicast group.
///
/// IPv6 multicast MACs are formed by placing the low-order 32 bits of the
/// multicast address into `33:33:XX:XX:XX:XX` (RFC 2464 §7).
///
/// For solicited-node multicast (`ff02::1:ffXX:XXXX`), this produces
/// `33:33:ff:XX:XX:XX` where XX:XX:XX are the low 24 bits of the original
/// unicast address.
pub fn solicited_node_multicast_mac(addr: &Ipv6Addr) -> [u8; 6] {
    let octets = addr.octets();
    [0x33, 0x33, 0xFF, octets[13], octets[14], octets[15]]
}

/// Derive the Ethernet multicast MAC for any IPv6 multicast address.
///
/// Per RFC 2464 §7, the MAC is `33:33` followed by the low-order 32 bits
/// of the IPv6 multicast address.
pub fn ipv6_multicast_mac(mcast_addr: &Ipv6Addr) -> [u8; 6] {
    let octets = mcast_addr.octets();
    [0x33, 0x33, octets[12], octets[13], octets[14], octets[15]]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_link_local ---

    #[test]
    fn link_local_basic() {
        assert!(is_link_local(&"fe80::1".parse().unwrap()));
        assert!(is_link_local(&"fe80::dead:beef:cafe:1234".parse().unwrap()));
        assert!(is_link_local(&"fe80::".parse().unwrap()));
    }

    #[test]
    fn link_local_full_range() {
        // fe80::/10 covers fe80:: through febf::ffff:...
        assert!(is_link_local(&"fe80::1".parse().unwrap()));
        assert!(is_link_local(&"fe9f::1".parse().unwrap()));
        assert!(is_link_local(&"fea0::1".parse().unwrap()));
        assert!(is_link_local(&"febf::1".parse().unwrap()));
    }

    #[test]
    fn not_link_local() {
        // Global unicast
        assert!(!is_link_local(&"2001:db8::1".parse().unwrap()));
        // Loopback
        assert!(!is_link_local(&"::1".parse().unwrap()));
        // Unspecified
        assert!(!is_link_local(&"::".parse().unwrap()));
        // Multicast
        assert!(!is_link_local(&"ff02::1".parse().unwrap()));
        // Site-local (deprecated, fec0::/10)
        assert!(!is_link_local(&"fec0::1".parse().unwrap()));
        // ULA (fc00::/7)
        assert!(!is_link_local(&"fd00::1".parse().unwrap()));
    }

    #[test]
    fn link_local_boundary() {
        // 0xFE80 = 1111 1110 1000 0000 — first valid
        assert!(is_link_local(&"fe80::".parse().unwrap()));
        // 0xFEBF = 1111 1110 1011 1111 — last valid
        assert!(is_link_local(&"febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap()));
        // 0xFEC0 = 1111 1110 1100 0000 — first invalid (site-local)
        assert!(!is_link_local(&"fec0::".parse().unwrap()));
        // 0xFE7F = 1111 1110 0111 1111 — invalid (top 2 bits of byte 1 are 01)
        assert!(!is_link_local(&"fe7f::".parse().unwrap()));
    }

    // --- parse_scoped_address ---

    #[test]
    fn parse_with_interface_name() {
        let result = parse_scoped_address("fe80::1%eth0").unwrap();
        assert_eq!(result.addr, "fe80::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(result.scope_id, Some("eth0".to_string()));
    }

    #[test]
    fn parse_with_numeric_scope() {
        let result = parse_scoped_address("fe80::1%2").unwrap();
        assert_eq!(result.addr, "fe80::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(result.scope_id, Some("2".to_string()));
    }

    #[test]
    fn parse_without_scope() {
        let result = parse_scoped_address("2001:db8::1").unwrap();
        assert_eq!(result.addr, "2001:db8::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(result.scope_id, None);
    }

    #[test]
    fn parse_loopback() {
        let result = parse_scoped_address("::1").unwrap();
        assert_eq!(result.addr, Ipv6Addr::LOCALHOST);
        assert_eq!(result.scope_id, None);
    }

    #[test]
    fn parse_empty_scope_is_invalid() {
        // "fe80::1%" has an empty scope — invalid
        assert!(parse_scoped_address("fe80::1%").is_none());
    }

    #[test]
    fn parse_invalid_address() {
        assert!(parse_scoped_address("not-an-address").is_none());
        assert!(parse_scoped_address("not-an-address%eth0").is_none());
    }

    #[test]
    fn parse_complex_interface_name() {
        // Interface names can contain dots, dashes, numbers
        let result = parse_scoped_address("fe80::1%ens192.100").unwrap();
        assert_eq!(result.scope_id, Some("ens192.100".to_string()));
    }

    // --- solicited_node_multicast_addr ---

    #[test]
    fn solicited_node_basic() {
        // fe80::1 → low 24 bits are 00:00:01
        let addr: Ipv6Addr = "fe80::1".parse().unwrap();
        let snm = solicited_node_multicast_addr(&addr);
        assert_eq!(snm, "ff02::1:ff00:1".parse::<Ipv6Addr>().unwrap());
    }

    #[test]
    fn solicited_node_full_low_bits() {
        // 2001:db8::dead:beef → low 24 bits are ad:beef
        let addr: Ipv6Addr = "2001:db8::dead:beef".parse().unwrap();
        let snm = solicited_node_multicast_addr(&addr);
        assert_eq!(snm, "ff02::1:ffad:beef".parse::<Ipv6Addr>().unwrap());
    }

    #[test]
    fn solicited_node_all_ones() {
        let addr: Ipv6Addr = "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap();
        let snm = solicited_node_multicast_addr(&addr);
        assert_eq!(snm, "ff02::1:ffff:ffff".parse::<Ipv6Addr>().unwrap());
    }

    #[test]
    fn solicited_node_all_zeros() {
        let addr = Ipv6Addr::UNSPECIFIED;
        let snm = solicited_node_multicast_addr(&addr);
        assert_eq!(snm, "ff02::1:ff00:0".parse::<Ipv6Addr>().unwrap());
    }

    #[test]
    fn solicited_node_rfc_example() {
        // RFC 4861 example: 4037:01:02:03:04:05:06 → ff02::1:ff05:0006
        // Using a more standard example: fe80::2aa:ff:fe28:9c5a
        // Low 24 bits: 28:9c5a → ff02::1:ff28:9c5a
        let addr: Ipv6Addr = "fe80::2aa:ff:fe28:9c5a".parse().unwrap();
        let snm = solicited_node_multicast_addr(&addr);
        assert_eq!(snm, "ff02::1:ff28:9c5a".parse::<Ipv6Addr>().unwrap());
    }

    // --- solicited_node_multicast_mac ---

    #[test]
    fn solicited_node_mac_basic() {
        // fe80::1 → low 24 bits 00:00:01 → 33:33:ff:00:00:01
        let addr: Ipv6Addr = "fe80::1".parse().unwrap();
        let mac = solicited_node_multicast_mac(&addr);
        assert_eq!(mac, [0x33, 0x33, 0xFF, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn solicited_node_mac_complex() {
        // 2001:db8::dead:beef → low 24 bits ad:be:ef → 33:33:ff:ad:be:ef
        let addr: Ipv6Addr = "2001:db8::dead:beef".parse().unwrap();
        let mac = solicited_node_multicast_mac(&addr);
        assert_eq!(mac, [0x33, 0x33, 0xFF, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn solicited_node_mac_all_ones() {
        let addr: Ipv6Addr = "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap();
        let mac = solicited_node_multicast_mac(&addr);
        assert_eq!(mac, [0x33, 0x33, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    // --- ipv6_multicast_mac ---

    #[test]
    fn multicast_mac_all_nodes() {
        // ff02::1 (all-nodes) → low 32 bits = 00:00:00:01 → 33:33:00:00:00:01
        let addr: Ipv6Addr = "ff02::1".parse().unwrap();
        let mac = ipv6_multicast_mac(&addr);
        assert_eq!(mac, [0x33, 0x33, 0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn multicast_mac_all_routers() {
        // ff02::2 (all-routers) → 33:33:00:00:00:02
        let addr: Ipv6Addr = "ff02::2".parse().unwrap();
        let mac = ipv6_multicast_mac(&addr);
        assert_eq!(mac, [0x33, 0x33, 0x00, 0x00, 0x00, 0x02]);
    }

    #[test]
    fn multicast_mac_solicited_node() {
        // ff02::1:ff28:9c5a → low 32 bits = ff:28:9c:5a → 33:33:ff:28:9c:5a
        let addr: Ipv6Addr = "ff02::1:ff28:9c5a".parse().unwrap();
        let mac = ipv6_multicast_mac(&addr);
        assert_eq!(mac, [0x33, 0x33, 0xFF, 0x28, 0x9C, 0x5A]);
    }

    #[test]
    fn multicast_mac_mld() {
        // ff02::16 (MLDv2) → 33:33:00:00:00:16
        let addr: Ipv6Addr = "ff02::16".parse().unwrap();
        let mac = ipv6_multicast_mac(&addr);
        assert_eq!(mac, [0x33, 0x33, 0x00, 0x00, 0x00, 0x16]);
    }

    // --- Synthetic performance test ---

    #[test]
    fn perf_solicited_node_derivation() {
        let iterations = 100_000;
        let addr: Ipv6Addr = "2001:db8::dead:beef:cafe:1234".parse().unwrap();

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            let _ = std::hint::black_box(solicited_node_multicast_addr(
                std::hint::black_box(&addr),
            ));
            let _ = std::hint::black_box(solicited_node_multicast_mac(
                std::hint::black_box(&addr),
            ));
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!(
            "[PERF] solicited-node addr+mac: {} iterations in {:?} ({} ns/op)",
            iterations, elapsed, ns_per_op
        );
        // These are trivial array operations — should be well under 100ns
        assert!(ns_per_op < 1_000, "solicited-node derivation too slow: {} ns/op", ns_per_op);
    }

    #[test]
    fn perf_scope_parsing() {
        let iterations = 100_000;
        let input = "fe80::dead:beef:cafe:1234%ens192";

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            let _ = std::hint::black_box(parse_scoped_address(
                std::hint::black_box(input),
            ));
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!(
            "[PERF] scope parsing: {} iterations in {:?} ({} ns/op)",
            iterations, elapsed, ns_per_op
        );
        // String parsing with allocation — should be under 1µs
        assert!(ns_per_op < 5_000, "scope parsing too slow: {} ns/op", ns_per_op);
    }
}
