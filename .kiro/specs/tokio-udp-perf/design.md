# Design: tokio-udp-perf (0.3.1)

## Decisions (review round 2)

These decisions were made by the maintainer's delegate in response to review round 2 and are
**not reopened** here. Each is recorded with the evidence that drove it. Line numbers are from
this checkout and are expected to drift; the stable anchor is the function/struct name.

- **D1 (CRITICAL-1).** The pipeline builder's `send_fn` closure calls `tx_burst(0, ..)`
  directly (`lib.rs` pipeline builder, `send_fn` closure body, the `resources_for_send.port.tx_burst(0, ..)`
  line 4825) — the **same hardware queue 0** that the RTC `SocketBackend::send_frame` Dpdk arm
  uses (`lib.rs:1603`). `rx_loop` calls this `send_fn` to drain `tx_ring` and to send ARP/ICMP
  replies (`topology.rs:445, 486, 498, 530`), so in pipeline mode it runs `tx_burst(0)` at the
  same time as app-thread queue-0 sends that take `q_tx0` (e.g. `resolve_arp` on an ARP miss in
  `send_to_v4`/`send_to_v6`, `send_gratuitous_arp`, `send_arp_request`, `send_ndp_solicitation`).
  **Decision:** the builder's `send_fn` takes `q_tx0` like every other `tx_burst(0)` caller; it
  is added to Layer A, the caller audit table, and task 3.1. The caller audit table below now
  lists **every** `tx_burst`/`rx_burst` call site in `lib.rs` and `topology.rs` with the guard it
  takes; none is left unguarded.

- **D2 (HIGH-1, LOW-3).** E2 "spawn" measured 232k at 512 B and 176k at 1400 B for the concurrent
  config, so a literal `abs_min:290000` at `[64,512,1400]` would fail every run. **Decision:** the
  `tokio-dpdk-concurrent` floor is `abs_min 290000` at sizes `[64]` **only**; 512/1400 are
  measured and reported but **report-only** (non-gating) for that config. The floor carries an
  explicit **`"enabled": false`** flag (MEDIUM-1 r3, replacing the round-2 `active_from_pr:4` that
  was wrongly compared with the GitHub `PR_NUMBER`): a gate with `enabled=false` is report-only,
  and **PR 4's diff flips it to `true`** — PR 4 is the PR that makes the concurrent path fast. PR 2
  wires and **reports** the config without gating it. Wording is "every gate pins **its own** sizes
  list" (not "every gate pins `[64,512,1400]`").

- **D3 (HIGH-2).** On CI history, whenever TRex was not line-rate-capped at 1400 B/700k,
  `rust-dpdk` was 81-87 % of `native-dpdk` at 1400 B: Run #51 561k/654k, #49 559k/644k,
  #48 565k/687k, #47 561k/689k, #43 555k/664k (`docs/perf-test-log.md`). Only line-rate-capped
  runs pass a 95 % gate at 1400 B. **Decision:** the `rust-dpdk ≥ 95 % of native-dpdk` gate is
  pinned at `[64, 512]` only; 1400 B is **report-only**, and the spec adds a ROADMAP-bound
  follow-up item ("rust-dpdk 1400 B at 81-87 % of native-dpdk when TRex is not line-rate capped —
  investigate"), listed in tasks.md as a **non-0.3.1 follow-up**, citing runs #43 #47 #48 #49 #51.

- **D4 (HIGH-3 r1, CRITICAL-1 r3 — revised).** Reading the source: `dpdk_udp::UdpSocket` holds
  `Arc<dyn PacketBackend>` (`lib.rs:1466`) and `Option<Arc<dyn Fn(..) + Send + Sync>>`
  (`cached_direct_send`, ~2087), **neither of which is `UnwindSafe`/`RefUnwindSafe`**, so
  `dpdk_udp::UdpSocket` is **not** `UnwindSafe`/`RefUnwindSafe` and therefore `Arc<UdpSocket>` is
  **not** `UnwindSafe` either. `DpdkUdpSocket.connected_addr` is `Arc<tokio::sync::Mutex<..>>`
  (`socket.rs:93`) and tokio's `Mutex` wraps an `UnsafeCell` with no `RefUnwindSafe` impl. So
  **0.3.0** has: `Send + Sync + Unpin` for `dpdk_udp::UdpSocket` and `DpdkUdpSocket`; `Send + Sync
  + Unpin + UnwindSafe + RefUnwindSafe` for `compat::tokio::UdpSocket` — and in 0.3.0 the compat
  type gets its two unwind-safe traits **only** because `DpdkAsyncSocket` holds
  `Arc<std::sync::Mutex<dpdk_udp::UdpSocket>>` (`std::sync::Mutex<T>: RefUnwindSafe`, which the
  inner `UdpSocket`'s own non-`UnwindSafe`-ness does not affect).

  **The round-2 bug (CRITICAL-1):** PR 4 changed `DpdkAsyncSocket.socket` to
  `Arc<dpdk_udp::UdpSocket>`, which **drops** the `std::sync::Mutex` that was the only reason the
  compat type was `UnwindSafe`/`RefUnwindSafe`. Because `Arc<UdpSocket>` is not `UnwindSafe`, the
  compat `compat::tokio::UdpSocket` would **lose both traits** under `--features dpdk`. The `dpdk`
  feature is **unified in a workspace build** (`tokio-echo` enables it by default, `synthetic-bench`
  always enables it), so the `assert_impl_all!(compat::tokio::UdpSocket: .., UnwindSafe,
  RefUnwindSafe)` would **fail to compile from the workspace root**; dropping the assertion instead
  would be an auto-trait semver break that `cargo semver-checks` catches. So the round-2 "preserved
  by construction" wording was wrong: once the `std::sync::Mutex` is gone, the construction no
  longer preserves anything.

  **Decision (revised):** `DpdkAsyncSocket` holds
  `Arc<std::panic::AssertUnwindSafe<dpdk_udp::UdpSocket>>` (an explicit unwind-safe wrapper) so
  the compat `compat::tokio::UdpSocket` keeps **`Send + Sync + Unpin + UnwindSafe + RefUnwindSafe`
  both with and without the `dpdk` feature**. `AssertUnwindSafe<T>` is `UnwindSafe` and
  `RefUnwindSafe` for any `T`, and is `Send + Sync + Unpin` when `T` is, so the other three traits
  are unaffected. The concurrency correctness no longer depends on the outer mutex (PR 3's
  per-direction guards provide it), so the outer mutex is genuinely removable; the
  `AssertUnwindSafe` wrapper is a **zero-cost marker** that only restores the auto traits the outer
  mutex used to confer. `DpdkUdpSocket` (`socket.rs`) keeps `Arc<dpdk_udp::UdpSocket>` plain — it
  never had `UnwindSafe`/`RefUnwindSafe` in 0.3.0 (its `connected_addr: Arc<tokio::sync::Mutex<..>>`
  is not `RefUnwindSafe`), so there is nothing to restore and no wrapper is needed there.

  **SAFETY justification for `AssertUnwindSafe` (required):** `UnwindSafe`/`RefUnwindSafe` exist to
  flag types that could expose a logically-broken (half-updated) value if a panic unwinds across a
  `catch_unwind`. `AssertUnwindSafe` is sound here because every per-socket/per-queue guard uses
  the poison-tolerant `lock_or_recover` (`into_inner`) — a panic that poisons a guard is recovered
  from, not propagated — and every critical section is CPU-only build+send/receive with no partial
  public-observable invariant that a panic could strand (the TX scratch `Vec` and the RX
  `ReceiveQueue` are fully re-initialized on the next call; a panic mid-build leaves only private
  scratch, never a torn frame on the wire). So no broken invariant survives a panic, which is
  exactly the contract `AssertUnwindSafe` asserts. This reasoning is recorded in a SAFETY-style
  comment on the field.

  **Verification (task 4.x):** `static_assertions` `assert_impl_all!` checks assert **exactly** the
  0.3.0 trait set per type. The `dpdk_udp::UdpSocket` assertion (`Send + Sync + Unpin`) is added to
  PR 3; the two tokio-type assertions are added to PR 4. The compat assertion must compile **(a)
  from the workspace root, where the `dpdk` feature is unified on**, and **(b)** under
  `cargo test -p dpdk-stdlib-tokio` **without** the `dpdk` feature (the Tokio-only variant). In
  addition, `cargo semver-checks` is run against 0.3.0 **both `--features dpdk` and without the
  feature** so an auto-trait regression is caught in whichever build it appears.

- **D5 (HIGH-4 r1/r3; MEDIUM-6 r2 → MEDIUM-2 r3 revised).** The DUT `lat_p99_us` is measured from `rx_burst` return to end of
  frame processing (`lib.rs` recv paths) and **excludes** NIC-ring queueing — exactly where the
  idle backoff's sleeps would add latency — so a DUT-latency **gate** cannot see the hazard it
  would guard; and the rust-dpdk baseline itself sleeps on empty polls, making "tokio ≤ rust +
  10 µs" nearly vacuous. **Decision:** the 5b idle backoff is **opt-in and default OFF** in 0.3.1,
  so default latency behaviour is byte-for-byte the 0.3.0 behaviour (`try_recv_from` + spin +
  `yield_now`). Opt-in is an **additive, `#[doc]`-documented inherent method** on `DpdkUdpSocket`
  and `compat::tokio::UdpSocket` (`set_idle_backoff(Option<Duration>)` / `idle_backoff()`), and
  `tokio-echo` gets a `--idle-backoff-ms` flag (`N` = idle threshold, min 2 ms). When enabled: escalate only after **`max(threshold, 2 ms)`** with
  no packet and sleep with `tokio::time::sleep` (documented ~1 ms granularity, so
  first-packet-after-idle latency can rise by up to ~1-2 ms — this tradeoff is stated in
  `API_COMPATIBILITY.md` and the rustdoc). **Time-driver requirement (D5 revised, MEDIUM-2 r3):**
  `tokio::time::sleep` requires the runtime's time driver (`enable_time` / `enable_all`, which
  `#[tokio::main]` turns on). `Handle::try_current()` only proves a runtime **context** exists; it
  **cannot** tell whether the time driver is enabled, so the round-2 "cannot panic" guarantee does
  **not** hold — using `set_idle_backoff` on a runtime built **without** timers panics exactly like
  any other `tokio::time` call ("time driver not running"). **Decision:** drop the "cannot panic"
  guarantee. `set_idle_backoff` **documents** that the runtime must have the time driver enabled;
  calling it (and then going idle) on a timer-less runtime panics, by design, the same as any
  `tokio::time::sleep`. Because the feature is **default-OFF**, 0.3.0 users who never opt in are
  **unaffected** (their path never touches the timer). This requirement/limitation is stated in
  `API_COMPATIBILITY.md` and the method rustdoc. The latency **GATE is dropped**: DUT p50/p99 are
  **REPORTED (non-gating)** for
  every DPDK config via `[PERF]` lines. Display precision: `lat_p50_us`/`lat_p99_us` are printed
  with **one decimal place** (`{:.1}`) derived from ns (`p99_ns as f64 / 1000.0`). In the
  done-gate, "non-zero p50/p99" means a printed value **> 0.0 at the 350k and 700k steps only**
  (lower steps may legitimately print 0.0 at `{:.1}`). Idle-CPU acceptance (≤ 10 % of one vCPU at
  0 pps) applies **with the backoff enabled**, measured in a dedicated harness step that runs
  `tokio-echo` with `--idle-backoff-ms`.

- **D6 (HIGH-5; MEDIUM-7 r3).** The round-1 per-step rule was self-contradictory (2a said non-700k
  steps are not gated; 5b/task said every step is gated) and the "≥ 99 % of offered rate" variant
  fails at 70k for every config (69,000/70,000 = 98.6 % from TRex warm-up loss, E0/E1/Run #51).
  **Decision:** exactly **one** per-step throughput rule, encoded in `perf-targets.json`: at rate
  steps **70k, 140k, 350k** (the **700k step is EXCLUDED** — MEDIUM-7) and every gated size,
  `tokio-dpdk` (fair) median `rx_pps` ≥ **98 % of the same run's `rust-dpdk` median `rx_pps`** at
  that step and size. The **700k step is judged only by the headline `tokio-dpdk ≥ 95 % of
  rust-dpdk` gate** (the brief set that headline at 95 % because a fair rep dipped to 95.7 %; a
  98 % per-step rule at 700k would silently override it). There is **no** "% of offered rate" rule.
  Design 2a and 5b both state this one rule, and tasks.md adds the
  implementing task.

- **D7 (HIGH-6).** `main()` calls `aggregate_results` inside the `set +e` reporting block and
  ignores its status; the script exits non-zero only when `failed_configs` is non-empty
  (`run-perf-tests.sh:~2730-2764`). **Decision:** the gate evaluator **writes its verdict to a
  status file**; `main()` reads that file **after** the `set +e` reporting block (and after
  `set -e` is restored), and exits **1** on a missed target (a test failure, per CLAUDE.md), **2**
  stays infrastructure failure (the existing `write_failure_json`/`set -e` path), **0** on pass.
  The exact control flow is shown against `run-perf-tests.sh:~2730-2764` in 2c below.

- **D8 (MEDIUM-7 r2 → HIGH-4 + MEDIUM-4 r3 — replaced).** The round-2 sync gate was `rust-dpdk ≥
  97 % of the 0.3.0 absolute numbers`, with a targeted re-run on a miss. That is unworkable: CI
  history shows `rust-dpdk` routinely well below 97 % of 0.3.0 on runs where **`native-dpdk` was
  equally low in the same run**, i.e. the cause was a slow **deployment**, not a regression — and a
  re-run on the **same stack** cannot clear a bad deployment, so PRs 2-6 would go red with no
  regression present. The real CI figures (from `docs/perf-test-log.md`) are:

  | Run | 512 B @700k rust | % of 0.3.0 (697,552) | 64 B @700k rust | % of 0.3.0 (698,948) | 512 B native |
  |---|---|---|---|---|---|
  | #41 | 621,635 | 89.1 % | 659,356 | 94.3 % | 632,683 |
  | #42 | 620,084 | 88.9 % | 658,996 | 94.3 % | 598,345 |
  | #45 | 620,449 | 88.9 % | 648,686 | 92.8 % | 627,577 |
  | #50 | 624,461 | 89.5 % | 675,214 | 96.6 % | 654,924 |

  4 of the last ~11 runs fall far below 97 % of 0.3.0, and in each the same-run `native-dpdk` is
  just as low (598k-655k at 512 B), confirming a deployment cause rather than a `rust-dpdk`
  regression.

  **Decision (replaced):** the sync no-regression gate is **deployment-normalized**. The gate is:
  **`rust-dpdk` median ≥ 95 % of the SAME RUN's `native-dpdk` median at 64 B and 512 B.** This is
  exactly the D3 `rust-dpdk ≥ native` gate — it now **also serves as the regression gate**, so
  there is one gate, not two, and it is immune to a slow deployment (both numbers move together).
  - The comparison against the **0.3.0 absolute numbers** (698,948 / 697,552 / 475,898 at
    64 / 512 / 1400 B) is **REPORT-ONLY**: the summary shows a "vs 0.3.0" column with a **WARN
    marker when below 98 %** (the brief's sync number — round-5 LOW-14), but it **never gates** (never fails the run). This keeps the
    historical absolute comparison visible without making a slow deployment a test failure.
  - **The targeted D8 re-run mechanism is REMOVED entirely** (it could not fix a deployment
    problem and complicated the control flow — MEDIUM-4). Reps 2-3 of the 700k step still run for
    median stability (D9), but there is no conditional 3-extra-rep re-run on a sync-gate miss.
  - **Native-dpdk sanity rule (invalid-run, not test-failure):** if the `native-dpdk` median at
    64 B is **< 600,000 pps**, the whole run is **INVALID** — a slow/bad deployment, not a test
    result — and the harness exits **2 (infrastructure failure)** and should be redeployed, rather
    than reporting a misleading gate FAIL (exit 1). 600k is a **safety net for grossly broken
    deployments** only: it is below every healthy native-dpdk figure, but it would NOT have fired
    for the moderately degraded runs (#41 #42 #45 #50 had native-dpdk @64 B of 656,738-677,629).
    Those runs are handled by the deployment-normalized rust-vs-same-run-native gate instead,
    which a uniformly slow deployment cannot fail (round-4 LOW-13).

  This replaces the 97 %-of-0.3.0 gate everywhere; the vs-0.3.0 comparison survives **only** as
  the report-only WARN threshold on the "vs 0.3.0" column, set at the brief's 98 %.

- **D9 (MEDIUM-4 r2).** `native-dpdk` is now the **sole gate reference** for the
  deployment-normalized `rust-dpdk ≥ 95 % of native` regression gate (D8/HIGH-4), so reps 2-3 must
  cover it too, and the aggregator currently keys results by `config_name`
  (`run-perf-tests.sh:~1783`), so rep2/rep3 files would overwrite rep1. **Decision:** reps 2-3
  cover `rust-dpdk`, `tokio-dpdk`, `tokio-dpdk-concurrent`, **and `native-dpdk`**; per-rep result
  files are keyed by **(config, rep)** and the aggregator reads them by that key (fix the
  `config_name` keying at ~1783); the 700k-only reps run only the **gated sizes (64/512/1400)**,
  never 8500; the time budget is recomputed as configs × reps × sizes × step duration plus
  per-config restart overhead, and `perf-tests.yml` `timeout-minutes` is set accordingly (see 2a).
  Note (MEDIUM-4 r3): the round-2 D8 re-run that ran in Phase 6 after teardown is **removed**
  (D8/HIGH-4), so reps 2-3 are the only extra passes and they run **inside the per-config loop
  before teardown**.

- **D10 (MEDIUM-1).** `has_buffered_packets` is set only by the pipeline recv paths today
  (`lib.rs:2891, 3087`); the inline paths and `process_frame_zerocopy` push into `recv_queue`
  **without** setting it (push sites 3405/3419/3451/3465/3497/3511/3557/3578/3619/3640 and
  `resolve_arp` 4090), so the round-1 plan to keep a pre-lock `has_buffered_packets` fast-path on
  the inline paths would strand buffered packets. **Decision:** `has_buffered_packets` becomes
  "**set on every `rx_state.queue` push on ALL paths** (pipeline and inline), cleared when the
  queue empties", so the pre-lock fast path is valid everywhere. Every push site that must set it
  is listed in the RX-path section.

- **D11 (MEDIUM-5 r2; MEDIUM-3 r3).** A compat `try_*` that pulls an ARP/ICMP/NDP frame needing an
  auto-reply must not block on the TX side. The auto-reply is a **prebuilt frame** and takes
  **only `q_tx0`** (no `tx_state` — MEDIUM-3). **Decision:** the inner non-blocking try path uses
  **`try_lock` on `q_tx0`** (only); if `q_tx0` is busy the reply is **DROPPED and counted** in
  `auto_reply_drops` (a new private `AtomicU64` on `UdpSocket`, read through an additive
  `#[doc(hidden)] pub fn auto_reply_drops(&self) -> u64`; not `rx_arp_handled`, which counts handled
  ARP frames, and not `rx_drops_buffer_full`, which feeds App Drops — round-6 LOW-4), never blocking. This is
  documented.

- **D12 (MEDIUM-8).** Folding `recv_queue` into `rx_state` means `process_frame_zerocopy` runs
  while `rx_state` is held; `std::sync::Mutex` is not reentrant, so its `recv_queue.lock()` sites
  must not re-lock. **Decision:** `process_frame_zerocopy` and every helper it calls take
  `&mut RxState` (or the held guard) as a parameter instead of locking `recv_queue`. The sites are
  listed below (the 10 push sites in `process_frame_zerocopy` 3405-3640, plus the pop/push in the
  inline/pipeline recv functions and the `resolve_arp` 4090 push). This is folded into the
  deadlock-freedom argument. **Leaf-lock rule (LOW-13):** `connected_addr`, `connection_state`,
  the ARP/NDP caches, and `error_queue` are **leaves** — they may be taken under the `rx_state`/
  `tx_state` guards but are **never held while acquiring a per-direction or queue guard**.

- **D13 (MEDIUM-2; HIGH-2 r3).** PR 4 had no tests reaching the Dpdk arm of `DpdkUdpSocket` or the
  compat socket (compat skips DPDK under stubs, `compat/tokio.rs:74`). The round-2 plan to test the
  Layer A guards "through `UdpSocketBuilder` so the real pipeline builder closures run" **cannot
  run in stub mode**: `detect_topology` always returns run-to-completion under `is_stub()`
  (`topology.rs:661-669`), so `UdpSocketBuilder::bind` never enters the pipeline branch
  (`lib.rs` pipeline branch ~4771), the `send_fn`/`recv_fn`/`direct_send_fn` closures (rows
  16/17/22) are never built, and their `q_tx0`/`q_rx0`/`q_tx_direct` counters can never advance in
  a plain `cargo test`. **Decision (revised, no topology override):** PR 3 **extracts the three
  pipeline closures into named free-function constructors** on `DpdkResources` —
  `fn make_send_fn(res: Arc<DpdkResources>) -> impl Fn(&[u8]) -> io::Result<usize>`,
  `fn make_recv_fn(res: Arc<DpdkResources>) -> impl Fn(usize, &FramePool) -> io::Result<Vec<FrameRef>>`,
  and `fn make_direct_send_fn(res: Arc<DpdkResources>) -> Arc<dyn Fn(&[u8], Option<u16>) -> io::Result<usize> + Send + Sync>`
  — which `UdpSocketBuilder::bind` calls in its pipeline branch (so the production path is
  unchanged). The guard-instrumentation tests then **call these constructors directly in stub mode**
  over a **stub `DpdkResources`** and invoke the returned closures, asserting each one acquired its
  guard (`make_send_fn` → `q_tx0`, `make_recv_fn` → `q_rx0`, `make_direct_send_fn` → `q_tx_direct`)
  via the debug-only per-guard acquisition counter. No test-only topology override and no forced
  non-RTC plan is introduced. Also add (a) **tokio-layer tests over a `Generic` mock backend** for
  `DpdkUdpSocket` and `compat::tokio::UdpSocket`, built on the **existing public**
  `UdpSocket::bind_with_backend(addr, Arc<dyn PacketBackend>)` (`lib.rs:2346`) — **no new
  `UdpSocket` constructor** is added (LOW-2); the tokio types get a thin `#[doc(hidden)]` **pub**
  helper only where they must wrap such a `UdpSocket` for cross-crate construction (LOW-9).

- **D14 (MEDIUM-3 r2; LOW-11 r3).** `sysstat` (mpstat/pidstat) is not installed on the DUT
  (`perf-test-stack.ts` dnf lines install `clang-devel unzip` at :291 (pre-built-AMI path) and
  `git pciutils iperf3 clang-devel unzip numactl` at :298 (full-bootstrap path) but neither
  installs `sysstat`), and no harness step holds a 0-pps window. **Decision:** add `sysstat` to
  **BOTH** dnf lines — **:291 (pre-built-AMI) and :298 (full-bootstrap)** — because the DUT takes
  whichever path applies at deploy time; adding it only to :291 would leave `mpstat`/`pidstat`
  missing when no pre-built AMI is found and the full bootstrap runs (LOW-11). This is a
  stack-level change, chosen because the perf stack is **deployed fresh on every run** (teardown
  defaults to `true`), so a UserData change **replaces instances only on a fresh deploy** — which
  CI always does — meaning there is **no live instance to hot-replace** and the change takes effect
  on the next deploy with zero replacement risk. Also add a harness task that runs the **0-pps
  window step** in `run-perf-tests.sh` (hold 0 pps, run `mpstat`/`pidstat`).

- **LOWs.** LOW-10: reuse the existing overflow tests at `lib.rs:5975`/`5999` for Req 2.8 (adapted
  to `rx_state`) instead of a new one. LOW-11: Req 3.2's generic-`recv_frames` statement is made
  consistent with Req 3.1a. LOW-12: the 23 % figure is attributed to burst copies **and**
  `async_trait` boxing together (not burst copies alone). LOW-1/LOW-2: the idle-CPU figure is
  relabelled "measured under load (≥ 70k pps)", and the 0.3.0 baseline "measured on `5c74658`,
  perf-identical to `f21ed1f` for these files (no diff in the perf path)".

## Overview

This design turns the five brief items into concrete changes, grounded in the code as it
exists on `development`. The ordering is driven by hard dependencies: the soundness fix
(Requirement 3) must land before the tokio outer mutex is removed (Requirement 4), because the
outer mutex is currently the only thing that keeps tokio users safe from the `TxBuffer` race
(`context/sandbox-results.md`, H3). The benchmark-mode work (Requirement 1) must land before
the perf gates (Requirement 2) because the gates judge the fair default. Requirement 5's tokio
idle-backoff edits touch the same `socket.rs`/`compat/tokio.rs` recv methods as Requirement 4,
so **PR 5 lands after PR 4** (LOW-9) to avoid conflicting edits.

All work is semver-additive: no public signature changes, no trait changes, `UdpSocket` stays
`Send + Sync`. The synchronization is added inside the private fields of `dpdk_udp::UdpSocket`
(per-socket `tx_state`/`rx_state`) and the shared, already-private `DpdkResources` (per-hardware-
queue guards), both invisible to the public API.

### Code anchors (verified against `development`)
Line numbers are the current positions on `development` and are expected to drift; the
function/struct name is the stable anchor. Where a range is given it brackets the whole item.

| Thing | Location |
|---|---|
| `struct DpdkResources` (shared per-port: `_eal`, `port`, `mempool`, `src_mac`, `arp_cache`, offloads, `mtu`). The `Port` is shared by every socket's `socket_backend`; its hardware queues are the real contended resource. | `dpdk-stdlib-udp/src/lib.rs` `struct DpdkResources` (~1287) |
| `pub struct UdpSocket` fields (incl. `tx_buf: TxBuffer` @~2069, `recv_queue: Mutex<ReceiveQueue>` @~2052, `has_buffered_packets: AtomicBool` @~2093, `recv_from_rr_index: AtomicUsize`, `socket_backend`, `resources: Arc<DpdkResources>`, `cached_direct_send`) | `struct UdpSocket` (~2033) |
| `struct TxBuffer { buf: UnsafeCell<Vec<u8>>, #[cfg(debug_assertions)] in_use: AtomicBool }`, `unsafe impl Sync for TxBuffer` (~1969), `TxBufferGuard` (~2001). NB: `in_use` is **debug-only** — release builds have no race check. | `struct TxBuffer` (~1961-2031) |
| `send_to` (~2523) → `send_to_addr` (~2535) → `send_to_v4` (~2544): borrows `self.tx_buf.borrow_mut()` @~2618, builds (plain / VLAN / VXLAN / GENEVE / GUE), then `cached_direct_send(&tx_buf, ..)` @~2705 **or** `self.socket_backend.send_frame(&tx_buf, ..)` @~2714 | `send_to_v4` (~2544) |
| `send_to_v6` (NDP solicitation `send_frame` @~2771 (LOW-8) is sent **before** `tx_buf.borrow_mut()`; final `send_frame` @~2793) | `send_to_v6` (~2737) |
| `SocketBackend::send_frame` — Dpdk arm calls `res.port.tx_burst(0, ..)` @**1603** (LOW-8); Generic arm calls `b.send_frame`. `SocketBackend::recv_frames` — Dpdk arm calls `res.port.rx_burst(0, ..)` @~1623; Generic arm calls `b.recv_frames`. | `SocketBackend::send_frame` (~1491), `recv_frames` (~1620) |
| **Pipeline builder closures** (`UdpSocketBuilder::bind` pipeline branch): `recv_fn` calls `resources_for_recv.port.rx_burst(0, ..)` @4799; **`send_fn` calls `resources_for_send.port.tx_burst(0, ..)` @4825 — queue 0, the same hardware queue as the RTC path (D1/CRITICAL-1)**; `direct_send_fn` calls `resources_for_direct.port.tx_burst(1, ..)` @4858 (the dedicated pipeline TX queue). PR 3 extracts these three closures into named constructors `make_recv_fn`/`make_send_fn`/`make_direct_send_fn` (HIGH-2). | `UdpSocketBuilder::bind` pipeline branch (~4795-4880) |
| `recv_from` → `recv_from_inline` (`res.port.rx_burst(0, 32)` **directly** @3157, HIGH-1) / `recv_from_pipeline` (~3026); `try_recv_from` → `try_recv_from_inline` (`res.port.rx_burst(0, 32)` **directly** @2924, HIGH-1) / `try_recv_from_pipeline` (~2850, single-consumer app-ring dequeue) | fn names above |
| `process_frame_zerocopy` — sends ARP / ICMP / NDP / ICMPv6 auto-replies via `self.socket_backend.send_frame` from the RX path (@3297, 3311, 3351, 3366) and pushes `payload.to_vec()` into `recv_queue` at many sites | `process_frame_zerocopy` (~3270) |
| `resolve_arp` — sends the ARP request (`send_frame` @4068), then **polls `recv_frames(32)` @4071 in a loop**, processing ARP replies via `send_frame` @4077 and pushing UDP into `recv_queue.lock()` @4090; up to 3 s (3 × 10 000 × 100 µs) | `resolve_arp` (~4056) |
| `dequeue_app_rings(app_rings, rr_index)` free function (pipeline single-consumer dequeue) | `dequeue_app_rings` (~4538) |
| `ReceiveQueue` (`VecDeque<(Vec<u8>, SocketAddr)>`, `max_packets`, `max_bytes`, `current_bytes`); `push`/`pop`/`len`/`bytes`/`max_bytes`/`set_max_bytes`. Accessors: `recv_queue_len` (~4137), `recv_buffer_size` (~4151), `set_recv_buffer_size` (~4169), `recv_buffer_bytes` (~4176) | `struct ReceiveQueue` (~1826) |
| `PerfCounters` (incl. `worker_idle_polls`, `worker_packets_processed`, `rx_drops_ring_full`, `rx_drops_buffer_full`); `RateSnapshot` already carries `rx_ring_drops`, `rx_buf_drops`, `worker_idle_pct`, `lat_avg_us`, `lat_max_us`. `PerfCounters`/`CounterSnapshot`/`RateSnapshot` are **not** `#[non_exhaustive]`. | `dpdk-stdlib-udp/src/perf.rs` `struct PerfCounters` (~74) |
| `PerfReporter` `[PERF]` log line: `ts_unix=.. interval=Ns .. rx_ring_drops=.. rx_buf_drops=.. .. lat_avg_us=.. lat_p50_us=.. lat_p95_us=.. lat_p99_us=.. lat_max_us=.. .. worker_idle_pct=..` (the DUT's own measured latency + idle%) | `perf.rs` `PerfReporter::reporter_loop` |
| `worker_idle_pct` computed from `worker_idle_polls / (worker_packets_processed + worker_idle_polls)`; both counters are incremented **only in the pipeline `worker_loop`** (`topology.rs` `fn worker_loop` @541: `worker_idle_polls` @555, `worker_packets_processed` @581) — **not** in `rx_loop`, which touches neither (HIGH-7). In RTC/tokio (single-core) mode there is no `worker_loop`, so both read 0 today. | `perf.rs` `CounterSnapshot::rates_since`; `topology.rs` `worker_loop` (~541, ~555) |
| `rx_drops_ring_full` is bumped **only** in the pipeline `rx_loop` when a worker ring is full (`topology.rs:~520`). `rx_drops_buffer_full` is bumped **only** by `record_rx_drop` (`lib.rs:4214`). The RTC RX path has **no `rx_loop`**, so in RTC/tokio mode `rx_drops_ring_full` stays 0 and only `rx_drops_buffer_full` populates (HIGH-7). | `topology.rs` `rx_loop` (~520); `lib.rs` `record_rx_drop` (~4209) |
| tokio `DpdkUdpSocket { inner: Arc<std::sync::Mutex<dpdk_udp::UdpSocket>> }` | `dpdk-stdlib-tokio/src/socket.rs:91` |
| compat `DpdkAsyncSocket { socket: Arc<std::sync::Mutex<dpdk_udp::UdpSocket>> }` (0.3.0; PR 4 → `Arc<std::panic::AssertUnwindSafe<dpdk_udp::UdpSocket>>`, D4/CRITICAL-1) | `dpdk-stdlib-tokio/src/compat/tokio.rs:49` |
| tokio recv spin: `try_recv_from` + `tokio::task::yield_now().await`; compat `try_recv_from`/`try_send_to`/`try_recv`/`try_send` use the **outer** `socket.try_lock()` → `WouldBlock("socket locked")` | `socket.rs` recv_from; `compat/tokio.rs` recv/recv_from/try_* |
| `tokio-echo` main receive loop (today always: `buf[..len].to_vec()`, `Semaphore::acquire_owned`, `tokio::spawn`, `response = format!("echo: {}", ..)`, `tokio::time::timeout(send_to)`) | `apps/tokio-echo/src/main.rs` main loop (~205-290) |
| perf harness config launcher `start_dut_tokio_dpdk`; config→runner `case` (~2599); DUT-log `case` (~2676); default `CONFIGS` (~40) | `scripts/run-perf-tests.sh` ~1172 |
| perf summary: `aggregate_results` python (**~1439** (LOW-8), `PERF_RE` parses `rx_ring_drops`/`rx_buf_drops`; configs keyed by `config_name` at **~1783** — MEDIUM-4/D9); `generate_markdown_summary` (~1800); UDP table `Lat Avg`/`Lat Max` columns come from the **TRex** result JSON (`r.get('lat_avg_us'/'lat_max_us')`, ~1881); self-check config lists (~1870, ~1956); `main()` runs `aggregate_results` inside the `set +e` reporting block and exits non-zero only if `failed_configs` is non-empty, at **~2730-2764** (HIGH-6/D7) | `scripts/run-perf-tests.sh` |
| perf workflow (`workflow_dispatch`, `inputs.configs` default **~L21**, second hard-coded configs fallback at **~L154** (LOW-7), `perf-tests` job `timeout-minutes: 90` at **~L77** (LOW-8)) | `.github/workflows/perf-tests.yml` |
| perf stack DUT user-data dnf installs — pre-built-AMI path `dnf install -y clang-devel unzip` at :291, full-bootstrap `dnf install -y git pciutils iperf3 clang-devel unzip numactl` at :298 — **no `sysstat`** on **either** line today; `sysstat` added to **both** (MEDIUM-3/D14/LOW-11) | `deploy/cdk/lib/perf-test-stack.ts` |

---

## Item 1 — Benchmark app modes (`apps/tokio-echo`)

### Decision
Add a `--mode {fair|concurrent}` arg defaulting to `fair`. Keep `--verbose` gating all
`from_utf8_lossy`/`format!`. Do not ship the experiment's `--runtime`/`--worker-threads`/`spawn`/
`legacy` flags; the shipped surface is just the two modes real users care about, which also keeps
the app's CLI stable for anyone scripting it.

### Structure
```
fn main loop:
  recv = select!(shutdown, socket.recv_from(&mut buf))
  stats.record_recv(len)
  match mode:
    Fair       => socket.send_to(&buf[..len], from).await        // one task, no spawn
    Concurrent => data = buf[..len].to_vec();                     // E2 "spawn" behaviour
                  permit = semaphore.acquire_owned().await;
                  spawn { timeout(socket.send_to(&data, from)) }  // echo EXACT bytes
  (verbose only) println!("[RECV] ... {}", from_utf8_lossy(&buf[..len]))
```
- `fair` echoes `&buf[..len]` directly: no `to_vec`, no spawn, no semaphore, no timeout.
- `concurrent` is exactly the E2 **"spawn"** pattern (MEDIUM-1): `buf[..len].to_vec()`, a
  `Semaphore` permit, `tokio::spawn`, and `tokio::time::timeout(send_to(..))`. It echoes the
  **exact received bytes** (`&data`) — it does **not** `format!("echo: {}", ..)` as the shipped
  app does today, and it does not call `from_utf8_lossy` on the hot path. It keeps `--workers`
  (semaphore size) and `--timeout`.
- Startup prints `mode=<fair|concurrent>` so DUT/perf logs record which pattern ran
  (Req 1.6). The shutdown `select!` and `disable_perf_reporting()` path are untouched (Req 1.7).
- An unknown `--mode` is rejected before the runtime starts (Req 1.3).

### Why fair is the default
E1 shows fair = 100 % of sync rust-dpdk at all three sizes on one core with 0 % kernel time.
`concurrent` is kept as its own perf config because it is the pattern a drop-in
`tokio::net::UdpSocket` user naturally writes, and it is what Item 4 improves.

---

## Item 2 — Perf gates, counters, latency, log discipline

### 2a. Repetitions and median (`scripts/run-perf-tests.sh`, D9)
- Add `tokio-dpdk-concurrent` to the default `CONFIGS` (line ~40) so one `workflow_dispatch`
  run deploys one stack and benchmarks `plain-rust`, `rust-dpdk`, `tokio-dpdk`,
  `tokio-dpdk-concurrent`, `native-dpdk`.
- **Rep 1** is the existing full sweep (all sizes × all rate steps), writing a per-rep file
  `${config}.rep1.json`.
- **Reps 2 and 3** re-run **only the 700k step at the gated sizes** (a benchmark invocation with
  `--rate-steps 700000 --packet-sizes <requested sizes ∩ applicable gate sizes>` — with default
  inputs that is `64,512,1400`, **never 8500**, D9; see "Gate applicability" in 2b) for each
  **requested and judged** config, on the **already-deployed** stack (no re-deploy — Req 2.12), writing
  `${config}.rep2.json` / `${config}.rep3.json`. A new `--rep-700k-only` path in the per-config
  loop drives these two extra passes right after rep 1, before tearing the config down.
- **The judged configs for reps 2-3 are `rust-dpdk`, `tokio-dpdk`, `tokio-dpdk-concurrent`, AND
  `native-dpdk`** (D9). `native-dpdk` **must** get reps 2-3 because its median is the **reference**
  for the `rust-dpdk ≥ 95 % of native` gate, and missing data for a judged (config, size) counts
  as FAIL — a single-rep reference would make that gate's denominator noisy and could spuriously
  fail. (`native-dpdk` emits no `[PERF]`, so only its TRex RX pps is used — latency stays `—`.)
- **Per-(config, rep) keying (D9).** Today the aggregator keys results by `config_name`
  (`run-perf-tests.sh:~1783`: `configs[name] = data`), so `${config}.rep2.json`/`.rep3.json` would
  **overwrite** rep1. Fix: the aggregator reads per-rep files by **(config, rep)** — e.g. group by
  a `(config_name, rep)` key parsed from the filename (`${config}.rep${N}.json`) or an added
  `"rep": N` field in each file — and collects the three 700k rows per
  (config, size) before taking the median. Rep 1's non-700k rows are read for the per-step rule
  (D6) and the report-only sizes; they are not overwritten.
- The aggregator takes, for each judged (config, size) at 700k, the **per-size median** of the RX
  pps across the three per-rep files (rep1's 700k row plus rep2/rep3). There is **no** conditional
  D8 re-run (removed — HIGH-4/MEDIUM-4). Non-700k steps come from rep1 only; the D6 per-step rule
  reads rep1's rows at 70k/140k/350k (the 700k step is **not** per-step-gated — MEDIUM-7).
- `tokio-dpdk-concurrent` launches `tokio-echo --mode concurrent` via a new
  `start_dut_tokio_dpdk_concurrent` runner that mirrors `start_dut_tokio_dpdk` (same
  `--perf-interval 10`, same verify-by-`pgrep` loop, log file `/var/log/echo-tokio-dpdk-concurrent.log`).
  The config→runner dispatch `case` (~2599) gains one arm and the DUT-log `case` (~2676) gains
  the matching log-file arm.

**End-to-end `tokio-dpdk-concurrent` wiring.** The new config must appear in **every** place a
judged config is enumerated, or it will silently not be judged:
1. `perf-tests.yml` `workflow_dispatch` `inputs.configs` **default** (~L21) **and the second,
   hard-coded `--configs` fallback at ~L154** (`${{ inputs.configs || '...' }}` — LOW-7, this
   second copy was missed in round 1) and the harness default `CONFIGS` (~L40);
2. the config→runner dispatch `case` (~2599) → `start_dut_tokio_dpdk_concurrent`;
3. the DUT-log `case` (~2676) → `/var/log/echo-tokio-dpdk-concurrent.log`;
4. the `generate_markdown_summary` per-size table config list (~1870) and the NIC self-check
   config list (~1956).
Missing data for any judged (config, size) is a **FAIL**, so an un-wired config cannot pass.

**Per-step throughput rule (D6/HIGH-5; MEDIUM-7).** The aggregator also enforces the **one**
per-step rule from `perf-targets.json` (`per_step_tokio_vs_rust`): at the rate steps **70k, 140k,
350k** (the **700k step is excluded** — MEDIUM-7) and every gated size, `tokio-dpdk` (fair) median
`rx_pps` ≥ **98 % of the same-run `rust-dpdk` median `rx_pps`** at that step and size. There is
**no** "% of offered rate" rule (that variant failed at 70k for every config — 69,000/70,000 =
98.6 % from TRex warm-up loss). Because the baseline is the same run's `rust-dpdk` (which sees the
identical warm-up loss), the ratio is warm-up-neutral. The **700k step is judged only by the
headline `tokio-dpdk ≥ 95 % of rust-dpdk` gate** (Req 2.2, done gate 1) — the brief set that
headline at 95 % because a fair representative rep dipped to 95.7 %, and a 98 % per-step rule at
700k would silently override it. A miss at a per-step (70k/140k/350k) prints `GATE FAIL: tokio-dpdk
per-step @ <size>B @ <step> rx_pps=<x> < 98% of rust-dpdk <y>` and fails the run via the 2c
verdict file.

**Time budget (D9 — recomputed with the per-size factor).** Rep 1 is the existing full sweep
(unchanged cost) plus one added config (`tokio-dpdk-concurrent`) ≈ one more single-config sweep.
Reps 2-3 run the 700k step at **3 gated sizes** for **4 judged configs** (`rust-dpdk`,
`tokio-dpdk`, `tokio-dpdk-concurrent`, `native-dpdk`):

```
reps 2-3 traffic = 4 configs × 2 reps × 3 sizes × DURATION(30 s)
                 = 24 × 30 s = 720 s ≈ 12 min of pure traffic
```

plus per-(config,size,rep) SSM/settle/verify overhead (restart of TRex streams and the
`pgrep`-verify loop between runs, roughly 20-40 s each → ~24 × ~30 s ≈ 12 min). There is **no** D8
re-run (removed — HIGH-4/MEDIUM-4). Rep 1 (the full sweep over 4 sizes × 4 steps × ~5 configs)
dominates and is unchanged except for the one extra config. **Total added work over today's run
≈ 25-35 min.** The current `perf-tests` job `timeout-minutes` is **90**; raise `perf-tests.yml`
`perf-tests` job `timeout-minutes` from **90 to 150** (headroom for the extra config and the
per-size reps) and state this budget inline in the workflow. Still `workflow_dispatch`-only and
one deployed stack (Req 2.12).

### 2b. Targets in one place (Req 2.3)
A committed data file (`scripts/perf-targets.json`) holds the step, the 0.3.0 baseline, and the
gates. **Every gate pins its own `sizes` list** (not a single shared `[64,512,1400]` — D2).

**Gate applicability (round-4 MEDIUM-3).** `perf-tests.yml` exposes `configs`, `packet_sizes`
and `rate_steps` inputs, so a dispatch can request a subset (a v6-only or 64 B-only run). The
evaluator judges a gate only for (config, size) pairs where the gated config, its `rel_to`
reference (if any) and the size are all in the run's **requested** lists (`CONFIGS` /
`PACKET_SIZES` / `RATE_STEPS` after the workflow inputs are applied): the 700k gates need 700k
requested (then the median is over reps 1-3; without 700k, reps 2-3 do not run), and the per-step
rule needs the step requested (round-5 MEDIUM-1). A gate that is not applicable is
**skipped** with a `gate skipped: <gate> (config/reference/size not requested)` line in the
summary and does not affect the verdict. For an applicable gate, missing data is FAIL. The
native-dpdk `invalid_run_rule` applies only when native-dpdk, 64 B and 700k were all requested;
native-dpdk requested but producing no data = INVALID (exit 2, infra), not a gate FAIL. Reps 2-3 run the
requested configs × (requested sizes ∩ the union of applicable gate sizes) at 700k, never a
hard-coded list. A run with default inputs judges every gate below. The idle-CPU gate (PR 5,
`"idle_cpu": {"max_pct_of_one_vcpu": 10, "enabled": true}`) is judged only when the 0-pps step ran.

The sizes below reflect the CI-history decisions D2, D3, D6, D8:
```json
{
  "step_pps": 700000,
  "all_rate_steps": [70000, 140000, 350000, 700000],
  "baseline_0_3_0": { "rust-dpdk": {"64":698948,"512":697552,"1400":475898} },
  "invalid_run_rule": {
    "config":"native-dpdk","size":64,"metric":"rx_pps","min_abs":600000,"exit_code":2,
    "note":"D8/HIGH-4: if native-dpdk median @64B < 600,000 the deployment is bad (not a regression). Mark the run INVALID and exit 2 (infra), do NOT report a gate FAIL (exit 1)."
  },
  "gates": {
    "tokio-dpdk":            {"rel_to":"rust-dpdk","min_ratio":0.95,"sizes":[64,512,1400],
      "note":"Headline tokio-vs-rust gate. 700k step judged ONLY here (not per-step — MEDIUM-7)."},
    "rust-dpdk": {"rel_to":"native-dpdk","min_ratio":0.95,"sizes":[64,512],
      "report_only_sizes":[1400],
      "note":"D3+D8/HIGH-4: this ONE deployment-normalized gate is BOTH the rust>=native gate AND the sync no-regression gate. Same-run native-dpdk is the denominator, so a slow deployment moves both numbers together and cannot cause a false regression. 1400B report-only (rust is 81-87% of native when TRex is not line-rate capped — runs #43 #47 #48 #49 #51). The vs-0.3.0 absolute comparison is report-only (see report_only_metrics.vs_0_3_0)."},
    "tokio-dpdk-concurrent": {"abs_min":290000,"sizes":[64],
      "report_only_sizes":[512,1400],
      "enabled":false,
      "note":"D2/LOW-3/MEDIUM-1: floor at 64B only (E2 spawn = 232k@512B, 176k@1400B); 512/1400 report-only. enabled=false => REPORT-ONLY (not gated). PR 4's diff flips enabled:true (PR 4 makes the concurrent path fast). Not keyed on PR_NUMBER."},
    "per_step_tokio_vs_rust": {
      "rel_to":"rust-dpdk","metric":"rx_pps","min_ratio":0.98,
      "rate_steps":[70000,140000,350000],"sizes":[64,512,1400],
      "note":"D6/HIGH-5/MEDIUM-7: the ONE per-step throughput rule, at 70k/140k/350k ONLY (700k excluded — judged by the headline tokio-dpdk 95%-of-rust gate). tokio-dpdk (fair) median rx_pps >= 98% of the SAME-RUN rust-dpdk median rx_pps at that step+size. No '% of offered rate' rule."}
  },
  "idle_cpu": {"config":"tokio-dpdk","max_pct_of_one_vcpu":10,"enabled":false,
    "note":"Round-5 LOW-3: added by PR 2 as enabled:false (the 0-pps step does not exist yet); PR 5 adds the 0-pps step (task 5.6a) and flips enabled:true. Judged only when the 0-pps step ran (process CPU via pidstat)."},
  "report_only_metrics": {
    "dut_latency": {
      "configs":["rust-dpdk","tokio-dpdk","tokio-dpdk-concurrent"],
      "metric":["dut_lat_p50_us","dut_lat_p99_us"],"source_fields":["lat_p50_us","lat_p99_us"],"precision":"{:.1} from ns",
      "rate_steps":[70000,140000,350000,700000],
      "note":"D5/HIGH-4: DUT p50/p99 are REPORTED, NOT gated. The idle backoff is opt-in and default OFF, so there is no latency hazard in default 0.3.1."
    },
    "vs_0_3_0": {
      "config":"rust-dpdk","rel_to":"baseline_0_3_0","sizes":[64,512,1400],
      "warn_below_ratio":0.98,"gates":false,
      "note":"D8/HIGH-4/LOW-4: the rust-dpdk-vs-0.3.0-absolute comparison is REPORT-ONLY (shown as a 'vs 0.3.0' column), with a WARN marker when below 98% (the brief's sync number). It NEVER gates. CI runs #41 #42 #45 #50 (TCP-crate/harness PRs that do NOT touch the UDP data path; #50 added the TCP benchmark runner; only #51 was docs-only) dipped below 97% from deployment variance with same-run native equally low (see D8), so gating on the absolute would fail clean runs. The deployment-normalized rust>=95%-of-native gate is the real regression gate."
    }
  }
}
```
The aggregator reads this file; changing a threshold is a data edit, not a code edit.

Key points encoded here (and matched by 2a/2c/2d/5b):
- **No latency gate** (D5/HIGH-4). DUT p50/p99 are report-only metrics, not a gate. The idle
  backoff that would have threatened latency is opt-in and default OFF.
- **Concurrent floor** is `abs_min 290000` at `sizes:[64]` **only** (D2); 512/1400 are
  `report_only_sizes`. It carries `"enabled": false` (MEDIUM-1) — a gate with `enabled=false` is
  **report-only**. PR 4's diff flips it to `"enabled": true`. This replaces the round-2
  `active_from_pr` field, which was wrongly compared against the GitHub `PR_NUMBER` (already past
  137, and empty on a `development` dispatch) instead of the ROADMAP ordinal.
- **Sync no-regression = the deployment-normalized `rust-dpdk ≥ 95 % of native` gate** at
  `[64, 512]` (D8/HIGH-4). There is **one** `rust-dpdk` gate, not two; it doubles as the
  regression gate. 1400 B is `report_only_sizes` (D3).
- **vs-0.3.0 is report-only** with a WARN below 98 % (D8/HIGH-4) — never gates.
- **Native sanity:** `native-dpdk` median @64 B < 600,000 ⇒ INVALID run, exit **2** (infra), not a
  gate FAIL (D8/HIGH-4).
- **One per-step rule** `per_step_tokio_vs_rust` at **70k/140k/350k only** (700k excluded —
  MEDIUM-7).

### 2c. Gate evaluation and exit control flow (Req 2.1-2.4, HIGH-6/D7)
After aggregation, a gate step computes per-(config,size) medians and compares to the targets in
`perf-targets.json`. On any miss it prints
`GATE FAIL: <config> @ <size>B median=<x> target=<y> (<pct>%)`. **Missing data for any judged
(config, size) counts as FAIL** (not skipped) so a dropped config or crashed DUT cannot silently
pass.

**Exit control flow (D7 — the round-1 bug).** Today `main()` calls `aggregate_results` **inside**
the `set +e` reporting block and ignores its status; the script exits non-zero only when
`failed_configs` is non-empty (`run-perf-tests.sh:~2730-2764`). A gate that exits inside
`aggregate_results` would be swallowed. So the gate evaluator **writes a verdict file** and
`main()` checks it **after** the reporting block:
```sh
# ~2730: set +e  (reporting block — must not mask the outcome)
aggregate_results                 # writes perf-report.json
summary=$(generate_markdown_summary)
evaluate_perf_gates               # NEW: reads perf-targets.json + per-(config,rep) files.
                                  #   FIRST applies the native-dpdk sanity rule: if native-dpdk
                                  #   median @64B < 600,000 it writes "$RESULTS_DIR/run-invalid.txt"
                                  #   — or, when native-dpdk was requested and applicable
                                  #   but produced NO data, also writes run-invalid.txt
                                  #   (round-5 MEDIUM-1 / round-6 LOW-7)
                                  #   (do NOT write a gate verdict — the run is an invalid deploy).
                                  #   Otherwise writes "$RESULTS_DIR/gate-verdict.txt" = PASS|FAIL
                                  #   and "$RESULTS_DIR/gate-lines.md" (GATE FAIL / gate skipped /
                                  #   WARN lines).
summary+=$'\n'"$(cat "$RESULTS_DIR/gate-lines.md" 2>/dev/null)"   # NEW: append the gate
                                  #   lines to $summary BEFORE post_pr_comment (round-5 LOW-13).
# ... post_pr_comment / step summary / collect logs (unchanged) ...
set -e                            # ~2762

# NEW (after set -e, before the failed_configs check):
# (a) Invalid deploy (native-dpdk too slow) → infra failure, redeploy, NOT a gate FAIL (D8/HIGH-4).
if [[ -f "$RESULTS_DIR/run-invalid.txt" ]]; then
    log_error "native-dpdk @64B < 600k pps — bad deployment, run INVALID (redeploy)"
    exit 2                        # infrastructure failure (CLAUDE.md: 2 = infra)
fi

# (b) A missing OR unreadable verdict file is a FAIL, not a silent pass (MEDIUM-5): if the
#     evaluator crashed inside `set +e` and wrote nothing, the run must NOT pass open.
verdict="$(cat "$RESULTS_DIR/gate-verdict.txt" 2>/dev/null)"
if [[ "$verdict" != PASS ]]; then   # covers FAIL, missing file, unreadable, empty, garbage
    log_error "Perf gate verdict is '${verdict:-<missing/unreadable>}' — see GATE FAIL lines above"
    exit 1                        # test failure (CLAUDE.md: 1 = test assertion failure)
fi

if [[ ${#failed_configs[@]} -gt 0 ]]; then   # existing infra/start failures
    log_error "${#failed_configs[@]} config(s) failed: ${failed_configs[*]}"
    exit 1
fi
```
Infrastructure/setup failures (deploy, SSM, TRex) still fail via the existing
`write_failure_json`/`set -e` path, which exits **2** (CLAUDE.md: 2 = infrastructure failure),
**independently** of the perf gates. So: **0** = all gates pass, **1** = a gate missed target OR
the verdict file is missing/unreadable (test failure, fail-closed — MEDIUM-5), **2** = infra
failure **including an invalid deploy** (native-dpdk @64B < 600k — D8/HIGH-4).

**No sync-gate re-run (D8/HIGH-4/MEDIUM-4 — removed).** The round-2 targeted re-run (re-run the
failing 700k step 3 more times on the same stack) is **removed entirely**: it could not clear a
bad deployment (the same stack reproduces it), and it ran in Phase 6 **after** `dut_stop_all_apps`
and `stop_trex_server` and after the summary was already posted (`run-perf-tests.sh:2723-2724`),
which was never specified to restart/re-aggregate. The deployment-normalized `rust-dpdk ≥ 95 % of
same-run native` gate (D8) and the native-dpdk < 600k invalid-run rule together handle deployment
variance without any re-run: a slow deployment moves rust and native together (gate still passes)
or drags native below 600k (run is INVALID, redeploy). Reps 2-3 of the 700k step still run for
median stability (D9), unconditionally, inside the per-config loop before teardown.

### 2d. DUT latency from `[PERF]` — REPORTED, not gated (Req 2.5, 2.6, D5/HIGH-4)
- **Keep the existing TRex `Lat Avg`/`Lat Max` columns** in the UDP per-size table — those come
  from the TRex result JSON (`r.get('lat_avg_us'/'lat_max_us')`) and measure wire round-trip; do
  not change them.
- **Add DUT-measured p50/p99** columns, parsed from the DUT's own `[PERF]` `lat_p50_us=` and
  `lat_p99_us=` fields (the reporter already emits them; today they only live in
  `dut-<config>-perf.log`). Extend `PERF_RE` / add a latency regex in `aggregate_results` to
  capture `lat_p50_us` and `lat_p99_us` per `[PERF]` tick, keyed by the tick's `ts_unix`, and
  store them in the result rows as **`dut_lat_p50_us` / `dut_lat_p99_us`**. The TRex rows already
  use `lat_p50_us` / `lat_p99_us` (`run_benchmark.py:150`, `run-perf-tests.sh:1928`), so the DUT
  values must not reuse those keys (round-4 LOW-7).
- **Step-attribution rule** (so DUT latency maps to the right rate step): each `[PERF]` tick
  carries `ts_unix` (end of its interval) and `interval=Ns`, giving a `[ts_unix - N, ts_unix]`
  window. Attribute a tick to a rate step **only if its entire interval lies inside that step's
  TRex time window** (the harness already buckets ticks into TRex step windows for App Drops).
  For the reported DUT p99 of a (config, size, step) use the **max of the per-interval p99s** of
  the fully-contained ticks — **do not average** p99s. p50 is reported the same way (max of
  contained per-interval p50s). Ticks that straddle a boundary are discarded.
- **native-dpdk is excluded** from DUT latency (testpmd emits no `[PERF]`); its DUT-latency cells
  show `—`.
- Add a **"vs target"** column to the per-size table (median pps as % of the config's applicable
  pps target, with PASS/FAIL) (Req 2.5).

**Display precision (D5).** DUT `lat_p50_us`/`lat_p99_us` are printed with **one decimal place**
(`{:.1}`) derived from ns (`p50_ns as f64 / 1000.0`). Today `perf.rs` prints them `{:.0}`
(`perf.rs:~608`), so sub-µs samples at low rate steps round to `0` — change the reporter's format
specifiers for these two fields to `{:.1}`. The summary carries the `{:.1}` values.

**Latency is REPORTED, not gated (D5/HIGH-4 — the round-1 latency gate is dropped).** The round-1
design gated `tokio-dpdk` p99 against the same-run `rust-dpdk` p99. That gate could not see the
hazard it was meant to guard: DUT `lat_p99_us` is measured from `rx_burst` return to the end of
frame processing (`lib.rs` recv paths) and **excludes NIC-ring queueing** — exactly where the 5b
sleeps would add latency — and the `rust-dpdk` baseline itself sleeps on empty polls, making
"tokio ≤ rust + 10 µs" nearly vacuous. Since the 5b idle backoff is now **opt-in and default OFF**
(D5/5b), default 0.3.1 has **no** latency hazard to gate. So:
- DUT p50/p99 are **reported** (non-gating) for every DPDK config that emits `[PERF]`
  (`rust-dpdk`, `tokio-dpdk`, `tokio-dpdk-concurrent`; `native-dpdk` shows `—`) as `[PERF]`-derived
  columns, at all four rate steps.
- There is **no latency GATE** and no `latency_tokio_dpdk` entry in `perf-targets.json`.
- In the done gate, **"non-zero p50/p99"** means a printed value **> 0.0 at the 350k and 700k
  steps only** (at `{:.1}`, the 70k/140k steps may legitimately print `0.0` for sub-100 ns
  samples; the hazard-relevant steps are the loaded ones).

### 2e. Tokio-mode counters (Req 2.7, 2.8, HIGH-7)
- **`worker_idle_pct` (Req 2.7, HIGH-7).** `worker_idle_pct` is derived from the existing
  `worker_idle_polls` / `worker_packets_processed` counters. **Correction (HIGH-7):** those two
  counters are incremented in the pipeline **`worker_loop`** (`topology.rs` `fn worker_loop` @541;
  `worker_idle_polls` @555, `worker_packets_processed` @581), **not** in `rx_loop` (which touches
  neither). In
  RTC/tokio (single-core) mode there is **no** `worker_loop`, so both read 0 today. Fix: wire the
  RTC/tokio `try_recv_from` path to increment the **same existing counters**: `worker_idle_polls`
  on an empty poll (no packet) and `worker_packets_processed` on a delivered packet. No new public
  fields are added to `PerfCounters` / `CounterSnapshot` / `RateSnapshot` (they are **not**
  `#[non_exhaustive]`, so adding a field would be a breaking change) — only the increment sites
  change. The reporter then observes non-zero idle% while the RTC receiver is idle. **Evidence
  source (LOW-9):** the done-gate check "`worker_idle_pct` > 0 while idle" reads the DUT's `[PERF]`
  log line from the **pre-traffic idle period** every run already has. `start_dut_tokio_dpdk`
  starts the app with `--perf-interval 10`, then sleeps 15 s and sets up TRex, so the first ticks
  have `rx_pps=0`. The harness takes the `worker_idle_pct=` field of the first zero-`rx_pps` tick
  per config and prints it in the summary as "idle%" (task 2.7a, PR 2). This lands with Req 2.7 in
  PR 2, so it doesn't wait for PR 5's dedicated idle-CPU window (round-4 LOW-5).
- **Drop counters (Req 2.8, HIGH-7, MEDIUM-6).** **Correction (HIGH-7):** `record_rx_drop`
  (`lib.rs:~4209`) bumps **only** `rx_drops_buffer_full` (`lib.rs:4214`); `rx_drops_ring_full` is
  bumped **only** in the pipeline `rx_loop` on a full worker ring (`topology.rs:~520`). So the
  round-1 claim that the RTC path feeds `rx_drops_ring_full` via `record_rx_drop` is wrong. In
  RTC/tokio mode there is no `rx_loop`, so `rx_drops_ring_full` legitimately stays **0** and only
  `rx_drops_buffer_full` (→ `rx_buf_drops`) populates — which is correct, because the RTC path has
  no worker ring to overflow; its only software drop is the receive-buffer overflow. The reporter's
  `rx_buf_drops` therefore populates for tokio configs exactly as for rust-dpdk.
- **The self-check criterion (MEDIUM-6 — replaced).** The round-2 criterion claimed the drop
  counters make "the NIC-vs-ring self-check balance (Δ = 0)". That is vacuous:
  `compute_nic_consistency_check` (`run-perf-tests.sh:1645-1697`) compares only NIC `imissed` /
  `ierrors` / `rx_nombuf` FINAL−BASELINE against the per-tick NIC deltas — it **never reads**
  `rx_ring_drops` / `rx_buf_drops`, and it already shows **OK** for tokio-dpdk in E0/E1. So it
  checks nothing about the drop counters. The round-3 replacement ("App Drops == Σ ring + buf")
  was a tautology too: App Drops **is** the sum of `RateSnapshot.rx_drops` = `rx_ring_drops +
  rx_buf_drops` (`perf.rs:286`, `run-perf-tests.sh:1757,1767`), so it holds even with both
  counters stuck at 0 (round-4 MEDIUM-1). **The criterion is therefore:** for the tokio configs the
  App Drops column is **present and populated** by the same aggregation as rust-dpdk (**0 is a
  valid value**; a healthy fair run drops nothing), and the counter-increment itself is proven by
  the stub overflow tests (next bullet). No harness cross-check is added.
- **Overflow-test verification (Req 2.8, LOW-10).** The existing tests prove the counter path:
  `:5975` calls `record_rx_drop` twice and asserts `rx_drops_buffer_full == 2` (no queue involved,
  no adaptation needed); `:5999` pushes past the receive-buffer limit and asserts
  `recv_drops().packets == 1`. Only `:5999` and `recv_buffer_bytes_tracks_queued_payload` (`:5985`)
  lock `recv_queue` and are adapted to `rx_state.queue` in PR 3 (round-7 LOW-1).
- This is measurement wiring only — no public API change.

### 2f. Log template + known-limitation rule (Req 2.9-2.11)
- Add a run-entry template block to `docs/perf-test-log.md` whose results tables carry both
  **vs last run** and **vs target** for each judged (config,size). Add the entry for the proving
  run.
- Add to the `docs/perf-test-log.md` header and to `AGENTS.md`: a "known limitation" / "No
  Regression on a flat ceiling" write-up MUST link a ROADMAP item; otherwise the reviewer treats
  the flat number as an unexplained regression.

---

## Item 3 — Thread-safe `dpdk_udp::UdpSocket` (per-direction guards)

The soundness bug has **two** distinct shared resources, at two levels:

1. **Per-socket state** — the `TxBuffer` scratch `Vec<u8>` (borrowed on every send) and the
   per-socket `recv_queue` buffered dequeue (`has_buffered_packets` + `recv_queue.lock()`).
2. **Shared hardware queues** — the DPDK `Port` inside the one `Arc<DpdkResources>`. Every
   socket's `socket_backend` sends on **TX queue 0** (`res.port.tx_burst(0, ..)`) and receives
   on **RX queue 0** (`res.port.rx_burst(0, ..)`). In pipeline mode a worker-direct TX path
   (`cached_direct_send`) sends on a **dedicated TX queue** and a `rx_loop` thread owns the RX
   burst. DPDK forbids concurrent `tx_burst`/`rx_burst` on the *same* hardware queue. This is
   the real cross-socket hazard: two sockets sharing one port, or one socket shared across
   threads, both reach the same queue 0.

A guard that only covers (1) leaves (2) racing. So the design uses **two layers of guards**,
built on `std::sync::Mutex` (uncontended ~11 ns, Stage 0) so there is no new lock-free protocol
and therefore no loom requirement (Req 3.8).

### Core decision: per-queue guards in `DpdkResources` + per-socket per-direction guards

#### Layer A — per-hardware-queue guards (in the shared `DpdkResources`)
Add one `Mutex<()>` per hardware queue that is actually driven, living in the shared
`DpdkResources` so **all** sockets on the same port serialize on the same guard:

```
struct DpdkResources {
    ...
    q_rx0: Mutex<()>,        // guards res.port.rx_burst(0, ..)
    q_tx0: Mutex<()>,        // guards res.port.tx_burst(0, ..)  (RTC send path)
    q_tx_direct: Mutex<()>,  // guards the pipeline dedicated-TX queue (cached_direct_send / tx1)
}
```

Only the queues this crate actually uses get a guard: **rx0**, **tx0**, and the pipeline
dedicated TX queue (**tx1**). The pipeline `rx_loop` thread also takes `q_rx0` for its burst, so
an app-thread `recv_from` on the same port cannot race the worker's `rx_burst`.

**Raw-burst call-site re-audit (HIGH-1).** `grep -nE 'rx_burst\(|tx_burst\(' dpdk-stdlib-udp/src/*.rs`
finds **7** raw burst call sites, **all in `lib.rs`** (none in `topology.rs` — the pipeline threads
call the `recv_fn`/`send_fn`/`direct_send_fn` closures passed into `start_pipeline`, so the raw
bursts live in the builder closures, not in `topology.rs`). The 7 sites are: `tx_burst(0)` @1603
(`send_frame` Dpdk arm), `rx_burst(0)` @1623 (`recv_frames` Dpdk arm), `rx_burst(0,32)` @2924
(`try_recv_from_inline`, **direct — not via `recv_frames`**), `rx_burst(0,32)` @3157
(`recv_from_inline`, **direct**), `rx_burst(0)` @4799 (`recv_fn` closure), `tx_burst(0)` @4825
(`send_fn` closure), `tx_burst(1)` @4858 (`direct_send_fn` closure). The caller-audit table below
has **exactly these 7 raw-burst sites** (rows 1, 2, 16, 17, 22 plus the two inline rows 18/20) plus
the `send_frame`/`recv_frames`-mediated callers; the count in the table matches this grep (7
raw-burst sites).

These guards are taken by (this is the **complete** set of raw-burst call sites — D1/CRITICAL-1,
HIGH-1):
- `SocketBackend::recv_frames` (Dpdk arm, `rx_burst(0)` @1623) → `q_rx0`;
- `SocketBackend::send_frame` (Dpdk arm, `tx_burst(0)` @1603) → `q_tx0`;
- **`try_recv_from_inline` (`rx_burst(0,32)` @2924) → `q_rx0`** and **`recv_from_inline`
  (`rx_burst(0,32)` @3157) → `q_rx0`** — the two **hottest RX sites**, which call `res.port.rx_burst`
  **directly** (not through `SocketBackend::recv_frames`) so they can iterate `Vec<Mbuf>` zero-copy
  (`recv_frames` returns copied `Vec<Vec<u8>>`). Round-2 omitted these from the Layer A list and
  wrongly routed row 18 "via `recv_frames`"; guarding only `recv_frames` would leave the RTC/tokio
  inline RX path racing, breaking the hard soundness constraint. They take `q_rx0` via the
  `recv_guarded` helper (see below);
- the pipeline builder **`recv_fn` closure** (`rx_burst(0)` @4799) → `q_rx0`;
- the pipeline builder **`send_fn` closure** (`tx_burst(0)` @4825) → **`q_tx0`** — the one the
  round-1 design missed. `rx_loop` calls `send_fn` to drain `tx_ring` and to emit ARP/ICMP replies
  (`topology.rs:445, 486, 498, 530`), so without this guard it would run `tx_burst(0)` concurrently
  with app-thread queue-0 sends that already take `q_tx0` (e.g. `resolve_arp` on an ARP miss). The
  `send_fn` closure now takes `q_tx0` before its `tx_burst(0)`, exactly like every other
  `tx_burst(0)` caller;
- the pipeline builder **`direct_send_fn` / `cached_direct_send` closure** (`tx_burst(1)` @4858)
  → `q_tx_direct`;
- the pipeline **`rx_loop` thread's** `rx_burst` (via `recv_fn`) → `q_rx0`.

**`recv_guarded` returns `Vec<Mbuf>` so the zero-copy inline path is unchanged (HIGH-1).** The
guarded RX helper for the **Dpdk arm** is `fn recv_guarded(&self, max: u16) -> DpdkResult<Vec<Mbuf>>`:
it takes `q_rx0`, calls `res.port.rx_burst(0, max)`, releases `q_rx0`, and returns the raw
`Vec<Mbuf>`. The inline paths (rows 18/20) then iterate the mbufs and read `mbuf.data()` directly
into the caller's buffer / the buffered queue, exactly as today — **no copy is introduced**. The
`SocketBackend::recv_frames` Dpdk arm (row 2) keeps returning copied `Vec<Vec<u8>>` for its generic
callers (e.g. `resolve_arp`), and it, too, takes `q_rx0` for its own `rx_burst`. So the Dpdk arm
has two guarded entry points sharing `q_rx0` — `recv_guarded` (zero-copy `Vec<Mbuf>`, used by the
inline hot path) and `recv_frames` (copied, used by the slow/ARP path) — while the **Generic** arm
only has `recv_frames` (backend self-syncs, no `q_rx0`).

Because `send_fn`/`recv_fn`/`direct_send_fn` are built in the pipeline branch of
`UdpSocketBuilder::bind`, the queue-guard acquisition is added **inside those closures** (they
capture `Arc<DpdkResources>`, which now carries the guards). To make the guards testable in stub
mode — where `detect_topology` always collapses to RTC (`topology.rs:661-669`) and the pipeline
branch never runs — PR 3 **extracts the three closures into named constructors**
`make_send_fn`/`make_recv_fn`/`make_direct_send_fn` on `DpdkResources` that `UdpSocketBuilder::bind`
calls; the guard-instrumentation tests (D13/HIGH-2) call these constructors **directly** over a
stub `DpdkResources` and invoke the returned closures, asserting each acquired its guard. No
topology override is used.

Generic (`PacketBackend`) backends do **not** use these guards — the trait's `send_frame`/
`recv_frames` take `&self` and each implementation is responsible for its own internal
synchronization (`RawSocketBackend` already is `Send + Sync`); routing them through the DPDK
queue guards would be wrong. The per-socket guards (Layer B) still serialize the generic path's
scratch/queue state.

#### Layer B — per-socket per-direction guards
Split the socket's own mutable state so a sender on one thread and a receiver on another never
block each other on the per-socket state (needed for Item 4, matching the H3 finding that the
hazard is per-direction):

```
struct UdpSocket {
    ...
    tx_state: Mutex<TxState>,   // replaces the bare `tx_buf: TxBuffer`
    rx_state: Mutex<RxState>,   // owns the buffered recv_queue + rr index (see MEDIUM-3)
    ...
}
struct TxState { scratch: Vec<u8> }     // was TxBuffer's UnsafeCell<Vec<u8>>
struct RxState { queue: ReceiveQueue, rr_index: usize }  // recv_queue folded in (MEDIUM-3)
```

The per-socket `tx_state` guard protects **only** this socket's TX scratch `Vec`. The per-socket
`rx_state` guard protects **only** this socket's buffered `recv_queue` dequeue and round-robin
index. The actual hardware `tx_burst`/`rx_burst` serialization is Layer A.

- `TxBuffer`, `unsafe impl Sync for TxBuffer`, and `TxBufferGuard` are **deleted** (Req 3.4,
  3.6). The `UnsafeCell` disappears; `Vec<u8>` scratch lives inside `TxState` behind the mutex.
  `UdpSocket` stays `Sync` because every field is `Sync` (`Mutex<..>`) (Req 3.5).
- The existing standalone `recv_queue: Mutex<ReceiveQueue>` field is **removed**; the queue moves
  into `RxState` so one lock covers both the burst-drain and the buffered dequeue (MEDIUM-3). The
  public accessors `recv_queue_len`/`recv_buffer_size`/`set_recv_buffer_size`/`recv_buffer_bytes`
  keep their signatures and now lock `rx_state` once per call and read `rx.queue` (MEDIUM-3);
  the lock is never held across a sleep or the 3 s `resolve_arp` wait.

### Guard poisoning (Req 3, LOW-6)
Every guard holds only primitive/`Vec`/`VecDeque`/`usize` data and performs **no** user callbacks
and no **`panic!`-prone** work inside the critical section (`copy_from_slice` lengths are
pre-clamped; no `unwrap`/`expect` on fallible values). Some critical sections **do** use the `?`
operator — e.g. `send_to_v4` propagates a frame-build error with `?` while holding the TX scratch
(LOW-5) — but `?` is an **early return**, not a panic: it drops the guard normally on the way out
and **does not poison** the mutex. (The round-2 claim "no `?` inside critical sections" was
inaccurate; the accurate invariant is "no path **panics** while holding a guard".) A lock
therefore cannot be poisoned by a panic while held. To be robust anyway, every `.lock()` uses a
poison-tolerant helper `fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<T> { m.lock().unwrap_or_else(|e| e.into_inner()) }` so a poisoned guard **recovers** (`into_inner`) instead of
propagating a permanent failure. This keeps the socket usable after an unrelated panic and is
documented in the SAFETY note that replaces the old `TxBuffer` comment.

### One global lock order (deadlock freedom)
There is exactly **one** total order over every lock any path can hold at once:

```
rx_state  <  q_rx0  <  tx_state  <  q_tx0  <  q_tx_direct
```

Rules that keep every path within this order:
- A pure send takes `tx_state` then `q_tx0` (or `q_tx_direct`), in order, and releases both
  before returning. It never takes any `rx_*` lock.
- A pure receive takes `rx_state` then `q_rx0`, in order. For an **auto-reply** emitted from the
  RX path (ARP / ICMP / NDP / ICMPv6 inside `process_frame_zerocopy`) it then takes **only `q_tx0`**
  (MEDIUM-3): the reply is a **prebuilt frame** (`send_frame(&reply_frame, None)`) that does **not**
  touch the per-socket TX scratch, so **no `tx_state` guard is needed** — only the hardware-queue
  guard `q_tx0` serializes the `tx_burst(0)`. The nesting is therefore `rx_state < q_rx0 < q_tx0`,
  which sits inside the global order (`tx_state` is simply not acquired on this path). This is why
  `q_tx0` sorts **after** the RX locks: the only nesting that exists is RX-then-TX, never
  TX-then-RX. (The brief's earlier "TX never takes RX" phrasing is retained only as the
  observation that the TX path acquires no RX lock; the ordering itself is the normative rule.)
- `process_frame_zerocopy` is called **with `rx_state` already held** and takes it by
  `&mut RxState` — it does **not** re-lock `rx_state` (non-reentrant, MEDIUM-8/D12). Its only
  lock acquisitions are the leaf `connected_addr`/`connection_state` reads (leaves, never held
  while acquiring a guard) and, for an auto-reply, **only `q_tx0`** (the reply is a prebuilt frame,
  so no `tx_state` — MEDIUM-3), acquired after `rx_state`/`q_rx0` in the global order.
- Leaf locks (`connected_addr`, `connection_state`, ARP/NDP caches, `error_queue`) are never held
  while acquiring any per-direction or per-queue guard (LOW-13), so they never create a cycle.
- The per-socket guards (`tx_state`, `rx_state`) are only ever held by **one socket at a time**;
  the shared queue guards (`q_rx0`, `q_tx0`, `q_tx_direct`) are the only cross-socket locks.
  Because a thread acquires at most one per-socket guard of each direction and then the matching
  shared queue guard, always in the single order above, no cycle can form.

### `resolve_arp` (the RX-then-TX path, proven safe)
`resolve_arp` is the trickiest path because it does TX then RX then TX, interleaved, for up to
3 s. We fix it so it still obeys the global order and never holds a per-socket guard across the
3 s wait:
- The initial ARP-request `send_frame` and each reply `send_frame` are **prebuilt frames** and
  acquire **only `q_tx0`** (no `tx_state` — MEDIUM-3) **for that one frame only**, then release.
- Each `recv_frames(32)` poll acquires `rx_state` → `q_rx0` **for that one burst only**, then
  releases; the `recv_queue.push` happens while still holding `rx_state` (the queue lives in
  `rx_state`), before any `q_tx0` acquisition for that iteration's replies.
- Between polls it `sleep`s holding **no** lock.
Because each iteration acquires locks in the order `rx_state < q_rx0 < q_tx0` (a subsequence of
the global order, with `tx_state` not acquired) and releases them before the next, and because
`resolve_arp` is always called from a send path **before** that send takes its own `tx_state`
(today it runs before `tx_buf.borrow_mut()`), it cannot be nested inside a held TX guard. No path holds two locks in an order that contradicts
the global order, so the lock graph is acyclic and deadlock-free.

### TX path changes (Req 3.1)
Every TX path first takes `let mut tx = lock_or_recover(&self.tx_state);` and uses
`&mut tx.scratch` where it used `self.tx_buf.borrow_mut()` today. It then calls the single
guarded send helper (see the HIGH-1 call table below) which takes the matching queue guard
(`q_tx0` for the RTC backend, `q_tx_direct` for `cached_direct_send`). Covers `send_to_v4`,
`send_to_v6`, and the VXLAN/GENEVE/GUE/VLAN branches that write into the scratch buffer. The
`send_to_v6` NDP solicitation `send_frame` is emitted **before** `tx_state` is taken (as today
relative to `borrow_mut()`); it is a prebuilt frame (`ndp_handler.make_solicitation`) and takes
**only `q_tx0`** via `send_guarded`, no `tx_state` — matching call-table row 6 (round-4 LOW-3).

Critical-section discipline: a per-socket guard is held only for CPU-only build+send, never
across an `await` or a blocking wait. `resolve_arp` (which can block) runs **before** the TX
lock is taken.

### RX path changes (Req 3.2)
`recv_from_inline` / `try_recv_from_inline` / `recv_from_pipeline` / `try_recv_from_pipeline`
take `let mut rx = lock_or_recover(&self.rx_state);` around:
- draining already-buffered packets from `rx.queue` (the former `recv_queue`),
- the backend burst via the single guarded receive helper (which takes `q_rx0`), and
- the single-consumer app-ring dequeue (`dequeue_app_rings` + `rx.rr_index`), preserving the SPSC
  single-consumer invariant that `try_recv_from_pipeline` violates today when called from two
  threads.
The guard is dropped before any sleep/`yield`. The `is_connected`/`has_connection_state` atomic
fast-path flags stay as pre-lock checks so the common empty case still skips the lock.

#### `has_buffered_packets` discipline (MEDIUM-1 / D10)
Today `has_buffered_packets` is set **only** by the two pipeline recv paths
(`try_recv_from_pipeline` @2891, `recv_from_pipeline` @3087) and is **never** set by the inline
path or by `process_frame_zerocopy`, which push into `recv_queue` at 10 sites without touching
the flag (3405, 3419, 3451, 3465, 3497, 3511, 3557, 3578, 3619, 3640) or by `resolve_arp` (4090).
Round 1's plan — keep a pre-lock `has_buffered_packets` check on the inline paths too — would
therefore **strand** inline-buffered packets (the flag reads `false`, so the lock and the queue
drain are skipped).

**Decision (D10):** `has_buffered_packets` is **set on every push into `rx.queue` on ALL paths**,
and **cleared when the queue empties** (on the pop that drains the last element). Because the
queue now lives in `RxState` and every push/pop happens while `rx_state` is held, the flag is
maintained under the guard and the pre-lock fast-path check is valid on **every** path (inline,
pipeline, and the `resolve_arp` poll). The push sites that must `set(true)` after a successful
`rx.queue.push(..)` are exactly:
- `process_frame_zerocopy`: GUE 3405/3419, VXLAN 3451/3465, GENEVE 3497/3511, IPv4 3557/3578,
  IPv6 3619/3640 (10 sites);
- `try_recv_from_pipeline` connected-filter re-buffer (~2887) and `recv_from_pipeline` (~3083)
  (2 sites, already set it today);
- `resolve_arp` UDP buffering (~4090).
Every pop path (`try_recv_from_pipeline` ~2857, `try_recv_from_inline` ~2913, the inline/pipeline
drain) clears the flag when `rx.queue.is_empty()` after the pop. Since all of these run while
`rx_state` is held, there is no race between the flag and the queue contents.

**Benign pre-lock reorder (LOW-10).** The `has_buffered_packets` fast-path reads the `AtomicBool`
**without** the lock. A concurrent push can therefore flip it between the read and the lock
acquisition, so a path that observed `false` may skip the buffered-queue drain and return a
**newer** burst packet before an **older** buffered one. This **reorders** datagrams but **never
loses** them (the older packet stays queued and is returned by the next `recv_from`), and UDP has
no ordering guarantee, so it is correctness-preserving. This benign reorder is documented here so
the behaviour is explicit rather than assumed away.

#### `process_frame_zerocopy` takes `&mut RxState` (MEDIUM-8 / D12)
Folding `recv_queue` into `rx_state` means `process_frame_zerocopy` runs **while `rx_state` is
already held** by the calling recv path. `std::sync::Mutex` is **not reentrant**, so
`process_frame_zerocopy` and every helper it calls must **not** re-lock — they take the already-
held state by `&mut RxState` (or `&mut rx.queue`) as a parameter and operate on it in place. The
sites that currently do `self.recv_queue.lock()` and must instead use the passed `&mut RxState`
are: the 10 `process_frame_zerocopy` push sites above (3405-3640) and the `resolve_arp` push
(4090). The pop/drain sites in `recv_from_inline`/`try_recv_from_inline`/`recv_from_pipeline`/
`try_recv_from_pipeline` already hold `rx_state`, so they read `rx.queue` directly. The public
accessors (`recv_queue_len`/`recv_buffer_size`/`set_recv_buffer_size`/`recv_buffer_bytes`) lock
`rx_state` once and read `rx.queue`. This "pass the guard, never re-lock" rule is part of the
deadlock-freedom argument below.

#### Leaf locks (LOW-13 / D12)
`connected_addr`, `connection_state`, the ARP cache (`arp_handler.cache`), the NDP neighbor cache
(`ndp_handler.cache`), and the error queue (`error_queue`) are **leaf** locks: they may be taken
**under** the `rx_state`/`tx_state` guards (e.g. the connected-address filter inside
`process_frame_zerocopy` reads `connected_addr` while `rx_state` is held), but they are **never
held while acquiring** a per-direction (`rx_state`/`tx_state`) or per-queue (`q_rx0`/`q_tx0`/
`q_tx_direct`) guard. They therefore cannot participate in a lock cycle and are omitted from the
primary lock order.

### Single guarded helper per direction (HIGH-1)
Every raw-queue call is routed through exactly one helper per direction so the queue guard is
impossible to forget. All callers of `send_frame`/`recv_frames`/`tx_burst`/`rx_burst` in
`dpdk-stdlib-udp` and the pipeline:

This table lists **every** `tx_burst`/`rx_burst` call site in `lib.rs` and `topology.rs`
(D1/CRITICAL-1 re-audit). None is left unguarded. "Takes `tx_state`?" notes whether the caller
also acquires the per-socket TX scratch guard (only callers that build into `tx.scratch` do;
callers that pass an already-built frame do not — LOW-6).

| # | Caller (fn / closure) | Dir | Raw call today (file:line) | Takes per-socket guard? | Guarded helper → queue guard |
|---|---|---|---|---|---|
| 1 | `SocketBackend::send_frame` (Dpdk arm) | TX | `res.port.tx_burst(0, ..)` (`lib.rs:1603`) | caller-dependent (see rows 3-12) | helper **is** the guard: `send_guarded` → `q_tx0` |
| 2 | `SocketBackend::recv_frames` (Dpdk arm) | RX | `res.port.rx_burst(0, ..)` (`lib.rs:1623`) | caller takes `rx_state` | takes `q_rx0` then `rx_burst`, returns **copied** `Vec<Vec<u8>>` (slow/ARP path). Distinct from `recv_guarded` (zero-copy `Vec<Mbuf>`, rows 18/20) which shares `q_rx0` |
| 3 | `send_to_v4` RTC branch | TX | `socket_backend.send_frame(&tx_buf, ..)` | **yes** (`tx_state`) | `send_guarded(&tx.scratch, ..)` → `q_tx0` |
| 4 | `send_to_v4` worker-direct branch | TX | `cached_direct_send(&tx_buf, ..)` | **yes** (`tx_state`) | `send_direct_guarded(..)` → `q_tx_direct` |
| 5 | `send_to_v6` (final send) | TX | `socket_backend.send_frame(&tx_buf, None)` | **yes** (`tx_state`) | `send_guarded(..)` → `q_tx0` |
| 6 | `send_to_v6` NDP solicitation | TX | `socket_backend.send_frame(&ns_frame, None)` (before `tx_buf` borrow) | no (prebuilt frame) | `send_guarded(..)` → `q_tx0` |
| 7 | `resolve_arp` request + ARP replies | TX | `socket_backend.send_frame(&frame, None)` (~4068, ~4077) | **no `tx_state`** (prebuilt frame — MEDIUM-3); per-iter order `rx_state < q_rx0 < q_tx0` | `send_guarded(..)` → **`q_tx0` only** |
| 8 | `process_frame_zerocopy` ARP reply | TX | `socket_backend.send_frame(&reply_frame, None)` (`lib.rs:3297`) | **no `tx_state`** (prebuilt — MEDIUM-3); already holds `rx_state` | `send_guarded(..)` → **`q_tx0` only** |
| 9 | `process_frame_zerocopy` ICMP reply | TX | `socket_backend.send_frame(&reply_frame, None)` (`lib.rs:3311`) | **no `tx_state`** (prebuilt — MEDIUM-3); already holds `rx_state` | `send_guarded(..)` → **`q_tx0` only** |
| 10 | `process_frame_zerocopy` NDP advert | TX | `socket_backend.send_frame(&na_frame, None)` (`lib.rs:3351`) | **no `tx_state`** (prebuilt — MEDIUM-3); already holds `rx_state` | `send_guarded(..)` → **`q_tx0` only** |
| 11 | `process_frame_zerocopy` ICMPv6 reply | TX | `socket_backend.send_frame(&reply_frame, None)` (`lib.rs:3366`) | **no `tx_state`** (prebuilt — MEDIUM-3); already holds `rx_state` | `send_guarded(..)` → **`q_tx0` only** |
| 12 | `send_arp_request` (`lib.rs:3955`) | TX | `socket_backend.send_frame(&frame, None)` | **no** (prebuilt frame, no `tx.scratch`) — LOW-6 | `send_guarded(..)` → `q_tx0` |
| 13 | `send_ndp_solicitation` (`lib.rs:3980`) | TX | `socket_backend.send_frame(&frame, None)` | **no** (prebuilt) — LOW-6 | `send_guarded(..)` → `q_tx0` |
| 14 | `send_gratuitous_arp` (`lib.rs:3996`) | TX | `socket_backend.send_frame(&frame, None)` | **no** (prebuilt) — LOW-6 | `send_guarded(..)` → `q_tx0` |
| 15 | `send_gratuitous_na_if_enabled` (`lib.rs:4012`) | TX | `socket_backend.send_frame(&na_frame, None)` | **no** (prebuilt) — LOW-6 | `send_guarded(..)` → `q_tx0` |
| 16 | **pipeline builder `send_fn` closure** (`lib.rs:4825`) | TX | `resources_for_send.port.tx_burst(0, ..)` | n/a (closure, no per-socket state) | **`q_tx0`** acquired inside the closure (D1/CRITICAL-1) |
| 17 | **pipeline builder `direct_send_fn` closure** (`lib.rs:4858`) | TX | `resources_for_direct.port.tx_burst(1, ..)` | n/a (closure) | **`q_tx_direct`** acquired inside the closure |
| 18 | `recv_from_inline` (Dpdk) | RX | `res.port.rx_burst(0, 32)` **directly** (`lib.rs:3157`, **not** via `recv_frames` — HIGH-1) | yes (`rx_state`) | `recv_guarded(32) -> Vec<Mbuf>` → `q_rx0` (zero-copy, iterate mbufs in place) |
| 19 | `recv_from_inline` (Generic) | RX | `backend.recv_frames(32)` | yes (`rx_state`) | generic path — **no DPDK queue guard**; the backend self-syncs, `rx_state` serializes this socket's buffered state (LOW-11) |
| 20 | `try_recv_from_inline` (Dpdk) | RX | `res.port.rx_burst(0, 32)` **directly** (`lib.rs:2924`, **not** via `recv_frames` — HIGH-1) | yes (`rx_state`) | `recv_guarded(32) -> Vec<Mbuf>` → `q_rx0` (zero-copy) |
| 20b | `try_recv_from_inline` (Generic) | RX | `backend.recv_frames(..)` | yes (`rx_state`) | generic path — **no DPDK queue guard** (backend self-syncs, LOW-11) |
| 21 | `recv_from_pipeline` / `try_recv_from_pipeline` | RX | app-ring dequeue only (the `rx_burst` happens on the `rx_loop` thread) | yes (`rx_state` around dequeue + `rr_index`) | `rx_state` (dequeue); the worker-side burst is row 22 |
| 22 | **pipeline builder `recv_fn` closure** (called by `rx_loop`, `lib.rs:4799`) | RX | `resources_for_recv.port.rx_burst(0, ..)` | n/a (closure) | **`q_rx0`** acquired inside the closure |
| 23 | `resolve_arp` poll | RX | `socket_backend.recv_frames(32)` (`lib.rs:4071`) | yes (`rx_state` per iteration) | the guarded `SocketBackend::recv_frames` (Dpdk arm → `q_rx0`; `Generic` → backend's own lock) — not `recv_guarded`, which is Dpdk-only and returns `Vec<Mbuf>` (round-5 LOW-2) |

`send_guarded`/`recv_guarded`/`send_direct_guarded` live on `DpdkResources` (they take the queue
guard then call the raw burst). For rows 16/17/22 (pipeline builder closures) the guard is taken
**inside the closure** (the closure captures `Arc<DpdkResources>`); these are the sites the
round-1 design missed. The per-socket `tx_state`/`rx_state` are always acquired by the caller
first, so the global order holds.

### Independence (Req 3.3)
`tx_state` and `rx_state` are separate per-socket mutexes, and `q_rx0`/`q_tx0` are separate
queue mutexes, so a sender and a receiver on different threads only contend on the shared
queue guards for the actual hardware burst — never on each other's per-socket state. The only
nesting is RX→TX for auto-replies, handled by the global order above.

### Alternatives considered
- *One socket-wide `Mutex<UdpSocket inner>`*: simplest, but senders and the receiver would
  serialize on the socket — defeats Item 4. Rejected.
- *Guard only the per-socket scratch/queue, not the hardware queues*: leaves the
  concurrent-`tx_burst(0)`/`rx_burst(0)` race (DPDK UB) when one socket is shared across threads
  or two sockets share a port. Rejected — this is exactly the second half of the bug.
- *Lock-free TX ring / per-thread scratch*: more complex, needs loom, and E1 shows throughput
  doesn't need it. Rejected per brief ("not a lock-free rewrite").
- *Keep `UnsafeCell`, add only a debug flag in release*: still UB under the data race. Rejected.

### Non-blocking try path for the tokio compat layer (D11/MEDIUM-5)
Item 4 removes the tokio outer mutex, so the compat `try_recv_from`/`try_send_to`/`try_recv`/
`try_send` can no longer return `WouldBlock` by failing `outer_mutex.try_lock()`. To preserve
"a `try_*` method never blocks", `dpdk_udp::UdpSocket` gains additive, non-blocking internal
try variants that `try_lock()` the relevant **per-direction** guard (`tx_state`/`rx_state`) and
the matching queue guard, returning `io::ErrorKind::WouldBlock` on contention instead of
spinning. `TryLockError::WouldBlock` maps to `io::ErrorKind::WouldBlock`;
`TryLockError::Poisoned(e)` is **recovered** with `e.into_inner()` (the same rule as
`lock_or_recover`, Req 3.4a) and the call proceeds — so a poisoned guard can never make `try_*`
return `WouldBlock` forever (round-4 LOW-6). The non-blocking contract covers only the new
per-direction and queue guards: on a `Generic` backend the backend's own synchronization still
applies (e.g. `DpdkBackend` uses a single `Mutex<Port>` for both TX and RX,
`backend_dpdk.rs:25`), so a compat `try_recv_from` over such a backend can wait behind a
concurrent sender; `API_COMPATIBILITY.md` SHALL say so (round-4 LOW-12). The existing `try_recv_from`/`send_to`/`send` keep their blocking-lock behaviour for
the RTC callers; the new try-on-contention entry points are what the compat `try_*` call (see
Item 4).

**Auto-reply during a non-blocking RX try (D11/MEDIUM-5).** A `try_recv_from` can pull an
ARP/ICMP/NDP/ICMPv6 frame whose auto-reply (inside `process_frame_zerocopy`) is a **prebuilt
frame** and needs **only `q_tx0`** (not `tx_state` — MEDIUM-3). To honour "never blocks", the
non-blocking try path **`try_lock`s `q_tx0`**: if `q_tx0` is busy, the auto-reply is **DROPPED and
counted** in `auto_reply_drops` (private `AtomicU64`, `#[doc(hidden)]` getter; not `rx_arp_handled` /
`rx_drops_buffer_full` — round-6 LOW-4) and the try
path continues — it never blocks waiting for the TX queue guard. (A dropped auto-reply is harmless:
ARP/NDP/ICMP are idempotent and the peer retransmits; the next uncontended `recv_from` answers.)
This is documented in `API_COMPATIBILITY.md` alongside the `try_*` contract. The **blocking**
`recv_from`/`try_recv_from` RTC paths keep taking `q_tx0` for the auto-reply normally (they are
allowed to block on the guard), so only the compat-driven non-blocking try path
drops-on-contention.

**Visibility (LOW-9).** These try entry points are called from `dpdk-stdlib-tokio`, a **different
crate**, so they cannot be private. They are `pub` but **`#[doc(hidden)]`** (and may be
`cfg`/feature-gated as internal) — `pub` for cross-crate callability, `#[doc(hidden)]` so they
are not part of the documented surface. They are additive, so semver-compatible.

**Pre-existing `try_send_to` ARP-miss caveat (LOW-6).** The `WouldBlock`-on-contention contract
covers **guard** contention. It does **not** remove the existing behaviour that a compat
`try_send_to` to an **unresolved** peer reaches `resolve_arp`, which can block for **up to ~3 s**
on an ARP miss. This is 0.3.0 behaviour and is **unchanged** by this spec (removing the outer mutex
does not touch `resolve_arp`'s blocking poll). It is **documented** in `API_COMPATIBILITY.md` next
to the `try_*` contract so the caveat is explicit, not implied away.

### Tests (Req 3.7, MEDIUM-2/D13, Req 2.8/LOW-10)
`dpdk-stdlib-udp/tests` (or an in-crate `#[cfg(test)]` module) gains a stub-backend stress test:
4 threads × ≥100k `send_to` each + a 5th thread doing `try_recv_from`, on one `Arc<UdpSocket>`.
Assert: no panic in debug (the old `TxBuffer` assertion is gone), and that every frame the mock
backend sees is intact.

**Mock `PacketBackend` validates in `send_frame`, keeps only counters (LOW-4/Req 3.7).** The mock
does **not** store the frames (that would be ~500 MB of stored frames in a debug build at
≥400k frames). Instead it **validates each frame inside `send_frame`** — parse to valid
IPv4/IPv6+UDP, verify the checksum, confirm the payload equals exactly one sender's byte pattern
(no interleaving) — and keeps only **counters** (`frames_seen`, `frames_bad`). The stress test
asserts `frames_bad == 0` and `frames_seen == expected`. This is the `context/race_check.rs`
scenario, promoted to a passing test, and stays within ordinary CI memory.

So that the changed guarded paths actually execute under plain `cargo test` (stub, no DPDK), add
(D13/MEDIUM-2):
- **Mock-backend drive of the `Generic` paths.** The mock above drives `SocketBackend::Generic`
  so `send_to`/`recv_from`/`try_recv_from` and `process_frame_zerocopy` run (the stress test and
  the Item 5a allocation test use it), not just stubbed out.
- **Tokio-layer tests over the `Generic` mock (MEDIUM-2).** Round 1 had no test reaching the Dpdk
  arm of `DpdkUdpSocket` or the compat socket, because compat **skips DPDK under stubs**
  (`compat/tokio.rs:74`) and `socket.rs` has no tests. Add tests that construct `DpdkUdpSocket`
  and `compat::tokio::UdpSocket` **over the `Generic` mock backend** and exercise
  `recv_from`/`send_to`/`try_recv_from`/`try_send_to` through the tokio layer (so the outer-mutex
  removal and the non-blocking try path are covered). The underlying `UdpSocket` is built with the
  **existing public** `UdpSocket::bind_with_backend` (`lib.rs:2346`) — no new udp-crate constructor
  (LOW-2). The tokio types need only a thin **`pub` `#[doc(hidden)]`** helper to wrap that
  `UdpSocket` for cross-crate construction (it must be `pub` because the tests live in
  `dpdk-stdlib-tokio`, LOW-9); it is additive and hidden, so semver-clean.
- **Guard-instrumentation tests via the named pipeline-closure constructors (MEDIUM-2/HIGH-2).**
  Round-2's "build through `UdpSocketBuilder`" approach **cannot run in stub mode**: `detect_topology`
  always returns RTC under `is_stub()` (`topology.rs:661-669`), so `UdpSocketBuilder::bind` never
  enters the pipeline branch and the builder closures (rows 16/17/22) are never constructed —
  their `q_tx0`/`q_rx0`/`q_tx_direct` counters can never advance in `cargo test`. Instead, PR 3
  extracts the three closures into named constructors `make_send_fn`/`make_recv_fn`/
  `make_direct_send_fn(res: Arc<DpdkResources>)` (the production builder calls them, so the real
  path is unchanged), and the tests **call these constructors directly over a stub
  `DpdkResources`** and invoke the returned closures. Assert, via a **debug-only per-guard
  acquisition counter** on `DpdkResources` (`cfg(test)` or feature-gated; an `AtomicU64` per guard
  bumped inside `send_guarded`/`recv_guarded`/`send_direct_guarded` and inside each constructed
  closure), that `make_send_fn`'s closure took `q_tx0`, `make_recv_fn`'s took `q_rx0`, and
  `make_direct_send_fn`'s took `q_tx_direct`. This proves each pipeline closure takes its guard
  without any topology override and without a non-stub NIC. The **RTC Dpdk-arm guards** (rows
  1/2/18/20) are NOT covered by the mock (`Generic` never takes `q_tx0`/`q_rx0`), so a separate
  **stub-Dpdk instrumentation test** covers them (round-5 MEDIUM-2): `UdpSocket::bind` (stub DPDK
  resources) + `add_arp_entry` for the peer, then `send_to` (the stub `tx_burst` returns 0, so the
  call errors after taking `q_tx0`) and `try_recv_from` (the stub `rx_burst` returns no packets, so
  it returns `Ok(None)` after taking `q_rx0`), asserting the `q_tx0` / `q_rx0` acquisition counters
  each advanced by ≥ 1, plus a `set_read_timeout(Some(10 ms))` + `recv_from` call (it returns
  `WouldBlock`/`TimedOut` after the deadline handling at `lib.rs:3128`) so the blocking inline site
  at 3157 is reached too. This covers `send_guarded` and both inline `recv_guarded` sites (2924
  via `try_recv_from`, 3157 via `recv_from` with the read timeout — round-6 LOW-2).
- **Receive-buffer overflow test (Req 2.8, LOW-10).** Reuse the **existing** overflow tests at
  `lib.rs:5975` (`socket_record_rx_drop_also_bumps_perf_counter`, asserts
  `rx_drops_buffer_full == 2`) and `lib.rs:5999` (`recv_buffer_overflow_records_drops`) rather
  than writing a new one. `:5975` calls `record_rx_drop` directly (no queue) and asserts
  `rx_drops_buffer_full == 2`; `:5999` pushes past the byte limit via `socket.recv_queue.lock()`
  and asserts `recv_drops().packets`; `recv_buffer_bytes_tracks_queued_payload` (`:5985`) also locks
  `socket.recv_queue` directly (line 5991). The two that touch `recv_queue` (5985, 5999) are adapted
  to `rx_state.queue`; 5975 needs no change (round-6 LOW-3).

### Docs (Req 3.9)
State in `API_COMPATIBILITY.md` and the `dpdk-stdlib-udp` README: `UdpSocket` is `Send + Sync`
and safe to share across threads via `Arc`; concurrent `send_to` and `recv_from` from multiple
threads are sound; TX and RX are internally serialized per direction, and all sockets sharing a
port serialize on the per-hardware-queue guards for the actual burst (so N senders are
serialized among themselves, as `std::net::UdpSocket` effectively is at the syscall level).

---

## Item 4 — Drop the tokio outer mutex

### Decision
Once Item 3 makes `&dpdk_udp::UdpSocket` safe for concurrent `send_to`/`try_recv_from`, both
tokio socket types hold `Arc<dpdk_udp::UdpSocket>` directly.

```
// socket.rs
struct DpdkUdpSocket { inner: Arc<dpdk_udp::UdpSocket>, local_addr, connected_addr }
// compat/tokio.rs  — AssertUnwindSafe restores the UnwindSafe/RefUnwindSafe the outer
// std::sync::Mutex used to confer (D4/CRITICAL-1). SAFETY: every guard uses lock_or_recover
// (into_inner) so a poison never propagates, and no half-updated public invariant survives a
// panic — see the SAFETY comment on the field.
struct DpdkAsyncSocket { socket: Arc<std::panic::AssertUnwindSafe<dpdk_udp::UdpSocket>>, local_addr }
```

- `send_to`/`send`/`recv_from`/`recv`/`peer_addr`/`take_error`/`recv_drops` call
  `self.inner.<method>(..)` directly (no `.lock().unwrap()`). In compat, the field is the
  `AssertUnwindSafe` wrapper (D4/CRITICAL-1), so bodies call `self.socket.<method>(..)` — `Deref`
  on `AssertUnwindSafe<T>` yields `&T`, so no body changes beyond the wrapper construction in
  `bind`/`from_std`.
- `try_recv_from`/`try_send_to`/`try_recv`/`try_send` in `compat/tokio.rs` today fail
  `outer_mutex.try_lock()` → `WouldBlock("socket locked")` on contention (`compat/tokio.rs:315,
  333`). With the outer mutex gone, they MUST still **not block** (D11/MEDIUM-5): they call the
  inner socket's additive non-blocking try entry points (see Item 3 "Non-blocking try path"),
  which `try_lock()` the per-direction + queue guards — **including `q_tx0` only (no `tx_state`,
  the reply is a prebuilt frame — MEDIUM-3) for any auto-reply**, dropping-and-counting the reply
  if `q_tx0` is busy (D11) — and return
  `io::ErrorKind::WouldBlock` on contention instead of spinning. The `WouldBlock` is now sourced
  from the inner guard contention rather than the outer mutex, so the method's observable contract
  (never blocks; `WouldBlock` when busy or no data) is unchanged.
- `connect`/`disable_perf_reporting` keep using `spawn_blocking` where they do today (they clone
  the `Arc` and call the inner method; `Arc<UdpSocket>` is `Send`). **`enable_perf_reporting` does
  NOT use `spawn_blocking` today** (`socket.rs:~180` calls `self.inner.lock().unwrap().enable_perf_reporting(interval)`
  directly) — LOW-5. After the mutex drop it becomes `self.inner.enable_perf_reporting(interval)`;
  no `spawn_blocking` is added.
- Public signatures and the `AsyncUdpSocket` trait are untouched (Req 4.2, 4.3).

### Auto-trait preservation — assert the 0.3.0 trait set each type actually has (HIGH-3/D4)
Dropping the `std::sync::Mutex` wrapper changes the field types of `DpdkUdpSocket` /
`DpdkAsyncSocket` / compat `UdpSocket`, which can silently change their auto traits. The round-1
design asserted **all five** traits (`Send`, `Sync`, `Unpin`, `UnwindSafe`, `RefUnwindSafe`) on
all three types — but **0.3.0 does not have all five on two of the three**, so that assertion
would not compile against 0.3.0 and is wrong. The actual 0.3.0 trait sets, read from the source:

| Type | 0.3.0 auto traits | Why not the other two |
|---|---|---|
| `dpdk_udp::UdpSocket` | **`Send + Sync + Unpin`** | holds `Arc<dyn PacketBackend>` (`lib.rs:1466`) and `Option<Arc<dyn Fn(..) + Send + Sync>>` (`cached_direct_send`, ~2087) — a `dyn` trait object without `+ RefUnwindSafe` is **not** `UnwindSafe`/`RefUnwindSafe`, so `Arc<UdpSocket>` is **not** `UnwindSafe` |
| `DpdkUdpSocket` | **`Send + Sync + Unpin`** | `connected_addr: Arc<tokio::sync::Mutex<..>>` (`socket.rs:93`); tokio's `Mutex` wraps an `UnsafeCell` with no `RefUnwindSafe` impl, so not `UnwindSafe`/`RefUnwindSafe` |
| `compat::tokio::UdpSocket` | **`Send + Sync + Unpin + UnwindSafe + RefUnwindSafe`** | all five — in 0.3.0 via `DpdkAsyncSocket`'s `std::sync::Mutex` (`RefUnwindSafe`); after PR 4 drops that mutex, via the `AssertUnwindSafe` wrapper (D4/CRITICAL-1) |

**Decision (D4/CRITICAL-1):** assert **exactly** the trait set each type has in 0.3.0, and keep the
0.3.1 types at **at least** these. The round-2 plan stored `Arc<dpdk_udp::UdpSocket>` in
`DpdkAsyncSocket`, which **removes** the `std::sync::Mutex` that was the sole source of the compat
type's `UnwindSafe`/`RefUnwindSafe`; because `Arc<UdpSocket>` is not `UnwindSafe`, the compat type
would **lose both traits** under `--features dpdk`. Since the `dpdk` feature is **unified in a
workspace build** (`tokio-echo` default-on, `synthetic-bench` always-on), the compat
`assert_impl_all!` would fail to compile from the workspace root, and dropping the assertion would
be an auto-trait semver break. So:
- `DpdkAsyncSocket.socket` is `Arc<std::panic::AssertUnwindSafe<dpdk_udp::UdpSocket>>` (an
  unwind-safe wrapper) — `AssertUnwindSafe<T>` is `UnwindSafe + RefUnwindSafe` for any `T` and is
  `Send + Sync + Unpin` when `T` is, so the compat type keeps all five traits **with and without**
  the `dpdk` feature. The SAFETY justification (guards recover from poisoning via `into_inner`; no
  broken public invariant survives a panic) is recorded in a SAFETY-style comment on the field.
- `DpdkUdpSocket` keeps a plain `Arc<dpdk_udp::UdpSocket>` — it never had
  `UnwindSafe`/`RefUnwindSafe` (its `connected_addr: Arc<tokio::sync::Mutex<..>>` is not
  `RefUnwindSafe`), so there is nothing to restore and no wrapper is needed.

Assertions:
- `static_assertions::assert_impl_all!(dpdk_udp::UdpSocket: Send, Sync, Unpin);`
  — **added to PR 3** (the PR that changes `UdpSocket`'s fields; round 1 had no task for this).
- `static_assertions::assert_impl_all!(DpdkUdpSocket: Send, Sync, Unpin);` — PR 4.
- `static_assertions::assert_impl_all!(compat::tokio::UdpSocket: Send, Sync, Unpin, UnwindSafe, RefUnwindSafe);`
  — PR 4.

**Compile-probe matrix (task 4.4):** the compat assertion must compile **(a) from the workspace
root** (where `dpdk` is unified on) **and (b)** under `cargo test -p dpdk-stdlib-tokio` **without**
the `dpdk` feature (Tokio-only variant). `cargo semver-checks` is run against 0.3.0 **both
`--features dpdk` and without the feature**, so an auto-trait regression is caught in whichever
build it appears. The "preserved by construction" claim is now true **only** for the compat type's
three non-unwind traits and for the `AssertUnwindSafe`-restored two — stated explicitly rather than
implied.

### Verification
`tokio-dpdk-concurrent` (Item 1 + Item 2) measures the win; its floor gate (`abs_min 290000` at
`[64]`) becomes **active in this PR** by flipping `gates["tokio-dpdk-concurrent"].enabled` from
`false` to `true` in `perf-targets.json` (D2/LOW-3/MEDIUM-1 — replacing the mis-compared
`active_from_pr` field).
Existing `compat/tokio.rs` loopback tests must still pass (Req 4.5), plus the new tokio-layer
tests over the `Generic` mock (D13/MEDIUM-2). Perf is verified by running the perf workflow
(CLAUDE.md step 5: `gh workflow run perf-tests.yml`), not `ci-validate.sh`.

---

## Item 5 — CPU-efficiency items

### 5a. Burst RX without per-packet `Vec` (Req 5.1-5.3)
Today `process_frame_zerocopy` returns the first matching packet via `buf` and pushes every
other matched packet's payload as `payload.to_vec()` into `recv_queue`
(`VecDeque<(Vec<u8>, SocketAddr)>`). This is the 23 % malloc/free in the fair flamegraph
(`flame-fair-mt-summary.txt`: `_int_free` 9.2 %, `__free` 3.0 %, `_int_malloc`).

(**LOW-12:** the 23 % malloc/free is attributed to **both** the per-packet `recv_queue` payload
`Vec` copies **and** `async_trait` boxing on the tokio layer — not to the burst copies alone. 5a
removes only the `recv_queue` payload allocation; the `async_trait` boxing is explicitly out of
scope for 0.3.1, see Req 5.7 / 6.1.)

Approach: give `ReceiveQueue` a reusable buffer pool so `push` reuses `Vec<u8>` capacity
instead of allocating a fresh one each burst. Concretely, add a free-list of `Vec<u8>` to
`RxState`/`ReceiveQueue`: `pop()` returns the payload to the caller's `buf` and recycles the
backing `Vec` into the free-list; `push()` takes a `Vec` from the free-list (reusing its
capacity) and copies the payload in. Steady-state this reaches zero per-packet allocations
because the burst size (≤32) and queue depth bound the live set.
- Keep `recv_queue`'s byte/packet limits and drop accounting exactly as today (Req 5.3).
- Public API unchanged (`recv_from`/`try_recv_from` return `(len, addr)` into the caller's buf).

Test (Req 5.2): a stub/mock backend delivers a burst of N>1 matching packets through
the Dpdk-shaped RX path (or a mock that returns frames without a per-frame allocation). A
**scoped** allocation counter (a `#[global_allocator]` counting shim in this test's **own test
binary**, read immediately before and after the single burst-refill call) asserts **zero**
per-packet payload heap allocations for the buffered (N−1) packets after warm-up, and that each
later `recv_from` returns the right payload+addr. The counter is scoped tightly around the
refill call so unrelated allocations elsewhere in the process do not pollute the count. Scope
note: `async_trait` boxing on the tokio layer is **out of scope** for 5a — this item only
removes the per-packet `recv_queue` payload `Vec` allocation.

### 5b. Idle backoff in the tokio receive path — OPT-IN, default OFF (Req 5.4-5.6, HIGH-4/D5)
Today `recv_from`/`recv` loop `try_recv_from` + `tokio::task::yield_now().await`, which pins a
full core under load (E1: 25 % of 4 vCPU measured **under load at ≥ 70k pps**, LOW-1 — 0 pps was
not measured in E1).

**Decision (D5): the idle backoff is opt-in and default OFF in 0.3.1.** Default recv behaviour is
**byte-for-byte the 0.3.0 behaviour** (`try_recv_from` + spin + `yield_now`), so a drop-in user
sees no change, no new panic risk, and no latency shift. There is therefore **no latency gate**
(the round-1 same-run p99 gate is dropped — see 2d; DUT p50/p99 are reported, not gated).

**Opt-in surface (additive, semver-clean):**
- An inherent `#[doc]`-documented method on `DpdkUdpSocket` and `compat::tokio::UdpSocket`:
  `set_idle_backoff(&self, Option<Duration>)` — `Some(d)` enables it with **`d` = the idle
  threshold** (time with no packet before escalating from `yield_now` to sleep), clamped to a
  minimum of 2 ms; the sleep step is fixed at the ~1 ms timer tick and is not configurable; `None`
  disables (round-5 MEDIUM-3) and `idle_backoff(&self) -> Option<Duration>`. These are **additive** inherent
  methods (not trait methods — `AsyncUdpSocket` is untouched, Req 5.7/4.3), so semver-clean.
- `tokio-echo` gets a `--idle-backoff-ms <N>` flag; default (flag absent) = OFF. Because
  `set_idle_backoff` is **inherent on `DpdkUdpSocket`** (not on `AsyncUdpSocket`, which is frozen —
  Req 4.3), and `bind_udp_with_config` returns an `Arc<dyn AsyncUdpSocket>` on which the method is
  not callable (HIGH-3), the flag is wired as: **when set**, bind `DpdkUdpSocket::bind` directly,
  call `set_idle_backoff(Some(N ms))` on the concrete value, then `Arc::from` it into
  `Arc<dyn AsyncUdpSocket>`; **when unset**, the binding path is unchanged
  (`bind_udp_with_config`). PR 1 only parses the flag (the method does not exist until PR 5, so
  PR 1 has **no reference** to `set_idle_backoff`); PR 5 adds the method and this wiring.

**Behaviour when enabled:**
- Track the instant of the last received packet. While `try_recv_from` keeps returning empty,
  keep using `yield_now()` (hot) **until the socket has been idle for the threshold `max(d, 2 ms)`**
  with no packet.
- After that threshold, escalate to `tokio::time::sleep(..)`. Because tokio's timer granularity is
  **~1 ms** (the runtime timer wheel rounds short sleeps up to roughly a 1 ms tick), the design
  sleeps in **fixed ~1 ms steps (no growth, no cap above 1 ms)**, so the documented ~1-2 ms
  first-packet-after-idle latency holds, and does **not** claim sub-ms precision (round-6 LOW-5). The idle timer
  resets to "just received" the instant a packet arrives, so no sleep happens while packets flow.
- **Latency tradeoff (documented in `API_COMPATIBILITY.md` and the rustdoc):** because the first
  sleep after going idle rounds to a ~1 ms tick, the first-packet-after-idle latency can rise by
  up to **~1-2 ms**. This is why the feature is opt-in.

**Time-driver requirement — no "cannot panic" guarantee (D5 revised / MEDIUM-2 r3).**
`tokio::time::sleep` requires the runtime's time driver. The round-2 plan claimed the method
"cannot panic" by checking `tokio::runtime::Handle::try_current()` before sleeping — but
`try_current()` only reports that a runtime **context** exists; it **cannot** detect whether the
**time driver** is enabled. So `sleep` still panics ("time driver not running") on a runtime built
with `new_multi_thread()`/`new_current_thread()` **without** `enable_time()`/`enable_all()`. The
guarantee does not hold, so it is dropped:
- `set_idle_backoff`'s rustdoc and `API_COMPATIBILITY.md` **require** a time-enabled runtime
  (`enable_time` / `enable_all`, which `#[tokio::main]` sets by default). Using the opt-in backoff
  on a runtime without timers panics exactly like any other `tokio::time` call — this is stated
  plainly, not papered over.
- The feature is **default-OFF**, so a 0.3.0 drop-in user who never calls `set_idle_backoff` never
  reaches the timer and is unaffected regardless of runtime flavour.
- Applied in both `socket.rs` and `compat/tokio.rs` DPDK recv paths, gated on the opt-in flag.

Because the escalation is keyed on **2 ms of real idleness**, a loaded receiver (packets every
few µs) never sleeps, so throughput is unaffected (Req 5.5).

**Throughput checks at the per-step rates (D6/MEDIUM-7).** The backoff must cost no throughput. The
gate step enforces the single per-step rule (2b `per_step_tokio_vs_rust`): at rate steps
**70k/140k/350k** (the **700k step is excluded** — it is judged by the headline `tokio-dpdk ≥ 95 %
of rust-dpdk` gate, MEDIUM-7) and gated size, `tokio-dpdk` median `rx_pps` ≥ **98 % of the same-run
`rust-dpdk` median** at that step and size. There is **no** "% of offered rate" rule. A miss at a
per-step rate fails the run via the 2c verdict file.

**Idle-CPU measurement with the backoff enabled (D5/D14).** Add a harness step that runs
`tokio-echo` **with `--idle-backoff-ms`** and holds a **0-pps window** (e.g. 30 s after the DUT
app starts, before TRex traffic begins), running `mpstat` and (preferred) `pidstat -p
<tokio-echo pid>` across it. Report the tokio-echo process CPU. The acceptance target "≤ 10 % of
one vCPU" converts, on the 4-vCPU DUT, to **≤ 2.5 % all-CPU** from `mpstat`'s all-CPU figure; the
preferred reading is the per-process `pidstat` CPU for the `tokio-echo` process, already
normalized to one vCPU. (This acceptance applies only with the backoff enabled; default-OFF
behaviour matches 0.3.0 and is not gated on idle CPU.)

### 5c. No trait change (Req 5.7)
All of the above is internal to `UdpSocket`/the tokio socket types. `AsyncUdpSocket` and
`async_trait` are untouched.

---

## Cross-cutting: semver & no-regression (Requirement 6)

- **Semver (Req 6.2, HIGH-3/D4)**: add `cargo semver-checks` against the 0.3.0 release for every
  published crate as a CI or pre-release step. The design adds only private fields, additive
  `#[doc(hidden)]`/inherent methods, and new test/harness/doc files, so it is additive.
  `UdpSocket: Send + Sync` is preserved by construction (every field is `Sync`). Auto-trait
  preservation is asserted with `static_assertions::assert_impl_all!` against **the trait set each
  type actually has in 0.3.0** (D4/CRITICAL-1): `Send + Sync + Unpin` for `dpdk_udp::UdpSocket`
  (PR 3) and `DpdkUdpSocket` (PR 4); `Send + Sync + Unpin + UnwindSafe + RefUnwindSafe` for the
  compat `UdpSocket` (PR 4). `DpdkUdpSocket`/`UdpSocket` never had `UnwindSafe`/`RefUnwindSafe` in
  0.3.0, so there is nothing to retain and `DpdkUdpSocket` needs no wrapper. The compat type **does**
  need the `AssertUnwindSafe` wrapper on `DpdkAsyncSocket.socket`: dropping the outer
  `std::sync::Mutex` would otherwise lose `UnwindSafe`/`RefUnwindSafe` because
  `Arc<dpdk_udp::UdpSocket>` is not `UnwindSafe` (the earlier "preserved by construction / no
  wrapper needed" claim was wrong). `cargo semver-checks` is run **both with and without
  `--features dpdk`** and independently catches any auto-trait regression against 0.3.0.
- **No sync regression (Req 6.3, D8/HIGH-4)**: the sync no-regression gate is
  **deployment-normalized** — `rust-dpdk` median ≥ **95 % of the SAME-RUN `native-dpdk` median** at
  64 B and 512 B (1400 B report-only) — which is the single `rust-dpdk ≥ native` gate doubling as
  the regression gate, judged on the median of 3 reps. It is immune to deployment variance because
  both numbers move together (CI history: #41/#42/#45/#50 had rust-dpdk at ~89 % of 0.3.0 at 512 B
  with same-run native equally low — a slow deploy, not a regression). The comparison against the
  0.3.0 absolute numbers is **report-only** (a "vs 0.3.0" column, WARN below 98 %) and never gates.
  If `native-dpdk` @64 B < 600,000 pps the run is **INVALID** (exit 2, redeploy). The round-2 97 %
  gate and its targeted re-run are **removed** (HIGH-4/MEDIUM-4). This sync check runs in **every**
  PR that touches the data path (PRs 3, 4, 5) via `gh workflow run perf-tests.yml`, not deferred to
  the final PR. The TX path adds two uncontended locks per send (per-socket `tx_state` + per-queue
  `q_tx0`, ≈ 2 × ~11 ns) — negligible vs the ~1 µs per-packet budget; Stage 0 confirms uncontended
  mutex cost is in the noise.
- **Latency is reported, not gated (Req 6.4 / D5)**: there is **no** latency gate. The 5b idle
  backoff that would have threatened latency is **opt-in and default OFF**, so default 0.3.1 recv
  latency is byte-for-byte 0.3.0's. DUT p50/p99 are surfaced in the summary (report-only,
  `{:.1}` from ns) so latency remains observable, and the done-gate's "non-zero p50/p99" check
  reads a printed value > 0.0 at the 350k and 700k steps only.
- **Stub-first tests (Req 6.5)**: every new unit/stress test runs under `cargo test` with no
  DPDK, using the stub/mock backend (Item 3 / MEDIUM-5).
- **No new unsafe (Req 6.6)**: the fix removes `unsafe impl Sync`/`UnsafeCell`; net `unsafe`
  decreases.

## Testing strategy summary
| Area | Test | Mode |
|---|---|---|
| Item 1 | `tokio-echo` arg parse: default `fair`, `concurrent` accepted, unknown rejected; fair echoes exact bytes; concurrent echoes exact bytes (no `format!`); `--idle-backoff-ms` sets opt-in backoff | `cargo test` / `assert_cmd` |
| Item 3 | 4+ sender threads + 1 receiver on one `Arc<UdpSocket>`, ≥100k sends each; mock `PacketBackend` validates frames **inside `send_frame`** and keeps only counters (`frames_bad==0`); no debug assertion fires | `cargo test` (stub, debug) |
| Item 3 | guard-instrumentation: call the named closure constructors `make_send_fn` / `make_recv_fn` / `make_direct_send_fn` directly on stub `DpdkResources` (the same functions `UdpSocketBuilder::bind` uses in its pipeline branch) and assert the per-guard acquisition counters (`q_tx0`/`q_rx0`/`q_tx_direct`) advance. Not built through `UdpSocketBuilder`, whose pipeline branch never runs under stubs (`detect_topology` is always RTC when `is_stub()`) — D13/HIGH-2, round-4 LOW-4 | `cargo test` (stub, `cfg(test)`) |
| Item 3 | reuse existing overflow tests `lib.rs:5975`/`5999` (adapted to `rx_state`) — receive-buffer overflow increments `rx_buf_drops` (Req 2.8/LOW-10) | `cargo test` (stub) |
| Item 4 | tokio-layer tests over the `Generic` mock for `DpdkUdpSocket` and `compat::tokio::UdpSocket` via a `pub #[doc(hidden)]` test constructor (D13/MEDIUM-2/LOW-9); existing loopback tests; `assert_impl_all!` asserting the **0.3.0 trait set per type** (`Send+Sync+Unpin` for both tokio types; +`UnwindSafe`+`RefUnwindSafe` for compat via the `AssertUnwindSafe` wrapper), compiled **from the workspace root (dpdk unified on) AND `-p dpdk-stdlib-tokio` without the feature** (D4/CRITICAL-1) | `cargo test` |
| Item 3/4 | `assert_impl_all!(dpdk_udp::UdpSocket: Send, Sync, Unpin)` (**added in PR 3**, D4) | `cargo test` |
| Item 5a | scoped counting allocator in own test binary: zero per-packet alloc for buffered burst packets | `cargo test` (stub) |
| Item 5b | DUT `pidstat`/`mpstat` 0-pps window **with `--idle-backoff-ms`** ≤ 10 % of one vCPU (≤ 2.5 % all-CPU on 4 vCPU) | perf workflow / manual |
| Items 1,2,4 | median-of-3 (700k) gates: tokio-dpdk ≥95 % of rust-dpdk @[64,512,1400] (sole 700k judge); rust-dpdk ≥95 % of **same-run native** @**[64,512]** (1400 report-only) = deployment-normalized sync gate (D8/HIGH-4), vs-0.3.0 report-only WARN<98 %; native @64B ≥600k else INVALID (exit 2); concurrent ≥290k @**[64]** (`enabled` flipped in PR 4); per-step rule tokio ≥98 % of same-run rust-dpdk at **70k/140k/350k only** (MEDIUM-7). DUT p50/p99 **reported, not gated** | perf workflow |
| Item 6 | `cargo semver-checks` + `assert_impl_all!` vs 0.3.0 | CI / pre-release |
