# Requirements Document

## Introduction

Multi-core scaling and shared-memory IPC for dpdk-stdlib-rust. Today, `UdpSocket` runs single-threaded on one DPDK lcore. This spec extends the library to use multiple cores — via RSS (Receive Side Scaling) hardware flow distribution and software pipeline stages — while preserving the existing `UdpSocket::bind()` / `recv_from()` / `send_to()` API surface unchanged. A secondary goal adds a shared-memory backend so multiple independent processes can share a single DPDK NIC without any application code changes.

The core value proposition of this project is **hiding DPDK complexity behind well-known Rust interfaces** (`std::net::UdpSocket`, `tokio::net::UdpSocket`). Every feature in this spec must maintain that contract: the user writes `UdpSocket::bind(addr)` and gets the fastest available path, with multi-core topology, shared memory IPC, and NUMA awareness handled internally.

## Glossary

- **RSS**: Receive Side Scaling — NIC hardware feature that hashes incoming packets across multiple RX queues based on flow tuples (src/dst IP + port)
- **Lcore**: DPDK logical core — a thread pinned to a specific CPU core via EAL
- **Pipeline**: A processing model where RX polling and application-level work run on separate cores, connected by lock-free rings
- **Run-to-Completion**: A processing model where a single core does RX poll, protocol handling, and application delivery (current model)
- **SPSC Ring**: Single-Producer Single-Consumer lock-free ring buffer (one writer, one reader, no atomics on the fast path beyond load/store)
- **MPSC Ring**: Multi-Producer Single-Consumer ring buffer (multiple writers, one reader)
- **Shared Memory Backend**: A `PacketBackend` implementation that communicates with a DPDK daemon process via hugepage-backed shared memory rings
- **Daemon**: A long-running process that owns the DPDK NIC and multiplexes packets to/from application processes over shared memory
- **NUMA Node**: Non-Uniform Memory Access node — a CPU socket and its local memory; cross-NUMA memory access is slower

## Requirements

### Requirement 1: Unified RSS + Pipeline Model

**User Story:** As a developer, I want `UdpSocket` to automatically use multiple CPU cores for packet processing, so that my application scales without code changes.

#### Acceptance Criteria

1. WHEN `UdpSocket::bind()` is called, the library SHALL detect available lcores and NIC queue capabilities, and configure an appropriate multi-core topology automatically
2. WHEN multiple RSS queues are configured, each queue SHALL be serviced by a dedicated RX lcore that polls the NIC and forwards frames to worker cores via SPSC rings
3. WHEN only a single RSS queue is active (single-flow workload), the library SHALL pipeline to N-1 available worker cores for CPU-bound processing
4. WHEN `recv_from()` is called, it SHALL dequeue the next processed packet from any worker core transparently — the caller sees a single stream of packets
5. WHEN `send_to()` is called, the frame SHALL be routed to the appropriate TX path (back through the originating RX core) without the caller specifying which core to use
6. The existing `UdpSocket::bind()`, `recv_from()`, `send_to()`, and all 19 `std::net::UdpSocket` API methods SHALL continue to work with identical signatures and semantics
7. The existing `tokio::net::UdpSocket` async compat layer SHALL continue to work without modification

### Requirement 2: Builder-Based Configuration

**User Story:** As a developer who needs control over core allocation, I want an optional builder API to specify queue counts and worker topology, without affecting the default auto-detect path.

#### Acceptance Criteria

1. A `UdpSocket::builder()` method SHALL return a builder that accepts `.rx_queues(n)` and `.workers_per_queue(n)` configuration
2. WHEN no builder is used (plain `UdpSocket::bind()`), the library SHALL auto-detect a sensible topology based on available cores, NUMA layout, and NIC capabilities
3. WHEN environment variables `DPDK_RX_QUEUES` and `DPDK_WORKERS_PER_QUEUE` are set, they SHALL override auto-detection but be overridden by explicit builder calls
4. Configuration precedence SHALL be: builder API > environment variables > auto-detection

### Requirement 3: Auto-Scaling by Instance Size

**User Story:** As a DevOps engineer, I want the library to automatically choose the right core topology for my instance size, so I don't need per-instance tuning.

#### Acceptance Criteria

1. On a 2-vCPU instance, the library SHALL default to run-to-completion on a single core (no pipeline overhead)
2. On a 4-vCPU instance, the library SHALL default to 1-2 RSS queues with 1 worker each
3. On a 16+ vCPU instance, the library SHALL default to N/2 RSS queues each with a dedicated worker (or configurable pipeline depth)
4. The library SHALL respect NUMA boundaries — RX cores and their paired workers SHOULD be on the same NUMA node
5. The library SHALL query the NIC's maximum supported RX queue count and never exceed it

### Requirement 4: Shared Memory Multi-Process Backend

**User Story:** As a developer running multiple services on one host, I want each service to use `UdpSocket::bind()` and share the same DPDK NIC, without requiring DPDK multi-process support or code changes.

#### Acceptance Criteria

1. A daemon process (`dpdk_udp::serve()`) SHALL own the DPDK NIC and run the RX/TX poll loops
2. Application processes SHALL connect to the daemon automatically when `UdpSocket::bind()` detects it is not the DPDK primary process
3. Communication between daemon and application processes SHALL use hugepage-backed shared memory ring buffers (SPSC per direction per application)
4. The daemon SHALL classify incoming packets by destination port and route them to the appropriate application's RX ring
5. Application TX frames SHALL be enqueued to a TX ring that the daemon drains and transmits via the NIC
6. `ShmBackend` SHALL implement the existing `PacketBackend` trait — no changes to the trait are required
7. ARP, ICMP, and all protocol handlers SHALL work identically over the shared memory backend (they operate on `&[u8]` slices)

### Requirement 5: Transparent Backend Selection

**User Story:** As a developer, I want `UdpSocket::bind()` to automatically pick the best available backend without any configuration.

#### Acceptance Criteria

1. `bind()` SHALL use this selection order: (a) direct DPDK if running as DPDK primary process, (b) shared memory if a daemon is detected, (c) AF_PACKET raw socket fallback
2. The selection SHALL be invisible to the caller — `recv_from()` and `send_to()` behave identically regardless of backend
3. The backend name SHALL be queryable via the existing `backend_name()` method (returns `"dpdk"`, `"shared-memory"`, or `"raw-socket"`)

### Requirement 6: Cross-Language Shared Memory Compatibility

**User Story:** As a team with polyglot services, I want non-Rust processes to consume the shared memory rings, so the DPDK daemon can serve any language.

#### Acceptance Criteria

1. The shared memory ring layout SHALL be a simple, documented binary format: atomic head/tail pointers + fixed-size slots with length-prefixed frames
2. The ring protocol SHALL require only `mmap`, atomic load/store, and memory fences — no Rust-specific types or serialization
3. A C header file describing the ring layout SHALL be provided for cross-language consumers

### Requirement 7: Zero Regression on Existing Tests

**User Story:** As a maintainer, I want all 133+ existing tests to pass unchanged after this feature lands.

#### Acceptance Criteria

1. All existing tests SHALL pass without modification (they use stubs, which are single-threaded — multi-core features are inactive under stubs)
2. New tests for multi-core and shared memory SHALL use the stub system and be runnable without DPDK installed
3. The stub backend SHALL continue to work as a single-threaded, single-queue backend — multi-core topology configuration is silently ignored when stubs are active
