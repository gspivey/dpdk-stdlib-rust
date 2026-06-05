//! Neighbor resolution abstraction
//!
//! Provides the `NeighborResolver` trait for resolving IP addresses to MAC
//! addresses, abstracting over ARP (IPv4) and NDP (IPv6, future).

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Trait for resolving IP addresses to link-layer (MAC) addresses.
///
/// Implementations handle the protocol-specific resolution mechanism
/// (ARP for IPv4, NDP for IPv6).
pub trait NeighborResolver: Send + Sync {
    /// Resolve an IP address to a MAC address.
    ///
    /// May briefly block while performing resolution (e.g., sending ARP request
    /// and waiting for reply). Returns the resolved MAC address on success.
    fn resolve(&self, ip: IpAddr) -> io::Result<[u8; 6]>;

    /// Non-blocking cache lookup.
    ///
    /// Returns `Some(mac)` if the address is in the cache, `None` otherwise.
    /// Does not trigger resolution.
    fn lookup_cached(&self, ip: IpAddr) -> Option<[u8; 6]>;
}

/// Entry in the ARP cache with expiration time.
#[derive(Debug, Clone)]
struct CacheEntry {
    mac: [u8; 6],
    expires: Instant,
}

/// IPv4 ARP-based neighbor resolver.
///
/// Maintains a cache of IP→MAC mappings and uses ARP for resolution.
/// Supports a gateway MAC override for AWS VPC environments where all
/// traffic must be sent to the gateway's MAC address.
pub struct ArpResolver {
    cache: RwLock<HashMap<IpAddr, CacheEntry>>,
    gateway_mac: Option<[u8; 6]>,
    ttl: Duration,
}

impl ArpResolver {
    /// Create a new ARP resolver.
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            gateway_mac: None,
            ttl: Duration::from_secs(300),
        }
    }

    /// Create an ARP resolver with a fixed gateway MAC.
    ///
    /// In AWS VPC, all outbound frames must use the gateway MAC as the
    /// Ethernet destination. When set, `resolve()` always returns this MAC.
    pub fn with_gateway_mac(mac: [u8; 6]) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            gateway_mac: Some(mac),
            ttl: Duration::from_secs(300),
        }
    }

    /// Insert a static entry into the cache.
    pub fn insert(&self, ip: IpAddr, mac: [u8; 6]) {
        let entry = CacheEntry {
            mac,
            expires: Instant::now() + self.ttl,
        };
        self.cache.write().unwrap().insert(ip, entry);
    }
}

impl Default for ArpResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl NeighborResolver for ArpResolver {
    fn resolve(&self, ip: IpAddr) -> io::Result<[u8; 6]> {
        // If gateway MAC is configured, always use it
        if let Some(mac) = self.gateway_mac {
            return Ok(mac);
        }

        // Check cache
        if let Some(mac) = self.lookup_cached(ip) {
            return Ok(mac);
        }

        // No cached entry and no gateway MAC — cannot resolve without
        // a backend to send ARP requests through. Return an error.
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("no cached MAC for {ip} and no gateway MAC configured"),
        ))
    }

    fn lookup_cached(&self, ip: IpAddr) -> Option<[u8; 6]> {
        if let Some(mac) = self.gateway_mac {
            return Some(mac);
        }
        let cache = self.cache.read().unwrap();
        let entry = cache.get(&ip)?;
        if entry.expires > Instant::now() {
            Some(entry.mac)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_resolver_with_gateway_mac() {
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let resolver = ArpResolver::with_gateway_mac(mac);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(resolver.resolve(ip).unwrap(), mac);
        assert_eq!(resolver.lookup_cached(ip), Some(mac));
    }

    #[test]
    fn test_resolver_cache_insert_and_lookup() {
        let resolver = ArpResolver::new();
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

        assert_eq!(resolver.lookup_cached(ip), None);

        resolver.insert(ip, mac);
        assert_eq!(resolver.lookup_cached(ip), Some(mac));
        assert_eq!(resolver.resolve(ip).unwrap(), mac);
    }

    #[test]
    fn test_resolver_no_entry_returns_error() {
        let resolver = ArpResolver::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99));
        assert!(resolver.resolve(ip).is_err());
    }
}
