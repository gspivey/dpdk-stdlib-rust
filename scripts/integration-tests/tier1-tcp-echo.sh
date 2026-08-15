#!/usr/bin/env bash
# tier1-tcp-echo.sh - Tier 1: TCP DPDK <-> DPDK echo smoke test
#
# Tests TCP connect -> echo -> close between two dpdk-stdlib-tcp instances, both
# using the DPDK kernel-bypass stack. Verifies the handshake, data-phase echo
# integrity, and multi-round-trip stability over the real NIC.
#
# --server-binary selects the listener implementation: tcp-echo (sync, default)
# or tokio-tcp-echo (async) — same client either way.
#
# Usage:
#   # On Instance B (listener):
#   ./tier1-tcp-echo.sh --role listener --bind-ip 10.0.1.100 --port 9000 \
#       [--server-binary tcp-echo|tokio-tcp-echo]
#
#   # On Instance A (sender):
#   ./tier1-tcp-echo.sh --role sender --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier1-tcp-echo.xml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/harness-common.sh"

# ── Defaults ─────────────────────────────────────────────────────────────────

PROJECT_DIR="/opt/dpdk-stdlib"
SERVER_BINARY="$PROJECT_DIR/target/release/tcp-echo"
CLIENT_BINARY="$PROJECT_DIR/target/release/tcp-test-client"
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
        --role)          ROLE="$2";        shift 2 ;;
        --bind-ip)       BIND_IP="$2";     shift 2 ;;
        --peer-ip)       PEER_IP="$2";     shift 2 ;;
        --port)          PORT="$2";        shift 2 ;;
        --output)        OUTPUT="$2";      shift 2 ;;
        --gateway-mac)   GATEWAY_MAC="$2"; shift 2 ;;
        --server-binary) SERVER_BINARY="$PROJECT_DIR/target/release/$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$ROLE" || -z "$BIND_IP" ]]; then
    echo "Missing required arguments: --role and --bind-ip are required" >&2
    exit 1
fi

if [[ "$ROLE" == "sender" && -z "$PEER_IP" ]]; then
    echo "Sender role requires --peer-ip" >&2
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
    # In AWS VPC, the gateway is always .1 of the subnet
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

# ── Listener role ────────────────────────────────────────────────────────────

run_listener() {
    log_info "Starting TCP echo server ($SERVER_BINARY) on ${BIND_IP}:${PORT}"
    discover_gateway_mac
    ulimit -c unlimited 2>/dev/null || true

    # Ensure a clean DPDK primary (a prior server variant may have run here).
    rm -rf /var/run/dpdk/ 2>/dev/null || true

    local server_log="/tmp/tcp-echo-server.log"
    log_info "Launching: $SERVER_BINARY --ip $BIND_IP --port $PORT --gateway-mac $GATEWAY_MAC"
    "$SERVER_BINARY" --ip "$BIND_IP" --port "$PORT" --gateway-mac "$GATEWAY_MAC" \
        > /tmp/tcp-echo-server-stdout.log 2>"$server_log" &
    local server_pid=$!
    log_info "TCP echo server started with PID $server_pid"

    # The servers print "<name> listening on <addr>" to stderr — gate on that.
    local waited=0
    while ! grep -q "listening on" "$server_log" 2>/dev/null; do
        sleep 1
        waited=$((waited + 1))
        if [[ $waited -ge 30 ]]; then
            log_error "TCP echo server did not become ready within 30s"
            cat "$server_log" >&2 || true
            kill "$server_pid" 2>/dev/null || true
            return 1
        fi
        if ! kill -0 "$server_pid" 2>/dev/null; then
            log_error "TCP echo server process died during startup"
            cat "$server_log" >&2 || true
            check_process_crash "$server_pid" "tcp-echo" || true
            return 1
        fi
    done

    log_info "TCP echo server ready, waiting for client tests..."

    # Keep running until the sender finishes or we hit the ceiling.
    local max_wait=120
    waited=0
    while kill -0 "$server_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$((waited + 5))
    done

    if ! kill -0 "$server_pid" 2>/dev/null; then
        check_process_crash "$server_pid" "tcp-echo" || true
    else
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    log_info "Listener finished"
}

# ── Sender role ──────────────────────────────────────────────────────────────

run_sender() {
    log_info "Starting Tier 1 TCP sender: ${BIND_IP} -> ${PEER_IP}:${PORT}"
    discover_gateway_mac

    local client_log="/tmp/tcp-test-client.log"
    exec > >(tee -a "$client_log") 2>&1

    junit_start_suite "tier1-tcp-echo" 2

    # Give the listener time to bind.
    sleep 5

    # ── Test 1: bidir echo (single round-trip) ───────────────────────────
    log_info "Test: bidir_echo"
    local start end elapsed out ok err
    start=$(_timer_now)
    ok=true; err=""
    out=$(run_with_timeout "$TEST_TIMEOUT" "$CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --local-ip "$BIND_IP" \
        --gateway-mac "$GATEWAY_MAC" --mode bidir --count 1 2>&1) || { ok=false; err="client exited non-zero"; }
    if [[ "$ok" == "true" ]] && ! echo "$out" | grep -q "echo round-trip"; then
        ok=false; err="no 'echo round-trip' result line"
    fi
    end=$(_timer_now); elapsed=$(_timer_elapsed "$start" "$end")
    if [[ "$ok" == "true" ]]; then
        log_info "PASS: bidir echo"
        junit_add_pass "bidir_echo" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: bidir echo — $err"
        junit_add_failure "bidir_echo" "$CLASSNAME" "$elapsed" "$err" "$out"
    fi

    # Clean up DPDK shared memory so the next process can reinitialize EAL.
    rm -rf /var/run/dpdk/ 2>/dev/null || true

    # ── Test 2: bidir multi round-trip (20) ──────────────────────────────
    log_info "Test: bidir_multi"
    start=$(_timer_now)
    ok=true; err=""
    out=$(run_with_timeout "$TEST_TIMEOUT" "$CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --local-ip "$BIND_IP" \
        --gateway-mac "$GATEWAY_MAC" --mode bidir --count 20 2>&1) || { ok=false; err="client exited non-zero"; }
    if [[ "$ok" == "true" ]]; then
        local rtt
        rtt=$(echo "$out" | sed -n 's/^Result: \([0-9]*\) echo round-trips.*/\1/p' | head -1)
        if [[ -z "$rtt" || "$rtt" -lt 20 ]]; then
            ok=false; err="expected >=20 round-trips, got '${rtt:-none}'"
        fi
    fi
    end=$(_timer_now); elapsed=$(_timer_elapsed "$start" "$end")
    if [[ "$ok" == "true" ]]; then
        log_info "PASS: bidir multi (20 round-trips)"
        junit_add_pass "bidir_multi" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: bidir multi — $err"
        junit_add_failure "bidir_multi" "$CLASSNAME" "$elapsed" "$err" "$out"
    fi

    rm -rf /var/run/dpdk/ 2>/dev/null || true

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
    listener) run_listener ;;
    sender)   run_sender ;;
    *)
        echo "Invalid role: $ROLE (must be 'listener' or 'sender')" >&2
        exit 1
        ;;
esac
