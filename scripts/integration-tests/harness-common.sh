#!/usr/bin/env bash
# harness-common.sh - Shared test harness utilities for EC2 integration tests
#
# Provides:
#   - JUnit XML generation functions
#   - Test execution helpers (timeout, logging)
#   - Deterministic output path generation
#
# Source this file from test harness scripts:
#   source "$(dirname "$0")/harness-common.sh"

set -euo pipefail

# ── Logging ──────────────────────────────────────────────────────────────────

log_info() {
    echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] INFO: $*"
}

log_error() {
    echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ERROR: $*" >&2
}

# ── Deterministic output path generation ─────────────────────────────────────

# Generates a deterministic output path from tier and scenario names.
# Usage: result_path <tier> <scenario>
# Example: result_path "tier1" "dpdk-echo" => /tmp/test-results/tier1-dpdk-echo.xml
result_path() {
    local tier="$1"
    local scenario="$2"
    echo "/tmp/test-results/${tier}-${scenario}.xml"
}

# ── Test execution helpers ───────────────────────────────────────────────────

# Runs a command with a timeout. Captures exit code.
# Usage: run_with_timeout <timeout_seconds> <command...>
# Returns: the command's exit code, or 124 if timed out
run_with_timeout() {
    local timeout_secs="$1"
    shift
    local exit_code=0
    timeout --signal=KILL "$timeout_secs" "$@" || exit_code=$?
    return $exit_code
}

# ── JUnit XML generation ────────────────────────────────────────────────────
#
# Usage pattern:
#   junit_start_suite "tier1-dpdk-echo" 4
#   junit_add_pass "arp_resolution" "tier1.dpdk_echo" "2.100"
#   junit_add_failure "payload_integrity" "tier1.dpdk_echo" "1.500" "Payload mismatch" "Expected: abc\nActual: def"
#   junit_end_suite
#   junit_write "/tmp/test-results/tier1-dpdk-echo.xml"

# Internal state for XML assembly
_JUNIT_XML=""
_JUNIT_SUITE_NAME=""
_JUNIT_TEST_COUNT=0
_JUNIT_FAILURE_COUNT=0
_JUNIT_TOTAL_TIME="0"
_JUNIT_TESTCASES=""

# Escape special XML characters in a string
_xml_escape() {
    local s="$1"
    s="${s//&/&amp;}"
    s="${s//</&lt;}"
    s="${s//>/&gt;}"
    s="${s//\"/&quot;}"
    s="${s//\'/&apos;}"
    echo "$s"
}

# Start a new test suite.
# Usage: junit_start_suite <suite_name> <expected_test_count>
junit_start_suite() {
    _JUNIT_SUITE_NAME="$1"
    _JUNIT_TEST_COUNT=0
    _JUNIT_FAILURE_COUNT=0
    _JUNIT_TOTAL_TIME="0"
    _JUNIT_TESTCASES=""
}

# Add a passing test case.
# Usage: junit_add_pass <test_name> <classname> <time_seconds>
junit_add_pass() {
    local name="$1"
    local classname="$2"
    local time_secs="$3"

    _JUNIT_TEST_COUNT=$(( _JUNIT_TEST_COUNT + 1 ))
    _JUNIT_TOTAL_TIME=$(awk "BEGIN {printf \"%.3f\", $_JUNIT_TOTAL_TIME + $time_secs}")

    local escaped_name
    escaped_name=$(_xml_escape "$name")
    local escaped_classname
    escaped_classname=$(_xml_escape "$classname")

    _JUNIT_TESTCASES="${_JUNIT_TESTCASES}    <testcase name=\"${escaped_name}\" classname=\"${escaped_classname}\" time=\"${time_secs}\">
    </testcase>
"
}

# Add a failing test case.
# Usage: junit_add_failure <test_name> <classname> <time_seconds> <message> <details>
junit_add_failure() {
    local name="$1"
    local classname="$2"
    local time_secs="$3"
    local message="$4"
    local details="${5:-}"

    _JUNIT_TEST_COUNT=$(( _JUNIT_TEST_COUNT + 1 ))
    _JUNIT_FAILURE_COUNT=$(( _JUNIT_FAILURE_COUNT + 1 ))
    _JUNIT_TOTAL_TIME=$(awk "BEGIN {printf \"%.3f\", $_JUNIT_TOTAL_TIME + $time_secs}")

    local escaped_name
    escaped_name=$(_xml_escape "$name")
    local escaped_classname
    escaped_classname=$(_xml_escape "$classname")
    local escaped_message
    escaped_message=$(_xml_escape "$message")
    local escaped_details
    escaped_details=$(_xml_escape "$details")

    _JUNIT_TESTCASES="${_JUNIT_TESTCASES}    <testcase name=\"${escaped_name}\" classname=\"${escaped_classname}\" time=\"${time_secs}\">
        <failure message=\"${escaped_message}\" type=\"AssertionError\">${escaped_details}</failure>
    </testcase>
"
}

# End the current test suite (finalize internal state).
junit_end_suite() {
    local escaped_suite_name
    escaped_suite_name=$(_xml_escape "$_JUNIT_SUITE_NAME")

    _JUNIT_XML="<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<testsuite name=\"${escaped_suite_name}\" tests=\"${_JUNIT_TEST_COUNT}\" failures=\"${_JUNIT_FAILURE_COUNT}\" errors=\"0\" time=\"${_JUNIT_TOTAL_TIME}\">
${_JUNIT_TESTCASES}</testsuite>
"
}

# Write the assembled JUnit XML to a file.
# Usage: junit_write <output_path>
junit_write() {
    local output_path="$1"
    local output_dir
    output_dir=$(dirname "$output_path")
    mkdir -p "$output_dir"
    echo "$_JUNIT_XML" > "$output_path"
    log_info "JUnit XML written to: $output_path"
}

# ── Timer helpers ────────────────────────────────────────────────────────────

# Get current time in seconds (with fractional precision)
_timer_now() {
    date +%s.%N 2>/dev/null || date +%s
}

# Compute elapsed time between two timestamps
# Usage: _timer_elapsed <start> <end>
_timer_elapsed() {
    awk "BEGIN {printf \"%.3f\", $2 - $1}"
}
