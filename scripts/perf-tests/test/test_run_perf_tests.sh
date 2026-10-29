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

# ── 4. tokio-dpdk-concurrent is wired end to end ─────────────────────────────
echo "== tokio-dpdk-concurrent wiring =="
if [[ ",$CONFIGS," == *",tokio-dpdk-concurrent,"* ]]; then ok "default CONFIGS includes tokio-dpdk-concurrent"; else bad "default CONFIGS includes tokio-dpdk-concurrent"; fi
if declare -F start_dut_tokio_dpdk_concurrent >/dev/null; then ok "start_dut_tokio_dpdk_concurrent defined"; else bad "start_dut_tokio_dpdk_concurrent defined"; fi
if grep -q 'tokio-echo --mode concurrent' "$HARNESS"; then ok "runner launches tokio-echo --mode concurrent"; else bad "runner launches tokio-echo --mode concurrent"; fi
if grep -q 'start_dut_tokio_dpdk_concurrent *|| start_ok=false' "$HARNESS"; then ok "config->runner dispatch arm present"; else bad "config->runner dispatch arm present"; fi
if grep -q '/var/log/echo-tokio-dpdk-concurrent.log" ;;' "$HARNESS"; then ok "DUT-log case arm present"; else bad "DUT-log case arm present"; fi
WF="$TEST_DIR/../../../.github/workflows/perf-tests.yml"
if [[ $(grep -c 'tokio-dpdk-concurrent' "$WF") -ge 3 ]]; then ok "perf-tests.yml default + both fallbacks include it"; else bad "perf-tests.yml default + both fallbacks include it (found $(grep -c 'tokio-dpdk-concurrent' "$WF"))"; fi

# ── 5. Aggregation, gates and exit flow on synthetic per-rep results ─────────
echo "== aggregate_results / evaluate_perf_gates / exit flow =="
WORK="$STUB_DIR/work"; mkdir -p "$WORK"
RESULTS_DIR="$WORK/results"; LOGS_DIR="$WORK/logs"; export RESULTS_DIR LOGS_DIR
mkdir -p "$RESULTS_DIR" "$LOGS_DIR"
CONFIGS="native-dpdk,rust-dpdk,tokio-dpdk"; PACKET_SIZES="64"; RATE_STEPS="70000,700000"

# mkrep <config> <rep> <rx@700k> <ts_start> : one 64B 700k row (rep1 also has 70k)
mkrep() {
    python3 - "$@" <<'PY'
import json, sys, os
cfg, rep, rx, t0 = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4])
rows = [{"target_pps": 700000, "rx_pps": rx, "tx_pps": 700000, "drop_pct": 0.0,
         "ts_start_unix": t0, "ts_end_unix": t0 + 30}]
if rep == 1:
    rows.insert(0, {"target_pps": 70000, "rx_pps": 70000, "tx_pps": 70000, "drop_pct": 0.0,
                    "ts_start_unix": t0 - 100, "ts_end_unix": t0 - 70})
json.dump({"config_name": cfg, "results": {"64B": rows}},
          open(os.path.join(os.environ["RESULTS_DIR"], f"{cfg}.rep{rep}.json"), "w"))
PY
}
for c in native-dpdk rust-dpdk tokio-dpdk; do
    mkrep $c 1 700000 1000; mkrep $c 2 690000 2000; mkrep $c 3 700000 3000
done
mkrep tokio-dpdk 2 100000 2000   # outlier rep: median must ignore it

tick() { echo "[PERF] ts_unix=$1 interval=10s rx_pps=$2 rx_bps=0 tx_pps=0 tx_bps=0 rx_drops=0 rx_ring_drops=0 rx_buf_drops=0 tx_fails=0 nic_imissed=0 nic_ierrors=0 nic_rx_nombuf=0 lat_avg_us=1 lat_p50_us=$3 lat_p95_us=2 lat_p99_us=$4 lat_max_us=5 arp_hits=0 arp_misses=0 ring_drops=0 worker_idle_pct=$5 burst_avg=1.0"; }
{
    tick 900.000 0 0.0 0.0 99.9          # pre-traffic idle tick
    tick 1020.000 700000 0.4 2.5 40.0    # fully inside rep-1 700k window [1000,1030]
    tick 1035.000 700000 0.9 9.9 40.0    # straddles the window end -> discarded
    tick 2020.000 700000 0.6 3.5 40.0    # fully inside rep-2 window [2000,2030]
} > "$LOGS_DIR/dut-tokio-dpdk-perf.log"

aggregate_results >/dev/null 2>&1
REPORT="$RESULTS_DIR/perf-report.json"
jget() { python3 -c "import json,sys; d=json.load(open('$REPORT')); print(eval(sys.argv[1]))" "$1" 2>/dev/null; }
row='d["configs"]["tokio-dpdk"]["results"]["64B"][-1]'
if [[ "$(jget 'd["configs"]["tokio-dpdk"]["reps"]')" == "[1, 2, 3]" ]]; then ok "rep2/rep3 are keyed separately (not overwriting rep1)"; else bad "rep2/rep3 are keyed separately (got $(jget 'd["configs"]["tokio-dpdk"]["reps"]'))"; fi
if [[ "$(jget "$row[\"rx_pps_median\"]")" == "700000" ]]; then ok "700k median over 3 reps ignores the outlier"; else bad "700k median over 3 reps ignores the outlier (got $(jget "$row[\"rx_pps_median\"]"))"; fi
if [[ "$(jget "$row[\"dut_lat_p99_us\"]")" == "3.5" ]]; then ok "DUT p99 = max over fully-contained ticks of all reps (straddling tick discarded)"; else bad "DUT p99 max-over-contained (got $(jget "$row[\"dut_lat_p99_us\"]"))"; fi
if [[ "$(jget "$row[\"dut_lat_p50_us\"]")" == "0.6" ]]; then ok "DUT p50 is the max of per-interval p50s, not an average"; else bad "DUT p50 max (got $(jget "$row[\"dut_lat_p50_us\"]"))"; fi
if [[ "$(jget 'd["configs"]["tokio-dpdk"]["idle_pct"]')" == "99.9" ]]; then ok "idle% taken from the first rx_pps=0 tick"; else bad "idle% from first rx_pps=0 tick (got $(jget 'd["configs"]["tokio-dpdk"]["idle_pct"]'))"; fi

evaluate_perf_gates >/dev/null 2>&1
if [[ "$(cat "$RESULTS_DIR/gate-verdict.txt" 2>/dev/null)" == PASS ]]; then ok "healthy run -> gate-verdict PASS"; else bad "healthy run -> PASS (got '$(cat "$RESULTS_DIR/gate-verdict.txt" 2>/dev/null)'; $(cat "$RESULTS_DIR/gate-lines.md" 2>/dev/null))"; fi
( perf_gate_exit_check ) >/dev/null 2>&1; rc=$?
if [[ $rc -eq 0 ]]; then ok "PASS verdict -> exit check returns 0"; else bad "PASS verdict -> exit check returns 0 (rc=$rc)"; fi

SUMMARY="$(generate_markdown_summary 2>/dev/null)"
for needle in "vs target" "vs 0.3.0" "DUT p50 (us)" "DUT p99 (us)" "poll-idle %" "| tokio-dpdk |"; do
    if [[ "$SUMMARY" == *"$needle"* ]]; then ok "summary has '$needle'"; else bad "summary has '$needle'"; fi
done
if grep -E '^\| tokio-dpdk \| 700,000 ' <<< "$SUMMARY" | grep -q '105.3% PASS'; then ok "tokio-dpdk 700k row shows '105.3% PASS' (700k median / 95%-of-rust target)"; else bad "tokio-dpdk 700k row vs target"; fi

# tokio median 100k (< 95% of rust) -> FAIL, exit 1
for r in 1 2 3; do mkrep tokio-dpdk $r 100000 $((r * 1000)); done
aggregate_results >/dev/null 2>&1; evaluate_perf_gates >/dev/null 2>&1
if [[ "$(cat "$RESULTS_DIR/gate-verdict.txt")" == FAIL ]]; then ok "tokio far below rust -> verdict FAIL"; else bad "tokio far below rust -> verdict FAIL"; fi
( perf_gate_exit_check ) >/dev/null 2>&1; rc=$?
if [[ $rc -eq 1 ]]; then ok "FAIL verdict -> exit 1"; else bad "FAIL verdict -> exit 1 (rc=$rc)"; fi

# missing verdict file is a FAIL (fail-closed), never a silent pass
rm -f "$RESULTS_DIR/gate-verdict.txt"
( perf_gate_exit_check ) >/dev/null 2>&1; rc=$?
if [[ $rc -eq 1 ]]; then ok "missing verdict -> exit 1 (fail-closed)"; else bad "missing verdict -> exit 1 (rc=$rc)"; fi

# garbage verdict is a FAIL too
echo "PASSED?" > "$RESULTS_DIR/gate-verdict.txt"
( perf_gate_exit_check ) >/dev/null 2>&1; rc=$?
if [[ $rc -eq 1 ]]; then ok "garbage verdict -> exit 1"; else bad "garbage verdict -> exit 1 (rc=$rc)"; fi

# slow native @64B (< 600k) -> INVALID run, exit 2 (infra), not a gate FAIL
for r in 1 2 3; do mkrep native-dpdk $r 400000 $((r * 1000)); done
aggregate_results >/dev/null 2>&1; evaluate_perf_gates >/dev/null 2>&1
if [[ -f "$RESULTS_DIR/run-invalid.txt" && ! -f "$RESULTS_DIR/gate-verdict.txt" ]]; then ok "native @64B < 600k -> run-invalid.txt, no verdict"; else bad "native @64B < 600k -> run-invalid.txt, no verdict"; fi
( perf_gate_exit_check ) >/dev/null 2>&1; rc=$?
if [[ $rc -eq 2 ]]; then ok "invalid run -> exit 2"; else bad "invalid run -> exit 2 (rc=$rc)"; fi

# reps 2-3 plan comes from the requested inputs, never a hard-coded list
PACKET_SIZES="64,512,1400,8500"; RATE_STEPS="70000,140000,350000,700000"
CONFIGS="plain-rust,rust-dpdk,tokio-dpdk,tokio-dpdk-concurrent,native-dpdk"
PLAN="$(python3 "$EVALUATE_GATES" rep-plan --targets "$PERF_TARGETS" --configs "$CONFIGS" --packet-sizes "$PACKET_SIZES" --rate-steps "$RATE_STEPS")"
if grep -qx 'REP_SIZES=64,512,1400' <<< "$PLAN"; then ok "rep sizes exclude 8500"; else bad "rep sizes exclude 8500 ($PLAN)"; fi
if grep -qx 'REP_CONFIGS=rust-dpdk,tokio-dpdk,tokio-dpdk-concurrent,native-dpdk' <<< "$PLAN"; then ok "rep configs = judged set incl. native-dpdk, excl. plain-rust"; else bad "rep configs ($PLAN)"; fi

echo "== evaluate_gates.py unit tests =="
if python3 "$TEST_DIR/test_evaluate_gates.py" >/dev/null 2>&1; then ok "test_evaluate_gates.py passes"; else bad "test_evaluate_gates.py passes"; fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "== $PASS passed, $FAIL failed =="
rm -rf "$STUB_DIR"
[[ $FAIL -eq 0 ]]
