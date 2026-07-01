# Test Hardening — Working Notes (maintainer + Claude)

Shared, incremental checklist we work through over a few days. **This is active work, not roadmap** — the only roadmap item is the Containerlab program (`ROADMAP.md #37` → `.kiro/specs/containerlab-testing/`).

## How to pick this up next session

1. Read this file top-to-bottom; work the **first unchecked item** (they're ordered by dependency).
2. Context that anchors everything below: perf run **28509354005** (2026-07-01) was the first end-to-end hardware perf run. It **proved the DPDK TCP engine works** — `dut-rust-dpdk-tcp-app.log` shows `EAL: Probe PCI driver: net_ena … 0000:00:06.0` (a real vfio PCI probe via `new_real_nic`, not the `--no-pci` null device) and `tcp-echo listening on 10.0.1.10:9000`. Every failure in that run was a **harness** bug (below), not a stack bug.
3. Ownership tags: **[pushable]** = editable + pushable from the Claude/CI env via `gh` HTTPS. **[SSH]** = touches `.github/workflows/*`, which the CI PAT can't push (missing `workflow` scope) — the maintainer pushes these over the SSH remote. Claude drafts them either way.
4. Merge convention: PRs target `development` (squash); crate publishes go via `main`. Maintainer merges.

---

## 1. Perf harness fix — TRex ASTF mode + SSM timeouts  [pushable]

**Problem.** All three TCP configs failed in run 28509354005; the DUT stood up fine but the benchmark never ran.

**Root cause (two independent bugs + one follow-on):**
1. **TRex STL vs ASTF.** `start_trex_server` (`scripts/run-perf-tests.sh:~699`) launches `t-rex-64 -i`, which comes up in **STL** (stateless) mode. The TCP benchmark connects with an **ASTF** client (`run_tcp_benchmark.py` → `ASTFClient`), so the preflight dies with `RPC configuration mismatch - server 'STL', client 'ASTF'`. The comment at `run-perf-tests.sh:~916` (*"`t-rex-64 -i` serves both STL and ASTF, so no server-side change"*) is **wrong** — TRex serves one mode per server process.
2. **SSM `TimeoutSeconds` below AWS minimum.** The DUT-stop path sends an SSM command with `--timeout-seconds 15`; AWS rejects anything `< 30` (`ParamValidation … valid min value: 30`). The stop silently failed, so the previous config's `tcp-echo` kept the DPDK **primary-process lock** — the next config's `tokio-tcp-echo` then died with `EAL: Cannot create lock on '/var/run/dpdk/rte/config'. Is another primary process running?`.
3. **Follow-on:** `plain-rust-tcp`'s kernel re-bind timed out (60s), likely the SSM/ENI flake (item 2) compounded by the stale process; re-verify after 1+2.

**Fix approach.**
- Start (or restart) TRex in **ASTF** mode for `*-tcp` configs — e.g. `t-rex-64 -i --astf …` — and keep STL for UDP/v6 configs. Simplest: detect the suite's protocol before starting TRex and pass the right mode; or restart TRex between the UDP and TCP phases. Fix the stale comment.
- Clamp **every** SSM `send-command --timeout-seconds` to `>= 30` (grep the runner for `timeout-seconds`/`TimeoutSeconds`; the offender passes 15 in the stop path).
- Make "stop all DUT apps" actually kill the DPDK process before the next bind: `pkill -f target/release/…`, **verify** the process is gone, and `rm -rf /var/run/dpdk/*` so the EAL-lock cascade can't recur.

**Files.** `scripts/run-perf-tests.sh`, `scripts/perf-tests/trex/run_tcp_benchmark.py` (confirm ASTF client args).

**Acceptance.** Re-dispatch `gh workflow run perf-tests.yml --ref development -f configs=rust-dpdk-tcp,tokio-dpdk-tcp,plain-rust-tcp` → non-zero CPS/throughput in `perf-report.json`. Then run the remaining suites: `native-dpdk-v6,rust-dpdk-v6`, then `quic-stock,quic-native-dpdk`, then `quic-native-dpdk-nic`. (Validate scripts locally with `bash -n` + `python3 -m py_compile`; the flow itself needs AWS.)

- [ ] Done · PR: —

---

## 2. SSM/ENI flake — the "trio"  [pushable]

**Problem.** Integration CI flakes ~every other run on AWS SSM polling / ENI-bind timeouts, emitting a synthetic `0.000s` failure XML (the test never ran). Same commit passes on a plain re-run.

**Root cause.** (a) `configure-eni.sh` installs a **competing default route** on the secondary ENI (`ip route replace default via <gw> … metric 200`); both ENIs share the `/24` gateway, so during bind/unbind the SSM control traffic can egress the wrong interface (AWS drops asymmetric egress) → the command stalls at `InProgress`. (b) The SSM poll loop (`run-integration-tests.sh` `ssm_run_command`/`ssm_wait_command`) has **no `InProgress` handling** — it spins to the budget and hard-fails, with no cancel / idempotent resend / health probe. The only cooldown (15s) sits in `unbind_all_enis`, not after a per-tier bind.

**Fix approach (the "trio", all bash):**
- Drop the competing default route — use policy routing (a dedicated table + `ip rule from <eni_ip>`) or an on-link route only; the DPDK app bypasses the kernel and needs no default route on the data ENI.
- Add `ssm_settle <instance>` after each ENI op: poll `aws ssm describe-instance-information … PingStatus=Online` + a trivial `echo OK` probe with bounded backoff before the next command.
- Make the poll loop `InProgress`-resilient: on budget-exhaustion-while-`InProgress`, `cancel-command`, wait, and re-send the **idempotent** ENI command up to N times (bind/unbind are idempotent).
- Harden interface selection: replace `lspci | tail -1` with an IMDS device-number lookup so bind/unbind can never tear down the SSM-bearing **primary** ENI.

**Files.** `scripts/integration-tests/configure-eni.sh`, `scripts/run-integration-tests.sh`, `scripts/run-perf-tests.sh` (shares the SSM helpers).

**Acceptance.** ≥5 consecutive green integration runs with no synthetic `0.000s` XML. **Prerequisite for making any new tier a blocking gate.**

- [ ] Done · PR: —

---

## 3. Tier 1/2 functionality tiers — wire QUIC + add TCP + UDP-v6  [pushable]

**Problem.** Per-PR functionality coverage is **UDP-only** (tiers 1–4). TCP, QUIC, and IPv6 have **zero** per-PR wire coverage — even though the DUT binaries + client assertion modes already exist.

**What's ready to use (no app work):**
- **QUIC:** `scripts/integration-tests/tier1-quic-handshake.sh` is fully built (handshake + bidir echo, 2 JUnit cases) but **orphaned** — only `run-quic-integration-tests.sh` calls it, and nothing calls that. Just register it in the main runner.
- **TCP:** `tcp-test-client --mode {handshake,bidir,shutdown,std-parity}` — all four assertions are already coded (bidir = byte-equality, shutdown = FIN/clean-EOF, std-parity = dpdk-stdlib-tcp vs `std::net` byte-identical).
- **UDP IPv6:** `echo` binds v6 + seeds NDP (`--gateway-mac`/`--peer-ip`), post-#98.

**Fix approach.** Add tier scripts + register them in `run-integration-tests.sh`'s default sequence (no `--tier`, so **no workflow-YAML edit**): Tier 1 = TCP handshake/bidir/shutdown (DPDK↔DPDK) + QUIC handshake/bidir; Tier 2 = TCP `std-parity` + UDP-v6 echo. Register **non-blocking** (`continue-on-error` semantics — report but don't fail the run) first; flip to blocking after ≥10 green runs (per the tcp-support / s2n-quic specs). Note: some of these are cheaper on Containerlab (item 8) — decide per-tier whether it lives on EC2 or Containerlab.

**Needs app work (defer):** TCP retransmit (needs a loss-injection backend) and flow-control (needs a receiver-stall server mode); TCP/QUIC over IPv6 (both IPv4-only today).

**Files.** `scripts/integration-tests/tier*-{tcp,quic,udp6}*.sh`, `scripts/run-integration-tests.sh`.

**Acceptance.** TCP/QUIC/UDP-v6 tiers run + report JUnit on every PR (non-blocking), green on real hardware.

- [ ] Done · PR: —

---

## 4. Auto-run Tier 1/2 on PR + development merges  [SSH]

**Problem.** Integration tests trigger on `pull_request: [main, development]` only — **not** on merges to `development`, so the merged result is never validated on-branch. And Graviton runs the full tier set on **every PR**, doubling the AWS footprint just as we add tiers.

**Fix approach.** Add `push: branches: [development]` to `integration-tests.yml` so Tier 1/2 runs post-merge. Move the Graviton (arm64) integration from per-PR to a nightly `schedule` to offset the added per-PR cost (x86 stays per-PR). These are workflow-file edits → **maintainer SSH push**; the tier/runner changes they depend on are [pushable].

**Files.** `.github/workflows/integration-tests.yml`, `.github/workflows/integration-tests-graviton.yml`.

**Acceptance.** Tier 1/2 runs on every PR **and** on each `development` merge; Graviton runs nightly, not per-PR.

- [ ] Done · PR: — (maintainer pushes YAML)

---

## 5. Nightly perf + durable result sink + auto-PR halt gate  [SSH + pushable]

**Problem.** Perf is `workflow_dispatch` only. We want the full suite nightly, and — because `development` has no PR — the results have nowhere to post (`post_pr_comment` no-ops without a PR). And when the nightly is red, the external agent-router keeps generating auto-PRs on a broken base.

**Fix approach.**
- Add a nightly `schedule:` to `perf-tests.yml` running the full `--configs` set. **[SSH]**
- **Durable result sink** (no PR on `development`): append results to `docs/perf-test-log.md` and/or post to a pinned tracking issue. The nightly run's own conclusion is the machine-readable signal (a README badge is its human surface — item 7). **[pushable]** (runner/report side)
- **Halt gate:** the agent-router isn't in this repo (external cron), but its prompt is — `prompts/agent-router.md`. Add a preamble step: at cron start, `gh run list --workflow=<nightly> --branch development --limit 1 --json conclusion`; if `failure`, **pivot to fixing the nightly** instead of generating feature PRs, until green. **[pushable]**

**Files.** `.github/workflows/perf-tests.yml` **[SSH]**, `prompts/agent-router.md` **[pushable]**, `docs/perf-test-log.md` **[pushable]**.

**Acceptance.** Nightly perf runs unattended; results land in the durable sink; a red nightly flips the router to fix-mode on its next cron.

- [ ] Done · PR: —

---

## 6. README badges + coverage  [pushable, coverage job may be SSH]

**Fix approach.** Add badges to `README.md`: per-PR integration + nightly CI status (doubles as the halt-gate's human surface), per-crate crates.io versions (`dpdk-stdlib-net/udp/tcp/quic/tokio`), docs.rs, license, and **code coverage**. Coverage needs a small job running `cargo llvm-cov` → Codecov/lcov (the workflow file is **[SSH]**; the `README.md` badges + tooling config are **[pushable]**).

**Acceptance.** README shows live CI, version, and coverage badges.

- [ ] Done · PR: —

---

## 7. Publish crates — release 1 (TCP / QUIC / UDP-IPv6)  [pushable, via main]

**Gated on item 1** (perf proven green). Publish the `dpdk-stdlib-*` crates carrying the merged TCP engine (#99), IPv6 NDP (#98), and real-NIC QUIC (#100) to crates.io **via `main`** (publish convention). Bump versions, run `cargo publish --dry-run` in dependency order `net → udp → tcp → quic → tokio`, update `CHANGELOG`.

**Acceptance.** All five crates published at the new version; `cargo add dpdk-stdlib-tcp` pulls the TCP engine.

- [ ] Done · PR: —

---

## Later: L2-native + Containerlab (release 2)

Once items 1–7 land, the big piece is making TCP/QUIC **L2-native** (active ARP in the shared net crate) and standing up the **Containerlab** correctness matrix — then a **second crate release**. That work has its own spec: **`.kiro/specs/containerlab-testing/`** (ROADMAP #37). It's roadmap, not this checklist, because it's a design effort rather than a fix list.

## Open questions to decide together

- **Tier placement:** which Tier 1/2 tiers stay on EC2 vs move to Containerlab once item 8/the spec lands? (Leaning: correctness → Containerlab; DPDK-datapath smoke → EC2.)
- **Blocking flip:** what's the concrete gate to make TCP/QUIC/v6 tiers *blocking* — N green runs, and does item 2 (flake fix) have to land first? (Leaning: yes, flake first.)
- **Nightly scope:** full `--configs` every night, or rotate suites across nights to cap cost?
- **Result sink:** `perf-test-log.md` (git history of numbers) vs a pinned issue (less churn) — or both?
- **Coverage tool:** `cargo-llvm-cov` + Codecov vs self-hosted lcov badge?
