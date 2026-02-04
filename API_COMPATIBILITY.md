# API Compatibility Status

## Overview

This document tracks the API compatibility between our DPDK-based socket implementation and the standard Rust networking APIs (`std::net::UdpSocket` and `tokio::net::UdpSocket`).

## Compat Layer Status

The `dpdk-tokio` crate provides drop-in replacement sockets in `dpdk_tokio::compat`:

- `dpdk_tokio::compat::net::UdpSocket` - replaces `std::net::UdpSocket`
- `dpdk_tokio::compat::tokio::UdpSocket` - replaces `tokio::net::UdpSocket`

### std::net::UdpSocket Compatibility

| Function | Status | Notes |
|----------|--------|-------|
| `bind()` | ✅ | Correct signature with `ToSocketAddrs` |
| `recv_from()` | ✅ | Correct signature |
| `send_to()` | ✅ | Correct signature with `ToSocketAddrs` |
| `connect()` | ✅ | Implemented |
| `recv()` | ✅ | Implemented |
| `send()` | ✅ | Implemented |
| `local_addr()` | ✅ | Implemented |
| `peer_addr()` | ✅ | Implemented |
| `set_read_timeout()` | ✅ | Implemented |
| `read_timeout()` | ✅ | Implemented |
| `set_write_timeout()` | ✅ | Implemented |
| `write_timeout()` | ✅ | Implemented |
| `set_broadcast()` | ✅ | Implemented |
| `broadcast()` | ✅ | Implemented |
| `set_ttl()` | ✅ | Implemented |
| `ttl()` | ✅ | Implemented |
| `set_multicast_loop_v4()` | ✅ | Implemented |
| `multicast_loop_v4()` | ✅ | Implemented |
| `set_multicast_ttl_v4()` | ✅ | Implemented |
| `multicast_ttl_v4()` | ✅ | Implemented |
| `set_multicast_loop_v6()` | ✅ | Implemented |
| `multicast_loop_v6()` | ✅ | Implemented |
| `join_multicast_v4()` | ✅ | Implemented |
| `join_multicast_v6()` | ✅ | Implemented |
| `leave_multicast_v4()` | ✅ | Implemented |
| `leave_multicast_v6()` | ✅ | Implemented |
| `set_nonblocking()` | ✅ | Implemented |
| `take_error()` | ✅ | Implemented |
| `try_clone()` | ✅ | Implemented |
| `peek()` | ✅ | Implemented |
| `peek_from()` | ✅ | Implemented |

### tokio::net::UdpSocket Compatibility

| Function | Status | Notes |
|----------|--------|-------|
| `bind()` | ✅ | Async, correct signature |
| `recv_from()` | ✅ | Async |
| `send_to()` | ✅ | Async |
| `connect()` | ✅ | Async |
| `recv()` | ✅ | Async |
| `send()` | ✅ | Async |
| `local_addr()` | ✅ | Implemented |
| `peer_addr()` | ✅ | Implemented |
| `poll_recv_from()` | ✅ | Implemented |
| `poll_send_to()` | ✅ | Implemented |
| `poll_recv()` | ✅ | Implemented |
| `poll_send()` | ✅ | Implemented |
| `try_recv_from()` | ✅ | Implemented |
| `try_send_to()` | ✅ | Implemented |
| `try_recv()` | ✅ | Implemented |
| `try_send()` | ✅ | Implemented |
| `readable()` | ✅ | Async |
| `writable()` | ✅ | Async |
| `from_std()` | ✅ | Implemented |
| `into_std()` | ✅ | Implemented |
| All socket options | ✅ | Same as std::net |

### Not Implemented (OS-specific, N/A for DPDK)

- `AsRawFd` / `FromRawFd` / `IntoRawFd` (Unix)
- `AsRawSocket` / `FromRawSocket` / `IntoRawSocket` (Windows)

These traits don't apply to DPDK since it bypasses the kernel networking stack.

---

## DPDK Backend Implementation Status

The compat layer delegates to the underlying DPDK implementation. Here's the status of the actual DPDK packet I/O:

### Infrastructure (dpdk crate)

| Component | Status | Location |
|-----------|--------|----------|
| EAL initialization | ✅ | `dpdk/src/eal.rs` |
| Port configuration | ✅ | `dpdk/src/port.rs` |
| Port start/stop | ✅ | `dpdk/src/port.rs` |
| MAC address handling | ✅ | `dpdk/src/port.rs` |
| Link status | ✅ | `dpdk/src/port.rs` |
| Port statistics | ✅ | `dpdk/src/port.rs` |
| Mempool creation | ✅ | `dpdk/src/mbuf.rs` |
| Mbuf allocation | ✅ | `dpdk/src/mbuf.rs` |
| Bulk allocation | ✅ | `dpdk/src/mbuf.rs` |
| rx_burst | ✅ | `dpdk/src/port.rs` |
| tx_burst | ✅ | `dpdk/src/port.rs` |

### UDP Layer (dpdk-udp crate)

| Function | Status | Notes |
|----------|--------|-------|
| `bind()` | ✅ | Initializes DPDK EAL, port, and mempool |
| `send_to()` | ✅ | Builds Eth/IP/UDP packet, calls tx_burst |
| `send()` | ✅ | Uses connected address |
| `recv_from()` | ✅ | Calls rx_burst, parses Eth/IP/UDP, returns payload |
| `recv()` | ✅ | Delegates to recv_from |
| `local_addr()` | ✅ | Returns bound address |
| `peer_addr()` | ✅ | Returns connected address |
| `connect()` | ✅ | Sets connected address |
| `set_ttl()` / `ttl()` | ✅ | Configures IP TTL |

### Packet Processing

| Component | Status | Notes |
|-----------|--------|-------|
| Ethernet frame building | ✅ | `build_udp_packet()` in dpdk-udp |
| IPv4 header building | ✅ | `build_udp_packet()` in dpdk-udp |
| UDP header building | ✅ | `build_udp_packet()` in dpdk-udp |
| IP checksum calculation | ✅ | `ipv4_checksum()` in dpdk-udp |
| UDP checksum calculation | ✅ | `udp_checksum()` in dpdk-udp |
| Ethernet frame parsing | ✅ | `parse_udp_packet()` in dpdk-udp |
| IPv4 header parsing | ✅ | `parse_udp_packet()` in dpdk-udp |
| UDP header parsing | ✅ | `parse_udp_packet()` in dpdk-udp |
| ARP handling | ✅ | `arp` module in dpdk-udp |

### ARP Protocol Support (dpdk-udp/src/arp.rs)

| Component | Status | Notes |
|-----------|--------|-------|
| ARP packet parsing | ✅ | `parse_arp_packet()` |
| ARP packet building | ✅ | `build_arp_frame()`, `build_arp_request()`, `build_arp_reply()` |
| ARP cache | ✅ | `ArpCache` with TTL-based expiration |
| ARP handler | ✅ | `ArpHandler` - automatic response to requests |
| Opportunistic learning | ✅ | Learn from all ARP packets seen |
| Multiple IP support | ✅ | Can respond to multiple local IPs |

### ICMP Protocol Support (dpdk-udp/src/icmp.rs)

| Component | Status | Notes |
|-----------|--------|-------|
| ICMP packet parsing | ✅ | `parse_icmp_packet()` |
| ICMP packet building | ✅ | `build_icmp_frame()`, `build_echo_request()`, `build_echo_reply()` |
| Echo request/reply | ✅ | Full ping support |
| ICMP checksum | ✅ | `icmp_checksum()` |
| ICMP handler | ✅ | `IcmpHandler` - automatic echo reply |

### Connection Tracking

| Component | Status | Notes |
|-----------|--------|-------|
| Connection state | ✅ | `ConnectionState` struct |
| Packet counters | ✅ | packets_sent, packets_received |
| Byte counters | ✅ | bytes_sent, bytes_received |
| Receive queue | ✅ | Buffering for connected sockets |
| Stats access | ✅ | `connection_stats()` method |

### API Compatibility Tests

The `dpdk-udp` crate includes compile-time API compatibility tests that verify
our UdpSocket matches `std::net::UdpSocket` signatures:

- `test_api_bind_signature` - Verifies `bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket>`
- `test_api_send_to_signature` - Verifies `send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize>`
- `test_api_recv_from_signature` - Verifies `recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>`
- `test_api_local_addr_signature` - Verifies `local_addr(&self) -> io::Result<SocketAddr>`
- `test_api_peer_addr_signature` - Verifies `peer_addr(&self) -> io::Result<SocketAddr>`
- `test_api_connect_signature` - Verifies `connect<A: ToSocketAddrs>(&mut self, addr: A) -> io::Result<()>`
- `test_api_send_signature` - Verifies `send(&self, buf: &[u8]) -> io::Result<usize>`
- `test_api_recv_signature` - Verifies `recv(&self, buf: &mut [u8]) -> io::Result<usize>`
- `test_api_set_ttl_signature` - Verifies `set_ttl(&mut self, ttl: u32) -> io::Result<()>`
- `test_api_ttl_signature` - Verifies `ttl(&self) -> io::Result<u32>`

**Note:** `connect()` and `set_ttl()` take `&mut self` instead of `&self` for internal state management.

### ARP Tests (14 tests)

- `test_arp_constants` - Verifies ARP protocol constants
- `test_arp_request_creation` - Tests `ArpPacket::request()` constructor
- `test_arp_reply_creation` - Tests `ArpPacket::reply()` constructor
- `test_build_and_parse_arp_request` - Round-trip test for ARP requests
- `test_build_and_parse_arp_reply` - Round-trip test for ARP replies
- `test_parse_invalid_frame` - Tests rejection of invalid frames
- `test_arp_cache` - Tests `ArpCache` insert/lookup/remove
- `test_arp_cache_clear` - Tests `ArpCache::clear()`
- `test_arp_handler_request_response` - Tests automatic ARP reply generation
- `test_arp_handler_ignores_other_requests` - Tests filtering of non-local requests
- `test_arp_handler_learn_from_reply` - Tests opportunistic learning
- `test_arp_handler_make_request` - Tests ARP request generation
- `test_arp_handler_resolve` - Tests ARP resolution
- `test_arp_handler_multiple_ips` - Tests multi-IP support

### ICMP Tests (11 tests)

- `test_icmp_constants` - Verifies ICMP protocol constants
- `test_parse_echo_request` - Tests parsing of echo requests
- `test_parse_echo_reply` - Tests parsing of echo replies
- `test_parse_invalid_frame` - Tests rejection of invalid frames
- `test_make_echo_reply` - Tests echo reply generation
- `test_make_echo_reply_not_request` - Verifies only requests generate replies
- `test_build_and_parse_roundtrip` - Round-trip test for ICMP packets
- `test_icmp_checksum` - Tests ICMP checksum calculation
- `test_icmp_handler_echo_reply` - Tests automatic ping response
- `test_icmp_handler_ignores_other_ips` - Tests filtering of non-local pings
- `test_icmp_handler_multiple_ips` - Tests multi-IP support

---

## Implementation Roadmap

### Phase 1: Infrastructure ✅
- [x] Port initialization with configuration
- [x] Mempool creation with configuration
- [x] rx_burst / tx_burst wrappers
- [x] Comprehensive unit tests (48 tests in dpdk crate)

### Phase 2: Packet I/O ✅
- [x] `send_to()` - build Ethernet/IP/UDP packet, call tx_burst ✅
- [x] IP checksum calculation ✅
- [x] UDP checksum calculation ✅
- [x] API compatibility tests (10 tests) ✅
- [x] `recv_from()` - call rx_burst, parse Ethernet/IP/UDP packet ✅
- [x] Packet parsing tests (7 tests) ✅

### Phase 3: Protocol Support ✅
- [x] ARP request/response handling ✅
- [x] ICMP echo reply (ping) ✅
- [x] Connection tracking for connected sockets ✅

### Phase 4: Advanced Features
- [ ] Multicast group management via DPDK
- [ ] Promiscuous mode integration
- [ ] Hardware offload configuration
