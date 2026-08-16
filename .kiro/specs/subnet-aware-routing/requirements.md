# Requirements: Subnet-Aware Routing

## Functional

1. The stack MUST distinguish same-subnet destinations (ARP for peer MAC
   directly) from cross-subnet destinations (ARP for gateway MAC).
2. The stack MUST support a configurable default gateway IP.
3. The stack MUST support static routes with longest-prefix-match.
4. The stack MUST support configurable interface MTU.
5. Broadcast, link-local, and multicast traffic MUST always use direct ARP
   (never routed through a gateway).
6. The routing table MUST be configurable both at bind time (via builder) and
   after construction (via setter).

## Backward Compatibility

7. With no routing configuration, behavior MUST be identical to pre-routing
   code: ARP always targets the destination IP directly.
8. No existing public API signatures may change.
9. AWS VPC deployments with ARP cache pre-population MUST continue to work
   without any code changes.

## Performance

10. Routing lookup MUST be O(n) in static routes or better — no allocations
    on the send path.
11. The routing table MUST NOT require locks on the read path (`send_to()`).

## Testability

12. All routing logic MUST be unit-testable without DPDK, real networking, or
    root privileges.
13. Tests MUST cover: same-subnet, cross-subnet, broadcast, link-local,
    multicast, static routes, longest-prefix-match, no-config fallback,
    MTU calculations.

## Future (Phase 3)

14. The stack SHOULD auto-detect subnet and gateway from the OS when no
    explicit config is provided.
15. Auto-detection SHOULD parse `/proc/net/route` and `/proc/net/arp` on Linux.
16. Auto-detection failure MUST fall back to passthrough (not error).
