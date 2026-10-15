#!/usr/bin/env bash
# test-known-failures.sh — Unit tests for the known-failures gate (Req 1).
#
# Self-contained: builds synthetic JUnit XML fixtures and a synthetic
# known-failures list in a temp dir, then asserts the gate's exit code and
# messages for every rule. No AWS, no DPDK, no network — safe to run locally
# and in CI.
#
# Run:  scripts/ci/test-known-failures.sh
# Exit: 0 if all assertions pass, 1 otherwise.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$SCRIPT_DIR/check-known-failures.sh"

PASS=0
FAIL=0

ok()   { echo "ok   - $1"; PASS=$((PASS + 1)); }
nok()  { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

# make_suite <dir> <file> <suite> -- then pairs of: <name> pass|fail
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

# assert_exit <expected_code> <desc> -- reads actual code + output from globals
assert_exit() {
    local expected="$1" desc="$2"
    if [[ "$ACTUAL_CODE" -eq "$expected" ]]; then
        ok "$desc (exit $expected)"
    else
        nok "$desc (expected exit $expected, got $ACTUAL_CODE)"
        echo "--- output ---"; echo "$ACTUAL_OUT"; echo "--------------"
    fi
}

# assert_contains <needle> <desc>
assert_contains() {
    local needle="$1" desc="$2"
    if grep -qF "$needle" <<<"$ACTUAL_OUT"; then
        ok "$desc"
    else
        nok "$desc (missing: '$needle')"
        echo "--- output ---"; echo "$ACTUAL_OUT"; echo "--------------"
    fi
}

run_gate() {   # run_gate <known_file> <results_dir> [extra args...]
    local kf="$1" rd="$2"; shift 2
    ACTUAL_OUT="$("$GATE" --known-file "$kf" --results-dir "$rd" "$@" 2>&1)"
    ACTUAL_CODE=$?
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ── Fixture 1: all three listed tests FAIL as expected -> gate passes ────────
D1="$TMP/all-xfail"
make_suite "$D1" "tier1-echo.xml" "tier1-tcp-echo-full" bidir_echo pass bidir_multi fail
make_suite "$D1" "tier2-echo.xml" "tier2-tcp-echo" kernel_to_dpdk_echo fail
make_suite "$D1" "tier1-hs.xml"   "tier1-tcp-handshake" execution fail
KF1="$TMP/kf1.txt"
cat > "$KF1" <<EOF
bidir_multi                    # Task 1
kernel_to_dpdk_echo            # Task 2
tier1-tcp-handshake/execution  # Task 3
EOF
run_gate "$KF1" "$D1"
assert_exit 0 "all listed tests fail as expected -> green"
assert_contains "failed as expected" "xfail-ok message present"

# ── Fixture 2: a listed test PASSES unexpectedly -> gate fails (Req 1.2) ──────
D2="$TMP/unexpected-pass"
make_suite "$D2" "tier1-echo.xml" "tier1-tcp-echo-full" bidir_echo pass bidir_multi pass
make_suite "$D2" "tier2-echo.xml" "tier2-tcp-echo" kernel_to_dpdk_echo fail
make_suite "$D2" "tier1-hs.xml"   "tier1-tcp-handshake" execution fail
run_gate "$KF1" "$D2"
assert_exit 1 "listed test passing unexpectedly -> fail"
assert_contains "unexpected pass: remove 'bidir_multi'" "unexpected-pass message (Req 1.2)"

# ── Fixture 3: a listed test did NOT run -> gate fails (Req 1.3) ─────────────
D3="$TMP/not-run"
make_suite "$D3" "tier1-echo.xml" "tier1-tcp-echo-full" bidir_echo pass bidir_multi fail
# tier2 echo suite entirely absent -> kernel_to_dpdk_echo did not run
make_suite "$D3" "tier1-hs.xml"   "tier1-tcp-handshake" execution fail
run_gate "$KF1" "$D3"
assert_exit 1 "listed test not running -> fail"
assert_contains "known failure did not run: kernel_to_dpdk_echo" "did-not-run message (Req 1.3)"

# ── Fixture 4: suite-qualified disambiguation ────────────────────────────────
# A bare 'execution' testcase appears in two suites; only the handshake one is
# listed and it fails -> gate passes. A passing 'execution' in another suite
# must NOT be treated as the listed entry.
D4="$TMP/qualified"
make_suite "$D4" "tier1-echo.xml" "tier1-tcp-echo-full" bidir_echo pass bidir_multi fail
make_suite "$D4" "tier2-echo.xml" "tier2-tcp-echo" kernel_to_dpdk_echo fail
make_suite "$D4" "tier1-hs.xml"   "tier1-tcp-handshake" execution fail
make_suite "$D4" "tier3-parity.xml" "tier3-tcp-std-parity" execution pass
run_gate "$KF1" "$D4"
assert_exit 0 "suite-qualified name matches only its suite -> green"

# ── Fixture 5: empty results dir -> all listed did not run -> fail ───────────
D5="$TMP/empty"; mkdir -p "$D5"
run_gate "$KF1" "$D5"
assert_exit 1 "empty results -> did-not-run failures"
assert_contains "known failure did not run: bidir_multi" "empty results flags each entry"

# ── Fixture 6: comment/blank-only known file -> nothing to enforce -> pass ───
KF_EMPTY="$TMP/kf-empty.txt"
printf '# just a comment\n\n   \n' > "$KF_EMPTY"
run_gate "$KF_EMPTY" "$D1"
assert_exit 0 "comment/blank-only list -> green"
assert_contains "nothing to enforce" "empty-list message"

# ── Fixture 6b: suite-qualified entry whose SUITE is absent -> inapplicable ──
# Mirrors an all-tiers run where the tcp-full-only `tier1-tcp-handshake` suite
# never runs. bidir_echo (async-fail) + bidir_multi (async-fail) +
# kernel_to_dpdk_echo (fail) are expected; the handshake entry is skipped.
D6B="$TMP/inapplicable"
# sync suite passes both; async suite fails both (bare names resolve to fail)
make_suite "$D6B" "echo-sync.xml"  "tier1-tcp-echo"       bidir_echo pass bidir_multi pass
make_suite "$D6B" "echo-async.xml" "tier1-tcp-echo-async" bidir_echo fail bidir_multi fail
make_suite "$D6B" "k2d.xml"        "tier2-tcp-echo"       kernel_to_dpdk_echo fail
KF_REAL="$TMP/kf-real.txt"
cat > "$KF_REAL" <<EOF
bidir_echo                     # Task 1
bidir_multi                    # Task 1
kernel_to_dpdk_echo            # Task 2
tier1-tcp-handshake/execution  # Task 3 (tcp-full only)
EOF
run_gate "$KF_REAL" "$D6B"
assert_exit 0 "all-tiers: bare bidir fail + k2d fail, handshake inapplicable -> GREEN"
assert_contains "not applicable" "inapplicable message for absent suite"
assert_contains "'bidir_echo' failed as expected" "bare name resolves fail across suites"

# ── Fixture 6c: suite ran but the listed testcase is absent -> notrun -> fail ─
D6C="$TMP/suite-ran-tc-absent"
make_suite "$D6C" "hs.xml" "tier1-tcp-handshake" some_other_case fail
KF_HS="$TMP/kf-hs.txt"
printf 'tier1-tcp-handshake/execution  # Task 3\n' > "$KF_HS"
run_gate "$KF_HS" "$D6C"
assert_exit 1 "suite ran but listed testcase absent -> did-not-run fail"
assert_contains "known failure did not run: tier1-tcp-handshake/execution" "notrun within present suite"

# ── Fixture 7: ratchet in a throwaway git repo ───────────────────────────────
GITREPO="$TMP/repo"
mkdir -p "$GITREPO/ci"
(
  cd "$GITREPO"
  git init -q
  git config user.email t@t && git config user.name t
  printf 'bidir_multi  # Task 1\n' > ci/known-failures.txt
  git add -A && git commit -qm base
  git branch base-ref
  # add a new xfail line on top of base
  printf 'bidir_multi  # Task 1\nnew_test  # sneaky\n' > ci/known-failures.txt
  git add -A && git commit -qm addline
)
ACTUAL_OUT="$(cd "$GITREPO" && "$GATE" --ratchet --base base-ref \
    --known-file "$GITREPO/ci/known-failures.txt" 2>&1)"
ACTUAL_CODE=$?
assert_exit 1 "ratchet detects an added xfail line (Req 1.4)"
assert_contains "may not add lines" "ratchet error message"

# ── Fixture 8: ratchet passes when only comments/removals change ─────────────
(
  cd "$GITREPO"
  # Remove the sneaky line (a fix PR deleting its own entry) + add a comment.
  printf '# header\nbidir_multi  # Task 1\n' > ci/known-failures.txt
  git add -A && git commit -qm removeline
)
ACTUAL_OUT="$(cd "$GITREPO" && "$GATE" --ratchet --base base-ref \
    --known-file "$GITREPO/ci/known-failures.txt" 2>&1)"
ACTUAL_CODE=$?
assert_exit 0 "ratchet allows removals + comment-only additions"

echo "────────────────────────────────────────"
echo "known-failures gate tests: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
