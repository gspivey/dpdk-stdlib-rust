#!/usr/bin/env bash
# tier1-tcp-handshake.sh - Tier 1: TCP three-way handshake over DPDK
#
# Tests TCP connection establishment (SYN → SYN-ACK → ACK) between two
# dpdk-stdlib-tcp instances on EC2. Verifies the handshake completes and
# the connection can be cleanly closed.
#
# Usage:
#   # On Instance B (server):
#   ./tier1-tcp-handshake.sh --role server --bind-ip 10.0.1.100 --port 9000 \
#       --gateway-mac AA:BB:CC:DD:EE:FF
#
#   # On Instance A (client):
#   ./tier1-tcp-handshake.sh --role client --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier1-tcp-handshake.xml \
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
CLASSNAME="tier1.tcp_handshake"

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
    OUTPUT=$(result_path "tier1" "tcp-handshake")
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
    log_info "Starting TCP echo server (handshake test) on ${BIND_IP}:${PORT}"
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
    local max_wait=120
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
    log_info "Starting TCP handshake tests against ${PEER_IP}:${PORT}"
    discover_gateway_mac

    mkdir -p "$(dirname "$OUTPUT")"
    junit_start_suite "tier1-tcp-handshake" 3

    # Give the server time to start
    sleep 5

    # ── Test 1: Single TCP handshake ─────────────────────────────────────
    log_info "Test 1: single_handshake"
    local test_start test_end elapsed
    test_start=$(_timer_now)
    local hs_output=""
    local hs_ok=true

    hs_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode handshake --count 1 2>&1) || {
        hs_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$hs_ok" == "true" ]] && echo "$hs_output" | grep -q "completed\|connections"; then
        log_info "PASS: Single TCP handshake succeeded"
        junit_add_pass "single_handshake" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Single TCP handshake failed"
        log_error "Output: $hs_output"
        junit_add_failure "single_handshake" "$CLASSNAME" "$elapsed" \
            "TCP handshake failed" "$hs_output"
    fi

    # ── Test 2: Repeated handshakes (10 connections) ─────────────────────
    log_info "Test 2: repeated_handshake"
    test_start=$(_timer_now)
    local rep_output=""
    local rep_ok=true

    rep_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode handshake --count 10 2>&1) || {
        rep_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$rep_ok" == "true" ]] && echo "$rep_output" | grep -q "10 connections\|completed 10"; then
        log_info "PASS: Repeated TCP handshakes (10) succeeded"
        junit_add_pass "repeated_handshake" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Repeated TCP handshakes failed"
        log_error "Output: $rep_output"
        junit_add_failure "repeated_handshake" "$CLASSNAME" "$elapsed" \
            "Repeated handshakes failed" "$rep_output"
    fi

    # ── Test 3: Handshake rate measurement ───────────────────────────────
    log_info "Test 3: handshake_rate"
    test_start=$(_timer_now)
    local rate_output=""
    local rate_ok=true

    rate_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode handshake --count 50 2>&1) || {
        rate_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$rate_ok" == "true" ]] && echo "$rate_output" | grep -q "conn/s\|connections"; then
        local rate
        rate=$(echo "$rate_output" | grep -o '[0-9.]*  *conn/s' | head -1 || echo "unknown")
        log_info "PASS: Handshake rate test succeeded: $rate"
        junit_add_pass "handshake_rate" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Handshake rate test failed"
        log_error "Output: $rate_output"
        junit_add_failure "handshake_rate" "$CLASSNAME" "$elapsed" \
            "Handshake rate test failed" "$rate_output"
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
