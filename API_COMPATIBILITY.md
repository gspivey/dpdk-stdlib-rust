# std::net::UdpSocket API Compatibility Checklist

Based on: https://doc.rust-lang.org/std/net/struct.UdpSocket.html

## ✅ Currently Implemented
- [ ] `bind(addr)` - Partially (our version has different signature)
- [ ] `recv_from(&mut buf)` - Partially (our version has different signature)  
- [ ] `send_to(buf, addr)` - Partially (our version has different signature)

## ❌ Missing Core Functions
- [ ] `connect(addr)` - Connect to remote address
- [ ] `recv(&mut buf)` - Receive from connected peer
- [ ] `send(buf)` - Send to connected peer
- [ ] `local_addr()` - Get local socket address
- [ ] `peer_addr()` - Get connected peer address (if connected)

## ❌ Missing Configuration
- [ ] `set_read_timeout(dur)` - Set read timeout
- [ ] `set_write_timeout(dur)` - Set write timeout  
- [ ] `read_timeout()` - Get read timeout
- [ ] `write_timeout()` - Get write timeout
- [ ] `set_broadcast(on)` - Enable/disable broadcast
- [ ] `broadcast()` - Get broadcast setting
- [ ] `set_multicast_loop_v4(on)` - IPv4 multicast loopback
- [ ] `multicast_loop_v4()` - Get IPv4 multicast loopback
- [ ] `set_multicast_ttl_v4(ttl)` - IPv4 multicast TTL
- [ ] `multicast_ttl_v4()` - Get IPv4 multicast TTL
- [ ] `set_multicast_loop_v6(on)` - IPv6 multicast loopback
- [ ] `multicast_loop_v6()` - Get IPv6 multicast loopback
- [ ] `set_ttl(ttl)` - Set TTL
- [ ] `ttl()` - Get TTL
- [ ] `join_multicast_v4(multiaddr, interface)` - Join IPv4 multicast
- [ ] `join_multicast_v6(multiaddr, interface)` - Join IPv6 multicast
- [ ] `leave_multicast_v4(multiaddr, interface)` - Leave IPv4 multicast
- [ ] `leave_multicast_v6(multiaddr, interface)` - Leave IPv6 multicast

## ❌ Missing Advanced Features
- [ ] `set_nonblocking(nonblocking)` - Set non-blocking mode
- [ ] `take_error()` - Get and clear pending error

## ❌ Missing Trait Implementations
- [ ] `AsRawFd` (Unix) - Get raw file descriptor
- [ ] `AsRawSocket` (Windows) - Get raw socket handle
- [ ] `FromRawFd` (Unix) - Create from raw file descriptor
- [ ] `FromRawSocket` (Windows) - Create from raw socket handle
- [ ] `IntoRawFd` (Unix) - Convert to raw file descriptor
- [ ] `IntoRawSocket` (Windows) - Convert to raw socket handle

## 🎯 Priority Order
1. **Core API compatibility** - Fix `bind()`, `recv_from()`, `send_to()` signatures
2. **Connection support** - `connect()`, `recv()`, `send()`
3. **Address queries** - `local_addr()`, `peer_addr()`
4. **Timeouts** - `set_read_timeout()`, `set_write_timeout()`
5. **Broadcast** - `set_broadcast()`, `broadcast()`
6. **Advanced features** - Non-blocking, multicast, etc.

## Current Signature Mismatches

### std::net::UdpSocket
```rust
pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket>
pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>
pub fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize>
```

### Our UdpSocket (WRONG)
```rust
pub fn bind(ip: [u8; 4], port: u16) -> UdpResult<Self>
pub fn recv_from(&self, buf: &mut [u8]) -> UdpResult<(usize, std::net::SocketAddr)>
pub fn send_to(&self, buf: &[u8], addr: std::net::SocketAddr) -> UdpResult<usize>
```

**NEEDS FIXING**: Our API should match std exactly!
