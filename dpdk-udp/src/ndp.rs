//! NDP (Neighbor Discovery Protocol) implementation for IPv6.
//!
//! Mirrors the ARP handler (`arp.rs`) for IPv6: resolves IPv6 addresses to
//! link-layer (MAC) addresses using ICMPv6 Neighbor Solicitation (type 135)
//! and Neighbor Advertisement (type 136) messages per RFC 4861.
//!
//! Wire format for Neighbor Solicitation:
//! ```text
//! [Eth 14B][IPv6 40B][ICMPv6: type=135, code=0, cksum, reserved(4B), target(16B), options...]
//! ```
//!
//! Wire format for Neighbor Advertisement:
//! ```text
//! [Eth 14B][IPv6 40B][ICMPv6: type=136, code=0, cksum, flags+reserved(4B), target(16B), options...]
//! ```

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::icmpv6::icmpv6_checksum;
use crate::ipv6::{ETH_TYPE_IPV6, IPV6_HEADER_LEN, IP_PROTO_ICMPV6};
use crate::ipv6_addr::{ipv6_multicast_mac, solicited_node_multicast_addr};
use crate::ETH_HEADER_LEN;

// ============================================================================
// Constants
// ============================================================================

/// ICMPv6 type: Neighbor Solicitation (RFC 4861 §4.3)
pub const ICMPV6_TYPE_NEIGHBOR_SOLICITATION: u8 = 135;

/// ICMPv6 type: Neighbor Advertisement (RFC 4861 §4.4)
pub const ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT: u8 = 136;

/// NDP option type: Source Link-Layer Address (RFC 4861 §4.6.1)
pub const NDP_OPT_SOURCE_LINK_LAYER: u8 = 1;

/// NDP option type: Target Link-Layer Address (RFC 4861 §4.6.1)
pub const NDP_OPT_TARGET_LINK_LAYER: u8 = 2;

/// ICMPv6 header (type + code + checksum + reserved/flags) = 4 bytes before target
pub const NDP_ICMPV6_HEADER_LEN: usize = 4;

/// Target address field length (16 bytes for IPv6)
pub const NDP_TARGET_LEN: usize = 16;

/// Minimum NS/NA ICMPv6 body: 4B header + 4B reserved/flags + 16B target = 24 bytes
pub const NDP_MIN_BODY_LEN: usize = 24;

/// NDP link-layer address option length: type(1) + len(1) + MAC(6) = 8 bytes
pub const NDP_OPT_LINK_LAYER_LEN: usize = 8;

/// Full NS frame: Eth(14) + IPv6(40) + ICMPv6 NS body(24) + source LL option(8) = 86
pub const NDP_NS_FRAME_LEN: usize = ETH_HEADER_LEN + IPV6_HEADER_LEN + NDP_MIN_BODY_LEN + NDP_OPT_LINK_LAYER_LEN;

/// Full NA frame: Eth(14) + IPv6(40) + ICMPv6 NA body(24) + target LL option(8) = 86
pub const NDP_NA_FRAME_LEN: usize = ETH_HEADER_LEN + IPV6_HEADER_LEN + NDP_MIN_BODY_LEN + NDP_OPT_LINK_LAYER_LEN;

/// Default NDP cache entry TTL (same as ARP: 5 minutes)
pub const NDP_CACHE_TTL: Duration = Duration::from_secs(300);

/// Hop limit for NDP messages (MUST be 255 per RFC 4861 §7.1.1)
pub const NDP_HOP_LIMIT: u8 = 255;

/// NA flag: Router (R) — bit 31 of the flags word
pub const NA_FLAG_ROUTER: u32 = 0x8000_0000;

/// NA flag: Solicited (S) — bit 30
pub const NA_FLAG_SOLICITED: u32 = 0x4000_0000;

/// NA flag: Override (O) — bit 29
pub const NA_FLAG_OVERRIDE: u32 = 0x2000_0000;

// ============================================================================
// NDP Packet Structures
// ============================================================================

/// Parsed NDP Neighbor Solicitation or Advertisement message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdpPacket {
    /// Ethernet source MAC
    pub src_mac: [u8; 6],
    /// Ethernet destination MAC
    pub dst_mac: [u8; 6],
    /// IPv6 source address
    pub src_ip: Ipv6Addr,
    /// IPv6 destination address
    pub dst_ip: Ipv6Addr,
    /// ICMPv6 type (135 = NS, 136 = NA)
    pub icmp_type: u8,
    /// NA flags (Router, Solicited, Override) — 0 for NS
    pub flags: u32,
    /// Target IPv6 address being solicited/advertised
    pub target: Ipv6Addr,
    /// Link-layer address from option (Source LL for NS, Target LL for NA)
    pub link_layer_addr: Option<[u8; 6]>,
}

// ============================================================================
// Parsing
// ============================================================================

/// Parse an NDP Neighbor Solicitation or Advertisement from a raw Ethernet frame.
///
/// Returns `None` if the frame is not a valid NDP NS/NA message.
/// Validates hop limit == 255 per RFC 4861 §7.1.1.
pub fn parse_ndp_packet(frame: &[u8]) -> Option<NdpPacket> {
    if frame.len() < ETH_HEADER_LEN + IPV6_HEADER_LEN + NDP_MIN_BODY_LEN {
        return None;
    }

    // Check ethertype
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETH_TYPE_IPV6 {
        return None;
    }

    let l3 = ETH_HEADER_LEN;

    // Verify IPv6 version
    if (frame[l3] >> 4) != 6 {
        return None;
    }

    // Check next header is ICMPv6 (no extension header support for NDP — RFC 4861
    // says NDP messages MUST NOT include extension headers between IPv6 and ICMPv6)
    let next_header = frame[l3 + 6];
    if next_header != IP_PROTO_ICMPV6 {
        return None;
    }

    // Hop limit MUST be 255 (RFC 4861 §7.1.1)
    let hop_limit = frame[l3 + 7];
    if hop_limit != NDP_HOP_LIMIT {
        return None;
    }

    let icmp_start = l3 + IPV6_HEADER_LEN;
    if frame.len() < icmp_start + NDP_MIN_BODY_LEN {
        return None;
    }

    let icmp_type = frame[icmp_start];
    if icmp_type != ICMPV6_TYPE_NEIGHBOR_SOLICITATION
        && icmp_type != ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT
    {
        return None;
    }

    let icmp_code = frame[icmp_start + 1];
    if icmp_code != 0 {
        return None;
    }

    // Extract MACs and IPs
    let dst_mac: [u8; 6] = frame[0..6].try_into().ok()?;
    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 8..l3 + 24]).unwrap());
    let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&frame[l3 + 24..l3 + 40]).unwrap());

    // Flags (4 bytes after type+code+checksum) — only meaningful for NA
    let flags = u32::from_be_bytes([
        frame[icmp_start + 4],
        frame[icmp_start + 5],
        frame[icmp_start + 6],
        frame[icmp_start + 7],
    ]);

    // Target address (16 bytes at offset 8 from ICMPv6 start)
    let target = Ipv6Addr::from(
        <[u8; 16]>::try_from(&frame[icmp_start + 8..icmp_start + 24]).unwrap(),
    );

    // Parse options (look for link-layer address)
    let link_layer_addr = parse_ndp_ll_option(frame, icmp_start + 24, icmp_type);

    Some(NdpPacket {
        src_mac,
        dst_mac,
        src_ip,
        dst_ip,
        icmp_type,
        flags: if icmp_type == ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT { flags } else { 0 },
        target,
        link_layer_addr,
    })
}

/// Parse NDP options to extract the link-layer address.
/// For NS: looks for Source Link-Layer Address (type 1).
/// For NA: looks for Target Link-Layer Address (type 2).
fn parse_ndp_ll_option(frame: &[u8], options_start: usize, icmp_type: u8) -> Option<[u8; 6]> {
    let expected_type = if icmp_type == ICMPV6_TYPE_NEIGHBOR_SOLICITATION {
        NDP_OPT_SOURCE_LINK_LAYER
    } else {
        NDP_OPT_TARGET_LINK_LAYER
    };

    let mut offset = options_start;
    while offset + 2 <= frame.len() {
        let opt_type = frame[offset];
        let opt_len = frame[offset + 1]; // in units of 8 bytes
        if opt_len == 0 {
            break; // prevent infinite loop
        }
        let opt_bytes = (opt_len as usize) * 8;
        if offset + opt_bytes > frame.len() {
            break;
        }
        if opt_type == expected_type && opt_bytes >= 8 {
            let mac: [u8; 6] = frame[offset + 2..offset + 8].try_into().ok()?;
            return Some(mac);
        }
        offset += opt_bytes;
    }
    None
}

// ============================================================================
// Building
// ============================================================================

/// Build a Neighbor Solicitation frame.
///
/// Sent to the solicited-node multicast address of `target_ip` with the
/// source link-layer address option containing `src_mac`.
pub fn build_neighbor_solicitation(
    src_mac: &[u8; 6],
    src_ip: &Ipv6Addr,
    target_ip: &Ipv6Addr,
) -> [u8; NDP_NS_FRAME_LEN] {
    let mut frame = [0u8; NDP_NS_FRAME_LEN];

    // Destination: solicited-node multicast MAC
    let snm_addr = solicited_node_multicast_addr(target_ip);
    let dst_mac = ipv6_multicast_mac(&snm_addr);
    frame[0..6].copy_from_slice(&dst_mac);
    frame[6..12].copy_from_slice(src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

    // IPv6 header
    let l3 = ETH_HEADER_LEN;
    let icmp_len = NDP_MIN_BODY_LEN + NDP_OPT_LINK_LAYER_LEN;
    frame[l3] = 0x60; // version 6
    // payload length
    frame[l3 + 4..l3 + 6].copy_from_slice(&(icmp_len as u16).to_be_bytes());
    frame[l3 + 6] = IP_PROTO_ICMPV6; // next header
    frame[l3 + 7] = NDP_HOP_LIMIT; // hop limit = 255
    frame[l3 + 8..l3 + 24].copy_from_slice(&src_ip.octets());
    frame[l3 + 24..l3 + 40].copy_from_slice(&snm_addr.octets());

    // ICMPv6 NS body
    let icmp_start = l3 + IPV6_HEADER_LEN;
    frame[icmp_start] = ICMPV6_TYPE_NEIGHBOR_SOLICITATION;
    frame[icmp_start + 1] = 0; // code
    // checksum at [2..4] — filled below
    // reserved [4..8] = 0
    // target address
    frame[icmp_start + 8..icmp_start + 24].copy_from_slice(&target_ip.octets());

    // Source Link-Layer Address option
    let opt_start = icmp_start + 24;
    frame[opt_start] = NDP_OPT_SOURCE_LINK_LAYER;
    frame[opt_start + 1] = 1; // length in 8-byte units
    frame[opt_start + 2..opt_start + 8].copy_from_slice(src_mac);

    // Compute ICMPv6 checksum
    let cksum = icmpv6_checksum(src_ip, &snm_addr, &frame[icmp_start..icmp_start + icmp_len]);
    frame[icmp_start + 2..icmp_start + 4].copy_from_slice(&cksum.to_be_bytes());

    frame
}

/// Build a Neighbor Advertisement frame (solicited reply).
///
/// Sent as a unicast reply to a Neighbor Solicitation.
pub fn build_neighbor_advertisement(
    src_mac: &[u8; 6],
    src_ip: &Ipv6Addr,
    dst_mac: &[u8; 6],
    dst_ip: &Ipv6Addr,
    flags: u32,
) -> [u8; NDP_NA_FRAME_LEN] {
    let mut frame = [0u8; NDP_NA_FRAME_LEN];

    frame[0..6].copy_from_slice(dst_mac);
    frame[6..12].copy_from_slice(src_mac);
    frame[12..14].copy_from_slice(&ETH_TYPE_IPV6.to_be_bytes());

    // IPv6 header
    let l3 = ETH_HEADER_LEN;
    let icmp_len = NDP_MIN_BODY_LEN + NDP_OPT_LINK_LAYER_LEN;
    frame[l3] = 0x60; // version 6
    frame[l3 + 4..l3 + 6].copy_from_slice(&(icmp_len as u16).to_be_bytes());
    frame[l3 + 6] = IP_PROTO_ICMPV6;
    frame[l3 + 7] = NDP_HOP_LIMIT;
    frame[l3 + 8..l3 + 24].copy_from_slice(&src_ip.octets());
    frame[l3 + 24..l3 + 40].copy_from_slice(&dst_ip.octets());

    // ICMPv6 NA body
    let icmp_start = l3 + IPV6_HEADER_LEN;
    frame[icmp_start] = ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT;
    frame[icmp_start + 1] = 0; // code
    // flags
    frame[icmp_start + 4..icmp_start + 8].copy_from_slice(&flags.to_be_bytes());
    // target = our IP (the address being advertised)
    frame[icmp_start + 8..icmp_start + 24].copy_from_slice(&src_ip.octets());

    // Target Link-Layer Address option
    let opt_start = icmp_start + 24;
    frame[opt_start] = NDP_OPT_TARGET_LINK_LAYER;
    frame[opt_start + 1] = 1; // length in 8-byte units
    frame[opt_start + 2..opt_start + 8].copy_from_slice(src_mac);

    // Compute ICMPv6 checksum
    let cksum = icmpv6_checksum(src_ip, dst_ip, &frame[icmp_start..icmp_start + icmp_len]);
    frame[icmp_start + 2..icmp_start + 4].copy_from_slice(&cksum.to_be_bytes());

    frame
}

/// Build a gratuitous Neighbor Advertisement (unsolicited).
///
/// Sent to the all-nodes multicast address (ff02::1) to announce our
/// presence on the link. Equivalent to Gratuitous ARP for IPv6.
pub fn build_gratuitous_na(
    src_mac: &[u8; 6],
    src_ip: &Ipv6Addr,
) -> [u8; NDP_NA_FRAME_LEN] {
    let all_nodes: Ipv6Addr = "ff02::1".parse().unwrap();
    let dst_mac = ipv6_multicast_mac(&all_nodes);
    let flags = NA_FLAG_OVERRIDE; // O=1, S=0 (unsolicited), R=0
    build_neighbor_advertisement(src_mac, src_ip, &dst_mac, &all_nodes, flags)
}

// ============================================================================
// NDP Cache
// ============================================================================

/// An entry in the NDP neighbor cache.
#[derive(Debug, Clone)]
pub struct NdpCacheEntry {
    pub mac: [u8; 6],
    pub timestamp: Instant,
}

impl NdpCacheEntry {
    pub fn new(mac: [u8; 6]) -> Self {
        Self { mac, timestamp: Instant::now() }
    }

    pub fn is_expired(&self) -> bool {
        self.timestamp.elapsed() > NDP_CACHE_TTL
    }
}

/// Thread-safe NDP neighbor cache (IPv6 → MAC).
///
/// Uses the same atomic fast-path pattern as `ArpCache`: the most recently
/// seen (IPv6, MAC) pair is cached in atomics for lock-free lookup in the
/// common single-peer case.
pub struct NdpCache {
    entries: RwLock<HashMap<Ipv6Addr, NdpCacheEntry>>,
    /// Fast-path: high 64 bits of last-seen IPv6 address.
    fast_ip_hi: AtomicU64,
    /// Fast-path: low 64 bits of last-seen IPv6 address.
    fast_ip_lo: AtomicU64,
    /// Fast-path: last-seen MAC (6 bytes packed into lower 48 bits).
    fast_mac: AtomicU64,
}

impl std::fmt::Debug for NdpCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NdpCache")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

#[inline]
fn ipv6_to_u64_pair(addr: &Ipv6Addr) -> (u64, u64) {
    let octets = addr.octets();
    let hi = u64::from_be_bytes(octets[0..8].try_into().unwrap());
    let lo = u64::from_be_bytes(octets[8..16].try_into().unwrap());
    (hi, lo)
}

#[inline]
fn mac_to_u64(mac: &[u8; 6]) -> u64 {
    (mac[0] as u64) << 40
        | (mac[1] as u64) << 32
        | (mac[2] as u64) << 24
        | (mac[3] as u64) << 16
        | (mac[4] as u64) << 8
        | (mac[5] as u64)
}

#[inline]
fn u64_to_mac(val: u64) -> [u8; 6] {
    [
        (val >> 40) as u8,
        (val >> 32) as u8,
        (val >> 24) as u8,
        (val >> 16) as u8,
        (val >> 8) as u8,
        val as u8,
    ]
}

impl NdpCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            fast_ip_hi: AtomicU64::new(0),
            fast_ip_lo: AtomicU64::new(0),
            fast_mac: AtomicU64::new(0),
        }
    }

    /// Look up a MAC address for an IPv6 address.
    pub fn lookup(&self, ip: &Ipv6Addr) -> Option<[u8; 6]> {
        let (hi, lo) = ipv6_to_u64_pair(ip);

        // Fast-path
        let cached_hi = self.fast_ip_hi.load(Ordering::Relaxed);
        let cached_lo = self.fast_ip_lo.load(Ordering::Relaxed);
        if (cached_hi != 0 || cached_lo != 0) && cached_hi == hi && cached_lo == lo {
            let mac_bits = self.fast_mac.load(Ordering::Relaxed);
            return Some(u64_to_mac(mac_bits));
        }

        // Slow-path
        let entries = self.entries.read().unwrap();
        entries.get(ip).and_then(|entry| {
            if entry.is_expired() { None } else { Some(entry.mac) }
        })
    }

    /// Insert or update an entry.
    pub fn insert(&self, ip: Ipv6Addr, mac: [u8; 6]) {
        let (hi, lo) = ipv6_to_u64_pair(&ip);
        self.fast_ip_hi.store(hi, Ordering::Relaxed);
        self.fast_ip_lo.store(lo, Ordering::Relaxed);
        self.fast_mac.store(mac_to_u64(&mac), Ordering::Relaxed);

        let mut entries = self.entries.write().unwrap();
        entries.insert(ip, NdpCacheEntry::new(mac));
    }

    /// Insert only if the mapping has changed (avoids write lock in steady state).
    #[inline]
    pub fn insert_if_changed(&self, ip: &Ipv6Addr, mac: &[u8; 6]) {
        let (hi, lo) = ipv6_to_u64_pair(ip);
        let mac_bits = mac_to_u64(mac);

        if self.fast_ip_hi.load(Ordering::Relaxed) == hi
            && self.fast_ip_lo.load(Ordering::Relaxed) == lo
            && self.fast_mac.load(Ordering::Relaxed) == mac_bits
        {
            return;
        }

        self.fast_ip_hi.store(hi, Ordering::Relaxed);
        self.fast_ip_lo.store(lo, Ordering::Relaxed);
        self.fast_mac.store(mac_bits, Ordering::Relaxed);

        let mut entries = self.entries.write().unwrap();
        entries.insert(*ip, NdpCacheEntry::new(*mac));
    }

    pub fn remove(&self, ip: &Ipv6Addr) {
        let (hi, lo) = ipv6_to_u64_pair(ip);
        if self.fast_ip_hi.load(Ordering::Relaxed) == hi
            && self.fast_ip_lo.load(Ordering::Relaxed) == lo
        {
            self.fast_ip_hi.store(0, Ordering::Relaxed);
            self.fast_ip_lo.store(0, Ordering::Relaxed);
            self.fast_mac.store(0, Ordering::Relaxed);
        }
        let mut entries = self.entries.write().unwrap();
        entries.remove(ip);
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().unwrap().is_empty()
    }

    pub fn clear(&self) {
        self.fast_ip_hi.store(0, Ordering::Relaxed);
        self.fast_ip_lo.store(0, Ordering::Relaxed);
        self.fast_mac.store(0, Ordering::Relaxed);
        self.entries.write().unwrap().clear();
    }

    pub fn purge_expired(&self) {
        let mut entries = self.entries.write().unwrap();
        entries.retain(|_, entry| !entry.is_expired());
    }
}

impl Default for NdpCache {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// NDP Handler
// ============================================================================

/// Handles NDP protocol operations for IPv6 neighbor resolution.
///
/// Mirrors `ArpHandler`: processes incoming NS/NA, generates solicitations,
/// and maintains the neighbor cache.
pub struct NdpHandler {
    /// Our MAC address.
    pub local_mac: [u8; 6],
    /// Our IPv6 addresses (we may have multiple: link-local + global).
    pub local_ips: Vec<Ipv6Addr>,
    /// Neighbor cache.
    pub cache: Arc<NdpCache>,
}

impl NdpHandler {
    pub fn new(local_mac: [u8; 6], local_ip: Ipv6Addr) -> Self {
        Self {
            local_mac,
            local_ips: vec![local_ip],
            cache: Arc::new(NdpCache::new()),
        }
    }

    pub fn with_cache(local_mac: [u8; 6], local_ip: Ipv6Addr, cache: Arc<NdpCache>) -> Self {
        Self {
            local_mac,
            local_ips: vec![local_ip],
            cache,
        }
    }

    pub fn add_local_ip(&mut self, ip: Ipv6Addr) {
        if !self.local_ips.contains(&ip) {
            self.local_ips.push(ip);
        }
    }

    /// Process an incoming NDP packet.
    ///
    /// Returns a Neighbor Advertisement frame if this was a solicitation for
    /// one of our addresses, or `None` if no response is needed.
    pub fn process_ndp(&self, frame: &[u8]) -> Option<Vec<u8>> {
        let pkt = parse_ndp_packet(frame)?;

        match pkt.icmp_type {
            ICMPV6_TYPE_NEIGHBOR_SOLICITATION => {
                // Learn sender's MAC from the source LL option
                if let Some(sender_mac) = pkt.link_layer_addr {
                    if !pkt.src_ip.is_unspecified() {
                        self.cache.insert_if_changed(&pkt.src_ip, &sender_mac);
                    }
                }

                // Is this solicitation for one of our IPs?
                if self.local_ips.contains(&pkt.target) {
                    let flags = NA_FLAG_SOLICITED | NA_FLAG_OVERRIDE;
                    let na = build_neighbor_advertisement(
                        &self.local_mac,
                        &pkt.target,
                        &pkt.src_mac,
                        &pkt.src_ip,
                        flags,
                    );
                    return Some(na.to_vec());
                }
            }
            ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT => {
                // Learn from NA: target address → link-layer address
                if let Some(target_mac) = pkt.link_layer_addr {
                    self.cache.insert_if_changed(&pkt.target, &target_mac);
                }
            }
            _ => {}
        }

        None
    }

    /// Resolve an IPv6 address to a MAC address from the cache.
    pub fn resolve(&self, ip: &Ipv6Addr) -> Option<[u8; 6]> {
        // All-nodes multicast
        if ip.octets()[0] == 0xFF {
            return Some(ipv6_multicast_mac(ip));
        }
        self.cache.lookup(ip)
    }

    /// Generate a Neighbor Solicitation frame for the given target.
    pub fn make_solicitation(&self, target_ip: &Ipv6Addr) -> Option<[u8; NDP_NS_FRAME_LEN]> {
        let src_ip = self.local_ips.first()?;
        Some(build_neighbor_solicitation(&self.local_mac, src_ip, target_ip))
    }

    /// Generate a gratuitous Neighbor Advertisement.
    pub fn make_gratuitous_na(&self) -> Option<[u8; NDP_NA_FRAME_LEN]> {
        let src_ip = self.local_ips.first()?;
        Some(build_gratuitous_na(&self.local_mac, src_ip))
    }

    /// Get a reference to the NDP cache.
    pub fn cache(&self) -> &Arc<NdpCache> {
        &self.cache
    }
}

// ============================================================================
// Kernel Neighbor Cache Seeding
// ============================================================================

/// An entry parsed from `/proc/net/ipv6_neigh` (Linux only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcNeighEntry {
    pub ip: Ipv6Addr,
    pub mac: [u8; 6],
    pub interface: String,
}

/// Parse `/proc/net/ipv6_neigh` content and return valid REACHABLE/STALE entries.
///
/// Format: `<ipv6_addr> <dev_idx> <mac> <state_hex> <flags_hex> <dev_name>`
/// We only import entries in REACHABLE (0x02) or STALE (0x01) state.
pub fn parse_proc_ipv6_neigh(content: &str) -> Vec<ProcNeighEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let ip: Ipv6Addr = match parts[0].parse() {
            Ok(ip) => ip,
            Err(_) => continue,
        };
        let mac = match parse_mac_str(parts[2]) {
            Some(m) => m,
            None => continue,
        };
        // State is hex: 0x02 = REACHABLE, 0x01 = STALE
        let state = match u32::from_str_radix(parts[3].trim_start_matches("0x"), 16) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if state != 0x01 && state != 0x02 {
            continue;
        }
        // Skip zero MAC (incomplete entries)
        if mac == [0; 6] {
            continue;
        }
        let interface = parts[5].to_string();
        entries.push(ProcNeighEntry { ip, mac, interface });
    }
    entries
}

/// Seed an NDP cache from parsed /proc/net/ipv6_neigh entries.
pub fn seed_cache_from_proc(cache: &NdpCache, entries: &[ProcNeighEntry]) {
    for entry in entries {
        cache.insert(entry.ip, entry.mac);
    }
}

fn parse_mac_str(s: &str) -> Option<[u8; 6]> {
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn local_mac() -> [u8; 6] {
        [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]
    }

    fn peer_mac() -> [u8; 6] {
        [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]
    }

    fn local_ip() -> Ipv6Addr {
        "fe80::1122:33ff:fe44:5566".parse().unwrap()
    }

    fn peer_ip() -> Ipv6Addr {
        "fe80::aabb:ccff:fedd:eeff".parse().unwrap()
    }

    fn global_ip() -> Ipv6Addr {
        "2001:db8::1".parse().unwrap()
    }

    // ====================================================================
    // Constants
    // ====================================================================

    #[test]
    fn constants_are_correct() {
        assert_eq!(ICMPV6_TYPE_NEIGHBOR_SOLICITATION, 135);
        assert_eq!(ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT, 136);
        assert_eq!(NDP_OPT_SOURCE_LINK_LAYER, 1);
        assert_eq!(NDP_OPT_TARGET_LINK_LAYER, 2);
        assert_eq!(NDP_NS_FRAME_LEN, 86);
        assert_eq!(NDP_NA_FRAME_LEN, 86);
        assert_eq!(NDP_HOP_LIMIT, 255);
    }

    // ====================================================================
    // Build + Parse roundtrip
    // ====================================================================

    #[test]
    fn build_and_parse_ns_roundtrip() {
        let frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &peer_ip());
        assert_eq!(frame.len(), NDP_NS_FRAME_LEN);

        let pkt = parse_ndp_packet(&frame).expect("should parse NS");
        assert_eq!(pkt.icmp_type, ICMPV6_TYPE_NEIGHBOR_SOLICITATION);
        assert_eq!(pkt.src_mac, local_mac());
        assert_eq!(pkt.src_ip, local_ip());
        assert_eq!(pkt.target, peer_ip());
        assert_eq!(pkt.link_layer_addr, Some(local_mac()));
        assert_eq!(pkt.flags, 0);
    }

    #[test]
    fn build_and_parse_na_roundtrip() {
        let flags = NA_FLAG_SOLICITED | NA_FLAG_OVERRIDE;
        let frame = build_neighbor_advertisement(
            &local_mac(), &local_ip(), &peer_mac(), &peer_ip(), flags,
        );
        assert_eq!(frame.len(), NDP_NA_FRAME_LEN);

        let pkt = parse_ndp_packet(&frame).expect("should parse NA");
        assert_eq!(pkt.icmp_type, ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT);
        assert_eq!(pkt.src_mac, local_mac());
        assert_eq!(pkt.dst_mac, peer_mac());
        assert_eq!(pkt.src_ip, local_ip());
        assert_eq!(pkt.dst_ip, peer_ip());
        assert_eq!(pkt.target, local_ip());
        assert_eq!(pkt.link_layer_addr, Some(local_mac()));
        assert_eq!(pkt.flags, flags);
    }

    #[test]
    fn build_gratuitous_na_roundtrip() {
        let frame = build_gratuitous_na(&local_mac(), &local_ip());
        let pkt = parse_ndp_packet(&frame).expect("should parse gratuitous NA");

        assert_eq!(pkt.icmp_type, ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT);
        assert_eq!(pkt.src_ip, local_ip());
        assert_eq!(pkt.target, local_ip());
        assert_eq!(pkt.flags, NA_FLAG_OVERRIDE);
        // Destination is all-nodes multicast MAC
        let all_nodes: Ipv6Addr = "ff02::1".parse().unwrap();
        assert_eq!(pkt.dst_mac, ipv6_multicast_mac(&all_nodes));
        assert_eq!(pkt.dst_ip, all_nodes);
    }

    // ====================================================================
    // NS destination addressing
    // ====================================================================

    #[test]
    fn ns_uses_solicited_node_multicast_dst() {
        let target: Ipv6Addr = "2001:db8::dead:beef".parse().unwrap();
        let frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &target);

        // IPv6 dst should be solicited-node multicast of target
        let expected_dst = solicited_node_multicast_addr(&target);
        let pkt = parse_ndp_packet(&frame).unwrap();
        assert_eq!(pkt.dst_ip, expected_dst);

        // Ethernet dst should be the multicast MAC for that address
        let expected_mac = ipv6_multicast_mac(&expected_dst);
        assert_eq!(pkt.dst_mac, expected_mac);
    }

    // ====================================================================
    // Parse validation
    // ====================================================================

    #[test]
    fn parse_rejects_too_short() {
        let frame = [0u8; 50]; // too short
        assert!(parse_ndp_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_wrong_ethertype() {
        let mut frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &peer_ip());
        frame[12] = 0x08; frame[13] = 0x00; // IPv4 ethertype
        assert!(parse_ndp_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_wrong_hop_limit() {
        let mut frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &peer_ip());
        frame[ETH_HEADER_LEN + 7] = 64; // not 255
        assert!(parse_ndp_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_non_icmpv6_next_header() {
        let mut frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &peer_ip());
        frame[ETH_HEADER_LEN + 6] = 17; // UDP instead of ICMPv6
        assert!(parse_ndp_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_wrong_icmpv6_type() {
        let mut frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &peer_ip());
        frame[ETH_HEADER_LEN + IPV6_HEADER_LEN] = 128; // Echo Request
        assert!(parse_ndp_packet(&frame).is_none());
    }

    #[test]
    fn parse_rejects_nonzero_code() {
        let mut frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &peer_ip());
        frame[ETH_HEADER_LEN + IPV6_HEADER_LEN + 1] = 1; // code must be 0
        assert!(parse_ndp_packet(&frame).is_none());
    }

    // ====================================================================
    // NDP Cache
    // ====================================================================

    #[test]
    fn cache_insert_and_lookup() {
        let cache = NdpCache::new();
        let ip = peer_ip();
        let mac = peer_mac();

        assert!(cache.is_empty());
        assert!(cache.lookup(&ip).is_none());

        cache.insert(ip, mac);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.lookup(&ip), Some(mac));
    }

    #[test]
    fn cache_fast_path() {
        let cache = NdpCache::new();
        let ip1 = peer_ip();
        let mac1 = peer_mac();
        let ip2 = global_ip();
        let mac2 = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];

        cache.insert(ip1, mac1);
        assert_eq!(cache.lookup(&ip1), Some(mac1));

        // Second insert moves fast-path to ip2
        cache.insert(ip2, mac2);
        assert_eq!(cache.lookup(&ip2), Some(mac2));

        // ip1 still accessible via slow path
        assert_eq!(cache.lookup(&ip1), Some(mac1));
    }

    #[test]
    fn cache_remove_invalidates_fast_path() {
        let cache = NdpCache::new();
        let ip = peer_ip();
        cache.insert(ip, peer_mac());
        assert!(cache.lookup(&ip).is_some());

        cache.remove(&ip);
        assert!(cache.lookup(&ip).is_none());
    }

    #[test]
    fn cache_clear() {
        let cache = NdpCache::new();
        cache.insert(peer_ip(), peer_mac());
        cache.insert(global_ip(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.lookup(&peer_ip()).is_none());
    }

    #[test]
    fn cache_insert_if_changed_skips_duplicate() {
        let cache = NdpCache::new();
        let ip = peer_ip();
        let mac = peer_mac();

        cache.insert(ip, mac);
        // This should be a no-op (same mapping)
        cache.insert_if_changed(&ip, &mac);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn mac_packing_roundtrip() {
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(u64_to_mac(mac_to_u64(&mac)), mac);

        let zeros = [0u8; 6];
        assert_eq!(u64_to_mac(mac_to_u64(&zeros)), zeros);

        let ones = [0xFF; 6];
        assert_eq!(u64_to_mac(mac_to_u64(&ones)), ones);
    }

    // ====================================================================
    // NDP Handler
    // ====================================================================

    #[test]
    fn handler_responds_to_ns_for_our_ip() {
        let handler = NdpHandler::new(local_mac(), local_ip());

        // Peer sends NS for our IP
        let ns = build_neighbor_solicitation(&peer_mac(), &peer_ip(), &local_ip());
        let reply = handler.process_ndp(&ns);
        assert!(reply.is_some(), "should reply to NS for our IP");

        let na = parse_ndp_packet(&reply.unwrap()).unwrap();
        assert_eq!(na.icmp_type, ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT);
        assert_eq!(na.src_mac, local_mac());
        assert_eq!(na.target, local_ip());
        assert!(na.flags & NA_FLAG_SOLICITED != 0);
        assert!(na.flags & NA_FLAG_OVERRIDE != 0);
    }

    #[test]
    fn handler_ignores_ns_for_other_ip() {
        let handler = NdpHandler::new(local_mac(), local_ip());
        let other_ip: Ipv6Addr = "fe80::9999".parse().unwrap();

        let ns = build_neighbor_solicitation(&peer_mac(), &peer_ip(), &other_ip);
        let reply = handler.process_ndp(&ns);
        assert!(reply.is_none());
    }

    #[test]
    fn handler_learns_from_ns() {
        let handler = NdpHandler::new(local_mac(), local_ip());

        let ns = build_neighbor_solicitation(&peer_mac(), &peer_ip(), &local_ip());
        handler.process_ndp(&ns);

        // Should have learned peer's MAC
        assert_eq!(handler.cache.lookup(&peer_ip()), Some(peer_mac()));
    }

    #[test]
    fn handler_learns_from_na() {
        let handler = NdpHandler::new(local_mac(), local_ip());

        let flags = NA_FLAG_SOLICITED | NA_FLAG_OVERRIDE;
        let na = build_neighbor_advertisement(
            &peer_mac(), &peer_ip(), &local_mac(), &local_ip(), flags,
        );
        let reply = handler.process_ndp(&na);
        assert!(reply.is_none(), "NA should not trigger a reply");

        // Should have learned peer's MAC from target LL option
        assert_eq!(handler.cache.lookup(&peer_ip()), Some(peer_mac()));
    }

    #[test]
    fn handler_multiple_ips() {
        let mut handler = NdpHandler::new(local_mac(), local_ip());
        handler.add_local_ip(global_ip());

        // Should respond to NS for either IP
        let ns1 = build_neighbor_solicitation(&peer_mac(), &peer_ip(), &local_ip());
        assert!(handler.process_ndp(&ns1).is_some());

        let ns2 = build_neighbor_solicitation(&peer_mac(), &peer_ip(), &global_ip());
        assert!(handler.process_ndp(&ns2).is_some());
    }

    #[test]
    fn handler_resolve_multicast() {
        let handler = NdpHandler::new(local_mac(), local_ip());
        let all_nodes: Ipv6Addr = "ff02::1".parse().unwrap();

        let mac = handler.resolve(&all_nodes);
        assert_eq!(mac, Some(ipv6_multicast_mac(&all_nodes)));
    }

    #[test]
    fn handler_resolve_unknown() {
        let handler = NdpHandler::new(local_mac(), local_ip());
        assert!(handler.resolve(&peer_ip()).is_none());
    }

    #[test]
    fn handler_make_solicitation() {
        let handler = NdpHandler::new(local_mac(), local_ip());
        let frame = handler.make_solicitation(&peer_ip());
        assert!(frame.is_some());

        let pkt = parse_ndp_packet(&frame.unwrap()).unwrap();
        assert_eq!(pkt.icmp_type, ICMPV6_TYPE_NEIGHBOR_SOLICITATION);
        assert_eq!(pkt.src_ip, local_ip());
        assert_eq!(pkt.target, peer_ip());
    }

    #[test]
    fn handler_make_gratuitous_na() {
        let handler = NdpHandler::new(local_mac(), local_ip());
        let frame = handler.make_gratuitous_na();
        assert!(frame.is_some());

        let pkt = parse_ndp_packet(&frame.unwrap()).unwrap();
        assert_eq!(pkt.icmp_type, ICMPV6_TYPE_NEIGHBOR_ADVERTISEMENT);
        assert_eq!(pkt.target, local_ip());
        assert_eq!(pkt.flags, NA_FLAG_OVERRIDE);
    }

    // ====================================================================
    // /proc/net/ipv6_neigh parsing
    // ====================================================================

    #[test]
    fn parse_proc_ipv6_neigh_valid() {
        let content = "\
fe80::1 00000001 aa:bb:cc:dd:ee:ff 0x02 0x00 eth0
2001:db8::2 00000001 11:22:33:44:55:66 0x01 0x00 eth0
fe80::3 00000001 00:00:00:00:00:00 0x02 0x00 eth0
fe80::4 00000001 de:ad:be:ef:00:01 0x04 0x00 eth0";

        let entries = parse_proc_ipv6_neigh(content);
        assert_eq!(entries.len(), 2); // skips zero MAC and non-REACHABLE/STALE

        assert_eq!(entries[0].ip, "fe80::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(entries[0].mac, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(entries[0].interface, "eth0");

        assert_eq!(entries[1].ip, "2001:db8::2".parse::<Ipv6Addr>().unwrap());
        assert_eq!(entries[1].mac, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn parse_proc_ipv6_neigh_empty() {
        assert!(parse_proc_ipv6_neigh("").is_empty());
        assert!(parse_proc_ipv6_neigh("garbage line").is_empty());
    }

    #[test]
    fn seed_cache_from_proc_entries() {
        let cache = NdpCache::new();
        let entries = vec![
            ProcNeighEntry {
                ip: "fe80::1".parse().unwrap(),
                mac: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
                interface: "eth0".to_string(),
            },
            ProcNeighEntry {
                ip: "2001:db8::2".parse().unwrap(),
                mac: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
                interface: "eth0".to_string(),
            },
        ];

        seed_cache_from_proc(&cache, &entries);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.lookup(&"fe80::1".parse().unwrap()), Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
        assert_eq!(cache.lookup(&"2001:db8::2".parse().unwrap()), Some([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]));
    }

    // ====================================================================
    // Synthetic performance test
    // ====================================================================

    #[test]
    fn perf_ndp_cache_lookup() {
        let cache = NdpCache::new();
        let ip = peer_ip();
        cache.insert(ip, peer_mac());

        let iterations = 100_000;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            let _ = std::hint::black_box(cache.lookup(std::hint::black_box(&ip)));
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!("[PERF] NDP cache lookup (fast-path): {} iterations in {:?} ({} ns/op)", iterations, elapsed, ns_per_op);
        assert!(ns_per_op < 500, "NDP cache lookup too slow: {} ns/op", ns_per_op);
    }

    #[test]
    fn perf_ndp_build_ns() {
        let iterations = 100_000;
        let src = local_mac();
        let src_ip = local_ip();
        let target = peer_ip();

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            let _ = std::hint::black_box(build_neighbor_solicitation(
                std::hint::black_box(&src),
                std::hint::black_box(&src_ip),
                std::hint::black_box(&target),
            ));
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!("[PERF] NDP build NS: {} iterations in {:?} ({} ns/op)", iterations, elapsed, ns_per_op);
        assert!(ns_per_op < 2_000, "NDP build NS too slow: {} ns/op", ns_per_op);
    }

    #[test]
    fn perf_ndp_parse_ns() {
        let frame = build_neighbor_solicitation(&local_mac(), &local_ip(), &peer_ip());
        let iterations = 100_000;

        let start = std::time::Instant::now();
        for _ in 0..iterations {
            let _ = std::hint::black_box(parse_ndp_packet(std::hint::black_box(&frame)));
        }
        let elapsed = start.elapsed();
        let ns_per_op = elapsed.as_nanos() / iterations as u128;
        eprintln!("[PERF] NDP parse NS: {} iterations in {:?} ({} ns/op)", iterations, elapsed, ns_per_op);
        assert!(ns_per_op < 2_000, "NDP parse NS too slow: {} ns/op", ns_per_op);
    }
}
