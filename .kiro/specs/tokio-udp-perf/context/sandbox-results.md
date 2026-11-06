# RESULTS: dpdk-stdlib-rust tokio UDP performance experiments

Brief: `BRIEF.md`
Repo: fresh clone of `development` @ `5c74658`, local experiment branch `kiro/tokio-perf-exp` (never pushed).

| Commit | What |
|---|---|
| `5c74658` | `development` baseline |
| `814705a` | `kiro/tokio-perf-exp`: `tokio-echo --mode legacy\|spawn\|fair`, `--runtime mt\|ct`, `--worker-threads N` (defaults reproduce the current app), `TOKIO_ECHO_EXTRA_ARGS` hook in `run-perf-tests.sh`, local-only `DpdkDevKiroPerf` stack in `deploy/cdk/app.ts` |
| `de288fd` | `run-perf-tests.sh`: `TREX_GATEWAY_MAC_OVERRIDE` so `--skip-deploy` reruns work (TRex's TX ENI stays DPDK-bound after the first run) |

## Headline

**The 305k pps tokio-dpdk ceiling is the benchmark app's concurrency pattern, not the library.**

- With tokio-echo doing what the sync echo does (receive, send the same bytes back, in one task), tokio-dpdk matches sync rust-dpdk exactly on c6in.xlarge: **698,956 / 698,771 / 475,953 pps** at 64 / 512 / 1400 B (100 % of sync), on one core, with 0 % kernel time (E1).
- The legacy app spawns one task per packet on the multi-thread runtime. Each spawn wakes a parked worker, and the workers then fight the receive loop for the socket's `std::sync::Mutex`. At the ceiling, **46 % of DUT CPU is syscalls (futex wake/park and contended-mutex sleeps)** and packet I/O is under 10 % (E5 flamegraph), with ~150k context switches/s (E2).
- Removing the formatting alone changes nothing (E2). Switching the same legacy app to `current_thread` alone gives **2.2x** at 64 B (E4b: 660k).
- The 90 % draft target is **already met** by a fair app on today's library. The spec should be about the app and the library's behaviour when users *do* use concurrent senders, not about the library's single-task fast path.

## Verdict per hypothesis

| ID | Hypothesis | Verdict | Evidence |
|---|---|---|---|
| H1 | Benchmark app does too much per packet | **Confirmed, and it's the whole gap.** But the cost is the *structure* (spawn + semaphore + timeout per packet on multi_thread), not the formatting. | E1 fair = 100 % of sync at all sizes. E2 (spawn, no format) = legacy ceiling. On one thread (E4b), the format/`to_vec` per-byte work still costs 6 % / 37 % / 39 % at 64 / 512 / 1400 B. |
| H2 | Receive loop busy-spins | **Confirmed as a CPU cost; refuted as a throughput cause.** | Fair mode pins one vCPU at 25.0 % usr (4 vCPUs) at every rate, including 70k pps (`logs/cap-E1.log`), and still reaches line rate. In the legacy pattern the spin isn't visible because the receive task rarely finds the queue empty. A waker-driven receive would save a core when idle; it isn't needed for throughput. Not run as a separate E3. |
| H3 | Outer `Mutex` is pure overhead | **Refuted as stated.** Uncontended (fair mode) it costs nothing measurable (0 % `lock_contended`). Contended (legacy pattern) it's 17.6 % of CPU. And it **can't simply be removed**: it's what keeps tokio users safe from the `TxBuffer` data race (Stage 0 `race_check`). | E5 flamegraphs; Stage 0 race check. |
| H4 | Multi-thread work stealing / cross-core | **Confirmed: the dominant cost of the legacy pattern.** | E4b `current_thread`: 2.2x / 1.9x / 1.8x over multi_thread with the same app; context switches 150k/s → 3/s. Legacy flamegraph: 18.5 % waking workers, 13.9 % parking, 18.1 % `finish_task_switch`. Stage 0 predicted it (1.23 M switches, 1.7x). |
| H5 | Pipeline path itself adds cost | **Refuted for these runs; there is no remaining gap.** Perf runs use run-to-completion (no pipeline). In fair mode the library reaches 100 % of sync. Remaining library costs are efficiency, not throughput: malloc/free 23 % (the 31-of-32 `to_vec` into `recv_queue` per burst, `async_trait` boxing), rx 12.6 %, tx 9.6 %. | E1, E5 fair flamegraph. |
| new | `dpdk_udp::UdpSocket` is `Sync` but `TxBuffer` isn't thread-safe | **Confirmed soundness bug** in the published crate. | Stage 0 `race_check`: 3/4 threads hit the in-use assertion. |
| new | Burst handling copies 31 of 32 packets into `recv_queue` | **Confirmed CPU cost** (part of the 23 % malloc/free in fair mode); shared with sync. | Code (`lib.rs:3577`), E5 fair flamegraph. |

## Achievable target
- **tokio-dpdk ≥ 90 % of rust-dpdk at 64 / 512 / 1400 B on c6in.xlarge is achievable today** with a fair echo (measured 100 %). Recommended acceptance: **≥ 95 %** for the fair benchmark, plus a second "concurrent senders" benchmark (spawned sends on multi_thread) with its own floor, so the pattern real users write is measured too.
- The legacy app's concurrent pattern on multi_thread is capped by tokio wakeups and the socket mutex. Getting that pattern close to sync needs library changes (below) and probably batching; it isn't a realistic 90 % target for a per-packet `tokio::spawn` app.

## Recommended spec scope (for spec-gen)
1. **Fix the benchmark** (`apps/tokio-echo`): default to the fair loop (receive, send the same bytes, one task); keep the spawn-per-packet behaviour as an explicit `--mode concurrent` benchmark config so both are tracked. Drop `from_utf8_lossy`/`format!` from the hot path (keep under `--verbose`).
2. **Perf gates and log** (from the "how did we miss it" review): absolute targets in `perf-tests.yml` (tokio-dpdk fair ≥ 95 % of rust-dpdk; rust-dpdk ≥ 95 % of native), a "vs target" column in `docs/perf-test-log.md`, wire up the tokio-mode counters that read 0 (`worker_idle_pct`, drop counters), and surface the DUT's real p99 latency in the summary (it reports 0 today).
3. **Fix the `TxBuffer` soundness bug** in `dpdk-stdlib-udp`: per-call or per-thread TX scratch (or a TX-only lock), so `UdpSocket: Sync` is actually true. Add a loom/Kani check per the verification plan, and a concurrent-`send_to` test. Consider a crates.io advisory for published versions.
4. **Concurrent-sender scalability** (once 3 lands): split the tokio wrapper's single socket mutex so the receive path and senders don't contend (send path needs no lock once `TxBuffer` is fixed; receive keeps single-consumer semantics). Measure with the `--mode concurrent` benchmark.
5. **CPU-efficiency items** (lower priority, no throughput gap today): waker-driven receive instead of spin-`yield_now` (H2, saves a core when idle), avoid the 31-of-32 `to_vec` copies per burst (keep mbufs or a pre-allocated ring), and avoid `async_trait` boxing on the hot path.

Not recommended: a lock-free rewrite of the tokio receive path for throughput. E1 shows the current path already reaches line rate.

---

## Stage 0: micro-benchmarks on the agentrouter VM (no DPDK, no AWS)

- Code: `stage0-bench/` (`src/main.rs`, `src/bin/race_check.rs`)
- Raw output: `stage0-run1.txt`, `stage0-run2-burst.txt`, `stage0-run3-ctxsw.txt`
- CPU: Intel i7-11700K (Ice Lake-generation, same generation as c6in's Xeon), VM, load ~0.1. 4 tokio workers to match c6in.xlarge's 4 vCPUs. Median of 3 reps.
- `FakeSock` replaces `DpdkUdpSocket`: same `std::sync::Mutex`, `recv_from` copies the payload into the caller's buffer, `send_to` runs the real `dpdk_udp::build_udp_frame_into`. Receive is always ready (the saturated case: `burst_avg = 32.0` in the DUT logs).
- `current_app` reproduces `apps/tokio-echo/src/main.rs:217-293` (100 handlers, 5 s timeout). `fair_dyn` is recv_from then send_to of the same bytes through `Arc<dyn AsyncUdpSocket>`.
- Payloads 18 / 467 / 1355 / 8475 B correspond to the 64 / 512 / 1400 / 8500 B frames.

### Run 1: simple socket (critical sections ~20 ns)

| variant | 18 B | 467 B | 1355 B | 8475 B | fit fixed ns | fit ns/B |
|---|---|---|---|---|---|---|
| sync, no mutex (apps/echo floor) | 17 | 160 | 423 | 2539 | 17 | 0.298 |
| sync + mutex | 29 | 166 | 449 | 2609 | 27 | 0.305 |
| fair async, multi-thread | 63 | 197 | 515 | 2540 | 78 | 0.291 |
| fair async, current_thread | 55 | 196 | 458 | 2556 | 55 | 0.295 |
| **current app, multi-thread** | **820** | **1267** | **1721** | **8692** | **705** | **0.938** |
| current app, current_thread | 429 | 767 | 1306 | 5241 | 486 | 0.562 |
| component: `to_vec` | 8 | 11 | 38 | 83 | 13 | 0.008 |
| component: `to_vec` + `from_utf8_lossy` + `format!` | 64 | 197 | 429 | 2090 | 83 | 0.237 |

Other components: uncontended `Mutex` lock+unlock 11.0 ns; 2-thread contended 32.6 ns; semaphore + `tokio::spawn` + `timeout` around a no-op: 599 ns (multi-thread), 286 ns (current_thread).

### Run 2: burst model (`BURST=1`, plus `RX_BURST_NS=300 TX_NS=150`)

Models `try_recv_from_inline`: one burst of 32 per 32 receives, 31 `to_vec()` pushes into a `Mutex<VecDeque>`, plus an assumed 300 ns PMD cost per `rx_burst` and 150 ns per send, all inside the socket mutex. The PMD costs are assumptions, not measurements.

| variant | 18 B | 467 B | 1355 B | 8475 B | fit fixed ns | fit ns/B |
|---|---|---|---|---|---|---|
| sync, no mutex | 244 | 405 | 702 | 3929 | 184 | 0.441 |
| sync + mutex | 261 | 428 | 707 | 3968 | 198 | 0.443 |
| fair async, multi-thread | 292 | 447 | 739 | 3442 | 265 | 0.374 |
| fair async, current_thread | 292 | 441 | 728 | 3980 | 220 | 0.442 |
| **current app, multi-thread** | **1217** | **1647** | **2683** | **11776** | **1082** | **1.260** |
| current app, current_thread | 715 | 1032 | 1639 | 5854 | 755 | 0.603 |

(Without the PMD costs: sync 51, fair 91, current app 826 ns at 18 B.)

### Run 3: context switches (burst model with PMD costs, one rep, ~6.3 M packets per variant)

| variant | voluntary ctx switches | sys time | wall |
|---|---|---|---|
| fair async, multi-thread | 43 | 0.28 s | 4.1 s |
| **current app, multi-thread** | **1,231,564** | **12.40 s** | 14.8 s |
| current app, current_thread | 11 | 0.06 s | 8.5 s |

About one futex park/unpark per 5 packets. Spawning one task per packet keeps waking idle tokio workers.

### Stage 0 reading

- **H1:** the app's extra work is real (fair async ≈ sync + 20-40 ns), but on this VM it only accounts for ~0.9 µs + ~0.8 ns/B of the 3.3 µs + 1.8 ns/B seen on c6in. A fair app alone is unlikely to reach 90 % of sync. EC2 has to show what remains.
- **H4:** most of the app's cost is the multi-thread spawn-per-packet pattern, which shows up as kernel time (futex wake/park). EC2 wakeups (IPIs to a halted vCPU) cost more than here, which could account for the missing ~2.4 µs. To confirm on EC2: E1/E4 plus `perf stat -e context-switches` and `mpstat` %sys on the DUT.
- **H3:** see below. Any lock-splitting or lock-removal fix has to fix `TxBuffer` (and the SPSC consumer side) first.

### H3 detail: `dpdk_udp::UdpSocket` is `Sync` but not thread-safe

- `dpdk-stdlib-udp/src/lib.rs:1961-1969`: `struct TxBuffer { buf: UnsafeCell<Vec<u8>> }` with `unsafe impl Sync for TxBuffer {}`. The SAFETY comment says "only accessed in RTC mode from a single thread".
- `send_to_v4` (`lib.rs:2618`) and `send_to_v6` (`lib.rs:2782`) borrow it on **every** send, including the pipeline's `cached_direct_send` path, not only in RTC mode.
- `assert_sync::<dpdk_udp::UdpSocket>()` compiles, so `Arc<UdpSocket>` shared across threads is accepted by the compiler. `std::net::UdpSocket` explicitly supports concurrent `send_to` from several threads, so drop-in users will do this.
- `race_check` (debug build, stub backend): 4 threads x 200k `send_to` on one `Arc<UdpSocket>` → **3 of 4 threads panicked** with `TxBuffer: concurrent access detected — this should only be used in RTC mode` (`lib.rs:1991`). Release builds have no check, so this is a data race: interleaved frames, or a use-after-free if one thread's `resize` reallocates while another writes.
- Similar concern (not yet tested): `try_recv_from_pipeline` dequeues `SpscRing` app rings from `&self`, so two concurrent receivers would break the single-consumer invariant.
- Consequence for the fix: `dpdk-stdlib-tokio`'s `Arc<std::sync::Mutex<UdpSocket>>` (`socket.rs:91`, `compat/tokio.rs:49`) is what keeps tokio users safe today. Removing it needs a library-level fix (per-call or per-thread TX scratch, or a TX-only lock), loom/Kani coverage per the verification plan, and probably a crates.io advisory for the published versions.

---

## Stage 1: sandbox EC2 (a dedicated sandbox AWS account, us-east-1)

- Stack: `DpdkDevKiroPerf` (`PerfTestStack`, DUT + TRex c6in.xlarge, DPDK/TRex built from source), deployed 2026-10-24 ~02:20 UTC via `run-perf-tests.sh --configs rust-dpdk,tokio-dpdk --packet-sizes 64,512,1400 --no-teardown`.
- Log: `logs/e0-run1.log`.

### E0: baseline reproduction (done 03:01 UTC, EXIT=0)
- Binary: `development` @ `5c74658` as built by the stack's user data (no `--mode` flag; `ldd` shows 9 `librte` libs). DUT `<dut-instance>`, 4 vCPU.
- Raw: `e0/perf-results/` (summary, per-config JSON), `e0/instance-logs/` (DUT/TRex logs). One rep.

RX pps echoed back (drop %):

| Target | rust-dpdk 64 B | tokio-dpdk 64 B | rust-dpdk 512 B | tokio-dpdk 512 B | rust-dpdk 1400 B | tokio-dpdk 1400 B |
|---|---|---|---|---|---|---|
| 70k | 69,000 (1.4 %) | 69,000 (1.4 %) | 69,000 | 69,000 | 69,000 | 69,000 |
| 140k | 139,000 (0.7 %) | 139,000 (0.7 %) | 139,000 | 139,000 | 139,000 | 139,000 |
| 350k | 349,000 (0.3 %) | **302,484 (13.6 %)** | 349,000 | **232,460 (33.6 %)** | 349,000 | **148,946 (57.4 %)** |
| 700k | 698,948 (0.15 %) | **301,774 (56.9 %)** | 697,552 (0.35 %) | **232,262 (66.8 %)** | 475,898 (line rate) | **159,043 (66.6 %)** |

- **Reproduces the 2026-10-15 CI baseline** (305k / 225k / 152k), so the sandbox setup is representative.
- tokio-dpdk ceiling = **43 % / 33 % / 33 %** of rust-dpdk at 64 / 512 / 1400 B.
- DUT at the tokio ceiling: `burst_avg=32.0`, `worker_idle_pct=0.0` (not wired in tokio mode), `lat_p99_us` 10-12 at 64 B. NIC `imissed`/`rx_nombuf` 0: the drops are the app not pulling packets off the ring fast enough.
- The ~1.4 % / 0.7 % "drops" at 70k/140k are the same for both configs (TRex warm-up), not app loss.

### E1: fair tokio-echo (`--mode fair`, multi_thread default workers) (done 03:23 UTC, EXIT=0)
- Binary: `814705a` source rebuilt on the DUT (`cargo build --release --features dpdk-sys/bindgen`, 9 `librte` libs). DUT log confirms `Experiment: mode=fair runtime=mt worker_threads=0`.
- Harness: commit `de288fd` (adds `TREX_GATEWAY_MAC_OVERRIDE`; the first E1 attempt failed EXIT=2 at TRex gateway-MAC discovery on the reused stack, no traffic sent).
- Raw: `e1/perf-results/`, `e1/instance-logs/`, DUT capture `logs/cap-E1.log`. One rep.

| Target | 64 B | 512 B | 1400 B |
|---|---|---|---|
| 350k | 349,000 (0.29 %) | 349,000 (0.29 %) | 348,989 (0.29 %) |
| 700k | **698,956 (0.15 %)** | **698,771 (0.18 %)** | **475,953 (0.15 %, line rate)** |

- **= rust-dpdk (E0: 698,948 / 697,552 / 475,898).** tokio-dpdk is at **100 % of sync** at all three sizes once the app does the same work.
- DUT p99 latency at 700k / 64 B: 3-11 µs.
- DUT CPU (capture every ~32 s for the whole run): **25.0 % usr, 0.0 % sys, 75 % idle** on 4 vCPUs at every rate, i.e. exactly one vCPU busy. **~3 context switches/s.** The receive loop polls a full core even at 70k pps (`yield_now` spin, H2), same as a DPDK poll loop; it costs CPU, not throughput.
- **Conclusion: the library's async path (async_trait boxing, `std::sync::Mutex`, spin-`yield_now` receive) is not the bottleneck.** The 302k ceiling in E0 comes entirely from what `apps/tokio-echo` does per packet in legacy mode.

### E2: spawn per packet, no formatting (`--mode spawn`, multi_thread) (done 03:34 UTC, EXIT=0)
- Same as legacy (`to_vec` + semaphore + `tokio::spawn` + `timeout` per packet) but echoes the original bytes: no `from_utf8_lossy`, no `format!`. DUT log: `Experiment: mode=spawn runtime=mt worker_threads=0`.
- Raw: `e2/perf-results/`, `e2/instance-logs/`, `logs/cap-E2.log`. One rep.

| Target | 64 B | 512 B | 1400 B |
|---|---|---|---|
| 350k | 306,970 (12.3 %) | 232,312 (33.6 %) | 165,093 (52.8 %) |
| 700k | **306,996 (56.1 %)** | **232,393 (66.8 %)** | **175,641 (63.2 %)** |

- **Same ceiling as legacy E0 (302k / 232k / 149-159k).** Removing the formatting changes nothing at 64/512 B and gains ~10 % at 1400 B. So `from_utf8_lossy` + `format!` is **not** the bottleneck; the spawn-per-packet structure is.
- DUT capture while saturated: **150k context switches/s** (up to 604,647 per 4 s sample), **sys 15-20 %**, usr 25-32 %, idle ~50 %. Fair mode (E1): ~3/s and 0 % sys. The DUT has idle CPU, yet throughput is capped: the receive task loses its core to the cost of waking and parking worker threads for each spawned send.
- The ceiling still falls with packet size (307k → 232k → 170k) even without formatting, so a per-byte cost remains in this pattern: `to_vec` plus the payload crossing cores to the worker that runs the spawned send (cold cache on another core). Fair mode has no such slope (it reaches line rate at 1400 B).

### E4b: legacy app on `current_thread` (`--mode legacy --runtime ct`) (done 03:45 UTC, EXIT=0)
- The unmodified per-packet work (to_vec, semaphore, spawn, timeout, `format!("echo: {}", from_utf8_lossy)`), only the runtime flavour changed. DUT log: `Experiment: mode=legacy runtime=ct worker_threads=0`.
- Raw: `e4b/perf-results/`, `e4b/instance-logs/`, `logs/cap-E4b.log`. One rep.

| Target | 64 B | 512 B | 1400 B |
|---|---|---|---|
| 350k | 349,000 (0.29 %) | 348,922 (0.31 %) | 274,668 (21.5 %) |
| 700k | **659,945 (5.7 %)** | **438,230 (37.4 %)** | **290,960 (39.0 %)** |

- **vs legacy multi_thread (E0): 2.2x / 1.9x / 1.8x** (302k → 660k, 232k → 438k, 159k → 291k).
- DUT capture: one vCPU busy (usr ~20 %, sys ~5 % in some samples, idle 75 %), **~3 context switches/s** (vs 150k/s in E2).
- What remains vs sync (94 % / 63 % / 61 %) grows with packet size: the per-byte app work (`to_vec`, `from_utf8_lossy` scan, `format!` copy, 6-byte longer reply), now all on one core. That is H1's part.

### Combined reading of E0-E4b (64 / 512 / 1400 B, 700k target)

| Variant | 64 B | 512 B | 1400 B | % of rust-dpdk |
|---|---|---|---|---|
| rust-dpdk (sync, E0) | 698,948 | 697,552 | 475,898 | 100 / 100 / 100 |
| tokio legacy, multi_thread (E0) | 301,774 | 232,262 | 159,043 | 43 / 33 / 33 |
| tokio spawn (no format), multi_thread (E2) | 306,996 | 232,393 | 175,641 | 44 / 33 / 37 |
| tokio legacy, current_thread (E4b) | 659,945 | 438,230 | 290,960 | 94 / 63 / 61 |
| **tokio fair, multi_thread (E1)** | **698,956** | **698,771** | **475,953** | **100 / 100 / 100** |

### E5: flamegraphs (done 03:58 UTC)
- Method: `flame.sh` on the DUT waits for a saturated 64 B `[PERF]` line, then `perf record -F 299 --call-graph dwarf,8192 -p <tokio-echo> -- sleep 8` (DWARF because release builds have no frame pointers; the 8 GB DUT disk had 1.2 GB free). Folded on the DUT with `inferno-collapse-perf`, rendered locally with `inferno-flamegraph`. Harness runs: `logs/E5-legacy.log`, `logs/E5-fair.log` (64 B only, EXIT=0).
- Files: **`flamegraph-legacy-mt.svg`**, **`flamegraph-fair-mt.svg`**; folded stacks and summaries in `e5/` (`*.folded`, `*.summary.txt`, `*.inclusive.txt`). Summaries from `fold-summary.awk`.

| Share of sampled CPU (inclusive) | legacy, multi_thread @ 305k pps (saturated) | fair, multi_thread @ 699k pps (line rate) |
|---|---|---|
| threads | 69 % tokio worker threads, 31 % main | 100 % one thread |
| syscalls | **46.1 %** | 0.0 % |
| futex | **42.8 %** | 0.0 % |
| ↳ `std::sync::Mutex::lock_contended` (socket mutex) | **17.6 %** | 0.0 % |
| ↳ waking workers (`unpark` / `wake_by_*` / `futex_wake`) | **18.5 %** | — |
| tokio park | 13.9 % | 7.1 % (idle-check path) |
| malloc / free | 10.1 % | 23.1 % |
| `from_utf8_lossy` / `format!` | 4.7 % | — |
| rx (`eth_ena_recv`, rx burst) | 2.2 % | 12.6 % |
| tx (`eth_ena_xmit`) | 4.6 % | 9.6 % |
| `try_recv_from` | 0.7 % | 7.2 % |
| `send_to_addr` | 1.0 % | 3.6 % |

Top leaves, legacy: `finish_task_switch` 18.1 %, `_raw_spin_unlock_irqrestore` 16.2 % (kernel context switch and wakeup), `Mutex::lock_contended` 4.1 %. Top leaves, fair: `Notify::poll_notified` 13.0 % (the `select!` on the shutdown signal polled per packet), `_int_free` 9.2 %, `DpdkUdpSocket::recv_from` 7.6 %, `try_recv_from` 6.3 %.

Reading:
- **Legacy:** most cycles go to the kernel, parking and waking tokio workers and sleeping on the contended socket mutex. Packet I/O is under 10 %. That's why the DUT shows idle CPU yet can't go past ~305k.
- **Fair:** no kernel time at all. The remaining costs are allocation (23 %: the 31-of-32 `to_vec` copies into `recv_queue` per burst, plus `async_trait` boxing two futures per packet), the PMD and the app's own shutdown `select!`. These are CPU-efficiency items; at line rate on one core they don't cap throughput on this instance.

### Repetitions: E1 fair mode, 700k target (reps 2-3 done 04:19 UTC, EXIT=0)
Raw: `e1/` (rep 1), `e1-rep2/`, `e1-rep3/`; logs `logs/E1.log`, `logs/E1-rep2.log`, `logs/E1-rep3.log`.

| Rep | 64 B | 512 B | 1400 B |
|---|---|---|---|
| 1 | 698,956 (0.15 %) | 698,771 (0.18 %) | 475,953 (0.15 %) |
| 2 | 698,737 (0.18 %) | 698,717 (0.18 %) | 475,986 (0.14 %) |
| 3 | **669,019 (4.43 %)** | 698,746 (0.18 %) | 475,878 (0.15 %) |
| **median** | **698,737** | **698,746** | **475,953** |
| min, % of rust-dpdk (E0) | 669,019 = **95.7 %** | 698,717 = 100.2 % | 475,878 = 100.0 % |

- One rep dipped to 95.7 % at 64 B. A single dip on one step, with the others at line rate, looks like noise (one 30 s step, shared tenancy), but it's real data: the 64 B fair path has about 4 % of headroom variance, not zero. The sync baseline has only one rep (E0), so its variance is unmeasured.
- This is why the recommended gate is ≥ 95 % rather than ≥ 99 %, and why the perf workflow should take the median of several reps before judging.
- Other variants (E0, E2, E4b) have one rep each. Their gaps (2-3x) are far larger than this variance.

## Teardown (verified 04:25 UTC)
- `aws cloudformation delete-stack --stack-name DpdkDevKiroPerf` at 04:20 UTC → `DELETE_COMPLETE` at 04:20:12. `describe-stacks` now returns "does not exist".
- Active stacks in <sandbox-account> / us-east-1: only `DpdkReaper` and `CDKToolkit`.
- `describe-instances` (pending/running/stopping/stopped/shutting-down): none. DUT `<dut-instance>` and TRex `<trex-instance>`: `terminated`.
- Stack lifetime ~02:20-04:20 UTC (2 h, two c6in.xlarge plus VPC endpoints): roughly $1-2.
- Nothing pushed. The experiment branch `kiro/tokio-perf-exp` (`814705a`, `de288fd`) exists only in `repo/`. No GitHub workflows triggered, and no other account touched.
