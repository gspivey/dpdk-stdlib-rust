#!/usr/bin/env bash
# tier1-tcp-shutdown.sh - Tier 1: TCP graceful FIN teardown over DPDK
#
# Tests graceful TCP connection teardown (FIN handshake) between two
# dpdk-stdlib-tcp instances on EC2. Verifies: write → shutdown(Write) → read
# echoed data → receive clean EOF without data loss.
#
# Usage:
#   # On Instance B (server):
#   ./tier1-tcp-shutdown.sh --role server --bind-ip 10.0.1.100 --port 9000 \
#       --gateway-mac AA:BB:CC:DD:EE:FF
#
#   # On Instance A (client):
#   ./tier1-tcp-shutdown.sh --role client --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier1-tcp-shutdown.xml \
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
CLASSNAME="tier1.tcp_shutdown"

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
    OUTPUT=$(result_path "tier1" "tcp-shutdown")
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
    log_info "Starting TCP echo server (shutdown test) on ${BIND_IP}:${PORT}"
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
    log_info "Starting TCP shutdown tests against ${PEER_IP}:${PORT}"
    discover_gateway_mac

    mkdir -p "$(dirname "$OUTPUT")"
    junit_start_suite "tier1-tcp-shutdown" 3

    # Give the server time to start
    sleep 5

    # ── Test 1: Graceful FIN teardown ────────────────────────────────────
    log_info "Test 1: graceful_fin_teardown"
    local test_start test_end elapsed
    test_start=$(_timer_now)
    local fin_output=""
    local fin_ok=true

    fin_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode shutdown 2>&1) || {
        fin_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$fin_ok" == "true" ]] && echo "$fin_output" | grep -q "graceful shutdown OK"; then
        log_info "PASS: Graceful FIN teardown succeeded"
        junit_add_pass "graceful_fin_teardown" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Graceful FIN teardown failed"
        log_error "Output: $fin_output"
        local failure_msg="Graceful FIN teardown failed"
        if echo "$fin_output" | grep -q "timed out"; then
            failure_msg="Timed out waiting for server FIN"
        fi
        junit_add_failure "graceful_fin_teardown" "$CLASSNAME" "$elapsed" \
            "$failure_msg" "$fin_output"
    fi

    # ── Test 2: Data integrity before FIN ────────────────────────────────
    #
    # Verify that data sent before shutdown(Write) is echoed back completely
    # before the server closes its side. This uses bidir mode with a single
    # iteration — the client writes, reads back, then shuts down.
    log_info "Test 2: data_before_fin"
    test_start=$(_timer_now)
    local data_output=""
    local data_ok=true

    data_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 1 --payload-size 1024 2>&1) || {
        data_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$data_ok" == "true" ]] && echo "$data_output" | grep -q "round-trips\|rtt/s"; then
        log_info "PASS: Data integrity before FIN verified"
        junit_add_pass "data_before_fin" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Data integrity before FIN failed"
        log_error "Output: $data_output"
        junit_add_failure "data_before_fin" "$CLASSNAME" "$elapsed" \
            "Data sent before FIN was not echoed correctly" "$data_output"
    fi

    # ── Test 3: Repeated shutdown cycles ─────────────────────────────────
    #
    # Verify multiple connect→write→shutdown→EOF cycles complete cleanly
    # (no resource leaks that break subsequent connections).
    log_info "Test 3: repeated_shutdown_cycles"
    test_start=$(_timer_now)
    local cycles_ok=true
    local cycles_err=""
    local cycle_count=5

    for i in $(seq 1 $cycle_count); do
        local cycle_output=""
        cycle_output=$(run_with_timeout "$TEST_TIMEOUT" \
            "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
            --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
            --mode shutdown 2>&1) || {
            cycles_ok=false
            cycles_err="Cycle $i/$cycle_count failed"
            break
        }
        if ! echo "$cycle_output" | grep -q "graceful shutdown OK"; then
            cycles_ok=false
            cycles_err="Cycle $i/$cycle_count: unexpected output: $cycle_output"
            break
        fi
    done
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$cycles_ok" == "true" ]]; then
        log_info "PASS: All $cycle_count shutdown cycles completed cleanly"
        junit_add_pass "repeated_shutdown_cycles" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Repeated shutdown cycles failed"
        log_error "Error: $cycles_err"
        junit_add_failure "repeated_shutdown_cycles" "$CLASSNAME" "$elapsed" \
            "$cycles_err" "Expected $cycle_count clean shutdown cycles"
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
