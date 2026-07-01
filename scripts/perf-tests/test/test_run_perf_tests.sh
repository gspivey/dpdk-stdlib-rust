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

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "== $PASS passed, $FAIL failed =="
rm -rf "$STUB_DIR"
[[ $FAIL -eq 0 ]]
