# Tasks: tokio-udp-perf (0.3.1)

Tasks are grouped into **PR-sized ROADMAP items** (~300-500 lines of diff each), in dependency
order. Each PR is independently buildable, testable, and semver-clean. The two hard orderings
from the design are respected:
- **Soundness fix (PR 3) before dropping the tokio outer mutex (PR 4).**
- **Benchmark modes (PR 1) before perf gates (PR 2).**

Every PR ends with `cargo build && cargo test` green from the workspace root (stub mode). Any PR
that touches the data path, the benchmark app or the harness (PRs 1-6) validates **perf** by running the
perf workflow — `gh workflow run perf-tests.yml`, polling with `gh run view --json
status,conclusion` (CLAUDE.md step 5) — **not** `ci-validate.sh`, which drives the integration
tier only. `ci-validate.sh` remains the integration-test entry point where integration coverage
is needed.

---

## PR 1 — tokio-echo fair/concurrent modes
**Boundary:** `apps/tokio-echo/src/main.rs` only (plus its tests). No library or harness change.
Ships independently; does not alter any published crate's API. ~150-250 lines.
**Depends on:** nothing.

- [x] 1.1 Add a `--mode {fair|concurrent}` clap arg to `Args`, defaulting to `fair`; reject
  unknown values with a non-zero exit and a clear message. (Req 1.3)
- [x] 1.2 Refactor the main receive loop so that after `stats.record_recv(len)` it branches on
  mode:
  - `fair`: `socket.send_to(&buf[..len], from_addr).await` in the same task — no `to_vec`,
    no `Semaphore`, no `tokio::spawn`, no `tokio::time::timeout`. (Req 1.1, 1.4)
  - `concurrent`: E2 "spawn" pattern — `to_vec` + `acquire_owned` permit + `tokio::spawn` +
    `tokio::time::timeout` — echoing the **exact received bytes** (`&data`), NOT the current
    `format!("echo: {}", ..)`. (Req 1.2)
  (Req 1.8: `--workers`/`--timeout` apply only in concurrent mode.)
- [x] 1.3 Gate all `String::from_utf8_lossy`/`format!` payload work behind `--verbose` in both
  modes; echo exact bytes when not verbose (removes the hot-path `format!` the app uses today).
  (Req 1.4, 1.5)
- [x] 1.4 Print `mode=<fair|concurrent>` at startup; keep the SIGTERM/SIGINT shutdown `select!`
  and the `disable_perf_reporting()` call intact so `[NIC-FINAL]` is still emitted. (Req 1.6, 1.7)
- [x] 1.4a Add a `--idle-backoff-ms <N>` clap arg (default absent = OFF). PR 1 **only parses the
  flag** and prints it at startup — it does **not** reference `set_idle_backoff` (that method only
  lands in PR 5, so referencing it in PR 1 would not compile — HIGH-3). The flag has no behavioural
  effect in PR 1. The full wiring (binding `DpdkUdpSocket` directly, calling `set_idle_backoff`,
  erasing to `Arc<dyn AsyncUdpSocket>`) is done in PR 5 task 5.4b. (Req 1.8)
- [x] 1.5 Add a test (LOW-10) that drives the `tokio-echo` binary via `assert_cmd` (or factors
  the receive loop into a function the test calls directly), forcing the **tokio** backend
  (`--force-tokio`), asserting that `fair` echoes **exact bytes** over loopback, that
  `concurrent` also echoes exact bytes (no `echo: ` prefix), and that `--mode bogus` exits
  non-zero. (Req 1.1, 1.2, 1.3)
- [x] 1.6 `cargo build && cargo test` from workspace root.
- [x] 1.7 **Run the perf workflow for PR 1 (LOW-7).** PR 1 changes the benchmarked app (the
  `tokio-dpdk` config now runs the fair loop, so its numbers move ~2.3×), and CLAUDE.md step 5
  applies to **every** code change. Run `gh workflow run perf-tests.yml`, poll with `gh run view
  --json status,conclusion`, and **record the new `tokio-dpdk` numbers** in a `docs/perf-test-log.md`
  entry. PR 1 adds no gates (the gate harness lands in PR 2), so this run is for recording the new
  baseline, not for gating. (Req 1.*, CLAUDE.md step 5)

---

## PR 2 — Perf gates, targets, latency, counters, log discipline
**Boundary:** `scripts/run-perf-tests.sh`, `.github/workflows/perf-tests.yml`,
`scripts/perf-targets.json` (new), `deploy/cdk/lib/perf-test-stack.ts` (add `sysstat`),
`docs/perf-test-log.md`, `AGENTS.md`, and the minimal `dpdk-stdlib-udp`/tokio wiring needed to
populate tokio-mode `worker_idle_pct`/drop counters in the reporter. No public API change.
~300-450 lines.
**Depends on:** PR 1 (gates judge `tokio-echo` fair default and the `--mode concurrent` config).

- [x] 2.1 Wire `tokio-dpdk-concurrent` end to end: add `start_dut_tokio_dpdk_concurrent`
  (mirror of `start_dut_tokio_dpdk`, launching `tokio-echo --mode concurrent --perf-interval 10`
  to `/var/log/echo-tokio-dpdk-concurrent.log`); add its arm to the config→runner dispatch
  `case` (~2599) and the DUT-log `case` (~2676); add it to the harness default `CONFIGS` (~40),
  the `perf-tests.yml` `inputs.configs` default (~L21), **AND the second hard-coded `--configs`
  fallback at ~L154** (`${{ inputs.configs || '...' }}` — LOW-7); add it to the
  `generate_markdown_summary` per-size table config list (~1870) and the NIC self-check list
  (~1956). (Req 2.1, 4.4)
- [x] 2.2 Add `scripts/perf-targets.json` with the 700k step, the `all_rate_steps`
  `[70000,140000,350000,700000]`, the 0.3.0 baseline numbers (698948/697552/475898), and the
  gate definitions with **each gate pinning its own `sizes` list** (D2 — not a shared
  `[64,512,1400]`):
  - `tokio-dpdk`: `rel_to:rust-dpdk, min_ratio:0.95, sizes:[64,512,1400]` (headline gate; also
    the sole judge of the 700k step — MEDIUM-7);
  - `rust-dpdk` vs native (**= the sync no-regression gate**, D8/HIGH-4):
    `rel_to:native-dpdk, min_ratio:0.95, sizes:[64,512], report_only_sizes:[1400]` — the ONE
    deployment-normalized `rust-dpdk` gate; there is NO separate `baseline_0_3_0` gate (D3+D8);
  - **native sanity:** top-level `invalid_run_rule` `{config:native-dpdk, size:64, min_abs:600000,
    exit_code:2}` — native @64B < 600k ⇒ INVALID run, exit 2 (D8/HIGH-4);
  - `tokio-dpdk-concurrent`: `abs_min:290000, sizes:[64], report_only_sizes:[512,1400],
    enabled:false` — report-only until PR 4's diff flips `enabled:true` (D2/LOW-3/MEDIUM-1; NOT
    keyed on PR_NUMBER);
  - `per_step_tokio_vs_rust`: `rel_to:rust-dpdk, metric:rx_pps, min_ratio:0.98,
    rate_steps:[70000,140000,350000], sizes:[64,512,1400]` — 700k excluded (D6/MEDIUM-7).
  Add `report_only_metrics.vs_0_3_0` (`rel_to:baseline_0_3_0, warn_below_ratio:0.98, gates:false`)
  so the vs-0.3.0 absolute comparison is a WARN-only column (D8/HIGH-4). There is **no**
  `latency_*` gate (D5). Add `report_only_metrics.dut_latency` listing the `[PERF]` p50/p99
  configs/steps (keys `dut_lat_p50_us`/`dut_lat_p99_us`) as **report-only**. Add the
  `idle_cpu` gate `{config:tokio-dpdk, max_pct_of_one_vcpu:10, enabled:false}`; PR 5 (task 5.6a)
  flips it to `enabled:true` when it adds the 0-pps step (round-5 LOW-3). (Req 2.2, 2.3, 5.4)
- [x] 2.3 Reps (D9): keep rep 1 as the full sweep, written to `${config}.rep1.json`; add a
  **700k-only rep mode** (`--rep-700k-only`, running `--rate-steps 700000 --packet-sizes
  <requested sizes ∩ applicable gate sizes>` — `64,512,1400` with default inputs, **never 8500**)
  that re-runs each requested and judged config **twice** more, only when 700k is requested
  on the already-deployed stack, writing `${config}.rep2.json`/`.rep3.json` (no re-deploy). The
  judged configs for reps 2-3 are the **requested** subset of `rust-dpdk`, `tokio-dpdk`,
  `tokio-dpdk-concurrent` **and `native-dpdk`** (native's median is the reference for the ≥ 95 %-of-native gate — D9). Raise the
  `perf-tests.yml` `perf-tests` job `timeout-minutes` from **90 to 150** and note the recomputed
  budget inline (4 configs × 2 reps × 3 sizes × 30 s ≈ 12 min traffic + overhead; no D8 re-run —
  removed, HIGH-4/MEDIUM-4). Reps 2-3 run **inside the per-config loop before teardown**. (Req 2.1,
  2.12)
- [x] 2.3a Fix the aggregator keying (D9/MEDIUM-4): today `configs[name] = data` keys by
  `config_name` (~1783), so rep2/rep3 files overwrite rep1. Read per-rep files by **(config, rep)**
  (parse the `.repN.json` suffix or an added `"rep":N` field), and collect all 700k rows per
  (config, size) before taking the median. (Req 2.1)
- [x] 2.4 In `aggregate_results`, compute the per-(config,size) **median** at 700k across the
  per-rep files; add an `evaluate_perf_gates` step that reads `perf-targets.json` and the
  per-(config,rep) files. **First apply the native-dpdk sanity rule** (`invalid_run_rule`): if the
  native-dpdk median @64B < 600,000 write `"$RESULTS_DIR/run-invalid.txt"` and stop gating (the
  deployment is bad — D8/HIGH-4). Otherwise compare each gate and print
  `GATE FAIL: <config> @ <size>B median=<x> target=<y> (<pct>%)` on any miss. The real regression
  gate is the **deployment-normalized** `rust-dpdk ≥ 95 % of same-run native-dpdk` @[64,512]
  (D8/HIGH-4); the vs-0.3.0 absolute is **report-only** (WARN below 98 %, never fails). Enforce the
  `per_step_tokio_vs_rust` rule at **70k/140k/350k only** (700k excluded — MEDIUM-7):
  `GATE FAIL: tokio-dpdk per-step @ <size>B @ <step> ...` (per-step values come from rep 1 only,
  a single sample — LOW-9). **Gate applicability (MEDIUM-3):** judge a gate only for (config,
  size) pairs where the gated config, its `rel_to` reference and the size were all **requested**
  (`CONFIGS` / `PACKET_SIZES` after workflow inputs); otherwise print
  `gate skipped: <gate> (config/reference/size not requested)` in the summary and do not fail.
  For an applicable gate, **missing data = FAIL** (not skip). The native-dpdk `invalid_run_rule`
  runs only when native-dpdk, 64 B and 700k were requested; native requested but no data = INVALID
  (exit 2). 700k gates are judged only when 700k is in `RATE_STEPS`; the per-step rule only at the
  requested steps among 70k/140k/350k (round-5 MEDIUM-1). Reps 2-3 derive their configs and sizes from the
  same requested lists (∩ each gate's sizes), never a hard-coded `64,512,1400`. The concurrent floor is gated only when
  `gates["tokio-dpdk-concurrent"].enabled == true` — `false` in PR 2 (report-only), flipped to
  `true` by PR 4's diff (MEDIUM-1; **not** keyed on `PR_NUMBER`). **No D8 re-run** (removed —
  HIGH-4/MEDIUM-4). (Req 2.1, 2.2, 2.4)
- [x] 2.4a **Exit control flow (D7/HIGH-6 + MEDIUM-5 + D8/HIGH-4):** `evaluate_perf_gates` writes
  `"$RESULTS_DIR/gate-verdict.txt"` = `PASS|FAIL` (and the GATE FAIL lines into the summary), or
  `"$RESULTS_DIR/run-invalid.txt"` on the native-dpdk sanity miss. `main()` runs it inside the
  `set +e` reporting block, and **after `set -e` is restored (~2762), before the `failed_configs`
  check**: (a) if `run-invalid.txt` exists → `exit 2` (infra, redeploy — bad deployment);
  (b) read the verdict and `exit 1` **unless it is exactly `PASS`** — a **missing or unreadable
  verdict file counts as FAIL** (fail-closed, MEDIUM-5), so an evaluator crash inside `set +e`
  cannot pass silently. Other infra failures still `exit 2` via the existing
  `write_failure_json`/`set -e` path. So 0=pass, 1=gate miss OR missing/unreadable verdict, 2=infra
  (incl. invalid deploy). Show the inserted block against `run-perf-tests.sh:~2730-2764`.
  (Req 2.2, 2.4)
- [x] 2.5 Add a **"vs target"** column to the per-size summary tables (median % of applicable
  target + PASS/FAIL). (Req 2.5)
- [x] 2.6 Add DUT p50/p99 latency columns parsed from the DUT's `[PERF]` `lat_p50_us`/
  `lat_p99_us` fields (extend/add a regex in `aggregate_results`), attributing a tick to a step
  only if its whole interval is inside the step's TRex window and reporting the **max** of the
  per-interval p99s/p50s (no averaging). **Change the reporter's format specifiers for
  `lat_p50_us`/`lat_p99_us` from `{:.0}` to `{:.1}`** (`perf.rs:~608`) so sub-µs values print.
  Keep the existing TRex `Lat Avg`/`Lat Max` columns unchanged. Exclude native-dpdk (no `[PERF]`).
  These columns are **report-only — there is NO latency gate** (D5). (Req 2.6)
- [x] 2.7 Wire tokio-mode counters so the `PerfReporter` observes non-zero `worker_idle_pct`
  while idle: increment the existing `worker_idle_polls`/`worker_packets_processed` counters on
  the RTC/tokio `try_recv_from` path (HIGH-7 — these counters live in `worker_loop` today, not
  `rx_loop`, and RTC has no `worker_loop`; no new public fields on `PerfCounters`/
  `CounterSnapshot`/`RateSnapshot`). `rx_drops_ring_full` legitimately stays 0 in RTC/tokio (no
  `rx_loop`); only `rx_drops_buffer_full` → `rx_buf_drops` populates via `record_rx_drop` (HIGH-7).
  The existing tests at `lib.rs:5975` (asserts `rx_drops_buffer_full == 2` via `record_rx_drop`) and
  `:5999` (asserts `recv_drops().packets == 1` after overflowing the buffer) are used **as-is** here; the **`rx_state` adaptation of those tests is done in PR 3** (task 3.7,
  since `rx_state` does not exist until PR 3 — LOW-3), not in this PR. Make sure the summary's
  **App Drops** column is **present and populated** for the tokio configs by the same aggregation
  as rust-dpdk (**0 is a valid value**). No App-Drops cross-check: App Drops is already the sum of
  `RateSnapshot.rx_drops` = `rx_ring_drops + rx_buf_drops`, so such a check is a tautology
  (round-4 MEDIUM-1); the stub overflow tests are the proof that the counter increments. (Req 2.7, 2.8)
- [x] 2.7a Extract tokio-mode **idle%** into the summary (Req 2.7 evidence source, LOW-5): for each
  `[PERF]`-emitting config, take the `worker_idle_pct=` field of the **first `rx_pps=0` tick**
  (the pre-traffic idle period every run already has — the app runs with `--perf-interval 10`
  while the harness sleeps 15 s and sets up TRex) and print it as an "idle%" column/line; for
  tokio configs it SHALL be > 0 after task 2.7. (Req 2.7)
- [x] 2.8 Add a run-entry template to `docs/perf-test-log.md` with **vs last run** AND
  **vs target** per judged (config,size). (Req 2.9)
- [x] 2.9 Add the known-limitation rule to the `docs/perf-test-log.md` header and to `AGENTS.md`
  (no "known limitation"/"No Regression on a flat ceiling" without a linked ROADMAP item).
  (Req 2.10)
- [x] 2.9a Install `sysstat` on the DUT (D14/MEDIUM-3/LOW-11): add `sysstat` to **BOTH** dnf lines
  in `deploy/cdk/lib/perf-test-stack.ts` — the **pre-built-AMI** line at **:291**
  (`dnf install -y clang-devel unzip`) **and** the **full-bootstrap** line at **:298**
  (`dnf install -y git pciutils iperf3 clang-devel unzip numactl`) — because the DUT takes
  whichever path applies at deploy time; adding it only to :291 leaves `mpstat`/`pidstat` missing
  when no pre-built AMI is found (LOW-11). This is a stack-level change rather than a run-time SSM
  install because the perf stack is **deployed fresh on every run** (teardown defaults to `true`),
  so a UserData change **replaces instances only on a fresh deploy** — which CI always does — hence
  **no live instance to hot-replace** and zero replacement risk. Note this reasoning inline.
  (Req 5.4) **The 0-pps-window measurement STEP that uses mpstat/
  pidstat lands in PR 5** (task 5.6a), since it must run `tokio-echo --idle-backoff-ms`.
- [x] 2.10 `cargo build && cargo test`; keep perf workflow `workflow_dispatch`-only. The log
  entry proving Req 2.11 is produced by PR 6's validation run.

---

## PR 3 — Thread-safe UdpSocket (per-direction guards) — SOUNDNESS FIX
**Boundary:** `dpdk-stdlib-udp/src/lib.rs` (replace `TxBuffer` with `tx_state`/`rx_state`
guards; thread through all TX/RX paths), a new stress test, and docs
(`API_COMPATIBILITY.md`, `dpdk-stdlib-udp` README). No changed or removed public API — only
additive `pub #[doc(hidden)]` items (the non-blocking try entry points, task 3.6); `UdpSocket` stays
`Send + Sync`. ~350-500 lines.
**Depends on:** nothing (but MUST land before PR 4).

- [ ] 3.1 Add the **per-hardware-queue guards** to the shared `DpdkResources` — `q_rx0`,
  `q_tx0`, `q_tx_direct` (`Mutex<()>`) — and the guarded helpers: `send_guarded` → `q_tx0` +
  `tx_burst(0)`, `send_direct_guarded` → `q_tx_direct` + `tx_burst(1)`, and **two RX helpers that
  share `q_rx0`**: `recv_guarded(max) -> Vec<Mbuf>` (takes `q_rx0`, calls `rx_burst(0,max)`,
  returns the raw mbufs so the inline zero-copy path can iterate them in place — HIGH-1) used by
  `recv_frames`-bypassing direct callers, and the `SocketBackend::recv_frames` Dpdk arm (which
  takes `q_rx0` then returns **copied** `Vec<Vec<u8>>` for the ARP/slow path). Route **every** one
  of the **7 raw-burst sites** from the HIGH-1 table through these helpers — including the two
  direct inline RX sites `res.port.rx_burst(0,32)` at `lib.rs:2924` (`try_recv_from_inline`) and
  `lib.rs:3157` (`recv_from_inline`), which call `rx_burst` **directly, not via `recv_frames`** —
  so no RX site is left racing (Generic `PacketBackend` paths excepted — they self-sync).
  (Req 3.1a, 3.2)
- [ ] 3.2 Introduce per-socket `tx_state: Mutex<TxState>` (scratch `Vec<u8>`) and
  `rx_state: Mutex<RxState>` (the former `recv_queue` folded in + the round-robin index) on
  `UdpSocket`; remove the standalone `recv_queue: Mutex<ReceiveQueue>` field; delete
  `struct TxBuffer`, `unsafe impl Sync for TxBuffer`, and `TxBufferGuard`. Add the
  poison-tolerant `lock_or_recover` helper (`into_inner`) used by every `.lock()`. (Req 3.1,
  3.4, 3.4a, 3.5, 3.6)
- [ ] 3.3 Convert every TX path to take `tx_state` then the guarded send helper where it used
  `self.tx_buf.borrow_mut()` + `send_frame`/`cached_direct_send`: `send_to_v4`, `send_to_v6`
  (incl. the pre-lock NDP solicitation), and the VXLAN/GENEVE/GUE/VLAN branches. Keep blocking
  `resolve_arp` before the TX lock. (Req 3.1)
- [ ] 3.4 Convert every RX path to take `rx_state` around draining `rx.queue`, the guarded
  receive helper, and the single-consumer app-ring dequeue (`dequeue_app_rings` + `rx.rr_index`):
  `recv_from_inline`, `try_recv_from_inline`, `recv_from_pipeline`, `try_recv_from_pipeline`,
  v4 and v6; drop the guard before any sleep/`yield`. Keep the `is_connected`/
  `has_connection_state` atomic fast-path checks before the lock, and update the public accessors
  (`recv_queue_len`/`recv_buffer_size`/`set_recv_buffer_size`/`recv_buffer_bytes`) to lock
  `rx_state` once per call. Make `process_frame_zerocopy` and the helpers it calls take
  `&mut RxState` (or `&mut rx.queue`) — **not** re-lock `recv_queue` (non-reentrant, D12/MEDIUM-8).
  The sites that must change from `self.recv_queue.lock()` to the passed state are the 10 push
  sites in `process_frame_zerocopy` (3405, 3419, 3451, 3465, 3497, 3511, 3557, 3578, 3619, 3640)
  and the `resolve_arp` push (4090). (Req 3.2)
- [ ] 3.4a `has_buffered_packets` discipline (D10/MEDIUM-1): set the flag `true` after **every**
  successful push into `rx.queue` on **all** paths — the 10 `process_frame_zerocopy` sites, the
  pipeline re-buffer sites (~2887, ~3083), and the `resolve_arp` push (~4090) — and clear it when
  `rx.queue.is_empty()` after a pop. Then the pre-lock `has_buffered_packets` fast-path check is
  valid on the inline paths too (round 1 left the inline paths stranding buffered packets). All
  push/pop happen under `rx_state`, so flag and contents stay consistent. (Req 3.2)
- [ ] 3.5 Make the ARP/ICMP/NDP/ICMPv6 auto-replies inside `process_frame_zerocopy` take
  **only `q_tx0`** for their guarded send — they are **prebuilt frames**, so **no `tx_state`** is
  acquired (MEDIUM-3) — and rework `resolve_arp` (whose request/reply sends are also prebuilt) to
  acquire/release locks per iteration in the subsequence `rx_state < q_rx0 < q_tx0` (no `tx_state`),
  holding **no** lock across the sleep. Document the single global lock order
  `rx_state < q_rx0 < tx_state < q_tx0 < q_tx_direct`, the leaf-lock rule (`connected_addr`,
  `connection_state`, ARP/NDP caches, `error_queue` are leaves — taken under rx/tx guards, never
  held while acquiring a guard — LOW-13/D12), and the deadlock-freedom argument (including that
  `process_frame_zerocopy` runs with `rx_state` already held and takes `&mut RxState`). (Req
  3.3, 3.3a)
- [ ] 3.5a **Extract and guard the pipeline builder closures (D1/CRITICAL-1 + D13/HIGH-2).**
  Refactor the three inline pipeline closures in `UdpSocketBuilder::bind` into named free-function
  constructors on `DpdkResources` — `make_send_fn(res)`, `make_recv_fn(res)`,
  `make_direct_send_fn(res)` — and have the builder call them (production path unchanged). Inside
  each constructed closure acquire the queue guard before its raw burst: `make_send_fn` → `q_tx0`
  (before `tx_burst(0)` @4825 — the site round 1 missed), `make_recv_fn` → `q_rx0` (before
  `rx_burst(0)` @4799), `make_direct_send_fn` → `q_tx_direct` (before `tx_burst(1)` @4858). This
  closes the concurrent-`tx_burst(0)` race between `rx_loop`'s `send_fn` calls (draining
  `tx_ring`/ARP/ICMP replies, `topology.rs:445,486,498,530`) and app-thread queue-0 sends, and
  makes the guards testable in stub mode (task 3.7 calls the constructors directly, since
  `detect_topology` is always RTC under stubs — `topology.rs:661-669`). (Req 3.1a)
- [ ] 3.6 Add additive **`pub` `#[doc(hidden)]`** (not private — LOW-9) non-blocking `try_*`
  entry points on `UdpSocket` that `try_lock` the per-direction + queue guards and return
  `WouldBlock` on contention (consumed by PR 4's compat `try_*`). The RX try variant that pulls
  an ARP/ICMP/NDP/ICMPv6 frame needing an auto-reply SHALL `try_lock` **only `q_tx0`** (the reply
  is a prebuilt frame, so NO `tx_state` — MEDIUM-3) and, if `q_tx0` is busy, **drop the reply and
  count it** in `auto_reply_drops` (a new private `AtomicU64` on `UdpSocket`, read through an additive
  `#[doc(hidden)] pub fn auto_reply_drops(&self) -> u64`; not `rx_arp_handled` or
  `rx_drops_buffer_full` — round-6 LOW-4) rather than block (D11/MEDIUM-5). (Req 4.2a)
- [ ] 3.6a Add `static_assertions = "1"` under a new `[dev-dependencies]` section in
  `dpdk-stdlib-udp/Cargo.toml` (none exists today, and the workspace has no `static_assertions`
  dependency — round-5 LOW-8), then add `static_assertions::assert_impl_all!(dpdk_udp::UdpSocket: Send, Sync, Unpin);`
  as a compile-time check (D4 — this is the PR that changes `UdpSocket`'s fields; round 1 had no
  task for it). Assert exactly the 0.3.0 set: `UdpSocket` is **not** `UnwindSafe`/`RefUnwindSafe`
  because it holds `Arc<dyn PacketBackend>` and `Option<Arc<dyn Fn+Send+Sync>>`. (Req 6.2)
- [ ] 3.7 Add stub-backend tests (Req 3.7, D13/MEDIUM-2):
  - a **mock `PacketBackend`** that **validates each frame inside `send_frame`** (valid v4/v6+UDP
    checksum, payload == one sender's pattern, no interleaving) and keeps **only counters**
    (`frames_seen`, `frames_bad`) — **not** the frames themselves (LOW-4/Req 3.7), staying within
    CI memory;
  - a concurrent stress test (N≥4 senders × ≥100k `send_to` + 1 `try_recv_from` on one
    `Arc<UdpSocket>`) asserting no debug panic, `frames_bad == 0`, `frames_seen == expected`
    (promote `context/race_check.rs`);
  - adapt `recv_buffer_bytes_tracks_queued_payload` (`lib.rs:5985`), which locks
    `socket.recv_queue` directly (line 5991) and so breaks when that field moves into `rx_state`
    (round-5 LOW-7; `lib.rs:5975` does not touch `recv_queue`);
  - reuse the **existing** tests for Req 2.8 — do not add a new one (LOW-10): `:5975` unchanged
    (no queue), `:5999` adapted to `rx_state` (round-7 LOW-1);
  - **guard-instrumentation tests via the named pipeline-closure constructors** (D13/MEDIUM-2/
    HIGH-2): add a debug-only (`cfg(test)`/feature-gated) per-guard acquisition counter on
    `DpdkResources` (`q_tx0`/`q_rx0`/`q_tx_direct`); **call `make_send_fn`/`make_recv_fn`/
    `make_direct_send_fn` directly over a stub `DpdkResources`** (task 3.5a) and invoke the returned
    closures, asserting each advanced its guard counter (`make_send_fn`→`q_tx0`,
    `make_recv_fn`→`q_rx0`, `make_direct_send_fn`→`q_tx_direct`). Building through `UdpSocketBuilder`
    in stub mode does NOT reach the pipeline branch (`detect_topology` is always RTC under stubs,
    `topology.rs:661-669`), so the constructors are tested directly — no topology override;
  - **stub-Dpdk RTC guard-instrumentation test** (round-6 MEDIUM-1, design "Testing"): stub
    `UdpSocket::bind` (builds `SocketBackend::Dpdk`, `lib.rs:2175`) + `add_arp_entry` for the peer;
    call `send_to` (stub `tx_burst` returns 0 → `WouldBlock`, after taking `q_tx0`),
    `try_recv_from` (reaches `rx_burst(0,32)` @2924 → `Ok(None)`, after taking `q_rx0`) and, with
    `set_read_timeout(Some(10 ms))`, `recv_from` (reaches @3157, returns after the timeout); assert
    the `q_tx0` and `q_rx0` acquisition counters advanced. This is the only test that covers the
    RTC Dpdk-arm guards (call-table rows 1/2/18/20), which the `Generic` mock never takes;
  - **no new `UdpSocket` constructor** — the existing **public** `UdpSocket::bind_with_backend(addr,
    Arc<dyn PacketBackend>)` (`lib.rs:2346`) already builds a socket over a `Generic` backend and is
    callable cross-crate, so reuse it for the mock-backend tests and the PR-4 tokio-layer tests
    (LOW-2); do not add a redundant `#[doc(hidden)]` constructor.
  (Req 3.7, 2.8)
- [ ] 3.8 Confirm no loom model is needed (design uses `Mutex`, not a new lock-free protocol);
  if any atomic protocol was introduced, add a loom model instead. (Req 3.8)
- [ ] 3.9 Document the thread-safety contract (per-direction + per-queue guards, lock order,
  leaf locks) in `API_COMPATIBILITY.md` and the `dpdk-stdlib-udp` README. (Req 3.9)
- [ ] 3.9a Add three behaviour notes to `API_COMPATIBILITY.md` (round-5 LOW-5): (1) the compat
  `try_send_to` can still block up to ~3 s in `resolve_arp` on an ARP miss (unchanged 0.3.0
  behaviour, Req 4.2a); (2) on a `Generic` backend, the compat `try_*` can wait inside the
  backend's own lock (e.g. `DpdkBackend`'s single `Mutex<Port>`, `backend_dpdk.rs:25`) — the
  non-blocking contract covers only the new guards; (3) the D11 auto-reply drop-and-count when
  `q_tx0` is busy during a non-blocking RX try, recorded in `auto_reply_drops()` (not in App
  Drops). (Req 4.2a, 3.9)
- [ ] 3.10 `cargo build && cargo test` (debug, stub). Verify perf (deployment-normalized sync
  gate: rust-dpdk ≥ **95 % of same-run native-dpdk** @[64,512]; vs-0.3.0 report-only — D8/HIGH-4)
  by running the perf workflow — `gh workflow run perf-tests.yml`, poll with `gh run view --json
  status,conclusion` (CLAUDE.md step 5) — **not** `ci-validate.sh` (which is the integration tier).
  (Req 6.3)

---

## PR 4 — Drop the tokio outer mutex
**Boundary:** `dpdk-stdlib-tokio/src/socket.rs` and `dpdk-stdlib-tokio/src/compat/tokio.rs`. No changed
or removed public API — only additive `pub #[doc(hidden)]` test helpers (task 4.4a); no trait
change. ~150-250 lines.
**Depends on:** PR 3 (the per-direction guards are what make this sound).
**Serialization (LOW-9):** PR 4 and PR 5 both edit `dpdk-stdlib-tokio` recv paths
(`socket.rs`/`compat/tokio.rs`) and must be **landed serially** (PR 4 then PR 5) to avoid
conflicting edits to the same methods.

- [ ] 4.1 Change `DpdkUdpSocket.inner` (`socket.rs`) from
  `Arc<std::sync::Mutex<dpdk_udp::UdpSocket>>` to `Arc<dpdk_udp::UdpSocket>`. Change
  `DpdkAsyncSocket.socket` (`compat/tokio.rs`) from `Arc<std::sync::Mutex<dpdk_udp::UdpSocket>>` to
  `Arc<std::panic::AssertUnwindSafe<dpdk_udp::UdpSocket>>` (D4/CRITICAL-1) — the wrapper restores
  the `UnwindSafe`/`RefUnwindSafe` the removed mutex conferred, because `Arc<dpdk_udp::UdpSocket>`
  is not `UnwindSafe`. Add a SAFETY comment on the field (guards recover from poisoning via
  `into_inner`; no broken invariant survives a panic). Method bodies use `self.socket.<method>()`
  via `AssertUnwindSafe`'s `Deref`. (Req 4.1)
- [ ] 4.2 Update all method bodies to call `self.inner.<method>(..)` / `self.socket.<method>(..)`
  directly, removing `.lock().unwrap()`; for `connect`/`disable_perf_reporting` keep
  `spawn_blocking` but clone the `Arc` instead of the mutex. **`enable_perf_reporting` does NOT
  use `spawn_blocking` today** (`socket.rs:~180` calls the inner method directly — LOW-5); after
  the mutex drop it becomes `self.inner.enable_perf_reporting(interval)` — do **not** add
  `spawn_blocking`. (Req 4.2)
- [ ] 4.3 In `compat/tokio.rs`, change the `try_recv_from`/`try_send_to`/`try_recv`/`try_send`
  arms (`~315`, `~333`, …) from the removed outer-mutex `try_lock` to the inner non-blocking try
  entry points (PR 3 task 3.6): `try_lock` the per-direction + queue guard, drop-and-count any
  auto-reply if `q_tx0` is busy (only `q_tx0`, no `tx_state` — D11/MEDIUM-3), and return
  `WouldBlock` on contention. Keep the
  methods non-blocking and the return types/semantics identical. (Req 4.2, 4.2a)
- [ ] 4.4 Add `static_assertions = "1"` to `dpdk-stdlib-tokio`'s `[dev-dependencies]` (round-5
  LOW-8), then add `static_assertions::assert_impl_all!` auto-trait assertions matching **the exact
  0.3.0 set each type has** (D4/CRITICAL-1): `DpdkUdpSocket: Send, Sync, Unpin`;
  `compat::tokio::UdpSocket: Send, Sync, Unpin, UnwindSafe, RefUnwindSafe`. `DpdkUdpSocket` needs
  **no** wrapper — it never had `UnwindSafe`/`RefUnwindSafe` (its `connected_addr:
  Arc<tokio::sync::Mutex<..>>` is not `RefUnwindSafe`). The compat type keeps its five traits via
  the `AssertUnwindSafe` wrapper on `DpdkAsyncSocket.socket` (task 4.1) — **not** a surviving
  `std::sync::Mutex`, which PR 4 removes. **Compile-probe matrix:** the compat assertion must
  compile **(a) from the workspace root** (where the `dpdk` feature is unified on by `tokio-echo`
  default / `synthetic-bench`) **and (b)** under `cargo test -p dpdk-stdlib-tokio` **without** the
  `dpdk` feature. Verify no signature change and `AsyncUdpSocket`/`async_trait` untouched. (Req
  4.2b, 4.3)
- [ ] 4.4a Add **tokio-layer tests over the `Generic` mock backend** (D13/MEDIUM-2): construct the
  underlying `UdpSocket` via the **existing public** `UdpSocket::bind_with_backend` (`lib.rs:2346`,
  LOW-2 — no new udp-crate constructor) and wrap it in `DpdkUdpSocket` / `compat::tokio::UdpSocket`
  via a thin **`pub` `#[doc(hidden)]`** tokio-type helper (added here), and exercise
  `recv_from`/`send_to`/`try_recv_from`/
  `try_send_to` through the tokio layer so the outer-mutex removal and the non-blocking try path
  are actually covered under `cargo test` (compat skips DPDK under stubs, `compat/tokio.rs:74`, so
  without this the Dpdk arm is untested). **These tests and `DpdkUdpSocket` are behind
  `#[cfg(feature = "dpdk")]`, which is OFF by default — `cargo test -p dpdk-stdlib-tokio` alone
  skips them (LOW-8). Run them from the workspace root (where the `dpdk` feature is unified on by
  `tokio-echo` default / `synthetic-bench`) — i.e. `cargo test` at the root — and ensure CI runs
  them from the workspace root.** (Req 4.5)
- [ ] 4.5 **Enable the `tokio-dpdk-concurrent` ≥ 290k @64 floor gate in this PR (D2/LOW-3/MEDIUM-1).**
  Flip `gates["tokio-dpdk-concurrent"].enabled` from `false` to `true` in `perf-targets.json` as
  part of PR 4's diff (the evaluator gates the floor only when `enabled == true`). This replaces
  the round-2 `active_from_pr:4` field, which was wrongly compared against the GitHub `PR_NUMBER`
  (a per-PR number already past 137, and empty on a `development` dispatch) rather than the ROADMAP
  ordinal. 512/1400 B stay report-only. (Req 2.2, 4.4)
- [ ] 4.6 `cargo build && cargo test` (existing `compat/tokio.rs` loopback tests + the new
  tokio-layer mock tests pass). Verify `tokio-dpdk-concurrent` meets the floor and no sync
  regression by running the perf workflow — `gh workflow run perf-tests.yml`, poll with
  `gh run view --json` (CLAUDE.md step 5) — not `ci-validate.sh`. The deployment-normalized
  rust-dpdk sync check (≥ **95 % of same-run native** @[64,512] — D8/HIGH-4) runs here, not
  deferred. (Req 4.4, 4.5, 6.3)

---

## PR 5 — CPU-efficiency: burst RX allocation + opt-in tokio idle backoff
**Boundary:** `dpdk-stdlib-udp/src/lib.rs` (`ReceiveQueue`/`process_frame_zerocopy` buffer
reuse), `dpdk-stdlib-tokio` recv paths + the additive `set_idle_backoff`/`idle_backoff` methods,
`apps/tokio-echo` (`--idle-backoff-ms` call site), `scripts/run-perf-tests.sh` (0-pps-window
step), and `API_COMPATIBILITY.md` (idle-backoff tradeoff + opt-in note). **No trait change and no
changed/removed public items**; `set_idle_backoff`/`idle_backoff` are **additive** inherent
methods (allowed per Req 6.1). ~250-400 lines.
**Depends on:** PR 3 (RX state now lives behind `rx_state`). **Must land after PR 4 (LOW-9):**
PR 5 touches the same `socket.rs`/`compat/tokio.rs` recv methods PR 4 edits, so the two PRs are
serialized (PR 4 first).

- [ ] 5.1 Add a reusable `Vec<u8>` free-list to the RX buffered queue (now in `RxState`) so
  `push`/`pop` recycle backing `Vec`s instead of allocating a fresh `Vec` per buffered burst
  packet; preserve the byte/packet limits and drop accounting. (Req 5.1, 5.3)
- [ ] 5.2 Update `process_frame_zerocopy` and its callers to use the recycled buffers for the
  non-returned packets of a burst. (Req 5.1)
- [ ] 5.3 Add a scoped counting-allocator test in its **own test binary**: a counting global
  allocator read immediately before/after the single burst-refill call asserts zero per-packet
  payload allocations for the buffered (N−1) packets of an N-packet burst after warm-up, driven
  through the Dpdk-shaped path or a mock backend that returns frames without per-frame allocation,
  with payloads/addresses still correct. (The 23 % malloc/free in the flamegraph is from the
  per-packet `recv_queue` copies **and** `async_trait` boxing together — LOW-12; 5a removes only
  the `recv_queue` payload allocation, `async_trait` boxing is out of scope.) (Req 5.2, 5.3)
- [ ] 5.4 Add the **opt-in idle backoff, default OFF** (D5/HIGH-4). Add additive, documented
  inherent methods `set_idle_backoff(Option<Duration>)` / `idle_backoff()` on `DpdkUdpSocket`
  (`socket.rs`) and `compat::tokio::UdpSocket` (`compat/tokio.rs`). When enabled, the DPDK recv
  path uses `yield_now` until no packet for the idle threshold **`max(d, 2 ms)`** (`d` = the
  `Duration` argument — the one definition, stated in the rustdoc; MEDIUM-3 r5), then escalates to `tokio::time::sleep` in
  **fixed ~1 ms steps (no growth beyond 1 ms — round-6 LOW-5)**; resets to "just received" on the first packet. When **disabled
  (default)** the path is byte-for-byte the 0.3.0 spin (`try_recv_from` + `yield_now`). Document the
  ~1-2 ms first-packet-after-idle latency tradeoff in `API_COMPATIBILITY.md` and the method
  rustdoc. (Req 5.4, 1.8)
- [ ] 5.4b **Wire `tokio-echo`'s `--idle-backoff-ms` (HIGH-3).** `set_idle_backoff` is an inherent
  method on `DpdkUdpSocket` (`socket.rs`), not on the `AsyncUdpSocket` trait (which must not change,
  Req 4.3), so it cannot be called on the `Arc<dyn AsyncUdpSocket>` that `bind_udp_with_config`
  returns (`main.rs:140-152`). **When `--idle-backoff-ms` is set**, bind `DpdkUdpSocket::bind`
  **directly** (not via `bind_udp_with_config`), call `set_idle_backoff(Some(Duration::from_millis(N)))`
  on the concrete type, print `Backend: {backend_name()}` ("dpdk", `socket.rs:176`) exactly like
  the existing branch at `main.rs:150` (task 5.6a greps for it — round-6 LOW-6), then **erase** it
  to `Arc<dyn AsyncUdpSocket>` (`Arc::from(socket)`).
  **When the flag is unset**, the binding path is **unchanged** (`bind_udp_with_config` →
  `Arc<dyn AsyncUdpSocket>`). The `--force-tokio` path is unaffected (idle backoff is a DPDK-only
  method). If `--idle-backoff-ms` is set and the direct `DpdkUdpSocket::bind` fails, `tokio-echo`
  prints the error, prints `idle backoff unavailable: falling back to the standard bind`, and
  continues via `bind_udp_with_config` without backoff (the same fallback the unflagged path has).
  On `compat::tokio::UdpSocket`, `set_idle_backoff` on the `Tokio` (kernel) variant is a no-op that
  only stores the value (`idle_backoff()` returns it), because the kernel socket already parks on
  the reactor (LOW-10). (Req 5.4, 1.8)
- [ ] 5.4a **Time-driver requirement — document, do not over-promise (D5 revised/MEDIUM-2 r3).**
  Do **not** claim the method "cannot panic": `tokio::runtime::Handle::try_current()` only detects
  a runtime context, not whether the time driver is enabled, so `tokio::time::sleep` still panics
  on a runtime built without `enable_time()`/`enable_all()`. Instead, `set_idle_backoff`'s rustdoc
  and `API_COMPATIBILITY.md` SHALL **require** a time-enabled runtime (`enable_time` / `enable_all`,
  which `#[tokio::main]` provides) and state that using the backoff on a timer-less runtime panics
  exactly like any other `tokio::time` call. Note that default-OFF means 0.3.0 users who never opt
  in are unaffected. Document the behaviour regression note (sleep in recv path when enabled) in
  `API_COMPATIBILITY.md`. (Req 5.4)
- [ ] 5.5 Confirm the busy path is unchanged (first-poll success returns immediately, no sleep)
  so throughput is unaffected; the per-step throughput check (tokio ≥ **98 % of same-run
  rust-dpdk**, D6) runs at **70k / 140k / 350k only**; the 700k step is judged by the headline
  tokio ≥ 95 %-of-rust gate (MEDIUM-7, round-4 MEDIUM-2). There is **no latency gate** (D5);
  DUT p50/p99 are reported only. (Req 5.5, 5.6, 5.7)
- [ ] 5.6 `cargo build && cargo test`. Verify via the perf workflow (`gh workflow run
  perf-tests.yml`, poll `gh run view --json`; CLAUDE.md step 5), including the per-step throughput
  checks (70k/140k/350k) and the deployment-normalized rust-dpdk sync check (≥ **95 % of same-run
  native** @[64,512] — D8/HIGH-4). (Req 5.4, 5.5, 6.3)
- [ ] 5.6a **Add the 0-pps-window measurement STEP to `run-perf-tests.sh` (D14/MEDIUM-3).** Add a
  harness step that starts `tokio-echo` **with `--idle-backoff-ms`**, holds a ~30 s 0-pps window
  (before TRex traffic), and runs `mpstat` + (preferred) `pidstat -p <tokio-echo pid>` across it
  (`sysstat` is installed by PR 2 task 2.9a). Report the `tokio-echo` process CPU and judge it
  against `perf-targets.json` `"idle_cpu": {"max_pct_of_one_vcpu": 10, "enabled": true}` (the
  per-process `pidstat` figure; mpstat all-CPU ≤ 2.5 % on the 4-vCPU DUT as a cross-check). A miss
  writes a `GATE FAIL: idle-cpu ...` line and makes `gate-verdict.txt` FAIL (exit 1), like any
  other gate (LOW-8). The step SHALL confirm the DPDK backend is in use by grepping the app log
  for `Backend: dpdk` and for the absence of `idle backoff unavailable`; if the fallback line is
  present (direct `DpdkUdpSocket::bind` failed and tokio-echo fell back to the kernel socket), the
  idle-CPU gate FAILs with `GATE FAIL: idle-cpu (DPDK backend not in use)`, because a kernel
  socket parks on the reactor and would pass without testing anything (round-5 LOW-4). (Req 5.4, 2.3)

---

## PR 6 — semver check + proving perf run + log entry
**Boundary:** CI/workflow wiring for `cargo semver-checks`, and the committed
`docs/perf-test-log.md` entry from the validation run. No crate source change. ~100-200 lines.
**Depends on:** PR 1-5 (this is the done-gate evidence).

- [ ] 6.1 Add `cargo semver-checks` (or a documented equivalent) against 0.3.0 for every
  published crate as a CI or pre-release step. Feature matrix per crate (round-5 LOW-9): only
  `dpdk-stdlib-tokio` has a `dpdk` feature, so run it **both with `--features dpdk` and without**
  (D4/CRITICAL-1 — the auto-trait outcome differs by feature); every other crate runs with its
  default features only; confirm the
  `assert_impl_all!` checks match the **per-type** 0.3.0 trait sets (D4: `dpdk_udp::UdpSocket` +
  `DpdkUdpSocket` = `Send+Sync+Unpin`; compat `UdpSocket` = +`UnwindSafe`+`RefUnwindSafe` via the
  `AssertUnwindSafe` wrapper), confirming the compat assertion compiles from the workspace root
  (dpdk unified on) and under `cargo test -p dpdk-stdlib-tokio` without the feature; fix any
  additive-only violations surfaced. (Req 6.1, 6.2)
- [ ] 6.2 Run the full perf workflow (`gh workflow run perf-tests.yml`; median of 3, 700k step,
  configs rust-dpdk/native-dpdk/tokio-dpdk/tokio-dpdk-concurrent) and confirm all gates pass:
  tokio-dpdk ≥95 % of rust-dpdk @[64,512,1400] (this is the sole 700k judge for tokio-vs-rust);
  rust-dpdk ≥ **95 % of same-run native** @**[64,512]** (1400 B report-only) — the
  deployment-normalized sync gate (D8/HIGH-4); vs-0.3.0 is report-only (WARN below 98 %, never
  gates); native-dpdk @64B ≥ 600k (else INVALID, exit 2); tokio-dpdk-concurrent ≥290k @**[64]**
  (gate enabled in PR 4); the per-step rule (tokio ≥98 % of same-run rust-dpdk) at **70k/140k/350k
  only** (MEDIUM-7). DUT p50/p99 are
  **reported, not gated** (D5); confirm they print > 0.0 at the 350k and 700k steps (`{:.1}`).
  (Req 2.1-2.2, 6.3)
- [ ] 6.3 Commit the `docs/perf-test-log.md` entry (new template, vs-target + vs-last-run, real
  DUT p50/p99 at `{:.1}` > 0.0 at 350k/700k, tokio-mode idle% > 0 from the pre-traffic tick, App
  Drops columns present and populated — 0 allowed) proving Req 2.11. (Req 2.6, 2.7, 2.8, 2.11)
- [ ] 6.4 **Add the native-dpdk 1400 B ROADMAP follow-up (D3) — a NON-0.3.1 follow-up item**, not
  gated in this spec: "rust-dpdk 1400 B at 81-87 % of native-dpdk when TRex is not line-rate
  capped — investigate", citing runs #43 #47 #48 #49 #51 in `docs/perf-test-log.md`. Record it in
  `ROADMAP.md` and reference it from the perf-test-log entry. (Req 2.2)
- [ ] 6.4b **Add a second non-0.3.1 ROADMAP follow-up (round-4 LOW-14):** "rust-dpdk 1400 B has no
  regression gate in 0.3.1 (D3 + D8), and the tokio ≥ 95 % gate at 1400 B is backed only by
  line-rate-capped evidence (E1 476k); on uncapped deployments rust is CPU-bound at 1400 B (#51
  561k) and Stage 0's burst model puts fair async at 84-96 % of sync when CPU-bound — add a
  deployment-normalized 1400 B gate once the D3 investigation explains the native gap." Record it
  in `ROADMAP.md` next to the D3 item. (Req 2.2)
- [ ] 6.5 Final `cargo build && cargo test` from workspace root.

---

## Done gate ↔ PR map
| Acceptance criterion (brief) | PR |
|---|---|
| 1. median-of-3 gates (tokio ≥95 % of rust @[64,512,1400], sole 700k judge; rust ≥95 % of **same-run native** @**[64,512]** = deployment-normalized sync gate, 1400 report-only, vs-0.3.0 report-only WARN<98 %; native @64B ≥600k else INVALID exit 2; concurrent ≥290k @**[64]** `enabled` flipped in PR 4; per-step rule tokio ≥98 % of same-run rust at **70k/140k/350k only**), run fails on miss via verdict file (exit 1; missing/unreadable verdict = FAIL) | 2 (gate); 4 (concurrent enabled); 6 (proving run) |
| 2. summary vs-target + DUT p50/p99 (`[PERF]`, `{:.1}`, keys `dut_lat_p50_us`/`dut_lat_p99_us`, **report-only**) + TRex Lat cols kept; tokio idle% > 0 from the pre-traffic `rx_pps=0` tick (task 2.7a); App Drops columns present and populated for tokio configs (0 allowed; no tautological cross-check — round-4 MEDIUM-1; counter increment proven by the stub overflow tests); ring_drops=0 in RTC is correct; gates skipped with a notice when their config/reference/size was not requested (round-4 MEDIUM-3) | 2, 6 |
| 3. per-queue (incl. inline RX direct `rx_burst` @2924/@3157 → `q_rx0` via `recv_guarded`→`Vec<Mbuf>`, HIGH-1, covered by the **stub-Dpdk RTC guard-instrumentation test** in task 3.7; pipeline `make_send_fn`→`q_tx0`, D1) + per-direction guards, lock order + leaf-lock rule proven, mock validates-in-`send_frame` + guard-instrumentation tests via the named closure constructors (stub mode, no topology override, HIGH-2) pass, `unsafe impl Sync for TxBuffer` gone; `assert_impl_all!(dpdk_udp::UdpSocket: Send,Sync,Unpin)` | 3 |
| 4. tokio socket types no longer wrap an outer `Mutex` (compat holds `Arc<AssertUnwindSafe<UdpSocket>>`, D4/CRITICAL-1); **per-type** 0.3.0 auto traits asserted (both tokio types `Send+Sync+Unpin`; compat +`UnwindSafe`+`RefUnwindSafe`), compat assertion compiles from workspace root AND `-p dpdk-stdlib-tokio` without feature; tokio-layer mock tests; concurrent floor gate enabled (PR 4) | 4 |
| 5. burst RX allocates no per-packet `Vec` (scoped-counter asserted) | 5 |
| 6. **opt-in, default-OFF** idle backoff (requires a time-enabled runtime, documented; panics like any `tokio::time` call if misused — no "cannot panic" claim, MEDIUM-2; default-OFF so 0.3.0 users unaffected); with `--idle-backoff-ms`, idle tokio recv ≤10 % of one vCPU (0-pps window); per-step throughput holds | 5 |
| 7. DUT p50/p99 **reported, not gated** (D5) — printed > 0.0 at 350k/700k; no latency gate | 2, 6 |
| 8. `cargo semver-checks` (both with/without `--features dpdk`) + per-type `assert_impl_all!` pass vs 0.3.0 (compat assertion compiles from workspace root AND `-p dpdk-stdlib-tokio` without feature) | 3 (`UdpSocket`), 4 (tokio types), 6 (semver-checks) |
| 9. perf-test-log new template + entry; AGENTS.md known-limitation rule; native-dpdk 1400 B and rust-dpdk 1400 B gating ROADMAP follow-ups (non-0.3.1, tasks 6.4 / 6.4b) | 2, 6 |
