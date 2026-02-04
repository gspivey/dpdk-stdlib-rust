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
| `recv_from()` | ❌ | Returns `todo!()` - needs rx_burst + packet parsing |
| `recv()` | ❌ | Returns `todo!()` - depends on recv_from |
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
| Ethernet frame parsing | ⚠️ | `SyntheticUdpSocket` has parsing |
| IPv4 header parsing | ⚠️ | `SyntheticUdpSocket` has parsing |
| UDP header parsing | ⚠️ | `SyntheticUdpSocket` has parsing |
| ARP handling | ❌ | Not implemented (needed for real networks) |

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

---

## Implementation Roadmap

### Phase 1: Infrastructure ✅
- [x] Port initialization with configuration
- [x] Mempool creation with configuration
- [x] rx_burst / tx_burst wrappers
- [x] Comprehensive unit tests (48 tests in dpdk crate)

### Phase 2: Packet I/O
- [x] `send_to()` - build Ethernet/IP/UDP packet, call tx_burst ✅
- [x] IP checksum calculation ✅
- [x] UDP checksum calculation ✅
- [x] API compatibility tests (10 tests) ✅
- [ ] `recv_from()` - call rx_burst, parse Ethernet/IP/UDP packet

### Phase 3: Protocol Support
- [ ] ARP request/response handling
- [ ] ICMP echo reply (optional, for ping)
- [ ] Connection tracking for connected sockets

### Phase 4: Advanced Features
- [ ] Multicast group management via DPDK
- [ ] Promiscuous mode integration
- [ ] Hardware offload configuration
