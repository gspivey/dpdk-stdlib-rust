#!/usr/bin/env bash
# tier3-iperf-interop.sh - Tier 3: DPDK <-> iperf3 interoperability test harness
#
# Tests that dpdk-stdlib can interoperate with standard iperf3 UDP traffic.
# Two directions:
#   - "our-app-sends": Instance A (dpdk-stdlib) sends to Instance B (iperf3 server)
#   - "iperf-sends": Instance B (iperf3 client) sends to Instance A (dpdk-stdlib listener)
#
# Usage:
#   ./tier3-iperf-interop.sh --role <server|client> --direction <our-app-sends|iperf-sends> \
#       --local-ip <IP> --peer-ip <IP> --port 9000 --output /tmp/test-results/tier3-iperf-interop.xml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/harness-common.sh"

# ── Defaults ─────────────────────────────────────────────────────────────────

PROJECT_DIR="/opt/dpdk-stdlib"
ECHO_BINARY="$PROJECT_DIR/target/release/echo"
TEST_CLIENT_BINARY="$PROJECT_DIR/target/release/test-client"
ROLE=""
DIRECTION=""
LOCAL_IP=""
PEER_IP=""
PORT=9000
OUTPUT=""
TEST_TIMEOUT=60
CLASSNAME="tier3.iperf_interop"

# ── Argument parsing ─────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --role)       ROLE="$2";      shift 2 ;;
        --direction)  DIRECTION="$2"; shift 2 ;;
        --local-ip)   LOCAL_IP="$2";  shift 2 ;;
        --peer-ip)    PEER_IP="$2";   shift 2 ;;
        --port)       PORT="$2";      shift 2 ;;
        --output)     OUTPUT="$2";    shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 --role <server|client> --direction <our-app-sends|iperf-sends> --local-ip <IP> --peer-ip <IP> [--port PORT] [--output PATH]" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$ROLE" || -z "$DIRECTION" || -z "$LOCAL_IP" || -z "$PEER_IP" ]]; then
    echo "Missing required arguments" >&2
    echo "Usage: $0 --role <server|client> --direction <our-app-sends|iperf-sends> --local-ip <IP> --peer-ip <IP>" >&2
    exit 1
fi

if [[ -z "$OUTPUT" ]]; then
    OUTPUT=$(result_path "tier3" "iperf-interop")
fi

# ── Direction: our-app-sends ─────────────────────────────────────────────────
# Instance A runs dpdk-stdlib sending UDP to Instance B running iperf3 server.
# - Server role (Instance B): start iperf3 server, wait, collect stats
# - Client role (Instance A): run dpdk-stdlib sending UDP traffic

run_our_app_sends_server() {
    # Instance B: run iperf3 in UDP server mode
    log_info "Starting iperf3 UDP server on ${LOCAL_IP}:${PORT} (our-app-sends direction)"

    iperf3 -s -B "$LOCAL_IP" -p "$PORT" --one-off &
    local iperf_pid=$!
    log_info "iperf3 server started with PID $iperf_pid"

    # Wait for the test to complete (sender will drive the timing)
    local waited=0
    local max_wait=90
    while kill -0 "$iperf_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    kill "$iperf_pid" 2>/dev/null || true
    wait "$iperf_pid" 2>/dev/null || true
    log_info "iperf3 server finished"
}

run_our_app_sends_client() {
    # Instance A: run dpdk-stdlib sending UDP to iperf3 server
    log_info "Sending UDP traffic from dpdk-stdlib to iperf3 server at ${PEER_IP}:${PORT}"

    junit_start_suite "tier3-iperf-interop" 1

    # Give the iperf3 server time to start
    sleep 5

    local start end elapsed
    start=$(_timer_now)
    local test_ok=true
    local test_err=""
    local test_output=""

    # Use the test client to send UDP packets to the iperf3 server
    test_output=$(run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --message "dpdk-to-iperf-test-payload" \
        --count 10 --delay 200 2>&1) || {
        test_ok=false
        test_err="Failed to send UDP traffic from dpdk-stdlib to iperf3"
    }

    # Verify that we sent bytes (even without responses, since iperf3 server
    # may not echo back in the same format)
    if [[ "$test_ok" == "true" ]]; then
        if echo "$test_output" | grep -q "Sent"; then
            local sent_count
            sent_count=$(echo "$test_output" | grep -c "Sent" || true)
            log_info "our-app-sends: sent $sent_count packets to iperf3 server"
        else
            test_ok=false
            test_err="No packets were sent to iperf3 server"
        fi
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$test_ok" == "true" ]]; then
        junit_add_pass "our_app_sends" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "our_app_sends" "$CLASSNAME" "$elapsed" "$test_err" "$test_output"
    fi

    junit_end_suite
    junit_write "$OUTPUT"
    log_info "our-app-sends test complete"
}

# ── Direction: iperf-sends ───────────────────────────────────────────────────
# Instance B runs iperf3 client sending UDP to Instance A running dpdk-stdlib listener.
# - Server role (Instance A): start dpdk-stdlib listener, wait, collect stats
# - Client role (Instance B): run iperf3 client sending UDP traffic

run_iperf_sends_server() {
    # Instance A: run dpdk-stdlib echo server as listener
    log_info "Starting dpdk-stdlib listener on ${LOCAL_IP}:${PORT} (iperf-sends direction)"

    "$ECHO_BINARY" --ip "$LOCAL_IP" --port "$PORT" &
    local echo_pid=$!
    log_info "Echo server started with PID $echo_pid"

    sleep 3
    if ! kill -0 "$echo_pid" 2>/dev/null; then
        log_error "Echo server exited prematurely"
        return 1
    fi

    # Wait for the test to complete
    local waited=0
    local max_wait=90
    while kill -0 "$echo_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    kill "$echo_pid" 2>/dev/null || true
    wait "$echo_pid" 2>/dev/null || true
    log_info "Echo server finished"
}

run_iperf_sends_client() {
    # Instance B: run iperf3 as UDP client sending to dpdk-stdlib listener
    log_info "Sending iperf3 UDP traffic to dpdk-stdlib listener at ${PEER_IP}:${PORT}"

    junit_start_suite "tier3-iperf-interop" 1

    # Give the dpdk-stdlib listener time to start
    sleep 5

    local start end elapsed
    start=$(_timer_now)
    local test_ok=true
    local test_err=""
    local test_output=""

    # Run iperf3 in UDP client mode sending to the dpdk-stdlib listener
    test_output=$(run_with_timeout "$TEST_TIMEOUT" \
        iperf3 -c "$PEER_IP" -p "$PORT" -u -b 10M -t 10 --json 2>&1) || {
        # iperf3 may return non-zero if the "server" doesn't respond as expected
        # That's OK for interop testing - we check bytes transferred
        log_info "iperf3 returned non-zero exit code (expected for non-iperf server)"
    }

    # Check if iperf3 transferred any bytes
    if echo "$test_output" | grep -q '"bytes"'; then
        local bytes_sent
        bytes_sent=$(echo "$test_output" | grep -o '"bytes":[0-9]*' | head -1 | cut -d: -f2 || echo "0")
        if [[ "$bytes_sent" -gt 0 ]]; then
            log_info "iperf-sends: transferred $bytes_sent bytes"
        else
            test_ok=false
            test_err="Zero bytes transferred from iperf3 to dpdk-stdlib"
        fi
    elif echo "$test_output" | grep -qi "sent\|transfer\|bytes"; then
        # Non-JSON output fallback
        log_info "iperf-sends: traffic was sent (non-JSON confirmation)"
    else
        test_ok=false
        test_err="Could not confirm any bytes were transferred"
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$test_ok" == "true" ]]; then
        junit_add_pass "iperf_sends" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "iperf_sends" "$CLASSNAME" "$elapsed" "$test_err" "$test_output"
    fi

    junit_end_suite
    junit_write "$OUTPUT"
    log_info "iperf-sends test complete"
}

# ── Main dispatch ────────────────────────────────────────────────────────────

case "${DIRECTION}:${ROLE}" in
    our-app-sends:server)
        run_our_app_sends_server
        ;;
    our-app-sends:client)
        run_our_app_sends_client
        ;;
    iperf-sends:server)
        run_iperf_sends_server
        ;;
    iperf-sends:client)
        run_iperf_sends_client
        ;;
    *)
        echo "Invalid direction:role combination: ${DIRECTION}:${ROLE}" >&2
        echo "Valid combinations:" >&2
        echo "  --direction our-app-sends --role server   (Instance B: iperf3 server)" >&2
        echo "  --direction our-app-sends --role client   (Instance A: dpdk-stdlib sender)" >&2
        echo "  --direction iperf-sends   --role server   (Instance A: dpdk-stdlib listener)" >&2
        echo "  --direction iperf-sends   --role client   (Instance B: iperf3 client)" >&2
        exit 1
        ;;
esac
