# Requirements Document

## Introduction

The `dpdk-stdlib-quic` crate implements a native DPDK `io::Provider` for s2n-quic (pinned to v1.81.0 / s2n-quic-core v0.81.0), enabling QUIC protocol processing over kernel-bypassed UDP transport. The provider owns and drives an s2n-quic endpoint via a dedicated event loop thread, calling rx_burst and tx_burst directly against the DpdkBackend without Tokio runtime involvement.

s2n-quic's `io::Provider` is an own-and-drive model: `Provider::start(self, endpoint)` consumes the endpoint and runs an event loop that services wakeups, receives, transmits, and timers. The provider implements `rx::Queue` and `tx::Queue` directly (the endpoint calls into them during `receive`/`transmit`). s2n-quic never "requests a send" — the provider drives transmission by calling `endpoint.transmit(queue, &clock)`.

The I/O loop itself is runtime-free. The application side (server/client handles) still requires a Tokio runtime for `.accept().await` and `.connect().await`.

The crate lives inside the dpdk-stdlib-rust workspace alongside `dpdk-udp` and `dpdk`. A two-way benchmark compares stock s2n-quic (default Tokio I/O) against the native DPDK provider.

This spec is IPv4-only in behavior. IPv6 QUIC is deferred to a separate spec. All public APIs use `SocketAddress` (which has `IpV4`/`IpV6` variants) so the IPv6 follow-on is additive.

## Glossary

- **s2n-quic**: AWS's open-source Rust implementation of the IETF QUIC protocol (RFC 9000), used in production by CloudFront
- **QUIC**: An encrypted, multiplexed transport protocol built on UDP, the foundation of HTTP/3
- **io_Provider**: The `s2n_quic::provider::io::Provider` trait — an own-and-drive model where `start(self, endpoint)` consumes the endpoint and runs the I/O event loop
- **Endpoint**: An s2n-quic `Server` or `Client` that is consumed by the provider's `start()` method
- **rx::Queue**: s2n-quic's receive queue trait (`for_each`, `is_empty`) — the endpoint calls into it during `endpoint.receive()`
- **tx::Queue**: s2n-quic's transmit queue trait (`push`, `capacity`, `flush`) — the endpoint calls into it during `endpoint.transmit()`
- **PathHandle**: The type implementing `s2n_quic_core::path::Handle` that carries both remote and local socket addresses (`RemoteAddress`/`LocalAddress` by value)
- **Clock**: A monotonic time source implementing `s2n_quic_core::time::Clock`, supplied by the provider to the endpoint for timer management
- **DpdkBackend**: The existing `PacketBackend` implementation in `dpdk-udp` that performs userspace packet I/O via DPDK rx_burst/tx_burst, with `Mutex<Port>` for thread safety
- **ArpCache**: The ARP resolution cache in `dpdk-udp`, seeded from the kernel's `/proc/net/arp` via `seed_arp_cache_from_kernel`
- **GSO**: Generic Segmentation Offload — segmenting a single large write into multiple MTU-sized packets for batch transmission
- **GRO**: Generic Receive Offload — delivering multiple received datagrams from a single rx_burst to the endpoint
- **ECN**: Explicit Congestion Notification — IP TOS bits (ECT(0)=0b10, ECT(1)=0b01, CE=0b11) used by QUIC congestion control. s2n-quic uses `#[repr(u8)]` with wire-bit values.
- **MSS**: Maximum Segment Size — the largest QUIC payload per UDP datagram (derived from path MTU minus headers)
- **Workspace_Crate**: A Rust crate that is a member of the dpdk-stdlib-rust Cargo workspace
- **CI_Pipeline**: The GitHub Actions workflow that runs integration and performance tests on EC2

## Requirements

### Requirement 1: Workspace Crate Structure

**User Story:** As a developer, I want `dpdk-stdlib-quic` to be a proper workspace crate in dpdk-stdlib-rust, so that it integrates with the existing build system and CI.

#### Acceptance Criteria

1. THE dpdk-stdlib-quic crate SHALL be a Workspace_Crate with its own `Cargo.toml` at `dpdk-stdlib-quic/` in the repository root
2. THE dpdk-stdlib-quic crate SHALL declare `s2n-quic = "=1.81.0"` and `s2n-quic-core = "=0.81.0"` (exact version pins), `dpdk-udp`, and `dpdk` as dependencies, and SHALL NOT depend on `dpdk-tokio`
3. THE dpdk-stdlib-quic crate SHALL be added to the workspace `members` list in the root `Cargo.toml`
4. THE dpdk-stdlib-quic crate SHALL compile successfully with `cargo build` from the workspace root without DPDK installed (using the stub system)
5. WHEN DPDK is unavailable (stub mode), THE dpdk-stdlib-quic crate SHALL still compile and the provider SHALL initialize without performing packet I/O
6. THE dpdk-stdlib-quic crate SHALL document that the native provider implements lower-level s2n-quic types (rx::Queue, tx::Queue, Message, Header, Handle) that are not stable across s2n-quic releases, justifying the exact version pin
7. THE dpdk-stdlib-quic crate SHALL declare `tokio` (with "full" features) as a dev-dependency for test and benchmark executors

### Requirement 2: Native DPDK Provider — Event Loop Model

**User Story:** As a developer seeking maximum performance, I want a provider that owns the s2n-quic endpoint and drives it from a dedicated event loop thread, so I can minimize latency and maximize throughput for QUIC traffic.

#### Acceptance Criteria

1. THE Native_DPDK_Provider SHALL implement `io::Provider` such that `start(self, endpoint)` consumes the endpoint, initializes DPDK resources (EAL, port, mempool, queues), binds to the configured address, and returns the bound SocketAddress
2. THE Native_DPDK_Provider SHALL implement `rx::Queue` and `tx::Queue` directly for use with `endpoint.receive()` and `endpoint.transmit()`, and SHALL define `type PathHandle = DpdkPathHandle` carrying both remote and local socket addresses as `RemoteAddress`/`LocalAddress` values
3. THE Native_DPDK_Provider SHALL run a dedicated event loop on a std::thread (with `Mutex<Port>` thread safety) that each iteration: (a) services `endpoint.poll_wakeups(&mut cx, &clock)`, (b) fills the rx queue from recv_frames then calls `endpoint.receive(queue, &clock)`, (c) calls `endpoint.transmit(queue, &clock)` then drains the tx queue via send_frame, and (d) reads `endpoint.timeout()` for the next wake deadline
4. THE provider's Error type SHALL be `'static + Display + Send + Sync`
5. WHEN building an outbound frame, THE provider SHALL use `build_udp_frame_into_with_tos` (added to `dpdk-udp`) with software IPv4 checksum recompute, and SHALL set the Ethernet destination to the gateway MAC — acquired either via an explicit `--gateway-mac` builder parameter OR by reading the kernel ARP cache (matching existing `seed_arp_cache_from_kernel` behavior in `dpdk-udp`)
6. THE Native_DPDK_Provider SHALL dispatch ICMP packets (protocol == ICMP in the IPv4 header) to the existing `IcmpHandler::process_icmp_full()` from `dpdk-udp` in the RX path, sending any echo replies and handling error notifications
7. THE Native_DPDK_Provider SHALL support both `Server` and `Client` endpoint configurations
8. WHEN constructing an s2n-quic Server or Client, the provider SHALL be usable via the standard `.with_io()` builder method

### Requirement 3: Timer and Clock Servicing

**User Story:** As a developer, I want the provider to service s2n-quic's timer model correctly, so that connection timeouts, loss detection, and pacing work as specified by QUIC.

#### Acceptance Criteria

1. THE provider SHALL supply a monotonic Clock (implementing `s2n_quic_core::time::Clock`) to the endpoint
2. THE event loop SHALL, each iteration, read `endpoint.timeout()` and wake on the earliest of rx-readiness, tx-readiness, application wakeup, or that timer deadline
3. WHEN no rx/tx work is pending, THE event loop SHALL sleep or poll only until the next timer deadline (with a documented busy-poll-with-cooldown tradeoff: worst-case 1ms app-initiated send latency during cooldown sleep)
4. THE provider SHALL reuse s2n-quic's timer model (servicing `endpoint.timeout()`) and SHALL NOT introduce a parallel timer abstraction
5. THE provider SHALL set `SUPPORTS_PACING = false` on the tx::Queue, delegating pacing to s2n-quic's internal timer-based pacer. The event loop drains all pending frames immediately each iteration.

### Requirement 4: Provider Configuration

**User Story:** As a developer, I want to configure the provider with bind addresses and DPDK parameters, so I can control resource allocation.

#### Acceptance Criteria

1. THE dpdk-stdlib-quic crate SHALL expose a builder API for constructing the provider with a bind address
2. WHEN no explicit DPDK EAL arguments are provided, THE provider SHALL use documented static defaults (e.g. lcore 0, 4 memory channels) matching the existing `dpdk-udp` pattern
3. WHEN explicit EAL arguments are provided via the builder, THE provider SHALL pass them to DPDK initialization
4. THE builder SHALL accept a local address parameter for binding (defaulting to `0.0.0.0:0` for clients, requiring an explicit address for servers)
5. WHEN a port number of 0 is specified, THE provider SHALL bind to an ephemeral port selected by the system

### Requirement 5: Datagram Handling

**User Story:** As a developer, I want the provider to correctly handle QUIC datagrams including source/destination addressing, so that s2n-quic can manage connections and path migration properly.

#### Acceptance Criteria

1. WHEN delivering received datagrams to s2n-quic, THE provider SHALL include the source socket address (IP and port) of the sender as `RemoteAddress`
2. WHEN delivering received datagrams to s2n-quic, THE provider SHALL include the destination socket address (the local bound address) as `LocalAddress` that the datagram was sent to
3. THE provider SHALL deliver every valid UDP datagram received on the bound port to s2n-quic regardless of source address — connection-ID demultiplexing and path migration are s2n-quic's responsibility
4. THE provider SHALL support datagrams up to the maximum UDP payload size (1472 bytes for 1500 MTU) per segment
5. IF a received frame is not a valid UDP datagram destined for the bound port, THEN THE provider SHALL silently discard the frame

### Requirement 6: ECN Support

**User Story:** As a developer, I want the provider to support ECN marking so that s2n-quic's congestion control algorithms can function optimally.

#### Acceptance Criteria

1. WHEN a received IP packet contains ECN bits (ECT(0)=0b10, ECT(1)=0b01, or CE=0b11), THE provider SHALL extract the ECN codepoint from the IPv4 TOS field low 2 bits and report it to s2n-quic via the `Header.ecn` field — using direct cast from wire bits to `ExplicitCongestionNotification` (which is `#[repr(u8)]` with matching values)
2. WHEN s2n-quic requests transmission with a specific ECN marking, THE provider SHALL set the corresponding bits in the outgoing IPv4 TOS field (via `ecn as u8`) AND recompute the IPv4 header checksum in software

### Requirement 7: Send and Receive Batching (GSO/GRO)

**User Story:** As a developer, I want the provider to batch multiple QUIC packets per syscall-equivalent, so that CPU overhead per packet is minimized.

#### Acceptance Criteria

1. THE provider's tx::Queue implementation SHALL support software GSO: calling `message.write_payload(PayloadBuffer, gso_offset)` repeatedly with advancing gso_offset, segmenting the result into N packets of at most one MSS each, transmitted as N frames in a single event loop iteration
2. THE provider SHALL query GSO support via `message.can_gso(segment_len, segment_count)` inside the `push()` method to determine whether to segment
3. THE provider SHALL deliver multiple received datagrams per rx_burst to the rx queue (GRO-equivalent batching)

### Requirement 8: Error Handling and Graceful Shutdown

**User Story:** As a developer, I want the provider to handle errors and shutdown cleanly, so that QUIC connections terminate gracefully.

#### Acceptance Criteria

1. IF DPDK initialization fails during provider start, THEN THE provider SHALL return a descriptive error through s2n-quic's provider error mechanism
2. IF the RX poll loop encounters an unrecoverable error, THEN THE provider SHALL signal the endpoint to shut down
3. WHEN the s2n-quic endpoint is dropped or shut down, THE provider SHALL release all DPDK resources (mbufs, mempool references, port state)
4. WHEN `ProviderHandle::shutdown()` is called (the authoritative shutdown path), THE provider SHALL stop its event-loop thread within 100 ms. `poll_wakeups` returning `CloseError` (when all app handles drop) is the secondary shutdown signal.

### Requirement 9: Observability

**User Story:** As a developer, I want per-provider counters so I can monitor health and validate benchmark results.

#### Acceptance Criteria

1. THE provider SHALL maintain counters for: rx_burst calls, tx_burst calls, datagrams received, datagrams transmitted, rx drops (recv_frames errors), tx drops (send_frame failures), and timer wakeups
2. THE provider SHALL expose these counters via `ProviderHandle::stats()` returning a `StatsSnapshot`
3. THE benchmark binary SHALL report these counters alongside throughput metrics

### Requirement 10: Stub Compatibility and Testing

**User Story:** As a maintainer, I want all dpdk-stdlib-quic tests to pass without DPDK installed, so CI and local development work on any platform.

#### Acceptance Criteria

1. THE dpdk-stdlib-quic crate SHALL compile and its unit tests SHALL pass on macOS and Linux without DPDK installed
2. WHEN running under the stub backend, THE Native_DPDK_Provider SHALL initialize without error but produce no packet I/O (matching existing stub behavior)
3. THE dpdk-stdlib-quic crate SHALL provide a `LoopbackBackend` implementing all 8 `PacketBackend` methods, enabling the native provider's full Rx/Tx event loop and a complete QUIC handshake to be exercised via `cargo test` without DPDK installed
4. All new tests SHALL be runnable via `cargo test -p dpdk-stdlib-quic` from the workspace root

### Requirement 11: Two-Way Benchmark

**User Story:** As a developer, I want to run the same QUIC workload across stock s2n-quic and the native DPDK provider, so I can quantify the performance difference.

#### Acceptance Criteria

1. THE dpdk-stdlib-quic crate SHALL include a benchmark binary that runs a configurable QUIC throughput test
2. THE benchmark SHALL support selecting the provider via command-line argument: `--provider=stock` or `--provider=native-dpdk`
3. THE benchmark SHALL report throughput (bytes/sec), packets per second, and connection establishment latency
4. WHEN `--provider=stock` is selected, THE benchmark SHALL use s2n-quic's default Tokio-based I/O provider as the baseline
5. THE benchmark SHALL configure a TLS provider with a self-signed certificate and matching trust anchor (via an rcgen dev-dependency or checked-in fixture) — s2n-quic cannot construct an endpoint without `.with_tls()`

### Requirement 12: EC2 Integration and Performance CI

**User Story:** As a maintainer, I want automated integration and performance testing on real hardware, so regressions are caught before merge.

#### Acceptance Criteria

1. THE CI_Pipeline SHALL include a native-DPDK QUIC integration test on EC2 (two instances) exercising a full handshake and bidirectional throughput
2. THE CI_Pipeline integration test SHALL be triggered automatically on PRs, marked `continue-on-error: true` while incomplete
3. THE CI_Pipeline SHALL produce JUnit XML artifacts and a PR-comment diagnostic summary matching the existing UDP integration test harness
4. THE CI_Pipeline SHALL include a performance job running the two-way benchmark (stock vs native-dpdk)

### Requirement 13: IPv6 Readiness

**User Story:** As a developer planning future IPv6 support, I want the provider's internal structures to accommodate IPv6 without breaking changes.

#### Acceptance Criteria

1. All public APIs and the PathHandle type SHALL use `SocketAddress` (which has `IpV4`/`IpV6` variants — capital V) so IPv6 addresses can be passed without API changes
2. Internal datagram handling SHALL reuse existing parsing from `dpdk-udp` (e.g. `parse_udp_packet_ref`), so the IPv6 follow-on is additive
3. WHEN an IPv6 address is provided in this release (SocketAddress::IpV6), THE provider SHALL return an `UnsupportedAddressFamily` error rather than silently misbehaving

### Requirement 14: Zero Regression on Existing Tests

**User Story:** As a maintainer, I want all existing workspace tests to continue passing after adding the dpdk-stdlib-quic crate.

#### Acceptance Criteria

1. All existing 133+ workspace tests SHALL pass without modification after adding dpdk-stdlib-quic to the workspace
2. THE dpdk-stdlib-quic crate SHALL not modify any existing crate's public API or behavior (the `build_udp_frame_into_with_tos` addition to `dpdk-udp` is additive and non-breaking)
