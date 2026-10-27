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

# ── 4. Perf gates end to end: reps keying, medians, verdict, summary ─────────
echo "== aggregate -> evaluate_perf_gates -> summary (synthetic reps) =="
GATE_TMP="$(mktemp -d)"
build_fixture() {   # $1=tokio 700k rx_pps  $2=native 700k rx_pps
    local d="$1" tokio_pps="$2" native_pps="$3"
    mkdir -p "$d/results" "$d/logs"
    python3 - "$d" "$tokio_pps" "$native_pps" <<'PYFIX'
import json, sys
d, tokio, native = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
T0 = 1_000_000.0
def steps(pps700, with_low=True):
    out = []
    t = T0
    plan = ([(70000, 69000), (140000, 138000), (350000, 340000)] if with_low else []) + [(700000, pps700)]
    for tgt, rx in plan:
        out.append({"target_pps": tgt, "rx_pps": rx, "tx_pps": tgt, "drop_pct": 0.0,
                    "ts_start_unix": t, "ts_end_unix": t + 30})
        t += 40
    return out
for cfg, pps in (("native-dpdk", native), ("rust-dpdk", 698000), ("tokio-dpdk", tokio), ("tokio-dpdk-concurrent", 300000)):
    for rep in (1, 2, 3):
        data = {"config_name": cfg, "results": {"64B": steps(pps, with_low=(rep == 1))}}
        json.dump(data, open(f"{d}/results/{cfg}.rep{rep}.json", "w"))
# tokio DUT log: an idle tick (rx_pps=0) then a tick fully inside the first step window.
with open(f"{d}/logs/dut-tokio-dpdk-perf.log", "w") as fh:
    fh.write("[PERF] ts_unix=999990.000 interval=10s rx_pps=0 rx_drops=0 rx_ring_drops=0 rx_buf_drops=0 "
             "lat_p50_us=0.0 lat_p99_us=0.0 worker_idle_pct=97.5 burst_avg=0.0\n")
    fh.write("[PERF] ts_unix=1000020.000 interval=10s rx_pps=69000 rx_drops=0 rx_ring_drops=0 rx_buf_drops=0 "
             "lat_p50_us=0.8 lat_p99_us=4.2 worker_idle_pct=10.0 burst_avg=3.0\n")
    fh.write("[PERF] ts_unix=1000030.000 interval=10s rx_pps=69000 rx_drops=0 rx_ring_drops=0 rx_buf_drops=0 "
             "lat_p50_us=0.9 lat_p99_us=9.9 worker_idle_pct=10.0 burst_avg=3.0\n")
PYFIX
}
run_pipeline() {    # $1=fixture dir; leaves verdict files in results/
    RESULTS_DIR="$1/results"; LOGS_DIR="$1/logs"; export RESULTS_DIR LOGS_DIR
    CONFIGS="rust-dpdk,tokio-dpdk,tokio-dpdk-concurrent,native-dpdk"
    PACKET_SIZES="64"; RATE_STEPS="70000,140000,350000,700000"
    aggregate_results >/dev/null 2>&1
    evaluate_perf_gates >/dev/null 2>&1
}

build_fixture "$GATE_TMP/good" 690000 699000
run_pipeline "$GATE_TMP/good"
if [[ "$(cat "$GATE_TMP/good/results/gate-verdict.txt" 2>/dev/null)" == PASS ]]; then ok "healthy fixture => gate-verdict PASS"; else bad "healthy fixture => gate-verdict PASS"; fi
if python3 - "$GATE_TMP/good/results/perf-report.json" <<'PYCHK'
import json, sys
r = json.load(open(sys.argv[1]))
vals = r["rep_values"]["tokio-dpdk"]["64B"]["700000"]
t = r["configs"]["tokio-dpdk"]
assert len(vals) == 3, vals                      # rep2/rep3 did not overwrite rep1
assert t["idle_pct"] == 97.5, t.get("idle_pct")  # first rx_pps=0 tick
step = t["results"]["64B"][0]                    # 70k step window [1000000, 1000030]
assert step["dut_lat_p50_us"] == 0.9 and step["dut_lat_p99_us"] == 9.9, step  # MAX of in-window ticks (the 999990 tick is outside), no averaging
PYCHK
then ok "aggregator keys by (config,rep), extracts idle% and in-window DUT latency"; else bad "aggregator keys by (config,rep), extracts idle% and in-window DUT latency"; fi
SUMMARY="$(generate_markdown_summary 2>/dev/null)"
if [[ "$SUMMARY" == *"vs target"* && "$SUMMARY" == *"DUT p99 (us)"* && "$SUMMARY" == *"Idle Worker %"* ]]; then ok "summary has vs-target, DUT latency and idle% sections"; else bad "summary has vs-target, DUT latency and idle% sections"; fi
if [[ "$SUMMARY" == *"tokio-dpdk-concurrent"* ]]; then ok "summary lists tokio-dpdk-concurrent"; else bad "summary lists tokio-dpdk-concurrent"; fi

build_fixture "$GATE_TMP/slow" 300000 699000
run_pipeline "$GATE_TMP/slow"
if [[ "$(cat "$GATE_TMP/slow/results/gate-verdict.txt" 2>/dev/null)" == FAIL ]]; then ok "tokio @ 300k vs rust 698k => gate-verdict FAIL"; else bad "tokio @ 300k => gate-verdict FAIL"; fi

build_fixture "$GATE_TMP/bad" 690000 100000
run_pipeline "$GATE_TMP/bad"
if [[ -f "$GATE_TMP/bad/results/run-invalid.txt" && ! -f "$GATE_TMP/bad/results/gate-verdict.txt" ]]; then ok "native @64B < 600k => run-invalid.txt (exit 2 path), no verdict"; else bad "native sanity => run-invalid.txt"; fi

# reps plan: requested sizes/configs only, never 8500.
PLAN="$(python3 "$PERF_GATES_PY" reps-plan --targets "$PERF_TARGETS_FILE" \
    --configs "plain-rust,rust-dpdk,tokio-dpdk,native-dpdk" --sizes "64,512,1400,8500" --steps "70000,700000")"
if [[ "$(echo "$PLAN" | sed -n 2p)" == "64,512,1400" ]]; then ok "reps plan sizes exclude 8500"; else bad "reps plan sizes exclude 8500 (got: $PLAN)"; fi
if [[ "$(echo "$PLAN" | sed -n 1p)" == "rust-dpdk,tokio-dpdk,native-dpdk" ]]; then ok "reps plan configs exclude non-judged plain-rust"; else bad "reps plan configs (got: $PLAN)"; fi

echo "== tokio-dpdk-concurrent wired end to end =="
if declare -F start_dut_tokio_dpdk_concurrent >/dev/null; then ok "start_dut_tokio_dpdk_concurrent defined"; else bad "start_dut_tokio_dpdk_concurrent defined"; fi
if grep -q 'tokio-echo --mode concurrent' "$HARNESS" && grep -q 'echo-tokio-dpdk-concurrent.log' "$HARNESS"; then ok "concurrent DUT launches tokio-echo --mode concurrent to its own log"; else bad "concurrent DUT launch/log"; fi
if [[ "$CONFIGS" == *tokio-dpdk-concurrent* ]] || grep -q '^CONFIGS=".*tokio-dpdk-concurrent' "$HARNESS"; then ok "default CONFIGS include tokio-dpdk-concurrent"; else bad "default CONFIGS include tokio-dpdk-concurrent"; fi
WF="$TEST_DIR/../../../.github/workflows/perf-tests.yml"
if [[ "$(grep -c 'tokio-dpdk-concurrent' "$WF")" -ge 2 ]]; then ok "perf-tests.yml default + --configs fallback include tokio-dpdk-concurrent"; else bad "perf-tests.yml default + --configs fallback include tokio-dpdk-concurrent"; fi
rm -rf "$GATE_TMP"

echo "== evaluate_perf_gates.py unit tests =="
if python3 -m unittest "$TEST_DIR/test_evaluate_perf_gates.py" >/dev/null 2>&1; then ok "python unit tests pass"; else bad "python unit tests pass"; fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "== $PASS passed, $FAIL failed =="
rm -rf "$STUB_DIR"
[[ $FAIL -eq 0 ]]
