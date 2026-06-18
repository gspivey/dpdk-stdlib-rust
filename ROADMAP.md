# ROADMAP

Ordered feature list for agent-router sessions. Pick the **first uncompleted item** and implement exactly that one. Read all linked spec files before writing code. After merging, check the box and add the PR link.

Each item targets ~300–500 lines of new/modified Rust (or equivalent for scripts/YAML). LOC estimates are noted where the spec includes them explicitly.

---

## Active Roadmap

### 1. Extract `dpdk-stdlib-net` shared crate

Move `PacketBackend` trait, `DpdkBackend`, `RawSocketBackend`, `ring_buffer.rs`, `ipv4_checksum`/`pseudo_header_checksum` helpers out of `dpdk-stdlib-udp` into a new `dpdk-stdlib-net` crate. Add a `NeighborResolver` trait + `ArpResolver` implementation. `dpdk-stdlib-udp` re-exports everything for backward compatibility. CI must enforce `dpdk-stdlib-tcp` never depends on `dpdk-stdlib-udp`. Use two commits: `1.1a` = git-mv + re-export (no behavior change), `1.1b` = add `rx_readiness()` to the trait.

- Spec: `.kiro/specs/tcp-support/` · tasks `1.1`, `1.2`, `1.3`, `1.4`
- [x] Complete · PR: #65

---

### 2. `dpdk-stdlib-quic`: Crate skeleton and walking-skeleton CI

Create `dpdk-stdlib-quic/` workspace crate with empty module stubs, a `quic-smoke` binary that compiles in stub mode and exits 0, and a `.github/workflows/quic-integration-tests.yml` skeleton (`continue-on-error: true`, single instance, no ENI bind, no gateway MAC). The walking-skeleton CI grows in later items.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `1.1`, `1.2`, `1.3`, `1.4`, `1.5`
- [x] Complete · PR: #66

---

### 3. `dpdk-stdlib-quic`: Foundational types

`DpdkQuicError` enum (`thiserror`, `Send + Sync`). `StdClock` wrapping `std::time::Instant`. `DpdkPathHandle` implementing `s2n_quic_core::path::Handle` (IPv4 only; IPv6 → `UnsupportedAddressFamily`). ECN helpers: `extract_ecn(u8) -> ExplicitCongestionNotification` (direct cast), `ecn_to_tos_bits` (as u8). Unit tests: clock monotonicity, path handle round-trip, ECN round-trip for all 4 codepoints, IPv6 rejection.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `2.1`, `2.2`, `2.3`, `2.4`, `2.5`
- [x] Complete · PR: #67

---

### 4. `dpdk-stdlib-quic`: Frame building with TOS/ECN support

Add `build_udp_frame_into_with_tos(tos: u8, ...)` to `dpdk-stdlib-udp` — non-breaking additive change, sets `frame[ip+1] = tos` and recomputes the IPv4 checksum. Create `dpdk-stdlib-quic/src/frame.rs` as a thin wrapper/re-export. Unit tests: TOS byte at the correct offset, checksum validity after TOS modification, ECN bits survive a round-trip through TOS.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `4.1`, `4.2`, `4.3`
- [x] Complete · PR: #68

---

### 5. `dpdk-stdlib-quic`: RX queue

`DpdkRxQueue` implementing `s2n_quic_core::io::rx::Queue` (`for_each`, `is_empty`). `parse_to_rx_datagram(frame, local_addr)` reuses `parse_udp_packet_ref` from `dpdk-stdlib-udp`, extracts TOS for ECN, constructs `Header { path: DpdkPathHandle, ecn }`, returns payload as `Vec<u8>`. Unit tests: valid parse, wrong dst_port drop, non-IPv4 drop, truncated drop, `for_each` drains all.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `5.1`, `5.2`
- [x] Complete · PR: #69

---

### 6. `dpdk-stdlib-quic`: TX queue

`DpdkTxQueue` implementing `s2n_quic_core::io::tx::Queue`. Constants: `SUPPORTS_ECN = true`, `SUPPORTS_PACING = false`, `SUPPORTS_FLOW_LABELS = false`. GSO: call `message.can_gso(segment_len, segment_count)`, loop `write_payload(buf, gso_offset)` advancing gso_offset. `drain()` method yields pending frames. Unit tests: single segment, GSO multi-segment correct frame count, capacity decrements, drain empties, ECN TOS byte correct.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `6.1`, `6.2`
- [x] Complete · PR: #70

---

### 7. `dpdk-stdlib-quic`: Stats, gateway-MAC acquisition, and `LoopbackBackend`

`ProviderStats` atomic counters + `StatsSnapshot::snapshot()`. `ProviderHandle` with `Arc<AtomicBool>` shutdown flag, `JoinHandle`, and `shutdown()` method. `ProviderBuilder::with_gateway_mac([u8; 6])` + kernel ARP cache fallback via `seed_arp_cache_from_kernel` pattern. `LoopbackBackend` implementing all 8 `PacketBackend` methods (send enqueues to `Mutex<VecDeque>`, recv drains). Unit tests for all 8 `LoopbackBackend` methods.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `8.1`, `8.2`, `8.3`, `8.4`
- [x] Complete · PR: #71

---

### 8. `dpdk-stdlib-quic`: Event loop

`event_loop` function: check shutdown flag → `poll_wakeups` (break on `CloseError`) → RX (`recv_frames` → ICMP dispatch → `parse_to_rx_datagram` → `endpoint.receive`) → TX (`endpoint.transmit` → `drain()` → `send_frame`) → busy-poll-with-cooldown (sleep until `min(endpoint.timeout(), now + 1ms)` after idle budget exceeded). Increment stats at each stage. ICMP dispatch: check `frame[ETH_HEADER_LEN+9] == IP_PROTO_ICMP` → `icmp_handler.process_icmp_full()`.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `9.1`, `9.2`
- [x] Complete · PR: #72

---

### 9. `dpdk-stdlib-quic`: Provider and builder

`ProviderBuilder` with `with_addr`, `with_eal_args`, `with_rx_burst` (default 32), `with_tx_burst` (default 32), `with_backend_config`, `with_gateway_mac`. `build()` creates shared `Arc<ProviderStats>` + `Arc<AtomicBool>`, returns `(DpdkProvider, ProviderHandle)`. `DpdkProvider::start(self, endpoint)`: initialize backend → resolve gateway MAC → bind address → clone stats/shutdown into thread → `std::thread::spawn(event_loop)`. Shutdown: `AtomicBool` flag (primary) + `CloseError` from `poll_wakeups` (secondary).

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `10.1`, `10.2`, `10.3`
- [x] Complete · PR: #73

---

### 10. `dpdk-stdlib-quic`: Loopback integration tests

Four integration tests using `LoopbackBackend` (no DPDK required): (a) full QUIC handshake — server + client, `rcgen` TLS, open stream, send data, echo, verify integrity; (b) provider init in stub mode — no error, IPv6 address → `UnsupportedAddressFamily`; (c) ECN round-trip — build frame with ECN via TX path, parse via RX path, verify codepoint preserved for all 4 values; (d) GSO segmentation — payload > MSS with `can_gso=true` → correct frame count + payload boundaries.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `12.1`, `12.2`, `12.3`, `12.4`
- [x] Complete · PR: #74

---

### 11. `dpdk-stdlib-quic`: Benchmark binary

`dpdk-stdlib-quic/src/bin/bench.rs`: CLI args `--provider=stock|native-dpdk`, `--duration=<secs>`, `--streams=<n>`, `--payload-size=<bytes>`. Both providers run N-stream echo workload. Reports: throughput (Gbps), PPS, handshake latency P50/P99, provider stats counters. TLS via `rcgen`. Uses `tokio` dev-dep for the app-side executor. Must compile in stub mode (won't fully run without DPDK).

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `14.1`, `14.2`
- [x] Complete · PR: #75

---

### 12. `dpdk-stdlib-quic`: EC2 integration and performance CI

Extend `quic-integration-tests.yml` to two EC2 instances (server + client with DPDK), full handshake + bidirectional throughput test, JUnit XML artifacts, PR-comment summary. Add a benchmark job running `bench --provider=stock` and `bench --provider=native-dpdk` side by side. Remove `continue-on-error: true` once 5+ consecutive runs pass.

- Spec: `.kiro/specs/s2n-quic-provider/` · tasks `15.1`, `15.2`
- [x] Complete · PR: #76

---

### 13. IPv6 UDP performance benchmarks

Run TRex at 64/512/1400 B with IPv6 UDP traffic. Compare against the IPv4 baseline from `docs/perf-test-log.md`. No PPS regression vs IPv4 required. Append results in the existing format. This closes out the IPv6 feature (protocol tasks 1–8 are all merged).

Trigger: `gh workflow run perf-tests.yml --ref <branch>` after all CI passes. Wait for the check_run delivery — do not poll manually.

- [x] Complete · PR: #77

---

### 14. `dpdk-stdlib-tcp`: Crate skeleton and codec types

Create `dpdk-stdlib-tcp/` crate (depends on `dpdk-stdlib-net` + `dpdk-stdlib`, CI gates must fail for `dpdk-stdlib-udp` and `tokio` dependencies). `SpscByteRing` (power-of-2 byte buffer, Acquire/Release atomics). `SeqNum(u32)` (modular arithmetic, no `Ord`). `TcpError` enum + `From<TcpError> for io::Error`. `TcpFlags` bitfield, `TcpOptions` (MSS/WScale/SACK-Perm/Timestamps/SACK/NOP/EOL), `ParsedTcpSegment`, `TcpFrameParams`. Constants: `MAX_TCP_PAYLOAD = 1460`, `DEFAULT_PEER_MSS = 536`.

- Spec: `.kiro/specs/tcp-support/` · tasks `3.1`, `3.2`, `3.3`, `3.4`, `3.5`, `3.6`
- [x] Complete · PR: #78

---

### 15. `dpdk-stdlib-tcp`: Codec implementation and property tests

`build_tcp_frame(params) -> Vec<u8>` (Eth + IPv4 + TCP; SYN/SYN-ACK frames include MSS, WScale, SACK-Perm, Timestamps). `tcp_checksum` with parameterized pseudo-header. `compute_mss(mtu, ip_hdr_len)`. `parse_tcp_packet` (validate data-offset ≥5, parse all options). `build_tcp_packet(mbuf, params)` (zero-copy DPDK path, byte-identical to `build_tcp_frame`). 7 property tests: round-trip, Mbuf equivalence, invalid frame rejection, SYN required options, MSS bound, checksum flip fails, sequence transitivity. (~500 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `3.7`, `3.8`, `3.9`, `3.10`, `3.11`
- [x] Complete · PR: #79

---

### 16. `dpdk-stdlib-tcp`: Contract types, TcpState, Clock, and IsnGenerator

`ConnectionHandle` (rx_ring/tx_ring SpscByteRing, AtomicU8 state, AtomicBool eof, Mutex error, condvar + notify_lock, AtomicWaker × 2, app_refcount, cmd_tx, key, linger). `EngineCommand` enum. `SocketOption` enum. `CommandSender` (wraps mpsc::Sender + Arc<EngineWakeup>; every send signals engine_wakeup). `OneshotSender/Receiver`. `TcpState` (11 states). `FourTuple`. `SystemClock` + `MockClock` (with `advance()`). `IsnGenerator` (RFC 6528: 128-bit per-boot secret via `getrandom`, SipHash-2-4 of FourTuple, M = elapsed µs / 4). (~400 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `5.1`, `5.2`, `5.3`, `5.4`
- [x] Complete · PR: #80

---

### 17. `dpdk-stdlib-tcp`: TimerWheel, CongestionState, and Tcb

`TimerWheel` (1 ms granularity, 6 timer types: RTO/Persist/Keepalive/TimeWait/FinWait2/DelayedAck; insert/cancel/tick-advance). `CongestionState`: `initial_window` (min(10×MSS, max(2×MSS, 14600))), RFC 6298 RTT/RTO (α=1/8, β=1/4, Karn's, clamped [1s,60s]), `on_ack` (slow-start + CA), `on_triple_dup_ack` (fast retransmit), `on_partial_ack`, `on_recovery_exit`, `effective_window`. `Tcb` struct (all fields as spec: snd/rcv sequence state, scales, MSS values, timers, retransmit_queue, reorder_buffer, send_buf, Nagle state, socket options, src_mac/dst_mac, handle). (~500 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `5.5`, `5.6`, `5.7`
- [x] Complete · PR: #81

---

### 18. `dpdk-stdlib-tcp`: Engine — SYN handshake

`TcpEngine::on_segment` handshake path: SYN → SYN_RECEIVED (send SYN-ACK with all required options, transition state). SYN-ACK → ESTABLISHED (send ACK, transition state, wake connect oneshot). RST in SYN_SENT → latch `ConnectionRefused` on handle. Accept-side: populate `Tcb.src_mac/dst_mac` from parsed frame. Property test: state machine validity (any valid event sequence → one of 11 `TcpState` values). (~450 LOC per spec)

- Spec: `.kiro/specs/tcp-support/` · task `5.8`
- [x] Complete · PR: #82

---

### 19. `dpdk-stdlib-tcp`: Engine — established in-order data and ACK

`on_segment` established path: in-order data delivery (verify seq == rcv_nxt, push payload to rx_ring, ACK with ack_num = rcv_nxt + len). Cumulative ACK: advance snd_una, free matching retransmit entries. Apply peer's window scale to advertised window. Wake condvar + read_waker after rx_ring push. Property tests: in-order ACK correctness (ack_num matches), window-scaling round-trip (encode+decode bounds effective send window). (~400 LOC per spec)

- Spec: `.kiro/specs/tcp-support/` · task `5.9`
- [x] Complete · PR: #83

---

### 20. `dpdk-stdlib-tcp`: Engine — out-of-order reorder buffer

`on_segment` OOO path: buffer segment in `reorder_buffer` (BTreeMap keyed on seq.diff(rcv_nxt)), send dup-ACK with ack_num == rcv_nxt. When a gap fills, drain contiguous data from reorder_buffer to rx_ring, advancing rcv_nxt. Property tests: OOO dup-ACK has ack_num == rcv_nxt, reorder buffer soundness (OOO including sequence-number wrap-around produces byte-identical output to in-order assembly). (~350 LOC per spec)

- Spec: `.kiro/specs/tcp-support/` · task `5.10`
- [x] Complete · PR: #84

---

### 21. `dpdk-stdlib-tcp`: Engine — FIN teardown and RST validation

FIN teardown state transitions: FIN_WAIT_1, FIN_WAIT_2, CLOSE_WAIT, LAST_ACK, CLOSING (simultaneous close), TIME_WAIT (2×MSL = 120s). Set eof flag on `ConnectionHandle` after final bytes enqueued. RST validation per RFC 5961: exact seq → abort + latch `ConnectionReset`; in-window non-exact → send challenge ACK; out-of-window → silently drop. Property tests: TIME_WAIT/FIN_WAIT_2 cleanup (TCB transitions to CLOSED after timeout), RST validation. (~550 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `5.11`, `5.12`
- [x] Complete · PR: #85

---

### 22. `dpdk-stdlib-tcp`: Engine — Nagle, delayed-ACK, and SWS avoidance

Nagle algorithm: if unacked data AND new write < MSS, buffer; send immediately if `nodelay` || no unacked bytes || write fills MSS. Delayed-ACK: coalesce ACKs up to 200 ms or every-other-segment; send immediately on OOO. SWS avoidance: withhold window update until available space ≥ min(MSS, half buffer). (~350 LOC per spec)

- Spec: `.kiro/specs/tcp-support/` · task `5.13`
- [x] Complete · PR: #86

---

### 23. `dpdk-stdlib-tcp`: Engine — `on_tick` (tx-drain and RTO)

`on_tick` tx-drain: drain `tx_ring` → `send_buf` → segment respecting `effective_window(rwnd)` → transmit; wake condvar + write_waker when send window opens. `on_tick` RTO: retransmit oldest unacked segment on RTO expiry, double RTO (exponential backoff), abort after max retries → latch `TimedOut`. (~450 LOC; implement tx-drain first — most engine tests depend on the TX path)

- Spec: `.kiro/specs/tcp-support/` · tasks `5.14`, `5.15`
- [x] Complete · PR: #87

---

### 24. `dpdk-stdlib-tcp`: Engine — `on_tick` (persist, keepalive, TIME_WAIT, delayed-ACK)

Persist timer: send 1-byte zero-window probe at exponentially backed-off intervals (capped 60s); NEVER abort. Keepalive: send probe after idle timeout, abort after max probes → latch `TimedOut`. TIME_WAIT expiry → CLOSED, free TCB. FIN_WAIT_2 timeout → free TCB. Delayed-ACK timer fire → send cumulative ACK. (~400 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `5.16`, `5.17`
- [x] Complete · PR: #88

---

### 25. `dpdk-stdlib-tcp`: Engine — `on_command`

Connect: allocate TCB, populate src_mac/dst_mac from command, send SYN with all required options, transition to SYN_SENT, arm RTO. Listen: register listener in listen_map with bounded accept queue (default 128). Accept: dequeue via oneshot, park if empty. Enforce max-TCBs limit → RST on new connection when at capacity. Enforce accept backlog limit → RST on new SYN when queue full. Teardown: Shutdown (set fin_pending, flush tx_ring → send_buf → FIN). Close (honor SO_LINGER: timeout=0 → RST, timeout>0 → wait/timeout). SetOption → update Tcb fields. (~600 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `5.18`, `5.19`
- [x] Complete · PR: #89

---

### 26. `dpdk-stdlib-tcp`: Engine property tests

Seven property-based tests covering the full engine: (1) state machine validity, (2) in-order ACK correctness, (3) OOO dup-ACK, (4) timer-driven segment generation (expired timer → outbound segment without app call), (5) TIME_WAIT/FIN_WAIT_2 cleanup, (6) resource limit enforcement (max_tcbs/backlog exceeded → RST), (7) RST validation per RFC 5961. Plus: (8) flight-size invariant (unacked ≤ min(cwnd, rwnd)), (9) slow-start cwnd growth, (10) initial window formula, (11) fast retransmit formula, (12) partial ACK in recovery, (13) persist-never-aborts. (~400 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `6.1`, `6.2`, `6.3`
- [x] Complete · PR: #90

---

### 27. `dpdk-stdlib-tcp`: Sync socket — engine loop and `DpdkTcpStream`

`engine_loop(backend, engine, cmd_rx, wakeup)`: select on `rx_readiness` | `engine_wakeup` | timer deadline; dispatch each arm (`parse_tcp_packet` → `on_segment`, `cmd_rx.try_recv()` → `on_command`, `on_tick(clock.now())`); send outbound frames via `backend.send_frame`. `DpdkTcpStream`: `io::Read` with P0-B recheck-under-lock (check error → try rx_ring.read → check eof → lock notify_lock → recheck → condvar.wait). `io::Write`: push to tx_ring, signal engine_wakeup, block if full. Honor `set_nonblocking` (→ `WouldBlock`) and `read_timeout`/`write_timeout` (condvar.wait_timeout). `Drop` → decrement app_refcount, send Close on last handle. (~500 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `8.1`, `8.2`, `8.3`
- [x] Complete · PR: #91

---

### 28. `dpdk-stdlib-tcp`: Sync socket — `TcpStream` and `TcpListener` public API

`TcpStream` enum `Inner { Dpdk(DpdkTcpStream), Std(std::net::TcpStream) }` with full `std::net::TcpStream` surface: `connect<A: ToSocketAddrs>` (v4 → DPDK, v6 → kernel fallback), `shutdown`, `peer_addr`, `local_addr`, `set_read_timeout`, `set_write_timeout`, `read_timeout`, `write_timeout`, `set_nodelay`, `nodelay`, `set_ttl`, `ttl`, `set_linger`, `linger`, `set_nonblocking`, `take_error`, `peek`, `try_clone` (Unsupported on DPDK arm). `impl Read for &TcpStream` / `impl Write for &TcpStream` (serialized via read_mutex/write_mutex). `TcpListener` enum with `bind`, `accept() -> (TcpStream, SocketAddr)`, `local_addr`, `set_ttl`, `incoming`. (~400 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `8.4`, `8.5`
- [ ] Complete · PR: —

---

### 29. `dpdk-stdlib-tcp`: Sync socket — options, split, and tests

Socket options via `EngineCommand::SetOption`: `set_nodelay`, `set_keepalive`, `set_linger`, `set_reuseaddr`, `set_recv_buffer_size`, `set_send_buffer_size`, `set_read_timeout`, `set_write_timeout`, `set_nonblocking`, `set_ttl`. `into_split`: set app_refcount = 2, create `OwnedReadHalf` (AsyncRead) + `OwnedWriteHalf` (AsyncWrite + Shutdown on drop). `mem::forget(self)` to avoid Drop→Close. Property tests: SPSC ring data integrity, `TcpError→io::Error` mapping. Loom/miri test: SPSC single-consumer invariant, read_mutex/write_mutex serialization. (~500 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `8.6`, `8.7`, `8.8`, `8.9`
- [ ] Complete · PR: —

---

### 30. `dpdk-stdlib-tcp`: Async compat layer

`dpdk-tokio/src/compat/tcp.rs` — async `TcpStream`: `AsyncRead` with register-first-then-recheck (register waker → try rx_ring.read → check eof/error → Poll::Pending), `AsyncWrite` with same pattern. `TcpStream::connect(addr).await` (DPDK-first, tokio-fallback for v6). Async `TcpListener` with `bind(addr).await` and `accept().await`. `OwnedReadHalf` (AsyncRead) + `OwnedWriteHalf` (AsyncWrite + shutdown-on-drop) for async split. Property test: `AtomicWaker` signaling under register-first-then-recheck (data → rx_ring + waker registered → waker called; no data between register and recheck without wake). (~350 LOC)

- Spec: `.kiro/specs/tcp-support/` · tasks `10.1`, `10.2`, `10.3`, `10.4`
- [ ] Complete · PR: —

---

### 31. `dpdk-stdlib-tcp`: DUT test binaries and synthetic benchmark

Three binary crates: `apps/tcp-echo` (sync echo server, `--ip`/`--port`, graceful shutdown), `apps/tcp-test-client` (modes: `handshake`/`bidir`/`shutdown`/`std-parity`), `apps/tokio-tcp-echo` (async echo via dpdk-tokio). `apps/tcp-synthetic-bench`: mock `PacketBackend`, measures connection establishment latency + single-stream throughput + engine tick time, outputs markdown (stdout) + JSON (stderr). Add `tcp-synthetic-perf` job to `integration-tests.yml`: run bench on PR, post markdown comment, upload artifact (30-day retention).

- Spec: `.kiro/specs/tcp-support/` · tasks `12.1`, `12.2`, `13.1`, `13.2`, `13.3`
- [ ] Complete · PR: —

---

### 32. `dpdk-stdlib-tcp`: TRex performance profile and benchmark runner

`scripts/perf-tests/tcp_echo_profile.py` (TRex TCP profile: connect, request-response, teardown; matches `udp_echo_profile.py` pattern). TCP benchmark runner covering 64/512/1400/65536 B payloads, P50/P90/P99 latency, CPS metrics. Structured JSON output schema (`test_name, backend, metric_name, metric_value, unit`). `plain-rust-tcp` DUT config using `std::net::TcpStream` for kernel comparison.

- Spec: `.kiro/specs/tcp-support/` · tasks `14.1`, `14.2`, `14.3`, `14.4`
- [ ] Complete · PR: —

---

### 33. `dpdk-stdlib-tcp`: Performance CI workflow

`.github/workflows/perf-tests-tcp.yml`: `workflow_dispatch` trigger with configurable inputs (payload sizes, duration, rate steps, DUT configs: `plain-rust-tcp`/`rust-dpdk-tcp`/`tokio-dpdk-tcp`). Deploy infrastructure → run TRex TCP traffic → collect results → post to PR comment (throughput, latency percentiles, CPS) → upload artifacts (90-day retention). Concurrency group `perf-tests-tcp` with safety-net teardown.

- Spec: `.kiro/specs/tcp-support/` · task `15.5`
- [ ] Complete · PR: —

---

### 34. `dpdk-stdlib-tcp`: EC2 tier-1 integration test scripts

Three scripts in `scripts/integration-tests/`: `tier1-tcp-handshake.sh` (DPDK↔DPDK three-way handshake), `tier1-tcp-echo.sh` (bidirectional data transfer), `tier1-tcp-shutdown.sh` (graceful FIN teardown). Each produces JUnit XML via `harness-common.sh`, 60-second timeout, targets `target/release/tcp-echo`.

- Spec: `.kiro/specs/tcp-support/` · task `15.1`
- [ ] Complete · PR: —

---

### 35. `dpdk-stdlib-tcp`: EC2 tier-2 and tier-3 integration test scripts

Tier-2: `tier2-tcp-retransmit.sh` (loss injection, verify retransmission + bounded recovery), `tier2-tcp-flow-control.sh` (zero-window probe, resume). Tier-3: `tier3-tcp-kernel-interop.sh` (ncat/iperf3 interop), `tier3-tcp-std-parity.sh` (byte-for-byte + ErrorKind comparison via `--mode std-parity`). All produce JUnit XML.

- Spec: `.kiro/specs/tcp-support/` · tasks `15.2`, `15.3`
- [ ] Complete · PR: —

---

### 36. `dpdk-stdlib-tcp`: EC2 CI jobs and remove continue-on-error gate

Add TCP integration test jobs to `integration-tests.yml` with `continue-on-error: true`. Post pass/fail/skip counts + log excerpts as PR comment. Upload JUnit XML artifacts (30-day retention). Use `dorny/test-reporter` for PR checks UI. Once ≥10/10 recent runs pass all MVP requirements, remove `continue-on-error: true` to make TCP test failures blocking CI.

- Spec: `.kiro/specs/tcp-support/` · tasks `15.4`, `16.1`
- [ ] Complete · PR: —

---

## Future Specs (Not Yet Written)

These require new kiro spec files before agents can pick them up.

- **TCP IPv6** — IPv6 address support in `dpdk-stdlib-tcp` (additive over IPv4 TCP; `SocketAddr` everywhere, v4/v6 codec pairs, NDP seam, MSS accounts for IPv6 header).
- **QUIC IPv6** — IPv6 address support in `dpdk-stdlib-quic` (additive over IPv4 QUIC).

---

## Completed

Items move here after merge.

*(none yet)*
