# Implementation Plan: dpdk-stdlib-quic (s2n-quic Native DPDK Provider)

## Overview

This plan implements the `dpdk-stdlib-quic` crate — a native DPDK I/O provider for s2n-quic (pinned to v1.81.0). The crate lives at `dpdk-stdlib-quic/` in the workspace root and depends on `dpdk-udp` and `dpdk` (never `dpdk-tokio`). Implementation proceeds bottom-up: foundational types first, then the event loop, then integration testing and benchmarking.

All tasks must pass `cargo build && cargo test` from the workspace root without DPDK installed (stub system).

## Tasks

- [ ] 1. Create crate skeleton and workspace integration
  - [ ] 1.1 Create `dpdk-stdlib-quic/Cargo.toml` with dependencies on `s2n-quic = "=1.81.0"`, `s2n-quic-core = "=0.81.0"`, `dpdk-udp`, `dpdk`, `thiserror`, `futures`; dev-dependencies on `rcgen` and `tokio = { version = "1", features = ["full"] }`
    - Pin `s2n-quic` and `s2n-quic-core` to exact versions
    - Add `dpdk-udp` and `dpdk` as path dependencies
    - Do NOT depend on `dpdk-tokio`
    - _Requirements: 1.1, 1.2, 1.7_
  - [ ] 1.2 Add `"dpdk-stdlib-quic"` to workspace `members` in root `Cargo.toml`
    - _Requirements: 1.3_
  - [ ] 1.3 Create `dpdk-stdlib-quic/src/lib.rs` with module declarations and public re-exports
    - Declare modules: `provider`, `event_loop`, `path_handle`, `rx`, `tx`, `clock`, `ecn`, `frame`, `stats`, `error`, `loopback`
    - Re-export `DpdkProvider`, `ProviderBuilder`, `ProviderHandle`, `DpdkQuicError`
    - _Requirements: 1.1, 1.4_
  - [ ] 1.4 Verify `cargo build` succeeds from workspace root with stub placeholders in each module
    - _Requirements: 1.4, 14.1, 14.2_
  - [ ] 1.5 Create walking-skeleton CI binary and workflow
    - Create `dpdk-stdlib-quic/src/bin/quic-smoke.rs`: build the provider in stub mode, call `start()` with a minimal endpoint config, print `QUIC_SMOKE_OK`, exit 0
    - Create `.github/workflows/quic-integration-tests.yml` (cloned from `integration-tests.yml`): single instance, no ENI bind, no gateway MAC, `continue-on-error: true`
    - Grow this workflow at later checkpoints instead of building one big CI task at the end
    - _Requirements: 1.4, 1.5, 10.2_

- [ ] 2. Implement foundational types (error, clock, path handle, ECN)
  - [ ] 2.1 Implement `DpdkQuicError` in `dpdk-stdlib-quic/src/error.rs`
    - Define enum variants: `DpdkInit`, `BackendInit`, `UnsupportedAddressFamily`, `BindFailed`, `EventLoopCrash`
    - Derive `thiserror::Error`, ensure `'static + Display + Send + Sync`
    - _Requirements: 2.4, 8.1, 13.3_
  - [ ] 2.2 Implement `StdClock` in `dpdk-stdlib-quic/src/clock.rs`
    - Wrap `std::time::Instant` as epoch
    - Implement `s2n_quic_core::time::Clock` trait (path: `s2n_quic_core::time::Clock`)
    - Convert elapsed `Duration` to s2n-quic `Timestamp`
    - _Requirements: 3.1_
  - [ ] 2.3 Implement `DpdkPathHandle` in `dpdk-stdlib-quic/src/path_handle.rs`
    - Store `remote: RemoteAddress` and `local: LocalAddress`
    - Implement `s2n_quic_core::path::Handle` trait (supertrait bounds: `'static + Copy + Send + Debug`)
    - Implement required methods: `from_remote_address`, `remote_address`, `set_remote_address`, `local_address`, `set_local_address`, `eq`, `strict_eq`, `maybe_update`
    - Accessors return `RemoteAddress`/`LocalAddress` by value (not references)
    - Use `s2n_quic_core::inet::SocketAddress` (`IpV4`/`IpV6` variants — capital V)
    - Reject `IpV6` → return `UnsupportedAddressFamily` error at construction boundaries
    - _Requirements: 2.2, 5.1, 5.2, 13.1_
  - [ ] 2.4 Implement ECN helpers in `dpdk-stdlib-quic/src/ecn.rs`
    - `extract_ecn(tos_byte: u8) -> ExplicitCongestionNotification` — direct cast: `unsafe { std::mem::transmute(tos_byte & 0x03) }` (s2n-quic's enum is `#[repr(u8)]` with wire-bit values: NotEct=0b00, Ect1=0b01, Ect0=0b10, Ce=0b11)
    - `ecn_to_tos_bits(ecn: ExplicitCongestionNotification) -> u8` — simply `ecn as u8`
    - _Requirements: 6.1, 6.2_
  - [ ]* 2.5 Write unit tests for clock, path handle, and ECN helpers
    - Test clock monotonicity
    - Test path handle: `from_remote_address` → `remote_address()` round-trip
    - Test ECN extraction for all 4 codepoints (NotEct=0b00, Ect1=0b01, Ect0=0b10, Ce=0b11)
    - Test ECN round-trip: `extract_ecn(ecn_to_tos_bits(ecn)) == ecn` for all variants
    - Test IPv6 address rejection returns `UnsupportedAddressFamily` error
    - _Requirements: 2.4, 3.1, 6.1, 13.3_

- [ ] 3. Checkpoint — Foundational types compile and test
  - Ensure `cargo build && cargo test -p dpdk-stdlib-quic` passes with all foundational types. Ask the user if questions arise.

- [ ] 4. Implement frame building with TOS/ECN support
  - [ ] 4.1 Add `build_udp_frame_into_with_tos` to `dpdk-udp/src/lib.rs`
    - Public function alongside existing `build_udp_frame_into`
    - Accepts additional `tos: u8` parameter
    - Sets `frame[ip + 1] = tos` instead of `0x00`
    - Recomputes IPv4 header checksum in software after setting TOS
    - This is a non-breaking additive change to `dpdk-udp`
    - _Requirements: 2.5, 6.2, 14.2_
  - [ ] 4.2 Create `dpdk-stdlib-quic/src/frame.rs` as re-export/wrapper
    - Re-export `dpdk_udp::build_udp_frame_into_with_tos` for internal use
    - Provide convenience wrapper if needed for the `(src_mac, gateway_mac, local_addr, remote_addr, payload, tos)` pattern
    - _Requirements: 2.5, 6.2_
  - [ ]* 4.3 Write unit tests for frame building with TOS
    - Verify TOS byte is correctly placed in output frame at offset `ETH_HEADER_LEN + 1`
    - Verify IPv4 checksum is valid after TOS modification
    - Verify ECN bits survive round-trip (set TOS → parse TOS → extract ECN matches)
    - _Requirements: 6.2_

- [ ] 5. Implement RX queue
  - [ ] 5.1 Implement `DpdkRxQueue` in `dpdk-stdlib-quic/src/rx.rs`
    - Define `RxDatagram { header: Header<DpdkPathHandle>, payload: Vec<u8> }`
    - Implement the s2n-quic `rx::Queue` trait (`for_each`, `is_empty`)
    - Implement `DpdkRxQueue::new()` and `DpdkRxQueue::push(datagram)` as inherent methods
    - Implement `parse_to_rx_datagram(frame: &[u8], local_addr: SocketAddr) -> Option<RxDatagram>`:
      - Reuse `parse_udp_packet_ref` from `dpdk-udp` for parsing
      - Extract TOS byte for ECN (direct cast to enum)
      - Construct `Header { path: DpdkPathHandle { remote, local }, ecn }` — Header fields are `{ path, ecn }`
      - Return payload as owned Vec
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 6.1_
  - [ ]* 5.2 Write unit tests for RX queue
    - Test parsing a valid UDP frame produces correct datagram with correct addresses and ECN
    - Test frames with wrong dst_port are discarded
    - Test non-IPv4 frames are discarded
    - Test truncated/malformed frames are discarded
    - Test `for_each` drains all queued datagrams
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 6.1_

- [ ] 6. Implement TX queue
  - [ ] 6.1 Implement `DpdkTxQueue` in `dpdk-stdlib-quic/src/tx.rs`
    - Define `TxDatagram { frame: Vec<u8> }`
    - Implement the s2n-quic `tx::Queue` trait (`push`, `capacity`, `flush`)
    - Set `SUPPORTS_ECN = true`, `SUPPORTS_PACING = false`, `SUPPORTS_FLOW_LABELS = false`
    - In `push()`: extract remote address, ECN marking via `ecn as u8` for TOS
    - Call `message.write_payload(PayloadBuffer::new(&mut buf), gso_offset)` — note: second arg is gso_offset (not segment_len), advance it for each segment
    - Query `message.can_gso(segment_len, segment_count)` on the message (not on the queue) to determine segmentation
    - Build one frame per segment via `build_udp_frame_into_with_tos`
    - Return `Result<Outcome { len, index }, Error { EmptyPayload | UndersizedBuffer | AtCapacity }>`
    - Store src_mac, gateway_mac, local_addr for frame construction; reusable frame buffer
    - Implement `drain()` method to yield pending frames for transmission
    - _Requirements: 2.5, 6.2, 7.1, 7.2_
  - [ ]* 6.2 Write unit tests for TX queue
    - Test push with single segment produces one frame
    - Test GSO segmentation: push with can_gso producing multiple frames
    - Test capacity decreases after push
    - Test drain empties the queue
    - Test ECN marking is set correctly in outgoing frames (via TOS byte)
    - _Requirements: 6.2, 7.1, 7.2_

- [ ] 7. Checkpoint — RX/TX queues compile and test
  - Ensure `cargo build && cargo test -p dpdk-stdlib-quic` passes. Ask the user if questions arise.

- [ ] 8. Implement stats, gateway-MAC acquisition, and loopback backend
  - [x] 8.1 Implement `ProviderStats` and `ProviderHandle` in `dpdk-stdlib-quic/src/stats.rs`
    - Atomic counters: `rx_burst_calls`, `tx_burst_calls`, `datagrams_received`, `datagrams_transmitted`, `rx_drops`, `tx_drops`, `timer_wakeups`
    - `StatsSnapshot` struct with plain u64 fields
    - `snapshot()` method loads all atomics with `Ordering::Relaxed`
    - `ProviderHandle` struct holding `Arc<ProviderStats>`, `Arc<AtomicBool>` (shutdown), `Option<JoinHandle<()>>`
    - `ProviderHandle::shutdown()` sets flag, joins thread
    - Both Arcs created in `build()`, not `start()`
    - _Requirements: 9.1, 9.2, 8.4_
  - [x] 8.2 Implement gateway-MAC acquisition in `dpdk-stdlib-quic/src/provider.rs`
    - Builder method: `with_gateway_mac(mac: [u8; 6])` for explicit configuration
    - Fallback: read kernel ARP cache via `seed_arp_cache_from_kernel` pattern (same as dpdk-udp) and look up default gateway IP
    - Store resolved MAC in provider config for use by TxQueue
    - _Requirements: 2.5_
  - [x] 8.3 Implement `LoopbackBackend` in `dpdk-stdlib-quic/src/loopback.rs`
    - Implement ALL 8 `PacketBackend` methods:
      - `send_frame()` enqueues frame into `Mutex<VecDeque<Vec<u8>>>`
      - `recv_frames()` drains all enqueued frames (up to max_frames)
      - `mac_address()` returns a fixed test MAC
      - `backend_name()` returns `"loopback"`
      - `set_promiscuous()` / `is_promiscuous()` — store in `AtomicBool`
      - `set_allmulticast()` / `is_allmulticast()` — store in `AtomicBool`
    - This enables full QUIC handshake testing without DPDK
    - _Requirements: 10.3_
  - [x]* 8.4 Write unit tests for LoopbackBackend
    - Test send then recv returns same frame
    - Test recv on empty returns empty vec
    - Test multiple sends are returned in order
    - Test all 8 PacketBackend methods work correctly
    - _Requirements: 10.3_

- [ ] 9. Implement the event loop
  - [ ] 9.1 Implement `event_loop` function in `dpdk-stdlib-quic/src/event_loop.rs`
    - Accept endpoint (generic over s2n-quic Endpoint trait), backend, config, shutdown flag, stats, IcmpHandler
    - Loop structure: check shutdown → poll_wakeups (break on CloseError) → RX (recv_frames → ICMP dispatch → parse → queue → endpoint.receive) → TX (endpoint.transmit → drain → send_frame) → timer/sleep logic
    - ICMP dispatch: check `frame[ETH_HEADER_LEN + 9] == IP_PROTO_ICMP`, call `icmp_handler.process_icmp_full()`, send replies via backend
    - Handle `recv_frames` Err arm: increment rx_drops, continue
    - Use noop_waker (accepted v1 tradeoff: no thread unpark during cooldown)
    - Implement busy-poll-with-cooldown: track idle cycles, sleep until next timer deadline after budget exceeded (max 1ms)
    - Increment stats counters at each stage
    - _Requirements: 2.3, 2.6, 3.2, 3.3, 3.4, 5.3, 7.3, 8.2, 8.3, 8.4_
  - [ ] 9.2 Implement timer/sleep logic
    - Read `endpoint.timeout()` each iteration
    - When idle cycles exceed budget: sleep until min(next_timeout, 1ms)
    - When no timeout: sleep 100µs
    - Do NOT read `message.delay()` — SUPPORTS_PACING = false means s2n-quic handles pacing internally
    - _Requirements: 3.2, 3.3, 3.4, 3.5_

- [ ] 10. Implement the provider and builder
  - [ ] 10.1 Implement `ProviderBuilder` in `dpdk-stdlib-quic/src/provider.rs`
    - Builder fields: `bind_addr`, `eal_args`, `backend_config`, `gateway_mac: Option<[u8; 6]>`, `max_rx_burst` (default 32), `max_tx_burst` (default 32), `busy_poll_budget`
    - `with_addr()`, `with_eal_args()`, `with_rx_burst()`, `with_tx_burst()`, `with_backend_config()`, `with_gateway_mac()` methods
    - `build()` creates both `Arc<ProviderStats>` and `Arc<AtomicBool>` (shutdown), returns `(DpdkProvider, ProviderHandle)`
    - Default bind: `0.0.0.0:0` for clients
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_
  - [ ] 10.2 Implement `DpdkProvider` and `io::Provider` trait in `dpdk-stdlib-quic/src/provider.rs`
    - `start(self, endpoint)` implementation:
      1. Initialize DPDK backend (EAL, port, mempool) via `dpdk-udp` backend factory
      2. Resolve gateway MAC: use builder's explicit MAC if set, else kernel ARP cache fallback
      3. Bind to configured address (handle ephemeral port 0)
      4. Move stats + shutdown clones into spawned thread
      5. Spawn event loop thread (std::thread — Mutex<Port> provides thread safety)
      6. Return bound `SocketAddress`
    - Support both Server and Client endpoints
    - Return `DpdkQuicError` on failure
    - _Requirements: 2.1, 2.5, 2.6, 2.7, 2.8, 4.4, 4.5, 8.1_
  - [ ] 10.3 Implement shutdown mechanism
    - `ProviderHandle::shutdown()` sets `AtomicBool` flag (authoritative shutdown path)
    - `poll_wakeups` returning `CloseError` is the secondary signal (when all app handles drop)
    - Event loop checks both: flag each iteration, CloseError from poll_wakeups
    - Thread join with timeout ensures cleanup within 100ms
    - Resources dropped when `Arc<dyn PacketBackend>` refcount reaches zero
    - _Requirements: 8.3, 8.4_

- [ ] 11. Checkpoint — Provider compiles with full event loop
  - Ensure `cargo build && cargo test -p dpdk-stdlib-quic` passes. The provider should initialize in stub mode without panic. Ask the user if questions arise.
  - Update `quic-smoke.rs` to exercise the real provider build path in stub mode.

- [ ] 12. Implement loopback integration test (QUIC handshake)
  - [ ] 12.1 Create `dpdk-stdlib-quic/tests/loopback_handshake.rs`
    - Use `LoopbackBackend` shared between a server and client provider
    - Configure TLS with self-signed cert via `rcgen`
    - Requires `tokio` dev-dep for the app-side executor (server.accept().await etc.)
    - Exercise full QUIC handshake: client connects, opens stream, sends data, server echoes
    - Verify data integrity
    - This validates the entire provider works end-to-end without DPDK
    - _Requirements: 10.3, 10.4, 2.7_
  - [ ] 12.2 Create `dpdk-stdlib-quic/tests/provider_init.rs`
    - Test provider construction with default config succeeds
    - Test provider start in stub mode initializes without error
    - Test IPv6 address (SocketAddress::IpV6) rejection returns `UnsupportedAddressFamily`
    - _Requirements: 1.5, 10.1, 10.2, 13.3_
  - [ ]* 12.3 Create `dpdk-stdlib-quic/tests/ecn_roundtrip.rs`
    - Build a frame with ECN marking via TX path (using build_udp_frame_into_with_tos)
    - Parse it back via RX path (extract_ecn from TOS byte)
    - Verify ECN codepoint is preserved for all 4 codepoints
    - _Requirements: 6.1, 6.2_
  - [ ]* 12.4 Create `dpdk-stdlib-quic/tests/gso_segmentation.rs`
    - Push a message with payload > MSS and can_gso returning true
    - Verify TX queue produces correct number of frames
    - Verify each frame payload is at most segment_len bytes
    - Verify all payload bytes are accounted for across segments
    - _Requirements: 7.1, 7.2_

- [ ] 13. Checkpoint — Integration tests pass
  - Ensure `cargo build && cargo test` from workspace root passes (all 133+ existing tests plus new dpdk-stdlib-quic tests). Ask the user if questions arise.
  - Update quic-integration-tests.yml to run `cargo test -p dpdk-stdlib-quic` in CI
  - _Requirements: 14.1, 14.2_

- [ ] 14. Implement benchmark binary
  - [ ] 14.1 Create `dpdk-stdlib-quic/src/bin/bench.rs`
    - CLI arguments: `--provider=stock|native-dpdk`, `--duration=<secs>`, `--streams=<n>`, `--payload-size=<bytes>`
    - Configure TLS with self-signed cert (rcgen)
    - Both providers run same workload: client opens N streams, sends payload_size bytes, server echoes
    - Collect metrics: total bytes, elapsed time, throughput (Gbps), packets/sec, handshake latency
    - Report provider stats counters alongside throughput
    - Requires tokio dev-dep for the app-side executor
    - _Requirements: 11.1, 11.2, 11.3, 11.4, 11.5, 9.3_
  - [ ] 14.2 Verify benchmark binary compiles (it won't fully run without DPDK but should compile in stub mode)
    - _Requirements: 1.4, 11.1_

- [ ] 15. Add CI workflow for QUIC integration and performance tests
  - [ ] 15.1 Extend `.github/workflows/quic-integration-tests.yml` (created in 1.5)
    - Add two EC2 instances (server + client) with DPDK
    - Run full handshake + bidirectional throughput test
    - Produce JUnit XML artifacts and PR-comment summary
    - Keep `continue-on-error: true` while provider is being stabilized
    - _Requirements: 12.1, 12.2, 12.3_
  - [ ] 15.2 Add performance benchmark job to CI workflow
    - Run bench binary with `--provider=stock` and `--provider=native-dpdk`
    - Report comparative results
    - _Requirements: 12.4_

- [ ] 16. Final checkpoint — Full workspace validation
  - Run `cargo build && cargo test` from workspace root
  - Verify all existing tests still pass (zero regression)
  - Verify `cargo test -p dpdk-stdlib-quic` passes all new tests
  - Ensure no modifications to existing crates' public APIs (only additive change: `build_udp_frame_into_with_tos` in dpdk-udp)
  - Ask the user if questions arise.
  - _Requirements: 14.1, 14.2_

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "1.2"] },
    { "id": 1, "tasks": ["1.3"] },
    { "id": 2, "tasks": ["1.4", "1.5"] },
    { "id": 3, "tasks": ["2.1", "2.2", "2.3", "2.4"] },
    { "id": 4, "tasks": ["2.5"] },
    { "id": 5, "tasks": ["4.1", "4.2"] },
    { "id": 6, "tasks": ["4.3", "5.1"] },
    { "id": 7, "tasks": ["5.2", "6.1"] },
    { "id": 8, "tasks": ["6.2"] },
    { "id": 9, "tasks": ["8.1", "8.2", "8.3"] },
    { "id": 10, "tasks": ["8.4", "9.1"] },
    { "id": 11, "tasks": ["9.2"] },
    { "id": 12, "tasks": ["10.1"] },
    { "id": 13, "tasks": ["10.2", "10.3"] },
    { "id": 14, "tasks": ["12.1", "12.2"] },
    { "id": 15, "tasks": ["12.3", "12.4"] },
    { "id": 16, "tasks": ["14.1"] },
    { "id": 17, "tasks": ["14.2", "15.1", "15.2"] },
    { "id": 18, "tasks": ["16"] }
  ]
}
```

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- The implementation language is Rust (matching the workspace)
- All tests must pass without DPDK installed — the stub system and LoopbackBackend enable this
- The CI integration tasks (15.x) require real DPDK on EC2 and follow the existing `integration-tests.yml` pattern
- Property-based testing is not applicable here — this is an I/O provider with external service integration; testing relies on unit tests, loopback integration, and EC2 integration
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation throughout implementation
- The walking-skeleton CI (task 1.5) is inserted early so CI grows incrementally rather than being a big-bang at the end
- `SUPPORTS_PACING = false` means s2n-quic paces internally via its timer — the immediate-drain loop is correct
- The `Mutex<Port>` threading model is explicitly documented as a v1 tradeoff (no EAL lcore pinning)
- Gateway MAC comes from explicit builder parameter or kernel ARP cache seed — no NeighborResolver/dpdk-net dependency
