#!/usr/bin/env bash
#
# Regression unit tests for scripts/run-perf-tests.sh harness logic.
#
# These guard the three harness bugs surfaced by perf run 28509354005 — which
# cost a full ~30-min EC2 deploy to find. They run in <1s with no AWS and no
# TRex by sourcing the harness (its `main` is source-guarded) and stubbing the
# `aws` CLI.
#
#   1. SSM TimeoutSeconds < 30  -> AWS ParamValidation rejection.
#   2. pkill pattern missed the TCP DUT binaries -> stale process held the DPDK
#      primary-process lock -> next config's EAL init failed.
#   3. TRex launched in STL mode while the TCP benchmark speaks ASTF -> RPC
#      configuration mismatch.
#
# Usage:  bash scripts/perf-tests/test/test_run_perf_tests.sh
set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HARNESS="$TEST_DIR/../../run-perf-tests.sh"

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); echo "  ok   - $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL - $1"; }

# ── Stub the `aws` CLI: record send-command args, report Success on poll ──────
STUB_DIR="$(mktemp -d)"
export AWS_LOG="$STUB_DIR/aws.log"
cat > "$STUB_DIR/aws" <<'STUB'
#!/usr/bin/env bash
echo "aws $*" >> "$AWS_LOG"
case "$*" in
    *send-command*)                   echo "cmd-stub-0000" ;;  # Command.CommandId
    *get-command-invocation*Status*)  echo "Success" ;;
    *)                                echo "" ;;
esac
STUB
chmod +x "$STUB_DIR/aws"
export PATH="$STUB_DIR:$PATH"

# Source the harness (source-guard keeps main() from running), then relax the
# strict flags it sets so the tests can drive failure paths deliberately.
# shellcheck disable=SC1090
source "$HARNESS"
set +e +u +o pipefail
trap - EXIT ERR INT TERM   # drop the harness's safety-net teardown trap

ere_matches() { printf '%s' "$1" | grep -Eq "$DUT_APP_ERE"; }

# ── 1. SSM TimeoutSeconds clamp (>= 30) ──────────────────────────────────────
echo "== SSM TimeoutSeconds clamp =="
: > "$AWS_LOG"
ssm_run_command "i-test" 15 "echo hi" >/dev/null 2>&1
if grep -q -- '--timeout-seconds 30' "$AWS_LOG"; then ok "15s call is clamped to --timeout-seconds 30"; else bad "15s call is clamped to --timeout-seconds 30"; fi
if grep -q -- '--timeout-seconds 15' "$AWS_LOG"; then bad "no send-command uses the invalid 15"; else ok "no send-command uses the invalid 15"; fi
: > "$AWS_LOG"
ssm_run_command "i-test" 90 "echo hi" >/dev/null 2>&1
if grep -q -- '--timeout-seconds 90' "$AWS_LOG"; then ok "valid 90s is passed through unchanged"; else bad "valid 90s is passed through unchanged"; fi

# ── 2. DUT_APP_ERE matches every DUT binary (incl. TCP) but not TRex/sshd ─────
echo "== DUT_APP_ERE composed from the source-of-truth lists =="
for b in "${DUT_RUST_BINS[@]}"; do
    if ere_matches "./target/release/$b --ip 10.0.1.10 --port 9000"; then ok "matches $b"; else bad "matches $b (would leak a stale DUT process)"; fi
done
for b in "${DUT_OTHER_BINS[@]}"; do
    if ere_matches "/usr/local/bin/$b -l 0-1 -- --forward-mode=5tswap"; then ok "matches $b"; else bad "matches $b"; fi
done
if ere_matches "/opt/trex/_t-rex-64 -i --cfg /etc/trex_cfg.yaml"; then bad "must NOT match t-rex-64 (would kill the generator)"; else ok "does not match t-rex-64"; fi
if ere_matches "/usr/sbin/sshd -D"; then bad "must NOT match sshd"; else ok "does not match sshd"; fi

echo "== dut_kill_snippet composition (grace + lock cleanup) =="
SNIP="$(dut_kill_snippet)"
if [[ "$SNIP" == *"for i in 1 2 3 4 5 6 7 8 9 10"* ]]; then ok "grace list expands to DUT_KILL_GRACE_SECS ($DUT_KILL_GRACE_SECS)"; else bad "grace list expands to $DUT_KILL_GRACE_SECS"; fi
if [[ "$SNIP" == *"rm -rf /var/run/dpdk/"* ]]; then ok "snippet clears the DPDK lock dir"; else bad "snippet clears the DPDK lock dir"; fi

echo "== trex_launch_cmd: ASTF gets --lro-disable (ENA has no hardware TCP_LRO) =="
ACMD="$(trex_launch_cmd astf)"; SCMD="$(trex_launch_cmd stl)"
if [[ "$ACMD" == *"--astf"* ]]; then ok "astf mode includes --astf"; else bad "astf mode includes --astf"; fi
if [[ "$ACMD" == *"--lro-disable"* ]]; then ok "astf launch includes --lro-disable (else TRex dies on ENA)"; else bad "astf launch includes --lro-disable"; fi
if [[ "$SCMD" == *"--lro-disable"* ]]; then ok "stl launch includes --lro-disable"; else bad "stl launch includes --lro-disable"; fi
if [[ "$SCMD" != *"--astf"* ]]; then ok "stl mode omits --astf"; else bad "stl mode omits --astf"; fi

echo "== run_tcp_benchmark.py: ASTFIPGenDist ip_range must be a [start,end] list =="
TCP_PY="$TEST_DIR/../trex/run_tcp_benchmark.py"
if grep -nE 'ASTFIPGenDist\(ip_range=' "$TCP_PY" | grep -qvE 'ip_range=\['; then
    bad "ASTFIPGenDist uses a single-IP ip_range (TRex needs [start,end]):"
    grep -nE 'ASTFIPGenDist\(ip_range=' "$TCP_PY" | grep -vE 'ip_range=\['
else
    ok "all ASTFIPGenDist ip_range args are lists"
fi
if grep -qE 'glob=ASTFIPGenDist' "$TCP_PY"; then bad "ASTFIPGen glob must be ASTFIPGenGlobal, not ASTFIPGenDist"; else ok "ASTFIPGen glob is ASTFIPGenGlobal (not ASTFIPGenDist)"; fi

# ── 3. TRex mode selection: stl for UDP, astf for any *-tcp ───────────────────
echo "== TRex STL/ASTF mode selection =="
for c in "rust-dpdk-tcp,tokio-dpdk-tcp,plain-rust-tcp" "rust-dpdk-tcp"; do
    if [[ "$(trex_mode_for_configs "$c")" == astf ]]; then ok "astf for '$c'"; else bad "astf for '$c'"; fi
done
for c in "plain-rust,rust-dpdk,native-dpdk" "native-dpdk-v6,rust-dpdk-v6"; do
    if [[ "$(trex_mode_for_configs "$c")" == stl ]]; then ok "stl for '$c'"; else bad "stl for '$c'"; fi
done

# ── ensure_trex_mode restarts only when the mode actually changes ────────────
echo "== ensure_trex_mode restart-on-change =="
START_LOG="$STUB_DIR/start.log"; : > "$START_LOG"
start_trex_server() { echo "$1" >> "$START_LOG"; CURRENT_TREX_MODE="$1"; return 0; }  # stub
CURRENT_TREX_MODE=stl
ensure_trex_mode stl >/dev/null 2>&1
if [[ -s "$START_LOG" ]]; then bad "no restart when mode is unchanged"; else ok "no restart when mode is unchanged"; fi
ensure_trex_mode astf >/dev/null 2>&1
if grep -qx astf "$START_LOG"; then ok "restarts to astf when switching from stl"; else bad "restarts to astf when switching from stl"; fi

# ── 4. Aggregation, gates, DUT latency, idle%, exit-control inputs ───────────
echo "== aggregate_results / evaluate_perf_gates / summary (fake data) =="
FAKE="$(mktemp -d)"
export RESULTS_DIR="$FAKE/results" LOGS_DIR="$FAKE/logs"
mkdir -p "$RESULTS_DIR" "$LOGS_DIR"
python3 -I - "$RESULTS_DIR" "$LOGS_DIR" <<'PYEOF'
import json, sys
res, logs = sys.argv[1:3]
T0 = 1_000_000.0
def rows(size, per_step, base):
    out, t = [], base
    for step, rx in per_step:
        out.append({"target_pps": step, "rx_pps": rx, "tx_pps": step, "drop_pct": 0.0,
                    "ts_start_unix": t, "ts_end_unix": t + 30})
        t += 40
    return out
STEPS = [70000, 140000, 350000]
def cfg(name, rx700):
    d = {"config_name": name, "results": {}}
    for size in (64, 512, 1400):
        d["results"][f"{size}B"] = rows(size, [(s, s - 1000) for s in STEPS] + [(700000, rx700[0])], T0)
    return d
def rep(name, rx700, n):
    return {"config_name": name, "results": {f"{s}B": rows(s, [(700000, rx700)], T0 + 1000 * n) for s in (64, 512, 1400)}}
for name, vals in (("native-dpdk", (699000, 699000, 699000)), ("rust-dpdk", (698000, 697000, 699000)),
                   ("tokio-dpdk", (698000, 690000, 696000)), ("tokio-dpdk-concurrent", (300000, 305000, 310000))):
    json.dump(cfg(name, vals), open(f"{res}/{name}.rep1.json", "w"))
    for n in (2, 3):
        json.dump(rep(name, vals[n - 1], n), open(f"{res}/{name}.rep{n}.json", "w"))
# DUT [PERF] log for tokio-dpdk: an idle pre-traffic tick, then ticks inside / straddling a step.
def perf(ts, rx, p50, p99, idle):
    return (f"[PERF] ts_unix={ts:.3f} interval=10s rx_pps={rx} rx_bps=0 tx_pps={rx} tx_bps=0 rx_drops=0 "
            f"rx_ring_drops=0 rx_buf_drops=0 tx_fails=0 nic_imissed=0 nic_ierrors=0 nic_rx_nombuf=0 "
            f"lat_avg_us=1 lat_p50_us={p50} lat_p95_us=2 lat_p99_us={p99} lat_max_us=9 arp_hits=0 "
            f"arp_misses=0 ring_drops=0 worker_idle_pct={idle} burst_avg=1.0")
lines = [perf(T0 - 20, 0, 0.0, 0.0, 99.5),
         perf(T0 + 85, 350000, 0.4, 1.1, 40.0),    # window [T0+75, T0+85]: inside the 350k step (T0+80..T0+110)? starts before -> straddles
         perf(T0 + 95, 350000, 0.6, 2.5, 40.0),    # [T0+85, T0+95]: fully inside
         perf(T0 + 105, 350000, 0.5, 1.9, 40.0)]   # [T0+95, T0+105]: fully inside
open(f"{logs}/dut-tokio-dpdk-perf.log", "w").write("\n".join(lines) + "\n")
PYEOF
CONFIGS="rust-dpdk,tokio-dpdk,tokio-dpdk-concurrent,native-dpdk"; PACKET_SIZES="64,512,1400"
RATE_STEPS="70000,140000,350000,700000"
aggregate_results >/dev/null 2>&1
if [[ -f "$RESULTS_DIR/perf-report.json" ]]; then ok "perf-report.json written"; else bad "perf-report.json written"; fi
python3 -I - "$RESULTS_DIR/perf-report.json" > "$FAKE/check.out" 2>&1 <<'PYEOF'
import json, sys
r = json.load(open(sys.argv[1]))["configs"]
t = r["tokio-dpdk"]
assert sorted(t["reps"]) == ["2", "3"], "reps kept separately from rep 1"
assert t["results"]["64B"][3]["rx_pps"] == 698000, "rep 2/3 must not overwrite rep 1"
assert t["idle_pct"] == 99.5, f"idle% from first rx_pps=0 tick: {t.get('idle_pct')}"
step350 = t["results"]["64B"][2]
assert step350["dut_lat_p99_us"] == 2.5 and step350["dut_lat_p50_us"] == 0.6, f"max of fully-contained ticks only: {step350}"
assert "app_drops" in step350, "App Drops populated for tokio configs (0 is valid)"
PYEOF
if [[ $? -eq 0 ]]; then ok "reps keyed by (config, rep); idle% and contained-tick DUT p50/p99 extracted"; else bad "aggregation: $(cat "$FAKE/check.out")"; fi
evaluate_perf_gates >/dev/null 2>&1
if [[ "$(cat "$RESULTS_DIR/gate-verdict.txt" 2>/dev/null)" == PASS ]]; then ok "healthy fake run -> verdict PASS"; else bad "healthy fake run -> verdict PASS"; fi
SUMMARY="$(generate_markdown_summary 2>/dev/null)"
if [[ "$SUMMARY" == *"vs target"* && "$SUMMARY" == *"DUT p99 (us)"* && "$SUMMARY" == *"tokio-dpdk-concurrent"* ]]; then ok "summary has vs target, DUT latency columns and the concurrent config"; else bad "summary columns"; fi
if [[ "$SUMMARY" == *"poll-idle %"* ]]; then ok "summary labels idle% as poll-idle"; else bad "summary labels idle% as poll-idle"; fi
# Degrade tokio-dpdk's rep-1 700k below 95% of rust-dpdk -> FAIL with a GATE FAIL line.
python3 -I - "$RESULTS_DIR" <<'PYEOF'
import json, sys
for n in (1, 2, 3):
    f = f"{sys.argv[1]}/tokio-dpdk.rep{n}.json"
    d = json.load(open(f))
    for rows in d["results"].values():
        for r in rows:
            if r["target_pps"] == 700000:
                r["rx_pps"] = 300000
    json.dump(d, open(f, "w"))
PYEOF
aggregate_results >/dev/null 2>&1; evaluate_perf_gates > "$FAKE/gates.out" 2>&1
if [[ "$(cat "$RESULTS_DIR/gate-verdict.txt" 2>/dev/null)" == FAIL ]] && grep -q "GATE FAIL: tokio-dpdk @ 64B" "$FAKE/gates.out"; then ok "degraded tokio-dpdk -> FAIL + GATE FAIL line"; else bad "degraded tokio-dpdk -> FAIL"; fi
# Native missing entirely -> INVALID (run-invalid.txt, no verdict file).
rm -f "$RESULTS_DIR"/native-dpdk.rep*.json
aggregate_results >/dev/null 2>&1; evaluate_perf_gates >/dev/null 2>&1
if [[ -f "$RESULTS_DIR/run-invalid.txt" && ! -f "$RESULTS_DIR/gate-verdict.txt" ]]; then ok "native-dpdk missing -> run-invalid.txt, no verdict"; else bad "native-dpdk missing -> INVALID"; fi
# Crashed evaluator leaves no verdict (main() treats a missing verdict as FAIL).
rm -f "$RESULTS_DIR/perf-report.json" "$RESULTS_DIR/run-invalid.txt"
evaluate_perf_gates >/dev/null 2>&1
if [[ ! -f "$RESULTS_DIR/gate-verdict.txt" ]]; then ok "evaluator crash leaves no verdict file (fail-closed in main)"; else bad "evaluator crash leaves no verdict file"; fi
for t in tokio-dpdk-concurrent; do
    if grep -q "start_dut_tokio_dpdk_concurrent" "$HARNESS" && grep -q -- "--mode concurrent" "$HARNESS"; then ok "$t runner launches tokio-echo --mode concurrent"; else bad "$t runner"; fi
done
rm -rf "$FAKE"

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "== $PASS passed, $FAIL failed =="
rm -rf "$STUB_DIR"
[[ $FAIL -eq 0 ]]
