# Ephemeral Port Allocation in DPDK Sockets

## Background

An ephemeral port is a short-lived transport protocol port allocated automatically
by the IP stack when an application binds to port 0. The OS assigns a port from a
predefined range for the duration of the communication session.

### Port Ranges by OS

| Range | Operating System |
|-------|-----------------|
| 49152-65535 | IANA suggested (RFC 6335), FreeBSD 4.6+, Windows Vista+ |
| 32768-60999 | Linux kernels (default) |
| 32768-65535 | Solaris, AIX |
| 1024-65535 | RFC 6056 |

Reference: [RFC 6335](https://www.ietf.org/rfc/rfc6335.txt), [Wikipedia: Ephemeral port](https://en.wikipedia.org/wiki/Ephemeral_port)

## Problem

DPDK operates in userspace and bypasses the kernel network stack entirely. When a
DPDK socket binds to port 0, the kernel's ephemeral port allocator is not involved.
Without explicit handling, the socket would literally use port 0 in the UDP header.

This causes two problems:

1. **Responses fail**: When a server (e.g., Python UDP echo server) receives a
   packet from source port 0 and tries to `sendto(data, (client_ip, 0))`, the
   response may be dropped or misrouted since port 0 is reserved/invalid on most
   systems.

2. **Port filtering breaks**: The DPDK `recv_from` filters incoming packets by
   `dst_port == local_port`. If local_port is 0, only packets explicitly addressed
   to port 0 would match — but most servers won't send to port 0.

## Solution

`dpdk-udp` implements its own ephemeral port allocator matching the Linux kernel
range (32768-60999):

```rust
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(32768);

fn allocate_ephemeral_port() -> u16 {
    // Atomically increment, wrap at range boundary
    // Returns ports in 32768-60999 range
}
```

When `UdpSocket::bind("0.0.0.0:0")` or `bind_with_backend("ip:0", backend)` is
called, port 0 is replaced with the next ephemeral port. This happens in both
`bind()` and `bind_with_backend()`.

## Async Recv Deadlock Fix

A related issue: the async `recv_from` wrapper in `dpdk-tokio` uses
`tokio::task::spawn_blocking` to call the synchronous DPDK `recv_from`, which is
an infinite spin loop. If `tokio::time::timeout` fires while the blocking thread
is still spinning, the thread continues running **holding the socket's Mutex lock**.
Any subsequent `send_to` or `recv_from` call deadlocks waiting for the lock.

The fix: the async wrapper sets a 1-second `read_timeout` on the DPDK socket before
each blocking recv call, then loops in the async context. This ensures:
- The blocking thread always returns within 1 second (releasing the lock)
- `tokio::time::timeout` and cancellation work correctly between iterations
- No deadlock since the lock is released between async loop iterations

```rust
// Async wrapper pattern:
loop {
    spawn_blocking(|| {
        socket.set_read_timeout(Some(1s));
        let res = socket.recv_from(&mut buf);
        socket.set_read_timeout(None);
        res
    }).await;
    match result {
        Ok(data) => return Ok(data),
        Err(WouldBlock) => { yield_now().await; continue; }
        Err(e) => return Err(e),
    }
}
```
