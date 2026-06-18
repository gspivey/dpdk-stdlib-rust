# Implementation Plan: TCP Support

## Overview

Build production-credible TCP support for dpdk-stdlib-rust, providing drop-in replacements for `std::net::TcpListener`, `std::net::TcpStream`, `tokio::net::TcpListener`, and `tokio::net::TcpStream`. Implementation proceeds bottom-up: shared crate extraction, pure codec, app↔engine contract types, stateful engine, sync socket API, async compat layer, test binaries, and CI infrastructure.

## Tasks

- [ ] 1. Extract dpdk-stdlib-net shared crate
  - [ ] 1.1 Create dpdk-stdlib-net crate and move PacketBackend trait
    - Create `dpdk-stdlib-net/Cargo.toml` and `dpdk-stdlib-net/src/lib.rs`
    - Move `PacketBackend` trait, `BackendConfig`, `BackendType`, and `create_backend()` factory from dpdk-udp
    - Move `DpdkBackend` (backend_dpdk.rs) and `RawSocketBackend` (backend_raw.rs) implementations
    - Move `ring_buffer.rs` (PACKET_MMAP ring buffer structures)
    - Move `ipv4_checksum()` and `pseudo_header_checksum()` helpers
    - Add `rx_readiness() -> RxReadiness` method to `PacketBackend` trait
    - Add `dpdk-stdlib-net` and `dpdk-stdlib-tcp` to workspace `Cargo.toml` members list
    - Note: commit split — 1.1a pure git-mv + re-export, 1.1b add rx_readiness()
    - _Requirements: 7.1, 7.2, 7.4, 7.7_

  - [ ] 1.2 Create NeighborResolver trait and ArpResolver
    - Define `NeighborResolver` trait in `dpdk-stdlib-net/src/neighbor.rs` with `resolve(ip: IpAddr) -> io::Result<[u8; 6]>` and `lookup_cached(ip: IpAddr) -> Option<[u8; 6]>`
    - Implement `ArpResolver` struct factored from dpdk-udp's ArpHandler/ArpCache + RoutingTable + gateway-MAC rule
    - Ensure ArpResolver implements `Send + Sync`
    - _Requirements: 4.18, 7.1_

  - [ ] 1.3 Update dpdk-udp to depend on dpdk-stdlib-net with re-exports
    - Add `dpdk-stdlib-net` as dependency of `dpdk-udp`
    - Re-export all moved items from `dpdk-udp/src/lib.rs` (`pub use dpdk_net::{PacketBackend, BackendConfig, BackendType, create_backend, ...}`)
    - Verify all existing dpdk-udp consumers compile without changes
    - _Requirements: 7.2_

  - [ ] 1.4 Verify crate extraction passes workspace build
    - Run `cargo build && cargo test` — all 133+ existing tests must still pass
    - _Requirements: 7.5, 7.7_

- [ ] 2. Checkpoint - Crate extraction complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 3. Implement TCP codec layer
  - [ ] 3.1 Create dpdk-stdlib-tcp crate skeleton
    - Create `dpdk-stdlib-tcp/Cargo.toml` with dependencies on `dpdk-stdlib-net` and `dpdk` (NOT dpdk-udp or tokio)
    - Create `dpdk-stdlib-tcp/src/lib.rs` with module declarations: `codec`, `engine`, `stream`, `listener`, `error`, `seq`, `ring`, `handle`, `contract`
    - Define constants: `MAX_TCP_PAYLOAD` (1460), `MAX_TCP_PAYLOAD_V6` (1440), `DEFAULT_PEER_MSS` (536)
    - Reserve public function names: `build_tcp6_frame`, `parse_tcp6_packet`, `tcp6_checksum`
    - _Requirements: 7.3, 7.7, 8.9, 8.12_

  - [ ] 3.2 Add CI gates for crate isolation
    - Add CI step in `rust.yml`: `cargo tree -p dpdk-stdlib-tcp -i dpdk-udp` must exit non-zero
    - Add CI step in `rust.yml`: `cargo tree -p dpdk-stdlib-tcp -i tokio` must exit non-zero
    - Verify workspace-wide `cargo build && cargo test` passes in rust.yml
    - _Requirements: 7.3, 1.3_

  - [ ] 3.3 Implement SpscByteRing
    - Create `dpdk-stdlib-tcp/src/ring.rs` with `SpscByteRing` struct (power-of-2 byte buffer, head/tail as AtomicUsize)
    - Implement `new(capacity)`, `write(&[u8]) -> usize`, `read(&mut [u8]) -> usize`, `available_read()`, `available_write()`, `is_empty()`
    - Use Acquire/Release ordering: producer store(Release) on head, consumer load(Acquire) on head
    - Round capacity up to next power of 2
    - _Requirements: 9.4, 9.5, 9.13, 9.14_

  - [ ] 3.4 Implement SeqNum with modular arithmetic
    - Create `dpdk-stdlib-tcp/src/seq.rs` with `SeqNum(pub u32)` — derive PartialEq, Eq, Hash, Clone, Copy; do NOT derive/implement Ord
    - Implement methods: `lt`, `le`, `gt`, `add`, `diff`, `in_range` using modulo-2³² serial-number arithmetic
    - _Requirements: 4.16, 12.5_

  - [ ] 3.5 Implement TcpError enum and io::Error mapping
    - Create `dpdk-stdlib-tcp/src/error.rs` with TcpError variants: ConnectionRefused, ConnectionReset, ConnectionAborted, BrokenPipe, NotConnected, TimedOut, AddrInUse, AddrNotAvailable, InvalidPacket(String), ResourceLimit(String)
    - Implement `From<TcpError> for std::io::Error` with specified ErrorKind mappings
    - Implement `Clone` and `thiserror::Error` derives
    - _Requirements: 7.6, 14.1, 14.2_

  - [ ] 3.6 Implement TCP frame types and options
    - Create `dpdk-stdlib-tcp/src/codec.rs` with `TcpFlags`, `TcpOptions` (MSS, WScale, SACK-Perm, Timestamps, SACK blocks, NOP, EOL)
    - Define `ParsedTcpSegment` struct (src/dst SocketAddr, seq, ack, flags, window, options, payload slice)
    - Define `TcpFrameParams` struct (all fields needed to build a frame)
    - _Requirements: 8.1, 8.2, 8.5, 8.6_

  - [ ] 3.7 Implement build_tcp_frame and tcp_checksum
    - Implement `build_tcp_frame(params: &TcpFrameParams) -> Result<Vec<u8>, TcpError>` constructing Eth + IPv4 + TCP
    - Factor TCP segment builder (header + options + payload + checksum) from IP-layer wrapper
    - Implement `tcp_checksum(src_ip: &[u8], dst_ip: &[u8], tcp_segment: &[u8]) -> u16` with parameterized pseudo-header
    - Implement `compute_mss(mtu: u16, ip_header_len: u16) -> u16`
    - Ensure SYN/SYN-ACK frames include MSS, WScale, SACK-Perm, Timestamps options
    - _Requirements: 8.1, 8.8, 8.9, 8.11, 4.10_

  - [ ] 3.8 Implement parse_tcp_packet
    - Implement `parse_tcp_packet(frame: &[u8]) -> Result<ParsedTcpSegment, TcpError>`
    - Validate minimum frame length (54 bytes: 14 Eth + 20 IPv4 + 20 TCP)
    - Validate data-offset >= 5 and data-offset*4 fits within frame
    - Parse all TCP options: MSS, Window Scale, SACK-Permitted, Timestamps, SACK blocks, NOP/EOL
    - _Requirements: 8.2, 8.5, 8.7_

  - [ ] 3.9 Implement build_tcp_packet (Mbuf path)
    - Implement `build_tcp_packet(mbuf: &mut Mbuf, params: &TcpFrameParams) -> Result<(), TcpError>` for zero-copy DPDK path
    - Must produce byte-identical frames to `build_tcp_frame`
    - _Requirements: 8.3_

  - [ ] 3.10 Write property tests for codec
    - **Property 1: Codec round-trip** — parse(build(params)) == params for arbitrary valid segments
    - **Property 2: Codec Mbuf equivalence** — build_tcp_frame and build_tcp_packet produce identical bytes
    - **Property 3: Invalid frame rejection** — frames < 54B or invalid data-offset return error
    - **Property 4: SYN required options** — SYN/SYN-ACK frames always contain MSS, WScale, SACK-Perm, Timestamps
    - **Property 5: MSS segment bound** — payload never exceeds min(local_mss, peer_mss)
    - **Property 6: TCP checksum validity** — compute then validate yields 0xFFFF; single-bit flip fails
    - **Property 7: Sequence arithmetic transitivity** — a < b < c ⟹ a < c; n < n+1 always holds
    - **Validates: Requirements 8.4, 8.1, 8.3, 8.7, 8.8, 8.10, 4.10, 4.11, 4.16, 1.4**

  - [ ] 3.11 Write property test for checksum round-trip after move
    - **Property 6: TCP checksum validity** (re-validates post-move)
    - Verify `ipv4_checksum` and `pseudo_header_checksum` produce identical results after move
    - **Validates: Requirements 4.10, 4.11**

- [ ] 4. Checkpoint - Codec complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 5. Implement app↔engine contract types and TCP engine
  - [ ] 5.1 Define ConnectionHandle and contract types
    - Create `dpdk-stdlib-tcp/src/contract.rs` (or `handle.rs`)
    - Define `ConnectionHandle` struct: rx_ring/tx_ring (SpscByteRing), state (AtomicU8), eof (AtomicBool), error (Mutex<Option<TcpError>>), condvar + notify_lock, read_waker/write_waker (AtomicWaker), read_mutex/write_mutex, app_refcount (AtomicUsize), cmd_tx (CommandSender), key (FourTuple), linger
    - Define `EngineCommand` enum: Connect { local, remote, src_mac, dst_mac, handle, response }, Listen, Accept, Shutdown, SetOption, Close
    - Define `SocketOption` enum: Nodelay, Keepalive, Linger, RecvBufSize, SendBufSize, ReuseAddr, Ttl, ReadTimeout, WriteTimeout, Nonblocking
    - Define `EngineWakeup` (eventfd on Linux, condvar for test/portable)
    - Define `CommandSender` wrapping `mpsc::Sender<EngineCommand>` + `Arc<EngineWakeup>` — every send() also signals engine_wakeup
    - Define `OneshotSender<T>` / `OneshotReceiver<T>` as `Arc<(Mutex<Option<T>>, Condvar)>` (no tokio dependency)
    - These are pure data/enum types with no dependency back on TcpEngine — no cycle
    - _Requirements: 5.1, 5.2, 9.1, 9.2, 9.3, 9.4, 9.13, 9.14_

  - [ ] 5.2 Implement TcpState enum and FourTuple
    - Create `dpdk-stdlib-tcp/src/engine.rs` (or `engine/mod.rs` with sub-modules)
    - Define `TcpState` enum with 11 states: Closed, Listen, SynSent, SynReceived, Established, FinWait1, FinWait2, CloseWait, Closing, LastAck, TimeWait
    - Define `FourTuple { local: SocketAddr, remote: SocketAddr }` with Hash, PartialEq, Eq
    - _Requirements: 4.1, 4.4, 4.17_

  - [ ] 5.3 Implement Clock trait and MockClock
    - Create `dpdk-stdlib-tcp/src/clock.rs` with `Clock` trait: `fn now(&self) -> Instant`
    - Implement `SystemClock` (delegates to `std::time::Instant::now()`)
    - Implement `MockClock` with `advance(duration)` and `set(instant)` for deterministic testing
    - _Requirements: 5.3, 1.1_

  - [ ] 5.4 Implement IsnGenerator
    - Create ISN generator using per-boot 128-bit secret (getrandom) + SipHash-2-4 of FourTuple + M (elapsed µs / 4 from clock)
    - Ensure ISNs are unpredictable per RFC 6528
    - _Requirements: 12.3, 4.3_

  - [ ] 5.5 Implement TimerWheel
    - Create `dpdk-stdlib-tcp/src/timer.rs` with hierarchical timer wheel (1ms granularity)
    - Support timer types: RTO, Persist, Keepalive, TimeWait, FinWait2, DelayedAck
    - Implement insert, cancel, and tick-advance operations
    - _Requirements: 5.1, 5.4_

  - [ ] 5.6 Implement CongestionState
    - Create `dpdk-stdlib-tcp/src/congestion.rs` with `CongestionState` struct
    - Implement `initial_window(mss)`: min(10*MSS, max(2*MSS, 14600))
    - Implement `update_rtt` with RFC 6298: α=1/8, β=1/4, Karn's algorithm, RTO clamped [1s, 60s]
    - Implement `on_ack`: slow-start (cwnd += MSS per ACK when cwnd < ssthresh) and congestion avoidance (cwnd += MSS*(MSS/cwnd))
    - Implement `on_triple_dup_ack`: ssthresh = max(flight/2, 2*MSS), cwnd = ssthresh + 3*MSS (fast retransmit)
    - Implement `on_partial_ack`: deflate cwnd by bytes acked, retransmit at new snd_una, stay in recovery
    - Implement `on_recovery_exit`: cwnd = ssthresh (deflate)
    - Implement `effective_window(rwnd) -> u32`: min(cwnd, rwnd)
    - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5, 6.6, 4.7_

  - [ ] 5.7 Implement Tcb structure
    - Create `Tcb` struct with: key, state, snd_una/nxt/wnd/wl1/wl2/iss, rcv_nxt/wnd/irs, snd_scale/rcv_scale, local_mss/peer_mss, congestion state, timer deadlines, retransmit_count
    - Include engine-internal buffers: `send_buf: VecDeque<u8>`, `retransmit_queue: Vec<RetransmitEntry>`, `reorder_buffer: BTreeMap<u32, Vec<u8>>` (key = seq.diff(rcv_nxt))
    - Include Nagle state, socket options, src_mac/dst_mac fields, `handle: Arc<ConnectionHandle>`
    - _Requirements: 4.4, 4.16_

  - [ ] 5.8 Implement TcpEngine::on_segment — Handshake + state transitions
    - SYN → SYN_RECEIVED: respond with SYN-ACK including options, transition state
    - SYN-ACK → ESTABLISHED: respond with ACK, transition state, wake connect oneshot
    - Connection-refused RST: latch ConnectionRefused to handle when RST received in SYN_SENT
    - Populate Tcb.src_mac/dst_mac from incoming parsed frame (accept-side MACs)
    - ~450 LOC
    - **Property 8: State machine validity**
    - _Requirements: 4.1, 4.2, 4.3, 11.1, 11.2, 11.3, 11.4, 11.5_

  - [ ] 5.9 Implement TcpEngine::on_segment — Established in-order data + ACK
    - In-order data delivery: ACK with ack_num = rcv_nxt + payload_len, push to rx_ring
    - Cumulative ACK processing: advance snd_una, free retransmit entries
    - Apply window-scale to peer's advertised window
    - Wake Condvar and read_waker after rx_ring push
    - ~400 LOC
    - **Properties 9, 23: In-order ACK correctness, Window scaling round-trip**
    - _Requirements: 4.5, 4.8, 11.3, 11.4_

  - [ ] 5.10 Implement TcpEngine::on_segment — OOO reorder buffer + dup-ACK
    - Out-of-order handling: buffer in reorder_buffer (BTreeMap<u32, Vec<u8>>) keyed on seq.diff(rcv_nxt)
    - Send dup-ACK with ack_num == rcv_nxt on OOO segment
    - Deliver contiguous data from reorder_buffer to rx_ring when gaps fill
    - ~350 LOC
    - **Properties 10, 21: OOO dup-ACK, Reorder buffer soundness**
    - _Requirements: 4.6, 4.16_

  - [ ] 5.11 Implement TcpEngine::on_segment — FIN + close states
    - FIN_WAIT_1, FIN_WAIT_2, CLOSE_WAIT, LAST_ACK transitions
    - Simultaneous close (both sides sending FIN) → CLOSING state
    - TIME_WAIT entry: hold TCB for 2*MSL (120s)
    - Set eof flag on ConnectionHandle after final bytes enqueued on FIN receipt
    - ~350 LOC
    - **Property 12: TIME_WAIT/FIN_WAIT_2 cleanup**
    - _Requirements: 4.13, 4.14, 4.15_

  - [ ] 5.12 Implement TcpEngine::on_segment — RST validation per RFC 5961
    - Exact seq → abort connection, latch ConnectionReset
    - In-window non-exact → send challenge ACK
    - Out-of-window → silently drop
    - ~200 LOC
    - **Property 14: RST validation per RFC 5961**
    - _Requirements: 4.12, 12.4_

  - [ ] 5.13 Implement TcpEngine::on_segment — Nagle + delayed-ACK + SWS avoidance
    - Nagle: if unacked data AND write < MSS, buffer; send immediately if nodelay || no unacked || fills MSS
    - Delayed-ACK: coalesce ACKs up to 200ms OR every-other-segment; immediate for OOO
    - SWS avoidance: withhold window update until >= min(MSS, half buffer)
    - ~350 LOC
    - **Property 9 (coalescing aspect)**
    - _Requirements: 13.4_

  - [x] 5.14 Implement TcpEngine::on_tick — tx-drain + segmentation + wakes
    - Drain tx_rings → send_buf → segment and transmit (respecting effective_window)
    - Wake Condvar and write_waker after send window opens
    - This lands first — most tests need the tx path
    - _Requirements: 5.1, 5.2, 5.4_

  - [x] 5.15 Implement TcpEngine::on_tick — RTO + backoff + max-retries
    - Retransmit oldest unacked segment on RTO expiry
    - Double RTO (exponential backoff) on each retransmit
    - Abort after max retries → latch TimedOut
    - _Requirements: 4.7, 14.6_

  - [ ] 5.16 Implement TcpEngine::on_tick — Persist + keepalive
    - Persist timer: send 1-byte window probe at exponentially backed-off intervals (capped 60s), NEVER abort
    - Keepalive: send probe after idle timeout, abort after max probes
    - _Requirements: 4.9, 13.3_

  - [ ] 5.17 Implement TcpEngine::on_tick — TIME_WAIT/FIN_WAIT_2 + delayed-ACK fire
    - TIME_WAIT: transition to CLOSED after 2*MSL, free TCB
    - FIN_WAIT_2 timeout: clean up TCB after timeout
    - Delayed-ACK timer fire: send cumulative ACK at 200ms deadline
    - _Requirements: 4.14, 4.15, 13.4_

  - [ ] 5.18 Implement TcpEngine::on_command — Control-plane
    - Connect: allocate TCB, populate src_mac/dst_mac from command, send SYN with options, transition to SYN_SENT, arm RTO
    - Listen: register listener in listen_map with bounded accept queue (default 128)
    - Accept: dequeue from accept queue via oneshot, park if empty
    - Enforce max TCBs limit: reject new connections with RST when at capacity
    - Enforce accept backlog limit: reject new SYNs with RST when queue full
    - _Requirements: 4.2, 4.3, 5.6, 9.1, 9.2, 9.3, 12.1, 12.2_

  - [ ] 5.19 Implement TcpEngine::on_command — Teardown + options
    - Shutdown: set fin_pending flag, drain tx_ring → send_buf → FIN (flush-before-FIN)
    - Close: honor SO_LINGER (timeout=0 → RST, timeout>0 → block/timeout), initiate FIN/TIME_WAIT cleanup
    - SetOption: update Tcb fields (nodelay, keepalive, linger, ttl, etc.)
    - _Requirements: 9.6, 13.1, 13.2, 13.3, 13.4, 13.5_

- [ ] 6. Checkpoint - Engine established-connection echo (intermediate)
  - Established-connection echo works end-to-end via mock backend (engine + sync read/write + SpscByteRing + CommandSender)
  - Validates data path before remaining protocol complexity lands
  - Ensure all tests pass, ask the user if questions arise.

  - [x] 6.1 Write property tests for engine state machine
    - **Property 8: State machine validity** — any sequence of valid events produces one of 11 TcpState values
    - **Property 9: In-order ACK correctness** — in-order segments produce ACK with correct ack_num
    - **Property 10: Out-of-order dup-ACK** — OOO segments produce dup-ACK with ack_num == rcv_nxt
    - **Property 11: Timer-driven segment generation** — expired timer produces outbound segment without app call
    - **Property 12: TIME_WAIT/FIN_WAIT_2 cleanup** — TCB transitions to CLOSED after timeout
    - **Property 13: Resource limit enforcement** — max_tcbs/accept_backlog exceeded → RST
    - **Property 14: RST validation per RFC 5961** — exact seq aborts, in-window challenges, out-of-window drops
    - **Validates: Requirements 4.1, 4.5, 4.6, 5.4, 4.14, 4.15, 5.6, 12.1, 12.2, 4.12, 12.4**

  - [x] 6.2 Write property tests for congestion control
    - **Property 15: Flight-size invariant** — unacked bytes never exceed min(cwnd, rwnd)
    - **Property 16: Slow-start cwnd growth** — each ACK in slow-start increases cwnd by MSS
    - **Property 17: Initial window formula** — IW = min(10*MSS, max(2*MSS, 14600))
    - **Property 18: Fast retransmit formula** — on 3 dup-ACKs: ssthresh = max(flight/2, 2*MSS), cwnd = ssthresh + 3*MSS
    - **Property 22: Partial-ACK in recovery** — partial ACK deflates cwnd, retransmits, stays in recovery
    - **Property 25: Persist never aborts** — zero-window probes indefinitely without TimedOut
    - **Validates: Requirements 6.4, 6.1, 6.3, 6.5, 4.9**

  - [x] 6.3 Write property tests for reorder buffer and window scaling
    - **Property 21: Reorder buffer soundness** — OOO segments including wrap-around produce byte-identical output to in-order assembly
    - **Property 23: Window scaling encoding round-trip** — encode then decode correctly bounds effective send window
    - **Validates: Requirements 4.5, 4.16, 11.3, 11.4**

- [ ] 7. Checkpoint - Engine fully complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 8. Implement sync socket API
  - [ ] 8.1 Wire ConnectionHandle into the socket layer (connect path)
    - Implement `connect_v4`: resolve dst_mac via `NEIGHBOR_RESOLVER.resolve()`, get src_mac via `backend.mac_address()`, create ConnectionHandle, send EngineCommand::Connect with src_mac/dst_mac, park on oneshot response
    - Implement `connect_timeout`: same but use condvar.wait_timeout, abort on expiry
    - _Requirements: 9.1, 9.10, 4.18_

  - [ ] 8.2 Wire ConnectionHandle into the socket layer (engine_loop integration)
    - Implement `engine_loop(backend, engine, cmd_rx, wakeup)` — select on rx_readiness | engine_wakeup | timer_deadline
    - Handle RxReadiness variants: Fd (epoll), PollOnly (busy-poll), Condvar (test)
    - Process inbound frames: `parse_tcp_packet` → `engine.on_segment`
    - Process commands: `cmd_rx.try_recv()` → `engine.on_command`
    - Service timers: `engine.on_tick(clock.now())`
    - Send all outbound frames via `backend.send_frame`
    - _Requirements: 5.1, 5.2, 5.3, 5.4_

  - [ ] 8.3 Implement DpdkTcpStream (blocking read/write with timeout/nonblocking)
    - Implement `io::Read` for DpdkTcpStream with P0-B recheck-under-lock pattern: check error → try rx_ring.read → check eof → lock notify_lock → recheck → condvar.wait
    - Implement `io::Write` for DpdkTcpStream: push to tx_ring, signal engine_wakeup, block if full
    - Honor `set_nonblocking`: if nonblocking, return `io::ErrorKind::WouldBlock` instead of parking on condvar
    - Honor `read_timeout`/`write_timeout`: use `condvar.wait_timeout`, return `io::ErrorKind::TimedOut` on expiry
    - Implement `impl Read for &TcpStream` and `impl Write for &TcpStream` (serialized via read_mutex/write_mutex)
    - Implement `Drop` for DpdkTcpStream: decrement app_refcount, send Close on last handle
    - _Requirements: 9.4, 9.5, 9.8, 9.11, 9.13, 9.14_

  - [x] 8.4 Implement TcpStream public API
    - Implement `TcpStream` with enum `Inner { Dpdk(DpdkTcpStream), Std(std::net::TcpStream) }`
    - Implement `connect<A: ToSocketAddrs>` with v4/v6 dispatch (v4 → DPDK, v6 → kernel fallback)
    - Implement `shutdown(how: Shutdown)`, `peer_addr()`, `local_addr()`
    - Implement `set_read_timeout`, `set_write_timeout`, `read_timeout`, `write_timeout`
    - Implement `set_nodelay`, `nodelay`, `set_ttl`, `ttl`
    - Implement `set_linger`, `linger`, `set_nonblocking`, `take_error`
    - Implement `try_clone()` — Unsupported on DPDK arm, delegates on Std
    - Implement `peek(buf)` — non-destructive ring read
    - _Requirements: 9.1, 9.4, 9.5, 9.6, 9.7, 9.8, 9.10, 9.11, 9.12_

  - [x] 8.5 Implement TcpListener public API
    - Implement `TcpListener` with enum `Inner { Dpdk(DpdkTcpListener), Std(std::net::TcpListener) }`
    - Implement `bind<A: ToSocketAddrs>` with v4/v6 dispatch
    - Implement `accept() -> io::Result<(TcpStream, SocketAddr)>` — via oneshot to engine
    - Implement `local_addr()`, `set_ttl`, `ttl`, `incoming() -> Incoming`
    - _Requirements: 9.2, 9.3, 9.9_

  - [ ] 8.6 Implement socket options via EngineCommand
    - Implement `set_nodelay`, `set_keepalive`, `set_linger`, `set_reuseaddr`, `set_recv_buffer_size`, `set_send_buffer_size`, `set_read_timeout`, `set_write_timeout`, `set_nonblocking`, `set_ttl`
    - Route each through `EngineCommand::SetOption` to update Tcb fields
    - _Requirements: 13.1, 13.2, 13.3, 13.4, 13.5_

  - [ ] 8.7 Implement split/into_split
    - Implement `OwnedReadHalf` and `OwnedWriteHalf` structs
    - `into_split`: set app_refcount = 2, create halves, mem::forget(self) to avoid Drop→Close
    - `OwnedWriteHalf::drop`: send Shutdown(Write), decrement app_refcount, Close on last
    - `OwnedReadHalf::drop`: decrement app_refcount, Close on last
    - _Requirements: 9.4, 9.5_

  - [ ] 8.8 Write property tests for SPSC ring and TcpError mapping
    - **Property 24: SPSC ring data integrity** — push/pop sequence preserved exactly, no reordering/duplication/loss
    - **Property 19: TcpError to io::Error mapping** — every variant maps to specified io::ErrorKind
    - **Validates: Requirements 9.4, 9.5, 14.2**

  - [ ] 8.9 Write loom/miri test for SPSC single-consumer invariant
    - Verify SpscByteRing is strictly single-producer/single-consumer under concurrent access
    - Verify read_mutex/write_mutex serialize concurrent (&stream).read/write calls
    - _Requirements: 9.13, 9.14_

- [ ] 9. Checkpoint - Sync API complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 10. Implement async compat layer
  - [ ] 10.1 Implement async TcpStream (AsyncRead/AsyncWrite)
    - Create `dpdk-tokio/src/compat/tcp.rs` with `TcpStream` struct (enum Inner { Dpdk, Tokio })
    - Implement `AsyncRead` with register-first-then-recheck pattern: register waker → try rx_ring.read → check eof/error → Poll::Pending
    - Implement `AsyncWrite` with same pattern for tx_ring
    - Implement `TcpStream::connect(addr).await` — DPDK first, tokio fallback for v6
    - _Requirements: 10.1, 10.5, 10.6, 10.8_

  - [ ] 10.2 Implement async TcpListener
    - Create `dpdk-tokio/src/compat/tcp_listener.rs` with async `TcpListener`
    - Implement `bind(addr).await` — DPDK first, tokio fallback
    - Implement `accept().await -> io::Result<(TcpStream, SocketAddr)>`
    - _Requirements: 10.2, 10.3, 10.4_

  - [ ] 10.3 Implement split/into_split for async TcpStream
    - Implement `OwnedReadHalf` (AsyncRead) and `OwnedWriteHalf` (AsyncWrite + shutdown-on-drop)
    - Implement IPv6 fallback dispatch (enum to tokio::net types)
    - _Requirements: 10.1, 10.7_

  - [ ] 10.4 Write property test for AtomicWaker signaling
    - **Property 20: AtomicWaker signaling with register-first-then-recheck**
    - Verify: when engine delivers data to rx_ring AND read_waker is registered, waker is called; no data arrives between register and recheck without a wake
    - **Validates: Requirements 5.5, 10.6**

- [ ] 11. Checkpoint - Async API complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 12. Implement CI scaffolding and synthetic benchmark
  - [ ] 12.1 Create tcp-synthetic-bench binary crate
    - Create `apps/tcp-synthetic-bench/Cargo.toml` and `src/main.rs`
    - Implement mock PacketBackend for benchmarking without real NIC
    - Measure: connection establishment latency, single-stream throughput with mock backend, engine tick processing time
    - Output markdown on stdout, JSON on stderr
    - _Requirements: 15.2, 15.4_

  - [ ] 12.2 Add tcp-synthetic-perf job and non-blocking integration scaffolding
    - Add `tcp-synthetic-perf` job to integration-tests.yml triggered on pull requests to main and development
    - Run tcp-synthetic-bench binary, post results as markdown PR comment with commit hash and run link
    - Upload results as GitHub Actions artifact with 30-day retention
    - Add scaffolding for non-blocking TCP integration test jobs (structure, scripts, JUnit placeholders)
    - _Requirements: 15.1, 15.3, 15.5_

- [ ] 13. Create TCP DUT binaries for EC2 tests
  - [ ] 13.1 Create apps/tcp-echo binary
    - Create `apps/tcp-echo/Cargo.toml` and `src/main.rs`
    - Sync TCP echo server with `--ip` and `--port` CLI flags matching `apps/echo` pattern
    - Accept connections, echo received data back, handle graceful shutdown
    - Add to workspace `Cargo.toml`
    - _Requirements: 2.1, 2.2, 2.3_

  - [ ] 13.2 Create apps/tcp-test-client binary
    - Create `apps/tcp-test-client/Cargo.toml` and `src/main.rs`
    - TCP client with modes: `--mode handshake` (connect-then-close), `--mode bidir` (bidirectional transfer), `--mode shutdown` (graceful FIN), `--mode std-parity` (compare vs std::net::TcpStream)
    - Add to workspace `Cargo.toml`
    - _Requirements: 2.1, 2.2, 2.3, 2.9_

  - [ ] 13.3 Create apps/tokio-tcp-echo binary
    - Create `apps/tokio-tcp-echo/Cargo.toml` and `src/main.rs`
    - Async TCP echo server using dpdk-tokio compat layer
    - Add to workspace `Cargo.toml`
    - _Requirements: 10.1, 10.4_

- [ ] 14. Implement TCP performance measurement (R3)
  - [ ] 14.1 Create tcp_echo_profile.py (TRex TCP profile)
    - Create `scripts/perf-tests/tcp_echo_profile.py` matching existing `udp_echo_profile.py` pattern
    - TCP profile for TRex: connection setup, request-response echo, teardown
    - _Requirements: 3.1, 3.2, 3.3, 17.3_

  - [ ] 14.2 Create TCP run_benchmark variant
    - Implement TCP benchmark runner covering four payload sizes: 64, 512, 1400, 65536 bytes
    - Collect P50/P90/P99 latency percentiles and connection-rate (CPS) metrics
    - _Requirements: 3.1, 3.2, 3.3_

  - [ ] 14.3 Define R3.5 JSON schema for TCP perf output
    - Define structured JSON output format: test_name, backend, metric_name, metric_value, unit
    - Ensure format is consistent with UDP perf output for tooling compatibility
    - _Requirements: 3.5_

  - [ ] 14.4 Implement vs-plain-rust-tcp (kernel) comparison
    - Add DUT configuration `plain-rust-tcp` using `std::net::TcpStream` (kernel path)
    - Measure same workloads as DPDK TCP, produce side-by-side comparison output
    - _Requirements: 3.4_

- [ ] 15. Create TCP integration test harness scripts
  - [ ] 15.1 Create tier1 TCP integration test scripts
    - Create `scripts/integration-tests/tier1-tcp-handshake.sh` (three-way handshake DPDK↔DPDK, targets `target/release/tcp-echo`)
    - Create `scripts/integration-tests/tier1-tcp-echo.sh` (bidirectional data transfer)
    - Create `scripts/integration-tests/tier1-tcp-shutdown.sh` (graceful FIN teardown)
    - All scripts produce JUnit XML using harness-common.sh, 60-second timeout per test
    - _Requirements: 2.1, 2.2, 2.3, 16.4_

  - [ ] 15.2 Create tier2 TCP integration test scripts
    - Create `scripts/integration-tests/tier2-tcp-retransmit.sh` (loss-injection, verify retransmission + bounded recovery)
    - Create `scripts/integration-tests/tier2-tcp-flow-control.sh` (zero-window, persist probe, resume)
    - _Requirements: 2.5, 2.6_

  - [ ] 15.3 Create tier3 TCP integration test scripts
    - Create `scripts/integration-tests/tier3-tcp-kernel-interop.sh` (ncat/iperf3 interop)
    - Create `scripts/integration-tests/tier3-tcp-std-parity.sh` (byte-for-byte + ErrorKind comparison using tcp-test-client --mode std-parity)
    - _Requirements: 2.4, 2.7, 2.9_

  - [ ] 15.4 Add TCP integration test jobs to CI workflow
    - Add TCP integration test jobs to `integration-tests.yml` with `continue-on-error: true`
    - Post test results and logs to PR comment (pass/fail/skip counts, app logs, network state)
    - Upload JUnit XML results as artifacts with 30-day retention
    - Use `dorny/test-reporter` for PR checks UI
    - _Requirements: 16.1, 16.2, 16.3, 16.4, 16.5, 16.6_

  - [ ] 15.5 Create TCP performance test workflow (TRex, manual trigger)
    - Create `.github/workflows/perf-tests-tcp.yml` with `workflow_dispatch` trigger
    - Accept configurable inputs: payload sizes, test duration, rate steps, DUT configurations (plain-rust-tcp, rust-dpdk-stdlib-tcp, tokio-dpdk-stdlib-tcp)
    - Deploy infrastructure, run TRex TCP traffic, collect results
    - Post results as PR comment (throughput, latency percentiles, connection rate)
    - Upload results as artifacts with 90-day retention
    - Use concurrency group `perf-tests-tcp` with safety-net teardown
    - _Requirements: 17.1, 17.2, 17.3, 17.4, 17.5, 17.6, 17.7_

- [ ] 16. Checkpoint - CI and test infrastructure complete
  - Ensure all tests pass, ask the user if questions arise.

  - [ ] 16.1 Remove continue-on-error gating flip
    - Remove `continue-on-error: true` from TCP integration test jobs once ≥10/10 recent runs pass and all MVP requirements are marked Implemented in tasks.md
    - TCP test failures become blocking CI
    - _Requirements: 16.7_

- [ ] 17. Final checkpoint - All tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Property tests are non-optional — they validate P0 bug fixes and R1.4 SHALL requirements. No tasks currently carry the `*` (optional) marker.
- Redefinition: `*` means "extra edge-case coverage beyond R1.4 required tests" — currently no tasks have this marker.
- Each task references specific requirements for traceability.
- Checkpoints ensure incremental validation between phases.
- Property tests validate universal correctness properties from the design document.
- Unit tests validate specific examples and edge cases.
- The implementation language is Rust (matching the design document).
- All code must compile and test without DPDK installed (stub system).
- CI gates enforce dpdk-stdlib-tcp never depends on dpdk-udp or tokio.
- Task 5.1 (contract types) defines pure data/enum types with no dependency back on TcpEngine — this breaks the original dependency inversion between socket API and engine.
- The on_segment split (5.8–5.13) ordering: 5.8 → 5.9 → {5.10, 5.11, 5.12, 5.13} parallel.
- The on_tick split (5.14–5.17) ordering: 5.14 first (most tests need tx path), then 5.15–5.17 parallel.
- Intermediate checkpoint (task 6) validates established-connection echo before remaining protocol complexity.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["1.2", "1.3"] },
    { "id": 2, "tasks": ["1.4"] },
    { "id": 3, "tasks": ["3.1"] },
    { "id": 4, "tasks": ["3.2", "3.3", "3.4", "3.5"] },
    { "id": 5, "tasks": ["3.6"] },
    { "id": 6, "tasks": ["3.7", "3.8"] },
    { "id": 7, "tasks": ["3.9", "3.10", "3.11"] },
    { "id": 8, "tasks": ["5.1", "5.2", "5.3"] },
    { "id": 9, "tasks": ["5.4", "5.5", "5.6"] },
    { "id": 10, "tasks": ["5.7"] },
    { "id": 11, "tasks": ["5.8", "5.14"] },
    { "id": 12, "tasks": ["5.9", "5.18"] },
    { "id": 13, "tasks": ["5.10", "5.11", "5.12", "5.13", "5.15", "5.16", "5.17", "5.19"] },
    { "id": 14, "tasks": ["6.1", "6.2", "6.3"] },
    { "id": 15, "tasks": ["8.1", "8.2"] },
    { "id": 16, "tasks": ["8.3", "8.4", "8.5"] },
    { "id": 17, "tasks": ["8.6", "8.7"] },
    { "id": 18, "tasks": ["8.8", "8.9"] },
    { "id": 19, "tasks": ["10.1", "10.2"] },
    { "id": 20, "tasks": ["10.3", "10.4"] },
    { "id": 21, "tasks": ["12.1", "13.1", "13.2", "13.3"] },
    { "id": 22, "tasks": ["12.2", "14.1", "14.2", "14.3"] },
    { "id": 23, "tasks": ["14.4", "15.1", "15.2", "15.3"] },
    { "id": 24, "tasks": ["15.4", "15.5"] },
    { "id": 25, "tasks": ["16.1"] }
  ]
}
```
