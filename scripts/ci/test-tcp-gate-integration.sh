#!/usr/bin/env bash
# test-tcp-gate-integration.sh — Tests for the orchestrator's
# evaluate_known_failures_gate reconciliation (tcp-ci-reliability Req 1).
#
# Sources scripts/run-integration-tests.sh (guarded so main() does not run) and
# drives evaluate_known_failures_gate against synthetic JUnit XML result sets,
# asserting the resulting TEST_EXIT_CODE. Proves the Task 0 verification: CI goes
# GREEN on a branch where all three known tests fail and everything else passes.
#
# No AWS, no DPDK, no network. Run: scripts/ci/test-tcp-gate-integration.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ORCH="$SCRIPT_DIR/../run-integration-tests.sh"

PASS=0; FAIL=0
ok()  { echo "ok   - $1"; PASS=$((PASS + 1)); }
nok() { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

# Source the orchestrator for its functions. The sourcing guard prevents main().
# shellcheck disable=SC1090
source "$ORCH" >/dev/null 2>&1 || { echo "FAIL - could not source orchestrator"; exit 1; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# make_suite <dir> <file> <suite> [<name> pass|fail]...
make_suite() {
    local dir="$1" file="$2" suite="$3"; shift 3
    local cases="" count=0 failures=0
    while [[ $# -gt 0 ]]; do
        local name="$1" status="$2"; shift 2
        count=$((count + 1))
        if [[ "$status" == "fail" ]]; then
            failures=$((failures + 1))
            cases+="    <testcase name=\"$name\" classname=\"c\" time=\"0.1\"><failure message=\"boom\" type=\"AssertionError\">x</failure></testcase>
"
        else
            cases+="    <testcase name=\"$name\" classname=\"c\" time=\"0.1\"></testcase>
"
        fi
    done
    mkdir -p "$dir"
    cat > "$dir/$file" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="$suite" tests="$count" failures="$failures" errors="0" time="0.3">
$cases</testsuite>
EOF
}

KF="$TMP/known.txt"
cat > "$KF" <<EOF
bidir_echo                     # Task 1
bidir_multi                    # Task 1
kernel_to_dpdk_echo            # Task 2
tier1-tcp-handshake/execution  # Task 3
EOF

# Point the orchestrator globals at our fixtures. evaluate_known_failures_gate
# reads $REPO_ROOT/ci/known-failures.txt, $RESULTS_DIR and $SCRIPT_DIR/ci/...
# We set REPO_ROOT to a temp root holding ci/known-failures.txt, and keep
# SCRIPT_DIR pointing at the real scripts dir so the gate script is found.
# tier arg lets a case choose tcp-full vs all-tiers (empty) mode.
run_eval() {   # run_eval <results_dir> [tier]
    local rd="$1"
    local tier="${2:-tcp-full}"
    local root="$TMP/root"
    mkdir -p "$root/ci"
    cp "$KF" "$root/ci/known-failures.txt"
    REPO_ROOT="$root"
    RESULTS_DIR="$rd"
    SCRIPT_DIR="$(cd "$ORCH" >/dev/null 2>&1; cd "$(dirname "$ORCH")" && pwd)"
    TIER_FILTER="$tier"
    TEST_EXIT_CODE=99
    evaluate_known_failures_gate >/dev/null 2>&1
}

# ── Case A: Task 0 verification (tcp-full) — all known tests fail, rest pass ──
DA="$TMP/a"
make_suite "$DA" "echo.xml"   "tier1-tcp-echo-full"    bidir_echo fail bidir_multi fail
make_suite "$DA" "hs.xml"     "tier1-tcp-handshake"    execution fail
make_suite "$DA" "shut.xml"   "tier1-tcp-shutdown"     bidir_shutdown pass
make_suite "$DA" "retx.xml"   "tier2-tcp-retransmit"   retransmit pass
make_suite "$DA" "fc.xml"     "tier2-tcp-flow-control" flow_control pass
make_suite "$DA" "k2d.xml"    "tier2-tcp-echo"         kernel_to_dpdk_echo fail
make_suite "$DA" "interop.xml" "tier3-tcp-kernel-interop" kernel_interop pass
make_suite "$DA" "parity.xml" "tier3-tcp-std-parity"   std_parity pass
run_eval "$DA" tcp-full
[[ "$TEST_EXIT_CODE" -eq 0 ]] && ok "Task 0 (tcp-full): all known fail + rest pass -> GREEN" \
    || nok "Task 0 (tcp-full): all known fail + rest pass -> GREEN (got $TEST_EXIT_CODE)"

# ── Case A2: all-tiers mode (the BLOCKING Graviton job) ──────────────────────
# sync tier1-tcp-echo passes; async tier1-tcp-echo-async fails both; k2d fails;
# handshake suite is NOT present (tcp-full-only) -> inapplicable. -> GREEN
DA2="$TMP/a2"
make_suite "$DA2" "echo-sync.xml"  "tier1-tcp-echo"       bidir_echo pass bidir_multi pass
make_suite "$DA2" "echo-async.xml" "tier1-tcp-echo-async" bidir_echo fail bidir_multi fail
make_suite "$DA2" "k2d.xml"        "tier2-tcp-echo"       kernel_to_dpdk_echo fail
make_suite "$DA2" "udp.xml"        "tier1-dpdk-echo"      udp_send_receive pass
make_suite "$DA2" "iperf.xml"      "tier3-iperf-interop"  iperf_sends pass
run_eval "$DA2" ""
[[ "$TEST_EXIT_CODE" -eq 0 ]] && ok "all-tiers: async-fail xfail + handshake inapplicable -> GREEN" \
    || nok "all-tiers: async-fail xfail + handshake inapplicable -> GREEN (got $TEST_EXIT_CODE)"
# On GREEN, the gate must have rewritten the known failures to <skipped> so
# dorny/test-reporter stays green; k2d + async bidir failures -> skipped.
if ! grep -q '<failure' "$DA2/echo-async.xml" && ! grep -q '<failure' "$DA2/k2d.xml" \
   && grep -q '<skipped' "$DA2/echo-async.xml"; then
    ok "all-tiers GREEN: known failures rewritten to <skipped> in JUnit XML"
else
    nok "all-tiers GREEN: expected known failures rewritten to skipped"
fi

# ── Case B: an unlisted test also fails -> RED ───────────────────────────────
DB="$TMP/b"
make_suite "$DB" "echo.xml" "tier1-tcp-echo-full" bidir_echo fail bidir_multi fail
make_suite "$DB" "hs.xml"   "tier1-tcp-handshake" execution fail
make_suite "$DB" "k2d.xml"  "tier2-tcp-echo"      kernel_to_dpdk_echo fail
make_suite "$DB" "shut.xml" "tier1-tcp-shutdown"  bidir_shutdown fail   # UNLISTED failure
run_eval "$DB" tcp-full
[[ "$TEST_EXIT_CODE" -eq 1 ]] && ok "unlisted failure -> RED" \
    || nok "unlisted failure -> RED (got $TEST_EXIT_CODE)"

# ── Case C: a known test is fixed (passes) but still listed -> RED ───────────
DC="$TMP/c"
make_suite "$DC" "echo.xml" "tier1-tcp-echo-full" bidir_echo pass bidir_multi pass  # fixed, still listed
make_suite "$DC" "hs.xml"   "tier1-tcp-handshake" execution fail
make_suite "$DC" "k2d.xml"  "tier2-tcp-echo"      kernel_to_dpdk_echo fail
run_eval "$DC" tcp-full
[[ "$TEST_EXIT_CODE" -eq 1 ]] && ok "fixed-but-listed test -> RED (forces line removal)" \
    || nok "fixed-but-listed test -> RED (got $TEST_EXIT_CODE)"

# ── Case D: one known bug fixed AND delisted, others still fail -> GREEN ──────
# Simulates the Task 1 PR: bidir_multi removed from the list and now passes;
# the other two still fail as expected.
DD="$TMP/d"
make_suite "$DD" "echo.xml" "tier1-tcp-echo-full" bidir_echo pass bidir_multi pass
make_suite "$DD" "hs.xml"   "tier1-tcp-handshake" execution fail
make_suite "$DD" "k2d.xml"  "tier2-tcp-echo"      kernel_to_dpdk_echo fail
# delisted known file
KF2="$TMP/known2.txt"
cat > "$KF2" <<EOF
kernel_to_dpdk_echo            # Task 2
tier1-tcp-handshake/execution  # Task 3
EOF
root2="$TMP/root2"; mkdir -p "$root2/ci"; cp "$KF2" "$root2/ci/known-failures.txt"
REPO_ROOT="$root2"; RESULTS_DIR="$DD"; TIER_FILTER="tcp-full"; TEST_EXIT_CODE=99
evaluate_known_failures_gate >/dev/null 2>&1
[[ "$TEST_EXIT_CODE" -eq 0 ]] && ok "one bug fixed+delisted, others xfail -> GREEN" \
    || nok "one bug fixed+delisted, others xfail -> GREEN (got $TEST_EXIT_CODE)"

echo "────────────────────────────────────────"
echo "tcp gate integration tests: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
