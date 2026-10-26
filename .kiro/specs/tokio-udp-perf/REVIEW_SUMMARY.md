# Review summary: tokio-udp-perf (target release 0.3.1)

The spec was generated with Kiro spec mode from the approved brief, then reviewed by Claude opus-5.5 against the real source of `development` @ `f21ed1f` (the published v0.3.0). The reviewer had read access to the checkout and verified every function, field, path and line the spec names. Ship gate: 0 CRITICAL, 0 HIGH, at most 1 MEDIUM. No round cap.

## Rounds

| Round | Verdict | CRITICAL | HIGH | MEDIUM | LOW | Fixed by |
|---|---|---|---|---|---|---|
| 1 | NEEDS_REVISION | 1 | 9 | 9 | 10 | Kiro (resumed session) + 3 direct LOW edits |
| 2 | NEEDS_REVISION | 1 | 7 | 8 | 13 | Kiro, with delegate decisions D1-D14 |
| 3 | NEEDS_REVISION | 1 | 4 | 7 | 11 | Kiro (crashed in its final consistency pass after writing all edits) |
| 4 | NEEDS_REVISION | 0 | 0 | 3 | 14 | direct edits |
| 5 | NEEDS_REVISION | 0 | 0 | 3 | 15 | direct edits |
| 6 | **SHIPPABLE** | 0 | 0 | 1 | 8 | direct edits (leftovers resolved, not parked) |
| 7 | **SHIPPABLE** (confirmation) | 0 | 0 | 0 | 2 | the 2 wording LOWs fixed exactly as the reviewer specified |

**Final state: 0 CRITICAL, 0 HIGH, 0 MEDIUM, 0 open LOW.**

## What the reviews changed most

- **Soundness scope (round 1 CRITICAL, round 2 CRITICAL, round 3 HIGH).** The race isn't per-socket. DPDK queue 0 is shared through the global `DPDK_RESOURCES` by every socket and by the pipeline `rx_loop`. So the fix uses two layers of guards:
  - per-hardware-queue guards (`q_rx0`, `q_tx0`, `q_tx_direct`) in `DpdkResources`;
  - per-socket `tx_state` / `rx_state`.

  All 7 raw `rx_burst`/`tx_burst` sites are listed with their guard, along with every `send_frame` / `recv_frames` caller and the lock order.
- **Perf gates that wouldn't have passed on clean runs (rounds 2-3).**
  - The repo's CI history shows rust-dpdk at ~89 % of 0.3.0 in 4 of the last 11 runs, purely from deployment variance (same-run native-dpdk was equally low). The sync no-regression gate is therefore deployment-normalized: rust-dpdk ≥ 95 % of same-run native-dpdk at 64/512 B. The absolute vs-0.3.0 comparison is report-only (WARN below 98 %).
  - At 1400 B, rust-dpdk is 81-87 % of native when TRex isn't line-rate capped, so that size is report-only, with ROADMAP follow-ups.
- **Harness wiring (rounds 1-5):**
  - per-rep result files keyed by (config, rep);
  - a fail-closed verdict file checked by `main()` (exit 1 = gate miss, exit 2 = infra or invalid deploy);
  - `tokio-dpdk-concurrent` wired into every hard-coded list;
  - gate applicability for subset `workflow_dispatch` runs (configs, sizes and rate steps);
  - DUT p50/p99 parsed from `[PERF]` under distinct `dut_lat_*` keys (report-only).
- **semver / auto traits (rounds 2-3).** Assertions cover the exact 0.3.0 trait set per type. The compat type keeps `UnwindSafe + RefUnwindSafe` through `Arc<AssertUnwindSafe<UdpSocket>>`. `cargo semver-checks` runs with and without `dpdk-stdlib-tokio`'s `dpdk` feature.

## Delegate decisions (recorded in design.md "Decisions")

Made on the maintainer's behalf during review. Each can be revisited:

- **D5:** the tokio idle backoff is **opt-in, default OFF** in 0.3.1, so default latency behaviour is exactly 0.3.0's. When enabled, the `Duration` is the idle threshold, clamped to ≥ 2 ms, with fixed ~1 ms sleeps. DUT latency is reported, not gated.
- **D8:** the sync no-regression gate is normalized to same-run native-dpdk (see above). Native-dpdk < 600k pps at 64 B marks the run INVALID (exit 2, redeploy).
- **D3:** rust-dpdk vs native is gated at 64/512 B only. 1400 B is report-only, with two non-0.3.1 ROADMAP follow-ups (tasks 6.4 / 6.4b).
- **D2:** the `tokio-dpdk-concurrent` floor (290k pps at 64 B) is report-only until PR 4 flips `enabled: true`.
- **D11:** a compat `try_*` that can't take `q_tx0` drops the ARP/ICMP auto-reply and counts it in a new private `auto_reply_drops` counter, never in App Drops.

## Verification limits

The reviewer could not run `cargo` in its sandbox, so auto-trait and compile claims rest on reading the std/tokio rules and the source. Task 6.1 (`cargo semver-checks` + `assert_impl_all!`) settles them in CI.

## Inputs

- Brief: the approved brief, summarized in `context/sandbox-results.md` (measured results E0-E5, flamegraph summaries, the `race_check.rs` reproduction).
- Experiment evidence: `context/`.
