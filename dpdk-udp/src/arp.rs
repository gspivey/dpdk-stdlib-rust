//! ARP (Address Resolution Protocol) implementation
//!
//! Handles ARP request/response for resolving IP addresses to MAC addresses.
//! Required for actual network communication beyond broadcast.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use dpdk::port::MacAddress;

// ============================================================================
// Constants
// ============================================================================

/// Ethernet type for ARP
pub const ETH_TYPE_ARP: u16 = 0x0806;

/// ARP hardware type for Ethernet
pub const ARP_HW_TYPE_ETHERNET: u16 = 1;

/// ARP protocol type for IPv4
pub const ARP_PROTO_TYPE_IPV4: u16 = 0x0800;

/// ARP operation: Request
pub const ARP_OP_REQUEST: u16 = 1;

/// ARP operation: Reply
pub const ARP_OP_REPLY: u16 = 2;

/// ARP packet size (excluding Ethernet header)
pub const ARP_PACKET_LEN: usize = 28;

/// Ethernet header + ARP packet
pub const ETH_ARP_FRAME_LEN: usize = 14 + ARP_PACKET_LEN;

/// Default ARP cache entry TTL
pub const ARP_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

// ============================================================================
// ARP Packet Structure
// ============================================================================

/// Parsed ARP packet
#[derive(Debug, Clone)]
pub struct ArpPacket {
    /// Hardware type (1 for Ethernet)
    pub hw_type: u16,
    /// Protocol type (0x0800 for IPv4)
    pub proto_type: u16,
    /// Hardware address length (6 for Ethernet)
    pub hw_len: u8,
    /// Protocol address length (4 for IPv4)
    pub proto_len: u8,
    /// Operation (1 = request, 2 = reply)
    pub operation: u16,
    /// Sender hardware address (MAC)
    pub sender_mac: [u8; 6],
    /// Sender protocol address (IP)
    pub sender_ip: Ipv4Addr,
    /// Target hardware address (MAC)
    pub target_mac: [u8; 6],
    /// Target protocol address (IP)
    pub target_ip: Ipv4Addr,
}

impl ArpPacket {
    /// Create a new ARP request
    pub fn request(sender_mac: [u8; 6], sender_ip: Ipv4Addr, target_ip: Ipv4Addr) -> Self {
        Self {
            hw_type: ARP_HW_TYPE_ETHERNET,
            proto_type: ARP_PROTO_TYPE_IPV4,
            hw_len: 6,
            proto_len: 4,
            operation: ARP_OP_REQUEST,
            sender_mac,
            sender_ip,
            target_mac: [0; 6], // Unknown - that's what we're asking for
            target_ip,
        }
    }

    /// Create a new ARP reply
    pub fn reply(
        sender_mac: [u8; 6],
        sender_ip: Ipv4Addr,
        target_mac: [u8; 6],
        target_ip: Ipv4Addr,
    ) -> Self {
        Self {
            hw_type: ARP_HW_TYPE_ETHERNET,
            proto_type: ARP_PROTO_TYPE_IPV4,
            hw_len: 6,
            proto_len: 4,
            operation: ARP_OP_REPLY,
            sender_mac,
            sender_ip,
            target_mac,
            target_ip,
        }
    }

    /// Check if this is an ARP request
    pub fn is_request(&self) -> bool {
        self.operation == ARP_OP_REQUEST
    }

    /// Check if this is an ARP reply
    pub fn is_reply(&self) -> bool {
        self.operation == ARP_OP_REPLY
    }
}

// ============================================================================
// ARP Parsing and Building
// ============================================================================

/// Parse an ARP packet from a raw Ethernet frame
///
/// Returns None if the frame is not a valid ARP packet
pub fn parse_arp_packet(frame: &[u8]) -> Option<ArpPacket> {
    // Minimum size check
    if frame.len() < ETH_ARP_FRAME_LEN {
        return None;
    }

    // Check Ethernet type
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETH_TYPE_ARP {
        return None;
    }

    // Parse ARP header (starts at byte 14)
    let arp = &frame[14..];

    let hw_type = u16::from_be_bytes([arp[0], arp[1]]);
    let proto_type = u16::from_be_bytes([arp[2], arp[3]]);
    let hw_len = arp[4];
    let proto_len = arp[5];
    let operation = u16::from_be_bytes([arp[6], arp[7]]);

    // Validate for Ethernet/IPv4
    if hw_type != ARP_HW_TYPE_ETHERNET || proto_type != ARP_PROTO_TYPE_IPV4 {
        return None;
    }
    if hw_len != 6 || proto_len != 4 {
        return None;
    }

    let sender_mac: [u8; 6] = arp[8..14].try_into().ok()?;
    let sender_ip = Ipv4Addr::new(arp[14], arp[15], arp[16], arp[17]);
    let target_mac: [u8; 6] = arp[18..24].try_into().ok()?;
    let target_ip = Ipv4Addr::new(arp[24], arp[25], arp[26], arp[27]);

    Some(ArpPacket {
        hw_type,
        proto_type,
        hw_len,
        proto_len,
        operation,
        sender_mac,
        sender_ip,
        target_mac,
        target_ip,
    })
}

/// Build an ARP packet into a raw Ethernet frame
pub fn build_arp_frame(
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    arp: &ArpPacket,
) -> [u8; ETH_ARP_FRAME_LEN] {
    let mut frame = [0u8; ETH_ARP_FRAME_LEN];

    // Ethernet header
    frame[0..6].copy_from_slice(dst_mac);
    frame[6..12].copy_from_slice(src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_ARP.to_be_bytes());

    // ARP header
    frame[14..16].copy_from_slice(&arp.hw_type.to_be_bytes());
    frame[16..18].copy_from_slice(&arp.proto_type.to_be_bytes());
    frame[18] = arp.hw_len;
    frame[19] = arp.proto_len;
    frame[20..22].copy_from_slice(&arp.operation.to_be_bytes());
    frame[22..28].copy_from_slice(&arp.sender_mac);
    frame[28..32].copy_from_slice(&arp.sender_ip.octets());
    frame[32..38].copy_from_slice(&arp.target_mac);
    frame[38..42].copy_from_slice(&arp.target_ip.octets());

    frame
}

/// Build an ARP request frame
pub fn build_arp_request(
    src_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
) -> [u8; ETH_ARP_FRAME_LEN] {
    let arp = ArpPacket::request(*src_mac, src_ip, target_ip);
    // ARP requests are sent to broadcast
    let broadcast_mac = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    build_arp_frame(src_mac, &broadcast_mac, &arp)
}

/// Build an ARP reply frame
pub fn build_arp_reply(
    src_mac: &[u8; 6],
    src_ip: Ipv4Addr,
    dst_mac: &[u8; 6],
    dst_ip: Ipv4Addr,
) -> [u8; ETH_ARP_FRAME_LEN] {
    let arp = ArpPacket::reply(*src_mac, src_ip, *dst_mac, dst_ip);
    build_arp_frame(src_mac, dst_mac, &arp)
}

// ============================================================================
// ARP Cache
// ============================================================================

/// An entry in the ARP cache
#[derive(Debug, Clone)]
pub struct ArpCacheEntry {
    /// MAC address
    pub mac: MacAddress,
    /// When this entry was added
    pub timestamp: Instant,
}

impl ArpCacheEntry {
    /// Create a new cache entry
    pub fn new(mac: MacAddress) -> Self {
        Self {
            mac,
            timestamp: Instant::now(),
        }
    }

    /// Check if this entry has expired
    pub fn is_expired(&self) -> bool {
        self.timestamp.elapsed() > ARP_CACHE_TTL
    }
}

/// Thread-safe ARP cache for storing IP -> MAC mappings
#[derive(Debug)]
pub struct ArpCache {
    entries: RwLock<HashMap<Ipv4Addr, ArpCacheEntry>>,
}

impl ArpCache {
    /// Create a new empty ARP cache
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Look up a MAC address for an IP
    pub fn lookup(&self, ip: &Ipv4Addr) -> Option<MacAddress> {
        let entries = self.entries.read().unwrap();
        entries.get(ip).and_then(|entry| {
            if entry.is_expired() {
                None
            } else {
                Some(entry.mac.clone())
            }
        })
    }

    /// Insert or update an entry
    pub fn insert(&self, ip: Ipv4Addr, mac: MacAddress) {
        let mut entries = self.entries.write().unwrap();
        entries.insert(ip, ArpCacheEntry::new(mac));
    }

    /// Remove an entry
    pub fn remove(&self, ip: &Ipv4Addr) {
        let mut entries = self.entries.write().unwrap();
        entries.remove(ip);
    }

    /// Remove all expired entries
    pub fn purge_expired(&self) {
        let mut entries = self.entries.write().unwrap();
        entries.retain(|_, entry| !entry.is_expired());
    }

    /// Get the number of entries (including expired)
    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.entries.read().unwrap().is_empty()
    }

    /// Clear all entries
    pub fn clear(&self) {
        self.entries.write().unwrap().clear();
    }
}

impl Default for ArpCache {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// ARP Handler
// ============================================================================

/// Handles ARP protocol operations
pub struct ArpHandler {
    /// Our MAC address
    pub local_mac: [u8; 6],
    /// Our IP addresses (we may have multiple)
    pub local_ips: Vec<Ipv4Addr>,
    /// ARP cache
    pub cache: Arc<ArpCache>,
}

impl ArpHandler {
    /// Create a new ARP handler
    pub fn new(local_mac: [u8; 6], local_ip: Ipv4Addr) -> Self {
        Self {
            local_mac,
            local_ips: vec![local_ip],
            cache: Arc::new(ArpCache::new()),
        }
    }

    /// Create with a shared cache
    pub fn with_cache(local_mac: [u8; 6], local_ip: Ipv4Addr, cache: Arc<ArpCache>) -> Self {
        Self {
            local_mac,
            local_ips: vec![local_ip],
            cache,
        }
    }

    /// Add a local IP address
    pub fn add_local_ip(&mut self, ip: Ipv4Addr) {
        if !self.local_ips.contains(&ip) {
            self.local_ips.push(ip);
        }
    }

    /// Process an incoming ARP packet
    ///
    /// Returns an ARP reply frame if this was a request for our IP,
    /// or None if no response is needed.
    pub fn process_arp(&self, frame: &[u8]) -> Option<[u8; ETH_ARP_FRAME_LEN]> {
        let arp = parse_arp_packet(frame)?;

        // Always learn from ARP packets we see (opportunistic learning)
        self.cache.insert(
            arp.sender_ip,
            MacAddress::new(arp.sender_mac),
        );

        if arp.is_request() {
            // Is this request for one of our IPs?
            if self.local_ips.contains(&arp.target_ip) {
                // Send a reply
                return Some(build_arp_reply(
                    &self.local_mac,
                    arp.target_ip,
                    &arp.sender_mac,
                    arp.sender_ip,
                ));
            }
        }

        // ARP reply - we already learned from it above
        None
    }

    /// Resolve an IP address to a MAC address
    ///
    /// Returns the cached MAC if available, or None if ARP is needed.
    pub fn resolve(&self, ip: &Ipv4Addr) -> Option<MacAddress> {
        // Check for broadcast
        if *ip == Ipv4Addr::BROADCAST {
            return Some(MacAddress::broadcast());
        }

        // Check cache
        self.cache.lookup(ip)
    }

    /// Generate an ARP request for an IP address
    pub fn make_request(&self, target_ip: Ipv4Addr) -> Option<[u8; ETH_ARP_FRAME_LEN]> {
        // Use the first local IP as the source
        let src_ip = self.local_ips.first()?;
        Some(build_arp_request(&self.local_mac, *src_ip, target_ip))
    }

    /// Get a reference to the ARP cache
    pub fn cache(&self) -> &Arc<ArpCache> {
        &self.cache
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arp_constants() {
        assert_eq!(ETH_TYPE_ARP, 0x0806);
        assert_eq!(ARP_HW_TYPE_ETHERNET, 1);
        assert_eq!(ARP_PROTO_TYPE_IPV4, 0x0800);
        assert_eq!(ARP_OP_REQUEST, 1);
        assert_eq!(ARP_OP_REPLY, 2);
        assert_eq!(ARP_PACKET_LEN, 28);
        assert_eq!(ETH_ARP_FRAME_LEN, 42);
    }

    #[test]
    fn test_arp_request_creation() {
        let sender_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let sender_ip = Ipv4Addr::new(192, 168, 1, 1);
        let target_ip = Ipv4Addr::new(192, 168, 1, 2);

        let arp = ArpPacket::request(sender_mac, sender_ip, target_ip);

        assert!(arp.is_request());
        assert!(!arp.is_reply());
        assert_eq!(arp.sender_mac, sender_mac);
        assert_eq!(arp.sender_ip, sender_ip);
        assert_eq!(arp.target_ip, target_ip);
        assert_eq!(arp.target_mac, [0; 6]);
    }

    #[test]
    fn test_arp_reply_creation() {
        let sender_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let sender_ip = Ipv4Addr::new(192, 168, 1, 1);
        let target_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let target_ip = Ipv4Addr::new(192, 168, 1, 2);

        let arp = ArpPacket::reply(sender_mac, sender_ip, target_mac, target_ip);

        assert!(!arp.is_request());
        assert!(arp.is_reply());
        assert_eq!(arp.sender_mac, sender_mac);
        assert_eq!(arp.target_mac, target_mac);
    }

    #[test]
    fn test_build_and_parse_arp_request() {
        let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let src_ip = Ipv4Addr::new(192, 168, 1, 100);
        let target_ip = Ipv4Addr::new(192, 168, 1, 1);

        let frame = build_arp_request(&src_mac, src_ip, target_ip);

        // Verify frame size
        assert_eq!(frame.len(), ETH_ARP_FRAME_LEN);

        // Parse it back
        let parsed = parse_arp_packet(&frame);
        assert!(parsed.is_some());

        let arp = parsed.unwrap();
        assert!(arp.is_request());
        assert_eq!(arp.sender_mac, src_mac);
        assert_eq!(arp.sender_ip, src_ip);
        assert_eq!(arp.target_ip, target_ip);
    }

    #[test]
    fn test_build_and_parse_arp_reply() {
        let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let src_ip = Ipv4Addr::new(192, 168, 1, 1);
        let dst_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let dst_ip = Ipv4Addr::new(192, 168, 1, 100);

        let frame = build_arp_reply(&src_mac, src_ip, &dst_mac, dst_ip);

        let parsed = parse_arp_packet(&frame);
        assert!(parsed.is_some());

        let arp = parsed.unwrap();
        assert!(arp.is_reply());
        assert_eq!(arp.sender_mac, src_mac);
        assert_eq!(arp.sender_ip, src_ip);
        assert_eq!(arp.target_mac, dst_mac);
        assert_eq!(arp.target_ip, dst_ip);
    }

    #[test]
    fn test_parse_invalid_frame() {
        // Too short
        let short = [0u8; 10];
        assert!(parse_arp_packet(&short).is_none());

        // Wrong ethertype
        let mut wrong_type = [0u8; ETH_ARP_FRAME_LEN];
        wrong_type[12..14].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4, not ARP
        assert!(parse_arp_packet(&wrong_type).is_none());
    }

    #[test]
    fn test_arp_cache() {
        let cache = ArpCache::new();

        let ip1 = Ipv4Addr::new(192, 168, 1, 1);
        let mac1 = MacAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);

        // Initially empty
        assert!(cache.is_empty());
        assert!(cache.lookup(&ip1).is_none());

        // Insert and lookup
        cache.insert(ip1, mac1.clone());
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());

        let found = cache.lookup(&ip1);
        assert!(found.is_some());
        assert_eq!(found.unwrap().octets(), mac1.octets());

        // Remove
        cache.remove(&ip1);
        assert!(cache.lookup(&ip1).is_none());
    }

    #[test]
    fn test_arp_cache_clear() {
        let cache = ArpCache::new();

        cache.insert(Ipv4Addr::new(10, 0, 0, 1), MacAddress::new([1, 2, 3, 4, 5, 6]));
        cache.insert(Ipv4Addr::new(10, 0, 0, 2), MacAddress::new([2, 3, 4, 5, 6, 7]));

        assert_eq!(cache.len(), 2);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_arp_handler_request_response() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = ArpHandler::new(local_mac, local_ip);

        // Create an ARP request for our IP
        let requester_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let requester_ip = Ipv4Addr::new(192, 168, 1, 100);
        let request = build_arp_request(&requester_mac, requester_ip, local_ip);

        // Process it - should get a reply
        let reply = handler.process_arp(&request);
        assert!(reply.is_some());

        // Parse the reply
        let reply_arp = parse_arp_packet(&reply.unwrap()).unwrap();
        assert!(reply_arp.is_reply());
        assert_eq!(reply_arp.sender_mac, local_mac);
        assert_eq!(reply_arp.sender_ip, local_ip);
        assert_eq!(reply_arp.target_mac, requester_mac);
        assert_eq!(reply_arp.target_ip, requester_ip);

        // The requester should now be in our cache
        let cached = handler.cache.lookup(&requester_ip);
        assert!(cached.is_some());
    }

    #[test]
    fn test_arp_handler_ignores_other_requests() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = ArpHandler::new(local_mac, local_ip);

        // Create an ARP request for a different IP
        let requester_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let requester_ip = Ipv4Addr::new(192, 168, 1, 100);
        let other_ip = Ipv4Addr::new(192, 168, 1, 50);
        let request = build_arp_request(&requester_mac, requester_ip, other_ip);

        // Process it - should NOT get a reply
        let reply = handler.process_arp(&request);
        assert!(reply.is_none());

        // But we should still learn the requester's MAC
        assert!(handler.cache.lookup(&requester_ip).is_some());
    }

    #[test]
    fn test_arp_handler_learn_from_reply() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = ArpHandler::new(local_mac, local_ip);

        // Create an ARP reply from another host
        let other_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let other_ip = Ipv4Addr::new(192, 168, 1, 2);
        let reply_frame = build_arp_reply(&other_mac, other_ip, &local_mac, local_ip);

        // Process it - no response needed for a reply
        let response = handler.process_arp(&reply_frame);
        assert!(response.is_none());

        // But we should have learned the MAC
        let cached = handler.cache.lookup(&other_ip);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().octets(), other_mac);
    }

    #[test]
    fn test_arp_handler_make_request() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = ArpHandler::new(local_mac, local_ip);

        let target_ip = Ipv4Addr::new(192, 168, 1, 254);
        let request_frame = handler.make_request(target_ip);
        assert!(request_frame.is_some());

        let arp = parse_arp_packet(&request_frame.unwrap()).unwrap();
        assert!(arp.is_request());
        assert_eq!(arp.sender_ip, local_ip);
        assert_eq!(arp.target_ip, target_ip);
    }

    #[test]
    fn test_arp_handler_resolve() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip = Ipv4Addr::new(192, 168, 1, 1);
        let handler = ArpHandler::new(local_mac, local_ip);

        // Unknown IP returns None
        let unknown = Ipv4Addr::new(192, 168, 1, 99);
        assert!(handler.resolve(&unknown).is_none());

        // Pre-populate cache
        let known_ip = Ipv4Addr::new(192, 168, 1, 2);
        let known_mac = MacAddress::new([0xde, 0xad, 0xbe, 0xef, 0x00, 0x01]);
        handler.cache.insert(known_ip, known_mac.clone());

        // Now it should resolve
        let resolved = handler.resolve(&known_ip);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().octets(), known_mac.octets());

        // Broadcast always resolves
        let broadcast = handler.resolve(&Ipv4Addr::BROADCAST);
        assert!(broadcast.is_some());
        assert_eq!(broadcast.unwrap().octets(), [0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn test_arp_handler_multiple_ips() {
        let local_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let local_ip1 = Ipv4Addr::new(192, 168, 1, 1);
        let local_ip2 = Ipv4Addr::new(10, 0, 0, 1);

        let mut handler = ArpHandler::new(local_mac, local_ip1);
        handler.add_local_ip(local_ip2);

        // Should respond to requests for either IP
        let requester_mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let requester_ip = Ipv4Addr::new(192, 168, 1, 100);

        let request1 = build_arp_request(&requester_mac, requester_ip, local_ip1);
        assert!(handler.process_arp(&request1).is_some());

        let request2 = build_arp_request(&requester_mac, requester_ip, local_ip2);
        assert!(handler.process_arp(&request2).is_some());

        // Should not respond to other IPs
        let other_ip = Ipv4Addr::new(172, 16, 0, 1);
        let request3 = build_arp_request(&requester_mac, requester_ip, other_ip);
        assert!(handler.process_arp(&request3).is_none());
    }
}
