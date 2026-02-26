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

# ── Crash diagnostics ────────────────────────────────────────────────────────
#
# These helpers detect and report crashes (segfaults, aborts) for binaries
# started as background processes. They capture signal info, coredumps,
# and kernel dmesg output to make CI failures debuggable without SSH access.

# Map signal numbers to names for readable diagnostics
_signal_name() {
    case "$1" in
        1) echo "SIGHUP";;    2) echo "SIGINT";;    3) echo "SIGQUIT";;
        4) echo "SIGILL";;    6) echo "SIGABRT";;   7) echo "SIGBUS";;
        8) echo "SIGFPE";;    9) echo "SIGKILL";;   11) echo "SIGSEGV";;
        13) echo "SIGPIPE";;  14) echo "SIGALRM";;  15) echo "SIGTERM";;
        *) echo "signal $1";;
    esac
}

# Check if a background process crashed (exited due to signal).
# Usage: check_process_crash <pid> <binary_name>
# Returns 0 if the process crashed, 1 if it exited normally or is still running.
# Prints detailed crash diagnostics to stderr on crash.
check_process_crash() {
    local pid="$1"
    local binary_name="$2"

    # Still running — not a crash
    if kill -0 "$pid" 2>/dev/null; then
        return 1
    fi

    local exit_code=0
    wait "$pid" 2>/dev/null || exit_code=$?

    if [[ $exit_code -le 128 ]]; then
        # Normal exit (or error exit) — not a signal-based crash
        if [[ $exit_code -ne 0 ]]; then
            log_error "${binary_name} exited with code ${exit_code}"
        fi
        return 1
    fi

    # Exit code > 128 means killed by signal (exit_code = 128 + signal_number)
    local signal_num=$(( exit_code - 128 ))
    local signal_name
    signal_name=$(_signal_name "$signal_num")

    log_error "=========================================="
    log_error "CRASH DETECTED: ${binary_name} killed by ${signal_name} (signal ${signal_num})"
    log_error "  PID: ${pid}"
    log_error "  Exit code: ${exit_code}"
    log_error "=========================================="

    # Dump kernel messages related to the crash (segfault logs show the faulting address)
    log_error "--- dmesg (last 20 lines, filtered for crash/segfault) ---"
    dmesg | grep -iE "segfault|trap|fault|oom|killed|${binary_name}" | tail -20 >&2 || true

    log_error "--- dmesg (last 10 lines, unfiltered) ---"
    dmesg | tail -10 >&2 || true

    # Check for coredumps
    local coredump_dir="/tmp/coredumps"
    if ls "${coredump_dir}/core.${binary_name}."* 2>/dev/null; then
        log_error "--- Coredump(s) found ---"
        ls -lh "${coredump_dir}/core.${binary_name}."* >&2 || true
        # If gdb is available, extract a backtrace from the most recent coredump
        local latest_core
        latest_core=$(ls -t "${coredump_dir}/core.${binary_name}."* 2>/dev/null | head -1)
        if [[ -n "$latest_core" ]] && command -v gdb >/dev/null 2>&1; then
            local binary_path
            binary_path=$(command -v "$binary_name" 2>/dev/null || echo "/opt/dpdk-stdlib/target/release/${binary_name}")
            if [[ -f "$binary_path" ]]; then
                log_error "--- GDB backtrace from coredump ---"
                gdb -batch -ex "thread apply all bt full" "$binary_path" "$latest_core" 2>&1 | head -100 >&2 || true
            fi
        fi
    else
        log_error "No coredumps found in ${coredump_dir}/"
        log_error "  (check that ulimit -c unlimited and core_pattern are set)"
    fi

    # Write a crash summary file for the log collector to pick up
    local crash_summary="/tmp/crash-report-${binary_name}-${pid}.txt"
    {
        echo "CRASH REPORT"
        echo "============"
        echo "binary: ${binary_name}"
        echo "pid: ${pid}"
        echo "signal: ${signal_name} (${signal_num})"
        echo "exit_code: ${exit_code}"
        echo "timestamp: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        echo ""
        echo "=== dmesg (crash-related) ==="
        dmesg | grep -iE "segfault|trap|fault|oom|killed|${binary_name}" | tail -20 2>/dev/null || echo "(none)"
        echo ""
        echo "=== coredumps ==="
        ls -lh "${coredump_dir}/core.${binary_name}."* 2>/dev/null || echo "(none found)"
    } > "$crash_summary" 2>/dev/null || true

    log_error "Crash report written to: ${crash_summary}"
    return 0
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
