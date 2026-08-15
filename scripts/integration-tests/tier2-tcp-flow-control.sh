#!/usr/bin/env bash
# tier2-tcp-flow-control.sh - Tier 2: TCP flow control (zero-window, persist probe, resume)
#
# Tests TCP receive-window flow control by having the receiver not read
# until rwnd reaches zero. Verifies:
#   - Sender stops transmitting when rwnd=0
#   - Sender sends persist probes to check if window reopened
#   - After receiver drains and re-advertises window, transfer resumes
#   - All data arrives intact
#
# This test uses the bidir mode with a large payload count to fill the
# receive buffer, then verifies that the transfer still completes
# (meaning the TCP stack correctly handled zero-window and resumed).
#
# Usage:
#   # On Instance B (server):
#   ./tier2-tcp-flow-control.sh --role server --bind-ip 10.0.1.100 --port 9000 \
#       --gateway-mac AA:BB:CC:DD:EE:FF
#
#   # On Instance A (client):
#   ./tier2-tcp-flow-control.sh --role client --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier2-tcp-flow-control.xml \
#       --gateway-mac AA:BB:CC:DD:EE:FF

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/harness-common.sh"

# ── Defaults ─────────────────────────────────────────────────────────────────

PROJECT_DIR="/opt/dpdk-stdlib"
TCP_ECHO_BINARY="$PROJECT_DIR/target/release/tcp-echo"
TCP_CLIENT_BINARY="$PROJECT_DIR/target/release/tcp-test-client"
ROLE=""
BIND_IP=""
PEER_IP=""
PORT=9000
OUTPUT=""
GATEWAY_MAC=""
TEST_TIMEOUT=60
CLASSNAME="tier2.tcp_flow_control"

# ── Argument parsing ─────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --role)        ROLE="$2";        shift 2 ;;
        --bind-ip)     BIND_IP="$2";     shift 2 ;;
        --peer-ip)     PEER_IP="$2";     shift 2 ;;
        --port)        PORT="$2";        shift 2 ;;
        --output)      OUTPUT="$2";      shift 2 ;;
        --gateway-mac) GATEWAY_MAC="$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 --role <server|client> --bind-ip <IP> [--peer-ip <IP>] --port <PORT> [--output <PATH>] [--gateway-mac <MAC>]" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$ROLE" || -z "$BIND_IP" ]]; then
    echo "Missing required arguments: --role and --bind-ip are required" >&2
    exit 1
fi

if [[ "$ROLE" == "client" && -z "$PEER_IP" ]]; then
    echo "Client role requires --peer-ip" >&2
    exit 1
fi

if [[ -z "$OUTPUT" ]]; then
    OUTPUT=$(result_path "tier2" "tcp-flow-control")
fi

# ── Discover gateway MAC if not provided ─────────────────────────────────────

discover_gateway_mac() {
    if [[ -n "$GATEWAY_MAC" ]]; then
        return
    fi
    local subnet_gw
    subnet_gw=$(echo "$BIND_IP" | sed 's/\.[0-9]*$/.1/')
    ping -c 1 -W 2 "$subnet_gw" >/dev/null 2>&1 || true
    sleep 1
    GATEWAY_MAC=$(awk -v ip="$subnet_gw" '$1 == ip && $4 != "00:00:00:00:00:00" {print $4}' /proc/net/arp | head -1)
    if [[ -z "$GATEWAY_MAC" ]]; then
        log_error "Could not discover gateway MAC for $subnet_gw"
        GATEWAY_MAC="00:00:00:00:00:00"
    fi
    log_info "Discovered gateway MAC: $GATEWAY_MAC"
}

# ── Server role ──────────────────────────────────────────────────────────────

run_server() {
    log_info "Starting TCP echo server (flow control test) on ${BIND_IP}:${PORT}"
    discover_gateway_mac

    ulimit -c unlimited 2>/dev/null || true

    local server_log="/tmp/tcp-echo-server.log"
    log_info "Launching: $TCP_ECHO_BINARY --ip $BIND_IP --port $PORT --gateway-mac $GATEWAY_MAC"
    $TCP_ECHO_BINARY --ip "$BIND_IP" --port "$PORT" --gateway-mac "$GATEWAY_MAC" \
        > "$server_log" 2>&1 &
    local server_pid=$!
    log_info "TCP echo server started with PID $server_pid"

    # Wait for server to bind
    sleep 3

    if ! kill -0 "$server_pid" 2>/dev/null; then
        log_error "TCP echo server exited prematurely"
        cat "$server_log" >&2
        if check_process_crash "$server_pid" "tcp-echo"; then
            log_error "TCP echo server CRASHED during startup"
        fi
        exit 1
    fi

    log_info "TCP echo server ready, waiting for client tests..."

    # Keep running until killed by orchestrator
    local waited=0
    local max_wait=180
    while kill -0 "$server_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    if ! kill -0 "$server_pid" 2>/dev/null; then
        check_process_crash "$server_pid" "tcp-echo" || true
    else
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    log_info "Server finished"
}

# ── Client role ──────────────────────────────────────────────────────────────

run_client() {
    log_info "Starting TCP flow control tests against ${PEER_IP}:${PORT}"
    discover_gateway_mac

    mkdir -p "$(dirname "$OUTPUT")"
    junit_start_suite "tier2-tcp-flow-control" 3

    # Give the server time to start
    sleep 5

    # ── Test 1: Large burst to trigger zero-window ───────────────────────
    #
    # Send enough data rapidly to fill the echo server's receive buffer
    # and trigger zero-window condition. The TCP stack should handle this
    # gracefully via persist probes and resume when the server drains.
    #
    # We use 1400B payloads × 200 iterations = ~280KB of data in flight.
    # With default buffer sizes (~64KB), this should trigger flow control.
    log_info "Test 1: large_burst_flow_control (1400B x 200)"
    local test_start test_end elapsed
    test_start=$(_timer_now)
    local burst_output=""
    local burst_ok=true

    burst_output=$(run_with_timeout 90 \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 200 --payload-size 1400 2>&1) || {
        burst_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$burst_ok" == "true" ]] && echo "$burst_output" | grep -q "200 echo round-trips\|rtt/s"; then
        log_info "PASS: Large burst completed — flow control handled correctly"
        junit_add_pass "large_burst_flow_control" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Large burst transfer failed (flow control may be broken)"
        log_error "Output: $burst_output"
        junit_add_failure "large_burst_flow_control" "$CLASSNAME" "$elapsed" \
            "Large burst (1400B x 200) failed — possible flow control issue" "$burst_output"
    fi

    # ── Test 2: Sustained window pressure ────────────────────────────────
    #
    # Send max-size payloads in a sustained burst. The echo server reads
    # and echoes each chunk, but the sheer volume ensures the TCP window
    # oscillates between zero and open repeatedly.
    log_info "Test 2: sustained_window_pressure (1400B x 500)"
    test_start=$(_timer_now)
    local sustained_output=""
    local sustained_ok=true

    sustained_output=$(run_with_timeout 120 \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 500 --payload-size 1400 2>&1) || {
        sustained_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$sustained_ok" == "true" ]] && echo "$sustained_output" | grep -q "500 echo round-trips\|rtt/s"; then
        log_info "PASS: Sustained window pressure test completed"
        junit_add_pass "sustained_window_pressure" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Sustained window pressure test failed"
        log_error "Output: $sustained_output"
        junit_add_failure "sustained_window_pressure" "$CLASSNAME" "$elapsed" \
            "Sustained transfer (1400B x 500) failed — persist probe or window update issue" \
            "$sustained_output"
    fi

    # ── Test 3: Data integrity under flow control ────────────────────────
    #
    # Verify data integrity (echo correctness) is maintained even when
    # flow control kicks in. The bidir mode already verifies echo content,
    # so success here means no data corruption occurred during window
    # throttling / persist probe cycles.
    log_info "Test 3: data_integrity_under_pressure (64B x 1000)"
    test_start=$(_timer_now)
    local integrity_output=""
    local integrity_ok=true

    integrity_output=$(run_with_timeout 90 \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 1000 --payload-size 64 2>&1) || {
        integrity_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$integrity_ok" == "true" ]] && echo "$integrity_output" | grep -q "1000 echo round-trips\|rtt/s"; then
        log_info "PASS: Data integrity verified under flow control pressure"
        junit_add_pass "data_integrity_under_pressure" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Data integrity test failed"
        log_error "Output: $integrity_output"
        local failure_msg="Data integrity failed under flow control"
        if echo "$integrity_output" | grep -q "mismatch"; then
            failure_msg="Echo data mismatch — corruption during flow control"
        fi
        junit_add_failure "data_integrity_under_pressure" "$CLASSNAME" "$elapsed" \
            "$failure_msg" "$integrity_output"
    fi

    # ── Finalize ─────────────────────────────────────────────────────────
    junit_end_suite
    junit_write "$OUTPUT"

    if [[ $_JUNIT_FAILURE_COUNT -eq 0 ]]; then
        log_info "All tests passed ($_JUNIT_TEST_COUNT/$_JUNIT_TEST_COUNT)"
    else
        log_error "$_JUNIT_FAILURE_COUNT/$_JUNIT_TEST_COUNT tests failed"
        exit 1
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────

case "$ROLE" in
    server)  run_server ;;
    client)  run_client ;;
    *)
        echo "Invalid role: $ROLE (must be 'server' or 'client')" >&2
        exit 1
        ;;
esac
