# Subnet-Aware Routing

This document explains the routing system in dpdk-stdlib-rust, which determines
how the stack resolves Ethernet (L2) destinations for outbound UDP packets.

## Overview

When `send_to(dst_ip)` is called, the routing table determines the **ARP target**:
the IP address whose MAC address should be used as the Ethernet destination.

```
send_to(dst_ip) -> routing_table.lookup(dst_ip) -> NextHop::Direct(peer_ip)
                                                  -> NextHop::Gateway(gw_ip)
                 -> arp_handler.resolve(arp_target) -> MAC address
```

The IP header destination is always `dst_ip`. Only the Ethernet destination changes
based on routing — this is standard IP routing behavior.

## Lookup Order

1. **Broadcast** (255.255.255.255) -> Direct
2. **Link-local** (169.254.0.0/16) -> Direct
3. **Multicast** (224.0.0.0/4) -> Direct
4. **Local subnet** (same prefix as local IP) -> Direct
5. **Subnet-directed broadcast** (e.g. 10.0.1.255 for /24) -> Direct
6. **Static routes** (longest-prefix-match) -> Gateway
7. **Default gateway** -> Gateway
8. **No match** -> Direct (fallback, backward compatible)

## Configuration

### Auto-Detection (Default)

On Linux, `UdpSocket::bind()` automatically reads `/proc/net/route` and
`/proc/net/arp` to discover:

- The local subnet and prefix length
- The default gateway IP
- ARP entries for the gateway (seeded into the DPDK ARP cache)

This makes routing work out of the box on bare-metal and on-premises without
manual configuration. If auto-detection fails (e.g. non-Linux OS, no `/proc`,
or no matching route), the stack falls back to passthrough mode where all
destinations are treated as direct.

### Manual Configuration via Builder

```rust
use dpdk_udp::{UdpSocket, NetworkConfig};
use std::net::Ipv4Addr;

let socket = UdpSocket::builder()
    .network(
        NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
            .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
            .with_mtu(9001)  // jumbo frames
    )
    .bind("10.0.1.10:9000")?;
```

### Manual Configuration via Setter

```rust
let mut socket = UdpSocket::bind("10.0.1.10:9000")?;
socket.set_routing(
    NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
        .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
);
```

### Static Routes

For multi-homed networks or non-standard topologies:

```rust
let config = NetworkConfig::new(Ipv4Addr::new(10, 0, 1, 10), 24)
    .with_gateway(Ipv4Addr::new(10, 0, 1, 1))
    .with_route(
        Ipv4Addr::new(172, 16, 0, 0), 16,    // 172.16.0.0/16
        Ipv4Addr::new(10, 0, 1, 254),         // via this gateway
    );
```

Static routes are checked before the default gateway using longest-prefix-match.

## MTU

`NetworkConfig` includes an `mtu` field (default 1500). This affects:

- **`max_udp_payload()`**: Returns `MTU - 20 (IPv4) - 8 (UDP)`. Default: 1472.
  With jumbo frames (MTU 9001): 8973.
- **`send_to()` guard**: Payloads exceeding the MTU-derived limit are rejected
  with `io::ErrorKind::InvalidInput` instead of silently truncating.
- **TxBuffer sizing**: Always allocated for jumbo frames (9KB), so changing MTU
  via `set_routing()` never requires reallocation.

## When to Use Manual Config vs Auto-Detect

| Scenario | Recommendation |
|----------|---------------|
| AWS VPC | Auto-detect works. Manual config also works. |
| Bare-metal Linux | Auto-detect works if `/proc/net/route` is populated. |
| On-premises with standard routing | Auto-detect works. |
| Non-standard topology (e.g. policy routing) | Use manual `NetworkConfig`. |
| Non-Linux OS | Use manual `NetworkConfig` (no `/proc`). |
| Jumbo frames | Set `.with_mtu(9001)` explicitly. |

## Backward Compatibility

- With no routing configuration (auto-detect fails), behavior is identical to
  pre-routing code: ARP always targets the destination IP directly.
- AWS VPC deployments with ARP cache pre-population continue to work.
- No existing public API signatures were changed.
