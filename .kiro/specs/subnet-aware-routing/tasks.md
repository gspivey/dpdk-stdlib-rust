# Tasks: Subnet-Aware Routing

## Phase 1: Core Routing Table (Done)

- [x] **1.1**: Implement `RoutingTable`, `NetworkConfig`, `RouteEntry`, `NextHop` in `dpdk-udp/src/routing.rs` — subnet mask awareness, longest-prefix-match static routes, default gateway, configurable MTU
- [x] **1.2**: Integrate routing into `UdpSocket::send_to()` — consult routing table for ARP target (same-subnet → peer IP, cross-subnet → gateway IP)
- [x] **1.3**: Add `UdpSocketBuilder::network()` for declarative routing config at bind time
- [x] **1.4**: Add `UdpSocket::set_routing()` / `routing_table()` for runtime config
- [x] **1.5**: Unit tests — 26 tests covering subnet math, route matching, longest-prefix-match, broadcast/link-local/multicast edge cases, real-world scenarios (bare-metal, AWS VPC, home network)

## Phase 2: TxBuffer Jumbo Frame Support

- [ ] **2.1**: Resize `TxBuffer` when `set_routing()` is called with a larger MTU — currently allocated at bind time for 1500-byte MTU, so jumbo frames (MTU 9001) would exceed the buffer
- [ ] **2.2**: Alternatively, always allocate `TxBuffer` for jumbo (9KB is negligible) — simpler, avoids reallocation edge cases
- [ ] **2.3**: Add `max_udp_payload()` method to `UdpSocket` that delegates to `routing_table.max_udp_payload()` — lets callers know the effective payload limit
- [ ] **2.4**: Guard `send_to()` against payloads exceeding the MTU-derived limit — return `io::ErrorKind::InvalidInput` instead of silently truncating or overflowing

## Phase 3: Auto-Detect Routing from OS

- [ ] **3.1**: Parse `/proc/net/route` on Linux to discover local subnet, prefix length, and default gateway for the bound interface
- [ ] **3.2**: Parse `/proc/net/arp` to seed the ARP cache with known entries (currently done in `seed_arp_cache_from_kernel`, extend to extract gateway MAC)
- [ ] **3.3**: Make auto-detection the default behavior — `RoutingTable::new()` with no config should try OS detection, falling back to passthrough if parsing fails
- [ ] **3.4**: This makes routing "just work" on bare-metal and on-prem without manual `NetworkConfig` — the user only needs explicit config for non-standard topologies
- [ ] **3.5**: Unit tests with mock `/proc` data (read from test fixtures instead of real `/proc`)

## Phase 4: Documentation

- [ ] **4.1**: Update `docs/aws-vpc-networking.md` — document `NetworkConfig` with gateway as the preferred alternative to ARP cache pre-population hack
- [ ] **4.2**: Add `docs/routing.md` — explain subnet-aware routing, when to use manual config vs auto-detect, MTU considerations, static routes
- [ ] **4.3**: Update `AGENTS.md` domain knowledge table to reference routing docs
- [ ] **4.4**: Update `README.md` roadmap to mark subnet-aware routing as in-progress/done
