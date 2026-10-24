# Requirements Document

## Introduction

Add production-credible TCP support to the dpdk-stdlib-rust project, providing drop-in replacements for `std::net::TcpListener`, `std::net::TcpStream`, `tokio::net::TcpListener`, and `tokio::net::TcpStream`. Unlike UDP, TCP is connection-oriented and requires autonomous timer management, a dedicated engine thread owning all TCB state, adaptive retransmission, congestion control, and waker-based async primitives. The implementation is split into a pure stateless codec layer (build/parse on `&[u8]`) and a stateful engine layer (state machine, timers, congestion). A new shared `dpdk-stdlib-net` crate extracts `PacketBackend` so that `dpdk-stdlib-tcp` does not depend on `dpdk-udp`. IPv4-only for MVP; IPv6 deferred to v1.1.

### IPv6 Readiness

This spec implements IPv4 only but is explicitly designed as a foundation for a follow-on IPv6 spec. All internal APIs, data structures, and codec signatures are structured so that adding IPv6 requires new code paths (not refactoring existing ones). Key design constraints enforced by this spec:

- All public APIs accept `SocketAddr` (not `SocketAddrV4`) and dispatch on address family internally
- The TCP segment builder is factored from the IP wrapper; pseudo-header checksum takes IP addresses as parameters
- The engine operates on TCP segment bytes + `SocketAddr` 4-tuple, never on raw IP bytes
- MSS is computed as `MTU − ip_header_len − tcp_header_len` (a parameter, not a constant)
- Neighbor resolution uses the existing MAC resolution abstraction (ARP for v4, NDP for v6 in follow-on)
- TCBs and accept queues are keyed on the full `(local: SocketAddr, remote: SocketAddr)` 4-tuple

## Glossary

- **TCP_Engine**: The stateful TCP protocol engine running on a dedicated thread, owning all TCBs and servicing timers independently of application calls
- **TCP_Codec**: The pure stateless codec layer providing `build_tcp_frame`/`parse_tcp_packet` on `&[u8]` slices
- **TCP_Stream**: The `TcpStream` type providing `std::net::TcpStream`-compatible API
- **TCP_Listener**: The `TcpListener` type providing `std::net::TcpListener`-compatible API
- **TCB**: Transmission Control Block — per-connection state (sequence numbers, window sizes, timers, congestion state, waker)
- **Engine_Thread**: Dedicated thread owning all TCBs and servicing connection timers independently of application read/write calls
- **PacketBackend**: The trait abstracting raw Ethernet frame I/O, extracted into a shared `dpdk-stdlib-net` crate
- **Compat_Layer**: The drop-in replacement types in `dpdk-tokio::compat` delegating to either std/tokio or DPDK TCP
- **CI_Pipeline**: The GitHub Actions workflow configuration for automated testing
- **Integration_Test_Suite**: TCP integration tests running on EC2 instances with real DPDK hardware
- **Performance_Test_Suite**: TCP performance benchmarks measuring throughput, latency, and connection rate
- **Stub_System**: The existing `dpdk-sys` stub mechanism allowing all tests to pass without real DPDK installed
- **Injectable_Clock**: A clock trait enabling deterministic testing of RTO, TIME_WAIT, and persist timers without wall-clock sleeps
- **AtomicWaker**: Per-TCB waker primitive that the engine signals after delivering data or opening send window
- **SRTT**: Smoothed Round-Trip Time used in adaptive RTO calculation per RFC 6298
- **RTTVAR**: Round-Trip Time Variation used in adaptive RTO calculation per RFC 6298
- **cwnd**: Congestion window — sender-side limit on unacknowledged data in flight
- **rwnd**: Receive window — receiver-advertised available buffer space
- **IW**: Initial Window — initial cwnd value per RFC 6928
- **MSS**: Maximum Segment Size — largest TCP payload per segment, derived from MTU minus headers
- **ISN**: Initial Sequence Number — randomized per RFC 6528 for security

## Requirements

### Requirement 1: TCP Unit Tests

**User Story:** As a developer, I want TCP unit tests that run automatically in CI on every PR and push, so that I have fast feedback on correctness of the codec, state machine, and engine logic without needing AWS infrastructure.

#### Acceptance Criteria

1. WHEN `cargo test` is executed, THE `dpdk-stdlib-tcp` crate SHALL include unit tests covering: TCP codec round-trip (build then parse), TCP options parsing, sequence-number modular arithmetic, ISN randomization, TcpError→io::Error mapping, and timer-driven state transitions using the Injectable_Clock
2. THE unit tests SHALL pass without DPDK installed by using the Stub_System, consistent with the existing 133+ tests in the UDP crates
3. WHEN a pull request is opened against main or development, THE existing `rust.yml` workflow SHALL automatically build and test the `dpdk-stdlib-tcp` and `dpdk-stdlib-net` crates as part of the workspace-wide `cargo build && cargo test`
4. THE unit test suite SHALL include property-based tests for: codec round-trip (arbitrary valid TCP segments), modular sequence arithmetic (wrap-around correctness), and MSS derivation from MTU

### Requirement 15: TCP Synthetic Performance Tests

**User Story:** As a developer, I want synthetic TCP performance benchmarks that run automatically on every PR without AWS credentials, so that I get fast regression detection on framework overhead.

#### Acceptance Criteria

1. THE CI_Pipeline SHALL include a `tcp-synthetic-perf` job in the integration-tests workflow (or a TCP-specific workflow) that runs automatically on pull requests to main and development branches
2. THE synthetic benchmark SHALL measure TCP framework overhead using a mock PacketBackend (no real NIC required), comparing: connection establishment latency, single-stream throughput with mock backend, and engine tick processing time
3. WHEN the synthetic benchmark completes, THE CI_Pipeline SHALL post results as a markdown PR comment including commit hash and run link, matching the existing UDP `synthetic-perf` job format
4. THE synthetic benchmark SHALL be implemented as a `tcp-synthetic-bench` binary crate in the workspace, producing markdown on stdout and JSON on stderr
5. THE CI_Pipeline SHALL upload synthetic benchmark results as a GitHub Actions artifact with 30-day retention

### Requirement 16: TCP Integration Tests (EC2 — Automatic)

**User Story:** As a developer, I want TCP integration tests on real DPDK hardware that run automatically on every PR, so that I catch TCP-over-DPDK regressions before merge.

#### Acceptance Criteria

1. THE CI_Pipeline SHALL include TCP integration test jobs that execute on EC2 instances using the existing DPDK test infrastructure, triggered automatically on pull requests to main and development branches
2. WHILE TCP implementation is incomplete, THE CI_Pipeline SHALL mark TCP integration test jobs as `continue-on-error: true` so that TCP test failures do not block CI
3. WHEN a TCP integration test job configured as non-blocking fails, THE CI_Pipeline SHALL still post test results and logs to the PR comment without marking the overall workflow as failed
4. THE TCP integration tests SHALL produce JUnit XML test result artifacts, consistent with the existing UDP tier1/tier2/tier3 output format, published via `dorny/test-reporter` in the PR checks UI
5. WHEN TCP integration tests complete (pass or fail), THE CI_Pipeline SHALL post a structured PR comment containing: test result summary (pass/fail/skip counts per suite), application logs (last 20 lines inline, full in collapsible), network state, and crash diagnostics — matching the existing UDP integration test comment format
6. THE CI_Pipeline SHALL upload TCP test results and instance logs as GitHub Actions artifacts with 30-day retention
7. WHEN all MVP-scope acceptance criteria are covered AND at least 10 of the 10 most recent scheduled CI runs have passed AND requirements are marked Implemented in tasks.md, THE CI_Pipeline SHALL be updated to remove `continue-on-error: true` from TCP test jobs, making TCP failures blocking

### Requirement 17: TCP Performance Tests (TRex — Manual Trigger)

**User Story:** As a developer, I want TRex-based TCP performance tests that I can trigger manually, so that I can measure real-world throughput, latency, and connection rates against the DPDK TCP stack without blocking CI on expensive long-running benchmarks.

#### Acceptance Criteria

1. THE CI_Pipeline SHALL provide a manually-triggered (`workflow_dispatch`) TCP performance test workflow, matching the existing UDP `perf-tests.yml` pattern
2. THE TCP performance workflow SHALL accept configurable inputs: payload sizes, test duration, rate steps, and DUT configurations (e.g., `plain-rust-tcp`, `rust-dpdk-stdlib-tcp`, `tokio-dpdk-stdlib-tcp`)
3. WHEN the TCP performance workflow is triggered, THE CI_Pipeline SHALL deploy infrastructure, run TRex TCP traffic generation against the DPDK TCP stack, and collect structured results
4. WHEN performance tests complete, THE CI_Pipeline SHALL post results as a PR comment (if a PR is associated with the branch) including per-config throughput, latency percentiles, and connection rate metrics
5. THE CI_Pipeline SHALL upload TCP performance results as GitHub Actions artifacts with 90-day retention
6. THE TCP performance workflow SHALL share the same concurrency group pattern (`perf-tests-tcp`) and safety-net teardown as the existing UDP performance workflow
7. IF the TCP performance workflow fails due to infrastructure issues, THE CI_Pipeline SHALL still attempt teardown and post failure diagnostics to the PR comment

### Requirement 2: TCP Integration Test Design

**User Story:** As a developer, I want comprehensive TCP integration tests that validate correctness across the full TCP lifecycle, so that I can catch regressions in connection management, data transfer, and error handling.

#### Acceptance Criteria

1. THE Integration_Test_Suite SHALL include a test that validates the TCP three-way handshake completes between two EC2 instances using the DPDK backend
2. THE Integration_Test_Suite SHALL include a test that validates bidirectional data transfer over an established TCP connection using the DPDK backend
3. THE Integration_Test_Suite SHALL include a test that validates graceful connection teardown (FIN handshake) completes without data loss
4. THE Integration_Test_Suite SHALL include a test that validates the TCP_Listener accepts multiple concurrent connections and handles each independently
5. THE Integration_Test_Suite SHALL include a test using a deterministic loss-injection PacketBackend wrapper that drops the Nth segment, asserting retransmission occurs after RTO expires AND complete delivery succeeds with max recovery time bounded by a multiple of RTT
6. THE Integration_Test_Suite SHALL include a test that holds the receiver not-reading until rwnd reaches zero, asserts the sender stops transmitting, verifies a persist probe is sent, and after drain plus re-advertise confirms transfer resumes with data intact
7. THE Integration_Test_Suite SHALL include a test that validates RST handling per RFC 5961 — challenge ACK for in-window-but-not-exact RST, abort only when seq equals RCV.NXT
8. IF a TCP integration test exceeds a 60-second timeout, THEN THE Integration_Test_Suite SHALL fail that test with a descriptive timeout error
9. THE Integration_Test_Suite SHALL include a test that validates the TCP_Stream API produces byte-for-byte identical received streams AND identical `io::ErrorKind` values on equivalent error conditions compared to `std::net::TcpStream`

### Requirement 3: TCP Performance Test Design

**User Story:** As a developer, I want TCP performance benchmarks that measure throughput, latency, and connection rate, so that I can track performance improvements and regressions as the TCP stack matures.

#### Acceptance Criteria

1. THE Performance_Test_Suite SHALL measure single-connection TCP throughput (bytes per second) for payload sizes of 64, 512, 1400, and 65536 bytes
2. THE Performance_Test_Suite SHALL measure TCP round-trip latency at P50, P90, and P99 percentiles using a request-response echo pattern
3. THE Performance_Test_Suite SHALL measure TCP connection establishment rate (connections per second) by repeatedly opening and closing connections
4. THE Performance_Test_Suite SHALL compare DPDK TCP performance against kernel TCP (`std::net::TcpStream`) using the same test workload
5. THE Performance_Test_Suite SHALL output results in a structured JSON format that includes test name, backend, metric name, metric value, and unit
6. WHEN a performance test completes, THE Performance_Test_Suite SHALL upload results as GitHub Actions artifacts with 90-day retention

### Requirement 4: TCP Protocol Engine — State Machine

**User Story:** As a developer, I want a correct TCP state machine with autonomous timer management, so that the DPDK TCP stack handles connections reliably without depending on application poll calls.

#### Acceptance Criteria

1. THE TCP_Engine SHALL implement the full TCP state machine with states: CLOSED, LISTEN, SYN_SENT, SYN_RECEIVED, ESTABLISHED, FIN_WAIT_1, FIN_WAIT_2, CLOSE_WAIT, CLOSING, LAST_ACK, and TIME_WAIT
2. WHEN a SYN segment is received on a listening port, THE TCP_Engine SHALL respond with SYN-ACK including options (MSS, WScale, SACK-Perm, Timestamps) and transition the connection to SYN_RECEIVED state
3. WHEN a SYN-ACK segment is received in SYN_SENT state, THE TCP_Engine SHALL respond with ACK and transition to ESTABLISHED state
4. THE TCP_Engine SHALL maintain a TCB for each connection keyed by the full 4-tuple `(local: SocketAddr, remote: SocketAddr)`, storing `SocketAddr` (never `SocketAddrV4`) for both endpoints, plus send and receive sequence numbers, send and receive window sizes, congestion state (cwnd, ssthresh, SRTT, RTTVAR), retransmission timer state, and an AtomicWaker
5. WHEN a data segment is received with a sequence number matching the expected receive sequence, THE TCP_Engine SHALL acknowledge the segment and deliver the payload to the application via the per-TCB readiness primitive
6. WHEN a data segment is received out of order, THE TCP_Engine SHALL buffer the segment and send a duplicate ACK for the last in-order sequence number received
7. THE TCP_Engine SHALL implement adaptive RTO per RFC 6298: SRTT updated with alpha=1/8, RTTVAR updated with beta=1/4, RTO = SRTT + max(G, 4*RTTVAR) clamped to [1s, 60s], doubling on each retransmit, applying Karn's algorithm to exclude retransmitted segments from RTT samples
8. THE TCP_Engine SHALL implement TCP receive window flow control, advertising available buffer space in each outgoing ACK segment
9. WHEN the advertised receive window of the remote peer reaches zero, THE TCP_Engine SHALL stop sending data segments and start a persist timer to probe the window periodically
10. THE TCP_Engine SHALL calculate TCP checksums for all outgoing segments using a pseudo-header checksum function that accepts source and destination IP addresses as parameters (not hardcoded to IPv4), enabling the same segment-building logic to serve both address families in the follow-on IPv6 spec
11. THE TCP_Engine SHALL validate TCP checksums on all incoming segments and discard segments with invalid checksums
12. WHEN a RST segment is received, THE TCP_Engine SHALL validate per RFC 5961: abort the connection only if the RST sequence number equals RCV.NXT; send a challenge ACK if the RST is in-window but not exact; silently drop if out-of-window
13. THE TCP_Engine SHALL implement simultaneous close (both sides sending FIN) and transition through CLOSING state correctly
14. WHEN entering TIME_WAIT state, THE TCP_Engine SHALL hold the TCB for 2*MSL (default 120 seconds) before transitioning to CLOSED
15. THE TCP_Engine SHALL implement FIN_WAIT_2 timeout to prevent indefinite resource consumption if the remote peer never sends FIN
16. ALL sequence number and acknowledgment number comparisons SHALL use modulo-2³² serial-number arithmetic as specified in RFC 9293 §3.4
17. THE TCP_Engine state machine SHALL operate on parsed TCP segment bytes plus the `(src: SocketAddr, dst: SocketAddr)` 4-tuple, never on raw IP frame bytes — the IP-layer parse (ethertype branch, header extraction) feeds the engine from outside, so adding IPv6 parsing in the follow-on spec requires no engine changes
18. THE TCP_Engine send path SHALL resolve the destination MAC address through the existing neighbor-resolution abstraction (ARP for IPv4, NDP for IPv6 in follow-on) and the AWS gateway-MAC rule, never calling ARP directly

### Requirement 5: TCP Engine Thread and Timer Architecture

**User Story:** As a developer, I want the TCP engine to service connection timers autonomously on a dedicated thread, so that retransmission, persist probes, and TIME_WAIT function correctly regardless of whether the application is making read/write calls.

#### Acceptance Criteria

1. THE Engine_Thread SHALL own all TCBs and service connection timers (RTO, persist, keepalive, TIME_WAIT, FIN_WAIT_2) independently of application read/write calls
2. THE Engine_Thread SHALL expose an API of `on_segment(&mut self, raw: &[u8]) -> Vec<Vec<u8>>` for processing inbound segments and `on_tick(&mut self, now: Instant) -> Vec<Vec<u8>>` for servicing timers, enabling deterministic testing
3. THE Engine_Thread SHALL accept an Injectable_Clock trait object so that RTO, TIME_WAIT, persist, and keepalive timers are deterministically testable without wall-clock sleeps
4. WHEN a timer fires (RTO expiry, persist probe, keepalive), THE Engine_Thread SHALL generate the appropriate outbound segment(s) without requiring any application call to be in flight
5. THE Engine_Thread SHALL wake the per-TCB AtomicWaker after delivering received data to the connection buffer or after send window opens, enabling async tasks to resume
6. THE Engine_Thread SHALL enforce a configurable maximum number of concurrent TCBs, rejecting new connections with RST when the limit is reached

### Requirement 6: TCP Congestion Control

**User Story:** As a developer, I want RFC 5681 congestion control in the MVP, so that the TCP stack does not cause network congestion collapse and behaves fairly with other TCP flows.

#### Acceptance Criteria

1. THE TCP_Engine SHALL implement RFC 5681 slow-start: increase cwnd by one MSS for each ACK received while cwnd is less than ssthresh
2. THE TCP_Engine SHALL implement RFC 5681 congestion avoidance: increase cwnd by MSS*(MSS/cwnd) for each ACK received while cwnd is greater than or equal to ssthresh
3. THE TCP_Engine SHALL initialize cwnd to the Initial Window (IW) per RFC 6928: min(10*MSS, max(2*MSS, 14600))
4. THE TCP_Engine SHALL compute the effective send window as min(cwnd, rwnd) and never have more than effective-window bytes of unacknowledged data in flight
5. WHEN three duplicate ACKs are received, THE TCP_Engine SHALL perform fast retransmit of the indicated segment and enter fast recovery per NewReno: set ssthresh = max(FlightSize/2, 2*MSS), set cwnd = ssthresh + 3*MSS
6. WHEN fast recovery ends (new ACK acknowledging all previously outstanding data), THE TCP_Engine SHALL set cwnd = ssthresh (deflate) and resume congestion avoidance

### Requirement 7: TCP Crate Structure and Backend Extraction

**User Story:** As a developer, I want TCP in a clean crate structure with a shared PacketBackend, so that dpdk-stdlib-tcp does not depend on dpdk-udp and the codebase remains modular.

#### Acceptance Criteria

1. THE PacketBackend trait SHALL be extracted into a new shared `dpdk-stdlib-net` crate that both `dpdk-udp` and `dpdk-stdlib-tcp` depend on
2. THE `dpdk-udp` crate SHALL re-export `PacketBackend` from `dpdk-stdlib-net` for backward compatibility with existing code
3. THE `dpdk-stdlib-tcp` crate SHALL depend on `dpdk-stdlib-net` for PacketBackend and on `dpdk` for safe DPDK wrappers, but SHALL NOT depend on `dpdk-udp`
4. THE `dpdk-stdlib-tcp` crate SHALL support the same three backends as UDP: DPDK, AF_PACKET, and AF_PACKET+MMAP, via the PacketBackend trait from `dpdk-stdlib-net`
5. THE `dpdk-stdlib-tcp` crate SHALL work correctly with the Stub_System, allowing all unit tests to pass without real DPDK installed, including timer-driven behaviors under a stub backend using Injectable_Clock so RTO, TIME_WAIT, and persist are deterministically testable without wall-clock sleeps
6. THE `dpdk-stdlib-tcp` crate SHALL define a `TcpError` enum with variants: ConnectionRefused, ConnectionReset, ConnectionAborted, BrokenPipe, NotConnected, TimedOut, AddrInUse, AddrNotAvailable, and implement `From<TcpError> for std::io::Error` mapping each variant to the corresponding `io::ErrorKind`
7. THE `dpdk-stdlib-tcp` crate SHALL be added to the workspace `Cargo.toml` members list alongside `dpdk-stdlib-net`

### Requirement 8: TCP Codec — Packet Building and Parsing

**User Story:** As a developer, I want a pure stateless TCP codec layer separated from the engine, so that frame construction and parsing are independently testable and reusable.

#### Acceptance Criteria

1. THE TCP_Codec SHALL provide a `build_tcp_frame` function that constructs a complete Ethernet + IPv4 + TCP frame as `Vec<u8>` from header fields, TCP flags, options, and payload, operating on `&[u8]` inputs only
2. THE TCP_Codec SHALL provide a `parse_tcp_packet` function that extracts TCP header fields, options, and payload from a raw Ethernet frame `&[u8]`, returning a structured result
3. THE TCP_Codec SHALL provide a `build_tcp_packet` function that writes directly into a DPDK `Mbuf` for the zero-copy DPDK path
4. FOR ALL valid TCP segments, building a frame and then parsing the frame SHALL produce equivalent header fields (including all parsed options), flags, and payload (round-trip property)
5. THE TCP_Codec parser SHALL correctly parse TCP options: MSS, Window Scale, SACK-Permitted, Timestamps, SACK blocks, and End-of-Options/NOP padding
6. THE TCP_Codec frame builder SHALL support setting all TCP flags: SYN, ACK, FIN, RST, PSH, URG
7. WHEN a frame shorter than the minimum IPv4+TCP header length (54 bytes: 14 Eth + 20 IPv4 + 20 TCP) is passed to the parser, THE parser SHALL return an error; the parser SHALL additionally validate that data-offset is at least 5 and that data-offset*4 bytes fit within the frame
8. WHEN building a SYN or SYN-ACK frame, THE TCP_Codec SHALL include MSS, Window Scale, SACK-Permitted, and Timestamps options
9. THE TCP_Codec SHALL compute MSS as `MTU − ip_header_len − tcp_header_len` where ip_header_len is a parameter (20 for IPv4, 40 for IPv6), defaulting to 1460 for IPv4 with a 1500-byte MTU; the constant `MAX_TCP_PAYLOAD` (1460) SHALL be paired with a reserved `MAX_TCP_PAYLOAD_V6` (1440) following the existing `MAX_UDP_PAYLOAD`/`MAX_UDP_PAYLOAD_V6` pattern
10. THE TCP_Codec SHALL never emit a segment with payload larger than min(local_mss, peer_mss)
11. THE TCP_Codec SHALL factor the TCP segment builder (header + options + payload + checksum) from the IP-layer wrapper, so that `build_tcp_frame` composes an inner segment-build step with an IPv4 framing step; the `tcp_pseudo_header_checksum` function SHALL accept `src_ip` and `dst_ip` as generic parameters (matching the `udp6_pseudo_header_checksum(src: &Ipv6Addr, dst: &Ipv6Addr, len)` pattern) to support both address families
12. THE TCP_Codec SHALL reserve the public function names `build_tcp6_frame`, `parse_tcp6_packet`, and `tcp6_checksum` for the follow-on IPv6 spec; the IPv4 implementations in this spec SHALL be named so that v6 siblings can be added without renaming

### Requirement 9: TCP Socket API (std::net Compatible)

**User Story:** As a developer, I want `TcpStream` and `TcpListener` types with the same API as `std::net`, so that existing Rust code can use DPDK TCP with minimal changes.

#### Acceptance Criteria

1. THE TCP_Stream SHALL implement `connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream>` matching the `std::net::TcpStream::connect` signature; internally, connect SHALL dispatch on address family — calling `connect_v4` for IPv4 addresses (DPDK path) and falling back to kernel for IPv6 addresses, with the internal split structured to accept a `connect_v6` sibling in the follow-on spec
2. THE TCP_Listener SHALL implement `bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener>` matching the `std::net::TcpListener::bind` signature; internally, bind SHALL dispatch on address family — calling `bind_v4` for IPv4 (DPDK path) and falling back to kernel for IPv6, with the internal split structured to accept a `bind_v6` sibling in the follow-on spec
3. THE TCP_Listener SHALL implement `accept(&self) -> io::Result<(TcpStream, SocketAddr)>` matching the `std::net::TcpListener::accept` signature
4. THE TCP_Stream SHALL implement `read(&mut self, buf: &mut [u8]) -> io::Result<usize>` via the `std::io::Read` trait
5. THE TCP_Stream SHALL implement `write(&mut self, buf: &[u8]) -> io::Result<usize>` via the `std::io::Write` trait
6. THE TCP_Stream SHALL implement `shutdown(how: Shutdown) -> io::Result<()>` matching the `std::net::TcpStream::shutdown` signature
7. THE TCP_Stream SHALL implement `peer_addr() -> io::Result<SocketAddr>` and `local_addr() -> io::Result<SocketAddr>`
8. THE TCP_Stream SHALL implement `set_read_timeout`, `set_write_timeout`, `read_timeout`, `write_timeout`, `set_nodelay`, `nodelay`, `set_ttl`, and `ttl` matching `std::net::TcpStream` signatures
9. THE TCP_Listener SHALL implement `local_addr() -> io::Result<SocketAddr>`, `set_ttl`, `ttl`, and `incoming() -> Incoming` matching `std::net::TcpListener` signatures
10. THE TCP_Stream SHALL implement `connect_timeout(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream>`
11. THE TCP_Stream SHALL implement `try_clone() -> io::Result<TcpStream>`, `set_nonblocking(nonblocking: bool) -> io::Result<()>`, `peek(buf: &mut [u8]) -> io::Result<usize>`, and `take_error() -> io::Result<Option<io::Error>>`
12. THE TCP_Stream SHALL implement `set_linger(linger: Option<Duration>) -> io::Result<()>` and `linger() -> io::Result<Option<Duration>>`
13. WHEN `TcpStream::read` is called and no data is available, THE TCP_Stream SHALL block the calling thread by parking on a per-connection readiness primitive signaled by the Engine_Thread, WITHOUT holding the TCB lock while waiting
14. WHEN `TcpStream::write` is called and the send buffer is full, THE TCP_Stream SHALL block the calling thread by parking on a per-connection readiness primitive signaled by the Engine_Thread, WITHOUT holding the TCB lock while waiting

### Requirement 10: Async TCP API (tokio::net Compatible)

**User Story:** As a developer, I want async `TcpStream` and `TcpListener` types compatible with `tokio::net`, so that async Rust applications can use DPDK TCP as a drop-in replacement.

#### Acceptance Criteria

1. THE Compat_Layer SHALL provide `dpdk_tokio::compat::tokio::TcpStream` with the same async API as `tokio::net::TcpStream`
2. THE Compat_Layer SHALL provide `dpdk_tokio::compat::tokio::TcpListener` with the same async API as `tokio::net::TcpListener`
3. THE Compat_Layer SHALL implement `TcpListener::bind(addr).await` that tries DPDK first and falls back to tokio, matching the existing UDP compat pattern
4. THE Compat_Layer SHALL implement `TcpListener::accept().await -> io::Result<(TcpStream, SocketAddr)>`
5. THE Compat_Layer SHALL implement `TcpStream::connect(addr).await` that tries DPDK first and falls back to tokio
6. THE Compat_Layer SHALL implement `AsyncRead` and `AsyncWrite` traits on `TcpStream` using real `Poll::Pending` returns with per-TCB AtomicWaker registration — the engine wakes the task after delivering data or opening send window; busy-spin and yield_now patterns are prohibited
7. THE Compat_Layer SHALL provide `dpdk_tokio::compat::net::TcpStream` and `dpdk_tokio::compat::net::TcpListener` as `std::net`-compatible drop-in replacements, following the existing UDP compat pattern
8. WHEN an IPv6 address is provided, THE Compat_Layer SHALL fall back to the kernel tokio/std implementation transparently (IPv4-only for DPDK in MVP)

### Requirement 11: TCP Window Scaling and SYN Options

**User Story:** As a developer, I want proper TCP option negotiation on connection setup, so that the stack supports large windows and interoperates with modern TCP implementations.

#### Acceptance Criteria

1. WHEN sending a SYN segment, THE TCP_Engine SHALL include options: MSS (derived from interface MTU - 40), Window Scale (per RFC 7323 §2), SACK-Permitted, and Timestamps
2. WHEN sending a SYN-ACK segment, THE TCP_Engine SHALL include the same options: MSS, Window Scale, SACK-Permitted, and Timestamps
3. WHEN a SYN or SYN-ACK with a Window Scale option is received, THE TCP_Engine SHALL record the peer's scale factor and apply it to all subsequent window advertisements from that peer
4. WHEN a SYN or SYN-ACK without a Window Scale option is received, THE TCP_Engine SHALL disable window scaling for that connection (scale factor = 0)
5. THE TCP_Engine SHALL never emit a segment with payload exceeding min(local_mss, peer_mss) where peer_mss is learned from the MSS option in the peer's SYN/SYN-ACK, defaulting to 536 if no MSS option is present

### Requirement 12: TCP Security and Resource Limits

**User Story:** As a developer, I want bounded resource usage and secure connection handling, so that the TCP stack is resistant to resource exhaustion and basic attacks.

#### Acceptance Criteria

1. THE TCP_Listener SHALL maintain a bounded pending-connection queue (accept backlog) keyed on `(local: SocketAddr, remote: SocketAddr)` 4-tuples, with a configurable maximum defaulting to 128, dropping new SYNs with RST when the queue is full
2. THE TCP_Engine SHALL enforce a configurable maximum number of concurrent TCBs across all listeners and connections
3. THE TCP_Engine SHALL generate Initial Sequence Numbers (ISN) using randomization per RFC 6528 to prevent sequence prediction attacks
4. THE TCP_Engine SHALL validate RST segments per RFC 5961: abort only when RST seq equals RCV.NXT, send challenge ACK for in-window non-exact RST, silently drop out-of-window RST
5. ALL sequence number and acknowledgment comparisons SHALL use modulo-2³² serial-number arithmetic preventing wrap-around bugs

### Requirement 13: TCP Socket Options and Control Surface

**User Story:** As a developer, I want full socket option support, so that applications can tune TCP behavior for their workload.

#### Acceptance Criteria

1. THE TCP_Stream SHALL support SO_REUSEADDR as an extension method, allowing multiple listeners on the same port when configured
2. THE TCP_Stream SHALL support SO_RCVBUF and SO_SNDBUF as extension methods, allowing applications to configure receive and send buffer sizes
3. THE TCP_Stream SHALL support SO_KEEPALIVE as an extension method with configurable idle time, interval, and probe count
4. THE TCP_Stream SHALL support TCP_NODELAY (Nagle's algorithm disable) and implement delayed-ACK (coalesce ACKs up to 200ms or every-other-segment) as the default behavior
5. THE TCP_Stream SHALL support SO_LINGER as an extension method controlling behavior on close: when linger is set with timeout > 0, close SHALL block until all data is sent or timeout expires; when linger is set with timeout = 0, close SHALL send RST and discard unsent data

### Requirement 14: TCP Error Taxonomy

**User Story:** As a developer, I want well-defined TCP error types that map to standard io::Error kinds, so that applications can handle TCP errors idiomatically.

#### Acceptance Criteria

1. THE `dpdk-stdlib-tcp` crate SHALL define `TcpError` with variants: ConnectionRefused, ConnectionReset, ConnectionAborted, BrokenPipe, NotConnected, TimedOut, AddrInUse, AddrNotAvailable
2. THE `TcpError` SHALL implement `From<TcpError> for std::io::Error` mapping: ConnectionRefused → ConnectionRefused, ConnectionReset → ConnectionReset, ConnectionAborted → ConnectionAborted, BrokenPipe → BrokenPipe, NotConnected → NotConnected, TimedOut → TimedOut, AddrInUse → AddrInUse, AddrNotAvailable → AddrNotAvailable
3. WHEN a connection attempt receives RST in response to SYN, THE TCP_Engine SHALL surface ConnectionRefused to the application
4. WHEN a connection is reset by the peer during data transfer, THE TCP_Engine SHALL surface ConnectionReset to the application
5. WHEN a write is attempted on a connection whose peer has closed its read half, THE TCP_Engine SHALL surface BrokenPipe to the application
6. WHEN all retransmission attempts are exhausted (RTO exceeded maximum retries), THE TCP_Engine SHALL surface TimedOut to the application

## Scope

### MVP (v1)

- Three-way handshake with MSS + WScale + SACK-Perm + Timestamps options on SYN/SYN-ACK
- Full 11-state TCP state machine including simultaneous close, TIME_WAIT (2·MSL), and FIN_WAIT_2 timeout
- Bidirectional data transfer with cumulative ACK and in-order delivery
- Adaptive RTO per RFC 6298 (SRTT/RTTVAR, α=1/8, β=1/4, Karn's algorithm, exponential backoff)
- Window scaling negotiated and applied per RFC 7323 §2
- Fast retransmit and fast recovery (3 duplicate ACKs, NewReno)
- RFC 5681 congestion control: slow-start, congestion avoidance, ssthresh; IW per RFC 6928
- Modulo-2³² serial-number arithmetic for all sequence/ack comparisons
- Receive window flow control with persist timer
- MSS-bounded segment emission: never exceed min(local_mss, peer_mss)
- Graceful FIN shutdown and RFC 5961 RST validation with challenge ACK
- Bounded accept queue (default 128) and bounded concurrent TCBs
- ISN randomization per RFC 6528
- Full `std::net` API: connect, connect_timeout, bind, accept, read, write, shutdown, try_clone, set_nonblocking, peek, take_error, set_linger/linger, incoming()
- TcpError taxonomy with proper io::Error mapping
- SO_REUSEADDR, SO_RCVBUF/SO_SNDBUF, SO_KEEPALIVE, TCP_NODELAY/delayed-ACK, SO_LINGER
- Dedicated engine thread owning TCBs and servicing timers autonomously
- Per-connection readiness primitives (blocking API parks without holding TCB lock)
- Per-TCB AtomicWaker for async API (real Poll::Pending, no busy-spin)
- PacketBackend extraction into shared `dpdk-stdlib-net` crate
- Injectable Clock for deterministic timer testing
- Pure codec layer separated from stateful engine
- IPv4 only; fallback to kernel for IPv6 addresses
- IPv6-readiness: all APIs use `SocketAddr`; internal v4/v6 dispatch pattern; codec factored from IP wrapper; parameterized pseudo-header checksum; MSS as function of ip_header_len; neighbor resolution via abstraction; TCBs keyed on full SocketAddr 4-tuple; reserved v6 function names

### v1.1

- Congestion control upgrade: CUBIC (replacing NewReno as default)
- SACK-based selective retransmission
- Timestamps echo and PAWS (Protection Against Wrapped Sequences)
- Full tokio API surface: split/into_split, ready/readable/writable, try_read/try_write, poll_read/poll_write, from_std/into_std, peek
- IPv6 dual-stack TCP over DPDK
- TCP_INFO observability (per-connection stats: RTT, cwnd, retransmits, etc.)
- TCP_USER_TIMEOUT and TCP_SYNCNT socket options

### Out of Scope

- TCP Fast Open (TFO)
- Explicit Congestion Notification (ECN)
- Path MTU Discovery (PMTUD, RFC 1191)
- TCP_CONGESTION pluggable algorithm selection
- Urgent/OOB (out-of-band) delivery semantics
- SO_REUSEPORT
- TCP_QUICKACK / TCP_CORK / TCP_NOTSENT_LOWAT

## Sequencing Recommendation

The recommended implementation order is:

1. **Crate extraction** — Extract PacketBackend into `dpdk-stdlib-net`; dpdk-udp re-exports for compatibility
2. **Codec** — `build_tcp_frame`/`parse_tcp_packet` (pure, stateless) + TcpError taxonomy
3. **Engine** — State machine + timers + congestion control + Injectable_Clock on dedicated thread
4. **Sync API** — TcpStream/TcpListener with blocking semantics, locking model, socket options
5. **Async API + CI tests** — Compat layer with AtomicWaker, integration test infrastructure on EC2
6. **Performance tests** — Throughput, latency, connection-rate benchmarks
