# Design: Subnet-Aware Routing

## Problem

The stack currently has no routing logic. `send_to()` always ARPs for the
destination IP's MAC directly. In AWS VPC this works because the gateway MAC is
pre-populated into the ARP cache at startup (a deliberate "lie" — see
`docs/aws-vpc-networking.md`). On bare-metal, on-prem, or any non-VPC
environment, this fails: cross-subnet traffic needs to ARP for the gateway MAC,
not the peer's MAC.

## Solution

A `RoutingTable` sits between `send_to()` and the ARP handler. It transforms
the destination IP into an "ARP target" IP:

```
send_to(dst_ip) → routing_table.lookup(dst_ip) → NextHop::Direct(peer_ip)
                                                → NextHop::Gateway(gw_ip)
                  → arp_handler.resolve(arp_target_ip) → MAC address
```

The IP header destination is always `dst_ip`. Only the L2 (Ethernet) destination
changes based on routing — this is standard IP routing behavior.

## Lookup Order

1. Broadcast (255.255.255.255) → Direct
2. Link-local (169.254.0.0/16) → Direct
3. Multicast (224.0.0.0/4) → Direct
4. Local subnet (same prefix as local IP) → Direct
5. Subnet-directed broadcast (e.g. 10.0.1.255 for /24) → Direct
6. Static routes (longest-prefix-match) → Gateway
7. Default gateway → Gateway
8. No match → Direct (fallback, backward compatible)

## Default Behavior

**Current (Phase 1):** No config = passthrough. All destinations resolve to
`NextHop::Direct(dst_ip)`. This preserves backward compatibility with existing
AWS VPC deployments.

**Target (Phase 3):** Auto-detect from OS. Parse `/proc/net/route` and
`/proc/net/arp` to discover subnet, gateway, and seed the ARP cache. This makes
routing work out of the box on bare-metal without manual config. Falls back to
passthrough if OS detection fails.

The Phase 3 default change is a separate PR because it:
- Changes observable behavior (ARP targets change for cross-subnet traffic)
- Requires careful testing on AWS to ensure the auto-detected gateway matches
  the pre-populated ARP cache entries
- Needs a feature flag or env var escape hatch during rollout

## MTU

`NetworkConfig` includes an `mtu` field (default 1500). This affects
`max_udp_payload()` (MTU - 20 IPv4 - 8 UDP). Phase 2 wires this into the
`TxBuffer` sizing and adds a guard in `send_to()` to reject oversized payloads.

## Thread Safety

`RoutingTable` is immutable after construction. `set_routing(&mut self)` takes
`&mut self`, so no concurrent mutation is possible. The `lookup()` call in
`send_to()` takes `&self` — no synchronization needed.
