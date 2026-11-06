# Requirements: tokio-udp-perf (0.3.1)

## Introduction

The sandbox experiments in `context/sandbox-results.md` established that the long-standing
tokio-dpdk UDP throughput ceiling (~305k pps at 64 B, flat for five months in
`docs/perf-test-log.md`) is **not** a library limit. It is caused by the benchmark app
`apps/tokio-echo`, which spawns one task per packet on the multi-thread runtime. When the
same library does a fair echo loop (`recv_from` then `send_to` of the same bytes in one
task) it reaches 100 % of sync rust-dpdk (698,737 / 698,746 / 475,953 pps median at
64 / 512 / 1400 B) on one core with 0 % kernel time (E1).

The same experiments surfaced a **soundness bug**: `dpdk_udp::UdpSocket` is `Send + Sync`
and invites cross-thread sharing (matching `std::net::UdpSocket` semantics), but its
`TxBuffer` is an `UnsafeCell<Vec<u8>>` with `unsafe impl Sync` that is borrowed on every
send, and the per-queue DPDK `rx_burst`/`tx_burst` calls take no lock. `context/race_check.rs`
trips the in-use assertion in 3 of 4 threads; release builds have no check, so it is a data
race. Today the only thing serializing access for tokio users is the outer
`Arc<std::sync::Mutex<dpdk_udp::UdpSocket>>` in `dpdk-stdlib-tokio`.

Finally, the measurement harness (`scripts/run-perf-tests.sh`, `.github/workflows/perf-tests.yml`,
`docs/perf-test-log.md`) has gaps that let the flat ceiling be written up as "No Regression"
33 times: no absolute targets, run-to-run comparison only, zeroed latency and tokio-mode
counters in the summary.

This spec ships as **0.3.1**, a patch release. Every published crate must stay
semver-compatible with 0.3.0. The scope is five items: fix the benchmark app, add perf gates
and fix the log/counters, make `dpdk_udp::UdpSocket` internally thread-safe per direction,
drop the tokio outer mutex, and land lower-priority CPU-efficiency items that must not reduce
throughput.

### Terminology
- **Fair mode / fair loop**: a single task doing `recv_from` then `send_to` of the same bytes
  (the pattern `apps/echo` uses). Becomes the `tokio-echo` default and the `tokio-dpdk` config.
- **Concurrent mode**: the E2 **"spawn"** per-packet behaviour (`buf[..len].to_vec()` +
  `Semaphore` permit + `tokio::spawn` + `tokio::time::timeout`) that echoes the **exact received
  bytes** (no `format!`). Preserved behind `--mode concurrent`, measured as the
  `tokio-dpdk-concurrent` config.
- **TX-side state / RX-side state**: the per-direction mutable state inside `dpdk_udp::UdpSocket`.
  TX = the `TxBuffer` scratch and `tx_burst`/`send_frame` on a queue. RX = `rx_burst`, the
  `recv_queue` refill, and the single-consumer app-ring dequeue.
- **700k step**: the 700,000 pps target rate step in the perf workflow.
- **0.3.0 numbers**: rust-dpdk 698,948 / 697,552 / 475,898 pps at 64 / 512 / 1400 B, 700k step
  (E0 baseline, reproduces the 2026-10-15 CI run). These numbers were **measured on `5c74658`**,
  which is perf-identical to `f21ed1f` for these files (no diff in the perf path) — LOW-2.

### Measured values referenced by acceptance criteria
| Quantity | Value | Source |
|---|---|---|
| rust-dpdk 0.3.0 (64 / 512 / 1400 B) | 698,948 / 697,552 / 475,898 pps | E0, `perf-summary-e0-baseline.md` |
| tokio-dpdk fair, median of 3 (64 / 512 / 1400 B) | 698,737 / 698,746 / 475,953 pps | E1 reps 1-3 |
| tokio-dpdk fair, worst 64 B rep | 669,019 pps = 95.7 % | E1 rep 3 |
| tokio-dpdk-concurrent floor (64 B) | ≥ 290k (E0 legacy spawn = 302k; E2 spawn = 307k) | E0 / E2 |
| Uncontended mutex lock+unlock | ~11 ns | Stage 0 |
| Fair-mode CPU, measured under load (≥ 70k pps) | 25.0 % of 4 vCPU = one core, ~3 ctx-sw/s (LOW-1; 0 pps was **not** measured in E1) | E1 `cap-E1.log` |
| Burst RX per-packet copy + async_trait boxing cost | 23 % malloc/free in fair flamegraph (both the per-packet `recv_queue` `Vec` copies **and** `async_trait` boxing contribute — LOW-12) | E5 `flame-fair-mt-summary.txt` |

---

## Requirement 1: Benchmark app modes (apps/tokio-echo)

**User story:** As a maintainer, I want `apps/tokio-echo` to default to the fair loop while
keeping the per-packet-spawn pattern available, so that both the library's fair-path ceiling
and the pattern real users write stay measured.

#### Acceptance criteria
1.1. WHEN `tokio-echo` runs with no `--mode` flag THEN it SHALL run the fair loop: receive a
     datagram and send the same bytes back to the source address within a single task, with no
     `tokio::spawn`, `Semaphore`, or `tokio::time::timeout` per packet.
1.2. WHEN `tokio-echo` runs with `--mode concurrent` THEN it SHALL reproduce the E2 "spawn"
     per-packet behaviour (`buf[..len].to_vec()`, acquire a `Semaphore` permit, `tokio::spawn`
     a handler, send under `tokio::time::timeout`) AND SHALL echo the **exact received bytes**
     (it SHALL NOT `format!("echo: {}", ..)` or otherwise transform the payload as the shipped
     app does today).
1.3. The `--mode` argument SHALL accept at least `fair` and `concurrent`, SHALL default to
     `fair`, and SHALL reject unknown values with a non-zero exit and a clear error message.
1.4. WHEN `--verbose` is NOT set THEN neither mode SHALL call `String::from_utf8_lossy` or
     `format!` on the payload on the hot path; the echoed bytes SHALL equal the received bytes.
1.5. WHEN `--verbose` IS set THEN the formatting/logging (`from_utf8_lossy`, per-packet log
     lines) SHALL be produced as before.
1.6. The app SHALL print a one-line identifier of the active mode at startup (e.g.
     `mode=fair`) so the perf log and DUT logs record which pattern ran.
1.7. The app SHALL keep the existing SIGTERM/SIGINT shutdown `select!` so `PerfReporter::drop()`
     still emits the `[NIC-FINAL]` line the harness cross-checks.
1.8. Existing flags (`--ip`, `--port`, `--workers`, `--timeout`, `--perf-interval`,
     `--stats`, `--force-tokio`) SHALL keep working; `--workers`/`--timeout` apply only in
     concurrent mode. A new `--idle-backoff-ms <N>` flag SHALL enable the opt-in idle backoff
     (Req 5.4). Because `set_idle_backoff` is an inherent method on `DpdkUdpSocket` and NOT on the
     `AsyncUdpSocket` trait (which SHALL NOT change, Req 4.3), WHEN the flag is set the app SHALL
     bind `DpdkUdpSocket` **directly** (not via `bind_udp_with_config`), call
     `set_idle_backoff(Some(N ms))` on the concrete type, THEN erase it to
     `Arc<dyn AsyncUdpSocket>`. WHEN the flag is absent the binding path and behaviour SHALL be
     **unchanged** (idle backoff OFF, default 0.3.0 behaviour). The flag SHALL be parsed in PR 1
     but SHALL have no effect until PR 5 (the PR that adds `set_idle_backoff`); PR 1 SHALL NOT
     reference `set_idle_backoff` (it does not yet exist).

> Reference: `context/experiment-tokio-echo-modes.diff` shows the experiment wiring. The
> shipped app keeps `fair` and `concurrent`; the experiment-only `spawn`/`legacy`/`--runtime`
> flags are not required.

---

## Requirement 2: Perf gates, counters, latency, and log discipline

**User story:** As a maintainer, I want the perf workflow to judge results against absolute
targets over multiple repetitions and surface real latency and counters, so that a flat
ceiling fails the run instead of being written up as "No Regression".

#### Acceptance criteria — gates
2.1. The perf workflow (`scripts/run-perf-tests.sh` driven by `.github/workflows/perf-tests.yml`)
     SHALL run each judged config and packet size at the 700k step for **3 repetitions** within
     a single deployed stack and SHALL judge on the **per-size median** RX pps of those 3 reps.
     Rep 1 SHALL be the existing full sweep; reps 2 and 3 SHALL re-run **only the 700k step**
     on the already-deployed stack. Each repetition SHALL write a separate per-rep result file
     (`${config}.rep1.json` / `.rep2.json` / `.rep3.json`), and the aggregator SHALL compute the
     per-(config,size) median across those per-rep files. Reps 2 and 3 SHALL use the run's
     **requested** configs and packet sizes (the workflow `configs` / `packet_sizes` inputs,
     intersected with each gate's own sizes) rather than hard-coded lists, and SHALL run only the
     700k step. The 70k / 140k / 350k steps are measured in rep 1 only, so the per-step rule below
     judges a **single sample** at those steps (LOW-9).
2.2. The run SHALL FAIL (non-zero exit) with a clear message naming the config, size, measured
     median, and target WHEN any of these is missed. **Gate applicability (MEDIUM-3):** a gate is
     judged only for the (config, size) pairs where the gated config, its `rel_to` reference config
     (if any) and the size were all **requested** by the run's inputs; a gate whose config or
     reference was not requested is **skipped** and the summary SHALL print an explicit
     "gate skipped: <gate> (config/reference/size not requested)" notice. For a gate that IS
     applicable, **missing data SHALL count as a FAIL**, not a skip (a requested config that
     produced no data fails). **Rate steps count too:** the headline 700k gates (tokio-vs-rust,
     rust-vs-native, concurrent floor) are applicable only when 700k is in the requested
     `rate_steps` (reps 2-3 run only then, and the median uses reps 1-3); the per-step rule is
     applicable only at the requested steps among 70k/140k/350k. The native-dpdk invalid-run rule
     applies only when native-dpdk AND 64 B AND 700k were requested; IF native-dpdk was requested
     and applicable but produced **no data**, the run is INVALID (exit 2, infrastructure — testpmd
     failed to run), not a gate FAIL (round-5 MEDIUM-1). Each gate SHALL pin **its own** packet-size list in
     `perf-targets.json` (gates do NOT all share `[64,512,1400]` — D2):
     - **tokio-dpdk (fair)** median < 95 % of the rust-dpdk median at **64, 512, 1400 B** (this
       headline gate is the **only** judge of the 700k step for tokio-vs-rust — MEDIUM-7);
     - **rust-dpdk ≥ native (also the sync no-regression gate)**: rust-dpdk median < 95 % of the
       **same-run** native-dpdk median at **64, 512 B** only; **1400 B is report-only** (non-gating)
       because on CI history rust-dpdk is 81-87 % of native at 1400 B whenever TRex is not
       line-rate-capped (runs #43 #47 #48 #49 #51), tracked by a ROADMAP follow-up (D3). This ONE
       **deployment-normalized** gate doubles as the sync no-regression gate (D8/HIGH-4): because
       the denominator is the **same run's** native-dpdk, a degraded deployment moves rust and native
       together and cannot produce a false regression. The comparison of rust-dpdk against its
       **0.3.0 absolute numbers** (698,948 / 697,552 / 475,898 pps) is **REPORT-ONLY** — surfaced
       as a "vs 0.3.0" column with a **WARN marker below 98 % (the brief's sync number)** — and SHALL NOT gate (CI runs
       #41 #42 #45 #50 — TCP-crate/harness PRs that do NOT touch the UDP data path, #50 added the
       TCP benchmark runner, only #51 was docs-only — dipped to ~89 % of 0.3.0 at 512 B purely
       from deployment variance, with same-run native equally low, so gating on the absolute would
       fail clean runs);
     - **native-dpdk sanity (invalid-run)**: IF the native-dpdk median at **64 B** is < 600,000 pps
       THEN the run is **INVALID** (a degraded deployment, not a regression) and the harness SHALL
       exit **2 (infrastructure failure)** for redeploy, NOT report a gate FAIL (exit 1) (D8/HIGH-4);
     - **tokio-dpdk-concurrent** median < 290,000 pps at **64 B only** (512/1400 B are measured and
       **report-only**, since E2 spawn measured 232k @512 B and 176k @1400 B); this concurrent
       floor gate carries `"enabled": false` (report-only) in `perf-targets.json` and PR 4's diff
       flips it to `"enabled": true` (PR 4 removes the tokio outer mutex and makes the concurrent
       path stop sharing one lock between senders and the receiver). It is reported-but-not-gated until PR 4 (D2/LOW-3/MEDIUM-1 — NOT keyed on the
       GitHub PR number);
     - **per-step throughput** (D6/MEDIUM-7): at rate steps **70k, 140k, 350k** (the **700k step is
       excluded** — it is judged only by the headline tokio-dpdk ≥ 95 %-of-rust gate) and every
       gated size, tokio-dpdk (fair) median rx_pps < **98 % of the same-run rust-dpdk** median
       rx_pps at that step and size. There is **no** "% of offered rate" rule.
     - **idle CPU** (PR 5 onward; `"idle_cpu"` in `perf-targets.json`, `enabled:false` until PR 5): with
       `tokio-echo --idle-backoff-ms` on the DPDK backend, the `tokio-echo` process CPU during the
       0-pps window SHALL be ≤ 10 % of one vCPU (Req 5.4); judged only when that step ran.
     There SHALL be **no DUT-latency gate** (D5): DUT p50/p99 are **reported, not gated** (see 2.6
     and Req 6.4), because the 0.3.1 idle backoff that could affect latency is opt-in and default
     OFF (Req 5.4).
2.3. The gate thresholds and 0.3.0 reference numbers SHALL be defined in one place
     (the harness or a committed data file) so they are auditable and updatable without code
     spelunking.
2.4. WHEN all gates pass THEN the run SHALL exit 0; WHEN infrastructure fails (deploy, SSM,
     TRex) THEN the run SHALL still fail as today, independently of the perf gates.

#### Acceptance criteria — summary, latency, counters
2.5. The per-size summary tables SHALL gain a **"vs target"** column showing the median
     measured pps as a percentage of that config's applicable target (and PASS/FAIL).
2.6. The summary SHALL add DUT p50 and p99 latency (µs) columns (**reported, not gated** — D5)
     for every DPDK config that produced traffic AND emits `[PERF]` (rust-dpdk, tokio-dpdk,
     tokio-dpdk-concurrent; native-dpdk is excluded because testpmd emits no `[PERF]`), sourced
     from the DUT's `[PERF]` `lat_p50_us` / `lat_p99_us` fields and stored in the result rows under
     the distinct keys **`dut_lat_p50_us` / `dut_lat_p99_us`** (the TRex rows already use
     `lat_p50_us` / `lat_p99_us` — LOW-7). These SHALL be printed with
     **one decimal place** (`{:.1}`) derived from the nanosecond samples (`p99_ns as f64 / 1000.0`),
     replacing today's `{:.0}` so sub-µs values are not rounded to 0. The existing TRex-measured
     `Lat Avg` / `Lat Max` columns SHALL be retained unchanged. DUT latency SHALL be attributed to
     a rate step using only the `[PERF]` intervals that lie **entirely** inside that step's TRex
     time window (rep 1's ticks for 70k/140k/350k; for 700k, the ticks of reps 1-3, taking the max
     across all three — round-5 LOW-11; this fully-contained rule is new and deliberately stricter
     than the overlap rule App Drops uses at `run-perf-tests.sh:1753`), and the reported p99 (and
     p50) SHALL be the **max** of those per-interval
     percentiles for the step (percentiles SHALL NOT be averaged). For the done gate, **"non-zero
     p50/p99"** means a printed value **> 0.0 at the 350k and 700k steps only** (lower steps MAY
     legitimately print 0.0 at `{:.1}`).
2.7. In tokio mode, `worker_idle_pct` SHALL report a value > 0 while the receiver is idle
     (0 pps), by incrementing the existing `worker_idle_polls` and `worker_packets_processed`
     counters on the RTC/tokio `try_recv_from` path (empty poll → `worker_idle_polls`, delivered
     packet → `worker_packets_processed`). Because every empty spin poll counts, tokio-mode idle%
     reads close to 100 % at loads below saturation (empty polls outnumber received packets
     there) and drops only near saturation, unlike pipeline mode;
     the summary and the perf-log template SHALL label it "poll-idle % (tokio: share of empty
     polls, not CPU idle)" so it is not misread (round-5 LOW-12). No new public fields SHALL be added to `PerfCounters`,
     `CounterSnapshot`, or `RateSnapshot` (they are not `#[non_exhaustive]`). The **evidence source**
     for "`worker_idle_pct` > 0 while idle" (LOW-9) SHALL be the DUT's `[PERF]` log line emitted
     during the **pre-traffic idle period** that every run already has (the app starts and emits
     `[PERF]` ticks with `rx_pps=0` before TRex sends: `start_dut_tokio_dpdk` starts the app with
     `--perf-interval 10` and then sleeps 15 s and runs TRex setup, and the sandbox DUT logs show
     two leading `rx_pps=0` ticks per config), i.e. the `worker_idle_pct=` field of the
     first zero-`rx_pps` tick. The harness SHALL extract it into the summary as a per-config
     "idle%" value (PR 2, task 2.7a), so the done-gate check has a named, machine-readable source
     without waiting for the PR 5 idle-CPU window.
2.8. In tokio mode (tokio-dpdk and tokio-dpdk-concurrent), the same software drop counters used
     by rust-dpdk SHALL be reported: `rx_ring_drops` and `rx_buf_drops` (from
     `rx_drops_ring_full` / `rx_drops_buffer_full`). In RTC/tokio mode there is no pipeline
     `rx_loop`, so `rx_ring_drops` legitimately stays 0 and only `rx_buf_drops` populates
     (HIGH-7). For the tokio configs the summary's **App Drops** column SHALL be **present and
     populated** from those counters by the same aggregation the harness already applies for
     rust-dpdk; a value of **0 is valid** (a healthy fair-mode run drops nothing — E0/E1 show 0 for
     every config). No harness cross-check is defined for this column: App Drops is itself the sum
     of `RateSnapshot.rx_drops` = `rx_ring_drops + rx_buf_drops` (`perf.rs:286`,
     `run-perf-tests.sh:1757,1767`), so comparing it against the same counters would be a
     tautology (round-4 MEDIUM-1). The counter-increment itself SHALL be proven by the stub
     overflow tests — **reuse the existing
     overflow tests** at `dpdk-stdlib-udp/src/lib.rs:5975`
     (`socket_record_rx_drop_also_bumps_perf_counter`) and `:5999`
     (`recv_buffer_overflow_records_drops`), plus `recv_buffer_bytes_tracks_queued_payload`
     (`:5985`), adapted to the new `rx_state`-owned queue (LOW-10). `:5975` calls `record_rx_drop`
     directly and asserts `rx_drops_buffer_full` increments; `:5999` pushes past the buffer limit
     and asserts `recv_drops().packets`; `:5985` locks `recv_queue` directly and must move to
     `rx_state` (round-6 LOW-3).

#### Acceptance criteria — log discipline
2.9. `docs/perf-test-log.md` SHALL gain a run-entry template whose results tables include a
     **"vs last run"** column (new: today's entries carry it only as a "No Regression?" note,
     not a column) and a **"vs target"** column next to it, for each judged (config, size).
2.10. The `docs/perf-test-log.md` header AND `AGENTS.md` SHALL state a rule that a "known
      limitation" / "No Regression on a flat ceiling" claim MUST NOT be written up without a
      linked ROADMAP item tracking the limitation.
2.11. `docs/perf-test-log.md` SHALL contain a new entry for the run that demonstrates
      Requirement 1 and 2 (tokio-dpdk fair ≥ 95 %; DUT p50/p99 > 0.0 at the 350k and 700k steps;
      tokio-mode idle% > 0 from the pre-traffic tick; App Drops columns present and populated,
      0 allowed), using the new template.
2.12. CI cost SHALL stay within free GitHub Actions minutes: the perf workflow stays
      `workflow_dispatch`-only (no new schedules), the 3 repetitions happen inside one run on
      one deployed stack, and the per-PR tier stays free (no EC2).

---

## Requirement 3: Thread-safe `dpdk_udp::UdpSocket` (soundness fix)

**User story:** As a user sharing one `Arc<dpdk_udp::UdpSocket>` across threads (as
`std::net::UdpSocket` semantics allow), I want concurrent `send_to`/`recv_from` to be sound,
so that I don't get interleaved frames or a data race.

#### Acceptance criteria
3.1. `dpdk_udp::UdpSocket` SHALL serialize **per-socket TX-side state** (the `TxBuffer` scratch
     buffer) behind a per-socket TX guard, covering every TX path: RTC DPDK
     (`send_to_v4`/`send_to_v6` → `socket_backend.send_frame`), the pipeline `cached_direct_send`
     path, the generic `PacketBackend` path, and the tunnel encap paths (VXLAN/GENEVE/GUE/VLAN)
     that write into `tx_buf`.
3.1a. The shared DPDK hardware queues SHALL be serialized by **per-queue guards living in the
     shared `DpdkResources`** — one each for the RX queue 0, the RTC TX queue 0, and the pipeline
     dedicated TX queue — so that concurrent `rx_burst`/`tx_burst` on the same hardware queue
     cannot occur across any combination of threads or sockets sharing the port (the pipeline
     `rx_loop` thread SHALL also take the RX-queue guard). Generic (`PacketBackend`) backends,
     which provide their own synchronization, SHALL NOT use these DPDK queue guards.
3.2. `dpdk_udp::UdpSocket` SHALL serialize **per-socket RX-side state** (the buffered
     `recv_queue` refill/pop, folded into the RX state, and the single-consumer app-ring dequeue
     + round-robin index in `try_recv_from_pipeline`/`recv_from_pipeline`) behind a per-socket RX
     guard, covering RTC inline, generic backend, and pipeline paths, for both IPv4 and IPv6.
     Consistent with 3.1a (LOW-11): for the **DPDK** backend the hardware `rx_burst` SHALL be
     serialized by the RX-queue guard `q_rx0` — **including the two inline RX paths
     (`try_recv_from_inline`, `recv_from_inline`) that call `res.port.rx_burst(0, 32)` directly
     rather than through `SocketBackend::recv_frames`** (HIGH-1), which SHALL take `q_rx0` via a
     `recv_guarded` helper that returns `Vec<Mbuf>` so the existing zero-copy iteration is
     preserved; for a **generic** (`PacketBackend`) backend the backend's own `recv_frames`
     provides its synchronization and SHALL NOT use `q_rx0` — only the per-socket RX guard
     serializes that socket's buffered state. `process_frame_zerocopy` and the
     helpers it calls SHALL operate on the already-held RX state (`&mut RxState`) and SHALL NOT
     re-lock it (`std::sync::Mutex` is not reentrant — D12). The `has_buffered_packets` pre-lock check
     flag SHALL be set on **every** push into the RX queue on **all** paths (inline, pipeline, and
     the `resolve_arp` poll) and cleared when the queue empties, so the pre-lock check is valid
     everywhere (D10).
3.3. The per-socket TX and RX guards SHALL be independent so a sender and a receiver on different
     threads do not serialize against each other on per-socket state; they SHALL contend only on
     the shared per-queue guards for the actual hardware burst.
3.3a. A single global lock order SHALL be documented and obeyed —
     `rx_state < q_rx0 < tx_state < q_tx0 < q_tx_direct` — and the design SHALL prove no path
     (including `resolve_arp`, which does RX-then-TX, and the ARP/NDP/ICMP auto-replies sent from
     the RX path in `process_frame_zerocopy`) can form a lock cycle. The auto-replies and
     `resolve_arp`'s sends are **prebuilt frames** that take **only `q_tx0`** (NOT `tx_state` —
     MEDIUM-3), so their acquisition is the subsequence `rx_state < q_rx0 < q_tx0`. No per-socket guard SHALL be
     held across a sleep or the up-to-3 s `resolve_arp` wait. The leaf locks (`connected_addr`,
     `connection_state`, the ARP/NDP caches, `error_queue`) are **leaves** (LOW-13): they MAY be
     taken under the `rx_state`/`tx_state` guards but SHALL NEVER be held while acquiring a
     per-direction or per-queue guard, so they cannot participate in a cycle.
3.4. The `unsafe impl Sync for TxBuffer` SHALL be removed, OR retained only if the new guard
     makes it sound and its SAFETY comment is updated to describe the actual invariant
     (no claim of "RTC single-thread only" if the guard is what provides safety).
3.4a. The guards SHALL NOT turn a transient panic into a permanent failure: either every
     critical section SHALL be proven panic-free, or lock acquisition SHALL recover from a
     poisoned mutex (`into_inner`) rather than propagating `PoisonError`.
3.5. `dpdk_udp::UdpSocket` SHALL remain `Send + Sync` (removing `Sync` is a breaking change and
     breaks the std/tokio drop-in contract).
3.6. On the uncontended path, the added synchronization SHALL cost no more than one per-socket
     guard lock/unlock pair plus one per-queue guard lock/unlock pair per direction per call
     (≈ 2 × ~11 ns budget from Stage 0, still negligible vs the ~1 µs per-packet budget) and
     SHALL NOT introduce a heap allocation or syscall on the hot path.
3.7. A stress test in plain `cargo test` (stub/mock backend) SHALL run N ≥ 4 threads each doing
     ≥ 100,000 `send_to` plus concurrent `try_recv_from` on one shared `Arc<UdpSocket>`, SHALL
     pass in a debug build with no in-use assertion firing, and SHALL verify 100 % of emitted
     frames are intact (valid IPv4/UDP checksums, payload equals one sender's payload — no
     interleaving). The mock backend SHALL validate each frame inside `send_frame` and keep
     only counters (frames seen, frames bad), not the frames themselves, so the test stays
     within ordinary CI memory (no ~500 MB of stored frames in a debug build).
3.8. A loom model SHALL be added ONLY IF the design introduces a new atomic or lock-free
     protocol; a design built on `std::sync::Mutex`/`parking_lot` guards does not require loom.
3.9. The thread-safety contract SHALL be documented in `API_COMPATIBILITY.md` and the
     `dpdk-stdlib-udp` crate README.

---

## Requirement 4: Concurrent-sender scalability (drop the tokio outer mutex)

**User story:** As a tokio user with concurrent senders and a receiver, I want senders and the
receiver not to contend on a single outer mutex, so that sends and receives no longer serialize
on one lock (the measured gate is 4.4 and Requirement 2's tokio-dpdk-concurrent floor).

**Depends on Requirement 3.**

#### Acceptance criteria
4.1. `dpdk-stdlib-tokio`'s `DpdkUdpSocket` (`socket.rs`) SHALL hold `Arc<dpdk_udp::UdpSocket>`
     WITHOUT an outer `std::sync::Mutex`. `compat::tokio::UdpSocket`'s `DpdkAsyncSocket`
     (`compat/tokio.rs`) SHALL hold `Arc<std::panic::AssertUnwindSafe<dpdk_udp::UdpSocket>>` (an
     unwind-safe wrapper) WITHOUT an outer `std::sync::Mutex` — the wrapper restores the
     `UnwindSafe`/`RefUnwindSafe` that the removed `std::sync::Mutex` conferred (D4/CRITICAL-1;
     `Arc<dpdk_udp::UdpSocket>` is **not** `UnwindSafe` because `UdpSocket` holds
     `Arc<dyn PacketBackend>`). The wrapper SHALL carry a SAFETY comment justifying it (guards
     recover from poisoning via `into_inner`; no broken invariant survives a panic).
4.2. All existing public methods SHALL keep their current signatures and observable
     behaviour:
     - `dpdk_stdlib_tokio::socket::DpdkUdpSocket` (via `AsyncUdpSocket`): `recv_from`,
       `send_to`, `local_addr`, `connect`, `recv`, `send`, `backend_name`,
       `enable_perf_reporting`, `disable_perf_reporting`, `recv_drops`, and the inherent
       `bind` / `bind_with_config`.
     - `dpdk_stdlib_tokio::compat::tokio::UdpSocket`: every public method in
       `compat/tokio.rs` today, including `bind`, `from_std`, `into_std`, `local_addr`,
       `peer_addr`, `connect`, `send`, `recv`, `send_to`, `recv_from`, the `poll_*` and
       `try_*` methods and the option getters/setters.
     The two types have different method sets; neither gains a required method.
4.2a. The compat `try_recv_from`/`try_send_to`/`try_recv`/`try_send` SHALL remain
     **non-blocking on guard contention** after the outer mutex is removed: they SHALL `try_lock`
     the inner per-direction (and matching queue) guard and return `io::ErrorKind::WouldBlock` on
     contention instead of blocking or spinning (replacing today's outer-mutex `try_lock`
     → `WouldBlock`). **Known pre-existing exception (LOW-6):** on a fresh send to an unresolved
     peer, compat `try_send_to` still reaches `resolve_arp`, which can block for **up to ~3 s** on
     an ARP miss. This is **existing 0.3.0 behaviour, unchanged** by this spec (the outer-mutex
     removal does not touch it), and SHALL be **documented** in `API_COMPATIBILITY.md` rather than
     silently implied to be fully non-blocking. WHEN a non-blocking `try_recv_from` pulls an ARP/ICMP/NDP/ICMPv6 frame whose
     auto-reply needs the TX side, it SHALL `try_lock` **only `q_tx0`** (the reply is a prebuilt
     frame, so NO `tx_state` is taken — MEDIUM-3) and, if `q_tx0` is busy, **DROP the auto-reply
     and count it** in `auto_reply_drops` (a new private `AtomicU64` on `UdpSocket`, read through an
     additive `#[doc(hidden)] pub fn auto_reply_drops(&self) -> u64`) rather than block. The counter is
     neither `rx_arp_handled` (which counts handled ARP frames) nor `rx_drops_buffer_full` (which
     feeds App Drops and would look like packet loss) (D11, round-6 LOW-4). `dpdk_udp::UdpSocket` SHALL gain additive
     non-blocking try entry points to support this; because they are called from the separate
     `dpdk-stdlib-tokio` crate they SHALL be **`pub` but `#[doc(hidden)]`** (not private — LOW-9),
     keeping them callable cross-crate while out of the documented surface.
4.2b. `DpdkUdpSocket` and the compat `UdpSocket` SHALL keep **the exact 0.3.0 auto-trait set each
     one actually has** (D4/CRITICAL-1), verified by `static_assertions` `assert_impl_all!`
     compile-time checks (one per type) and by `cargo semver-checks`:
     - `DpdkUdpSocket`: `Send + Sync + Unpin` (it holds `connected_addr: Arc<tokio::sync::Mutex<..>>`,
       which is **not** `UnwindSafe`/`RefUnwindSafe`, so 0.3.0 never had those two — they are not
       asserted and there is nothing to retain; a plain `Arc<dpdk_udp::UdpSocket>` suffices);
     - `compat::tokio::UdpSocket`: `Send + Sync + Unpin + UnwindSafe + RefUnwindSafe` (all five).
       Because `Arc<dpdk_udp::UdpSocket>` is **not** `UnwindSafe` (`UdpSocket` holds
       `Arc<dyn PacketBackend>`), dropping the outer `std::sync::Mutex` would lose the two
       unwind-safe traits; `DpdkAsyncSocket` SHALL therefore hold
       `Arc<std::panic::AssertUnwindSafe<dpdk_udp::UdpSocket>>`, which restores them (it is
       `UnwindSafe + RefUnwindSafe` for any `T`, and `Send + Sync + Unpin` when `T` is).
     The compat `assert_impl_all!` SHALL compile **both from the workspace root (where the `dpdk`
     feature is unified on) and under `cargo test -p dpdk-stdlib-tokio` without the feature**, and
     `cargo semver-checks` SHALL pass against 0.3.0 **both with `--features dpdk` and without it**.
4.3. The `AsyncUdpSocket` trait SHALL NOT change (no signature change, no added required method,
     no `async_trait` removal) — semver constraint.
4.4. The `tokio-dpdk-concurrent` perf config SHALL exercise this path (concurrent `send_to` from
     spawned tasks sharing one socket) and meet the ≥ 290k pps floor **at 64 B** from
     Requirement 2.2 (512/1400 B are report-only). This floor gate becomes **active in this PR**
     (PR 4) by flipping `perf-targets.json`'s `gates["tokio-dpdk-concurrent"].enabled` from `false`
     to `true` (D2/LOW-3/MEDIUM-1 — not keyed on the GitHub PR number).
4.5. Existing `dpdk-stdlib-tokio` tests (including `compat/tokio.rs` loopback tests) SHALL
     continue to pass under `cargo test`.

---

## Requirement 5: CPU-efficiency items (lower priority)

**User story:** As a maintainer, I want the fair path to waste fewer cycles on allocation and
idle spinning, so that an idle socket doesn't pin a core — without reducing throughput.

**These items MUST NOT reduce Requirement 1/2 throughput or violate the latency constraint.**

#### Acceptance criteria — 5a (burst RX allocation)
5.1. The burst RX path SHALL NOT allocate a fresh per-packet `Vec<u8>` for the buffered
     (non-returned) packets of a burst (today `process_frame_zerocopy` pushes
     `payload.to_vec()` into `recv_queue` for 31 of every 32 packets).
5.2. A test or counting allocator in stub mode SHALL assert that receiving a multi-packet burst
     performs zero per-packet payload heap allocations for the buffered packets.
5.3. Correctness SHALL be preserved: buffered packets returned by later `recv_from`/
     `try_recv_from` calls still carry the correct payload and source address, and the
     `recv_queue` byte/packet limits (`DEFAULT_RECV_BUFFER_BYTES`/`_PACKETS`) still apply.

#### Acceptance criteria — 5b (idle backoff, OPT-IN and default OFF — D5)
5.4. The idle backoff SHALL be **opt-in and default OFF** in 0.3.1, so default `recv_from`/`recv`
     behaviour is **byte-for-byte the 0.3.0 behaviour** (`try_recv_from` + spin + `yield_now`).
     Opt-in SHALL be via an **additive, documented inherent method** on `DpdkUdpSocket` and
     `compat::tokio::UdpSocket` — `set_idle_backoff(Option<Duration>)` / `idle_backoff()` — and
     `tokio-echo` SHALL gain a `--idle-backoff-ms` flag (absent = OFF). Because `set_idle_backoff`
     is inherent (not on the `AsyncUdpSocket` trait, Req 4.3), `tokio-echo` SHALL, when the flag is
     set, bind `DpdkUdpSocket` directly, call `set_idle_backoff`, then erase to
     `Arc<dyn AsyncUdpSocket>` (Req 1.8). WHEN enabled, the tokio
     DPDK receive path SHALL back off based on **elapsed idle time** — escalating from `yield_now`
     to `tokio::time::sleep` only after the socket has had **no packet for the configured idle
     threshold** — the `Duration` passed to `set_idle_backoff(Some(d))`, clamped to **≥ 2 ms**
     (values below 2 ms are treated as 2 ms; this is the one definition of the argument,
     documented in the rustdoc; `tokio-echo --idle-backoff-ms N` passes `Some(N ms)`) (not after a
     fixed empty-poll count), resetting to "just received" the instant a packet arrives — so that
     an idle socket at 0 pps uses ≤ 10 % of one vCPU. The sleep granularity SHALL account for
     tokio's ~1 ms timer resolution (sleeps SHALL NOT claim sub-millisecond precision); the opt-in
     method's docs SHALL state that first-packet-after-idle latency can therefore rise by up to
     ~1-2 ms. The opt-in method SHALL **require** a time-enabled runtime (`enable_time` /
     `enable_all`, which `#[tokio::main]` provides), documented in the method rustdoc and
     `API_COMPATIBILITY.md`. There is **no "cannot panic" guarantee** (MEDIUM-2 r3):
     `Handle::try_current()` only detects a runtime context, not whether the time driver is
     enabled, so using the backoff on a timer-less runtime SHALL panic exactly like any other
     `tokio::time` call. Because the feature is default-OFF, 0.3.0 users who do not opt in SHALL
     be unaffected. Idle CPU SHALL be
     measured on the DUT during a held 0-pps window **with `--idle-backoff-ms` enabled** using
     `mpstat`/`pidstat` (the ≤ 10 % of one vCPU target = ≤ 2.5 % all-CPU on the 4-vCPU DUT, or the
     per-process `pidstat` figure for the `tokio-echo` process).
5.5. The idle backoff SHALL NOT reduce throughput: at rate steps **70k, 140k, 350k** each step's
     tokio-dpdk median rx_pps SHALL be ≥ **98 % of the same-run rust-dpdk** median rx_pps at that
     step and size (the single per-step rule of Req 2.2 / D6 — the **700k step is excluded** from
     the per-step rule and judged only by the headline tokio-dpdk ≥ 95 %-of-rust gate, MEDIUM-7).
     There is no "% of offered rate" rule.
5.6. Because the idle backoff is opt-in and default OFF, default 0.3.1 recv latency is unchanged
     from 0.3.0 and there is **no latency gate**. DUT p50/p99 are **reported** (non-gating) per
     Req 2.6 / Req 6.4.

#### Acceptance criteria — 5c (no trait change)
5.7. No change to the public `AsyncUdpSocket` trait and no removal of `async_trait`; any
     hot-path win SHALL be internal to the socket types.

---

## Requirement 6: Hard constraints (apply across all items)

**User story:** As the maintainer publishing 0.3.1, I want every change to be semver-compatible
and free of regressions, so that it ships as a patch without breaking downstream users.

#### Acceptance criteria
6.1. Every published crate (`dpdk-stdlib`, `dpdk-stdlib-sys`, `dpdk-stdlib-net`,
     `dpdk-stdlib-udp`, `dpdk-stdlib-tcp`, `dpdk-stdlib-quic`, `dpdk-stdlib-tokio`) SHALL remain
     semver-compatible with 0.3.0: no removed/renamed public items, no changed signatures, no
     new required fields on public structs, no trait changes (incl. `AsyncUdpSocket`,
     `PacketBackend`). Additive items are allowed.
6.2. `cargo semver-checks` (or a documented equivalent) against 0.3.0 SHALL pass for all
     published crates, as a CI or pre-release step, run **both with `--features dpdk` and without
     the feature** (D4/CRITICAL-1), AND `static_assertions` `assert_impl_all!` checks SHALL confirm
     each type retains **the exact 0.3.0 auto-trait set it actually has** (D4):
     `dpdk_udp::UdpSocket` → `Send + Sync + Unpin`; `DpdkUdpSocket` → `Send + Sync + Unpin`;
     `compat::tokio::UdpSocket` → `Send + Sync + Unpin + UnwindSafe + RefUnwindSafe`. The compat
     type keeps its two unwind-safe traits via the `AssertUnwindSafe` wrapper on
     `DpdkAsyncSocket.socket` (not by a surviving `std::sync::Mutex` — that mutex is removed in
     PR 4). The compat assertion SHALL compile both from the workspace root (dpdk unified on) and
     under `cargo test -p dpdk-stdlib-tokio` without the feature. (The round-1 claim that all three
     have all five traits was incorrect: `UdpSocket` holds `Arc<dyn PacketBackend>` and
     `DpdkUdpSocket` holds `Arc<tokio::sync::Mutex<..>>`, neither of which is
     `UnwindSafe`/`RefUnwindSafe` in 0.3.0.)
6.3. The sync no-regression check SHALL be **deployment-normalized** (D8/HIGH-4): rust-dpdk median
     SHALL stay ≥ **95 % of the same-run native-dpdk** median at the 700k step, 64 / 512 B (1400 B
     report-only). This is the same gate as the `rust-dpdk ≥ native` gate of Req 2.2 — it doubles
     as the sync no-regression gate, and is immune to deployment variance because both numbers move
     together. The comparison against the 0.3.0 absolute numbers (698,948 / 697,552 / 475,898 pps)
     is **REPORT-ONLY** (a "vs 0.3.0" column, WARN below 98 %) and SHALL NOT gate. IF native-dpdk
     @64 B < 600,000 pps the run is INVALID (exit 2, redeploy), not a regression. This sync check
     SHALL run in each PR that touches the data path (the soundness fix, the outer-mutex drop, and
     the CPU-efficiency PR), via `gh workflow run perf-tests.yml` (CLAUDE.md step 5), not deferred
     to a later PR.
6.4. DUT p50/p99 latency SHALL be **reported, not gated** (D5). Because the idle backoff is opt-in
     and default OFF (Req 5.4), default 0.3.1 recv latency equals 0.3.0's, so there is no latency
     regression to gate. The summary SHALL surface DUT p50/p99 (`{:.1}` from `[PERF]` ns) so
     latency stays observable; the done-gate "non-zero p50/p99" check reads a printed value > 0.0
     at the 350k and 700k steps only.
6.5. `cargo build && cargo test` from the workspace root SHALL pass with no DPDK installed
     (stub backend), per repo convention; all new unit/stress tests SHALL run in stub mode.
6.6. New `unsafe` SHALL NOT be added outside `dpdk-sys`/`ring_buffer.rs` without justification;
     the soundness fix SHALL NOT increase the count of `unsafe` blocks/impls in `dpdk-stdlib-udp`.
6.7. Crate dependency direction SHALL be preserved (`dpdk-stdlib-udp` must not depend on
     `dpdk-stdlib-tokio`).

---

## Out of scope
- The 0.3.1 version bump, tag, and crates.io publish (maintainer).
- A crates.io / RustSec advisory for the 0.2.x/0.3.0 race (maintainer decision).
- TCP perf baseline and `perf-tests-tcp.yml`.
- New scheduled perf runs.
- Changing the `AsyncUdpSocket` trait or removing `async_trait`.
- Kani / Verus work (separate spec).
- A lock-free rewrite of the receive path (E1 shows it isn't needed for throughput).
- The harness's TRex gateway-MAC rediscovery on `--skip-deploy` reruns.
