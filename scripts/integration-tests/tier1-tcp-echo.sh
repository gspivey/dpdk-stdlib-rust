#!/usr/bin/env bash
# tier1-tcp-echo.sh - Tier 1: TCP bidirectional data transfer over DPDK
#
# Tests bidirectional TCP data transfer between two dpdk-stdlib-tcp instances
# on EC2. Verifies: data integrity across multiple payload sizes, sustained
# multi-iteration echo, and large-payload transfer.
#
# Usage:
#   # On Instance B (server):
#   ./tier1-tcp-echo.sh --role server --bind-ip 10.0.1.100 --port 9000 \
#       --gateway-mac AA:BB:CC:DD:EE:FF
#
#   # On Instance A (client):
#   ./tier1-tcp-echo.sh --role client --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier1-tcp-echo.xml \
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
CLASSNAME="tier1.tcp_echo"

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
    OUTPUT=$(result_path "tier1" "tcp-echo")
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
    log_info "Starting TCP echo server (bidir test) on ${BIND_IP}:${PORT}"
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
    log_info "Starting TCP echo (bidir) tests against ${PEER_IP}:${PORT}"
    discover_gateway_mac

    mkdir -p "$(dirname "$OUTPUT")"
    junit_start_suite "tier1-tcp-echo" 4

    # Give the server time to start
    sleep 5

    # ── Test 1: Small payload echo (64B) ─────────────────────────────────
    log_info "Test 1: small_payload_echo (64B x 10)"
    local test_start test_end elapsed
    test_start=$(_timer_now)
    local small_output=""
    local small_ok=true

    small_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 10 --payload-size 64 2>&1) || {
        small_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$small_ok" == "true" ]] && echo "$small_output" | grep -q "round-trips\|rtt/s"; then
        log_info "PASS: Small payload echo succeeded"
        junit_add_pass "small_payload_echo" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Small payload echo failed"
        log_error "Output: $small_output"
        junit_add_failure "small_payload_echo" "$CLASSNAME" "$elapsed" \
            "64B echo failed" "$small_output"
    fi

    # ── Test 2: Medium payload echo (512B) ───────────────────────────────
    log_info "Test 2: medium_payload_echo (512B x 10)"
    test_start=$(_timer_now)
    local med_output=""
    local med_ok=true

    med_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 10 --payload-size 512 2>&1) || {
        med_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$med_ok" == "true" ]] && echo "$med_output" | grep -q "round-trips\|rtt/s"; then
        log_info "PASS: Medium payload echo succeeded"
        junit_add_pass "medium_payload_echo" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Medium payload echo failed"
        log_error "Output: $med_output"
        junit_add_failure "medium_payload_echo" "$CLASSNAME" "$elapsed" \
            "512B echo failed" "$med_output"
    fi

    # ── Test 3: Large payload echo (1400B) ───────────────────────────────
    log_info "Test 3: large_payload_echo (1400B x 10)"
    test_start=$(_timer_now)
    local large_output=""
    local large_ok=true

    large_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 10 --payload-size 1400 2>&1) || {
        large_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$large_ok" == "true" ]] && echo "$large_output" | grep -q "round-trips\|rtt/s"; then
        log_info "PASS: Large payload echo succeeded"
        junit_add_pass "large_payload_echo" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Large payload echo failed"
        log_error "Output: $large_output"
        junit_add_failure "large_payload_echo" "$CLASSNAME" "$elapsed" \
            "1400B echo failed" "$large_output"
    fi

    # ── Test 4: Sustained multi-iteration (64B x 100) ────────────────────
    log_info "Test 4: sustained_echo (64B x 100)"
    test_start=$(_timer_now)
    local sustained_output=""
    local sustained_ok=true

    sustained_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 100 --payload-size 64 2>&1) || {
        sustained_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$sustained_ok" == "true" ]] && echo "$sustained_output" | grep -q "100 echo round-trips\|rtt/s"; then
        log_info "PASS: Sustained echo (100 iterations) succeeded"
        junit_add_pass "sustained_echo" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Sustained echo test failed"
        log_error "Output: $sustained_output"
        junit_add_failure "sustained_echo" "$CLASSNAME" "$elapsed" \
            "Sustained echo (100 iters) failed" "$sustained_output"
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
