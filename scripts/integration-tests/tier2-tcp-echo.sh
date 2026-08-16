#!/usr/bin/env bash
# tier2-tcp-echo.sh - Tier 2: kernel TCP client -> DPDK TCP server smoke test
#
# A pure-kernel (std::net) reference client connects to our DPDK tcp-echo server
# and performs connect -> echo -> close. Proves our DPDK TCP stack interoperates
# with a standard, independent TCP implementation over the real NIC — the exact
# scenario (NIC-padded bare ACKs from a normal stack) that exposed the codec
# padding bug. Mirrors the working kernel->DPDK direction of the UDP Tier 2.
#
# Usage:
#   # On Instance B (listener, DPDK):
#   ./tier2-tcp-echo.sh --role listener --bind-ip 10.0.1.100 --port 9000
#
#   # On Instance A (sender, kernel networking):
#   ./tier2-tcp-echo.sh --role sender --peer-ip 10.0.1.100 --port 9000 \
#       --output /tmp/test-results/tier2-tcp-echo.xml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/harness-common.sh"

# ── Defaults ─────────────────────────────────────────────────────────────────

PROJECT_DIR="/opt/dpdk-stdlib"
SERVER_BINARY="$PROJECT_DIR/target/release/tcp-echo"
KERNEL_CLIENT_BINARY="$PROJECT_DIR/target/release/tcp-kernel-client"
ROLE=""
BIND_IP=""
PEER_IP=""
PORT=9000
OUTPUT=""
GATEWAY_MAC=""
TEST_TIMEOUT=60
CLASSNAME="tier2.tcp_echo"

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
            exit 1
            ;;
    esac
done

if [[ -z "$ROLE" ]]; then
    echo "Missing required argument: --role" >&2
    exit 1
fi
if [[ "$ROLE" == "listener" && -z "$BIND_IP" ]]; then
    echo "Listener role requires --bind-ip" >&2
    exit 1
fi
if [[ "$ROLE" == "sender" && -z "$PEER_IP" ]]; then
    echo "Sender role requires --peer-ip" >&2
    exit 1
fi

if [[ -z "$OUTPUT" ]]; then
    OUTPUT=$(result_path "tier2" "tcp-echo")
fi

# ── Discover gateway MAC if not provided (listener/DPDK side only) ────────────

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

# ── Listener role (DPDK tcp-echo server) ─────────────────────────────────────

run_listener() {
    log_info "Starting DPDK TCP echo server on ${BIND_IP}:${PORT}"
    discover_gateway_mac
    ulimit -c unlimited 2>/dev/null || true

    rm -rf /var/run/dpdk/ 2>/dev/null || true

    local server_log="/tmp/tcp-echo-server.log"
    log_info "Launching: $SERVER_BINARY --ip $BIND_IP --port $PORT --gateway-mac $GATEWAY_MAC"
    "$SERVER_BINARY" --ip "$BIND_IP" --port "$PORT" --gateway-mac "$GATEWAY_MAC" \
        > /tmp/tcp-echo-server-stdout.log 2>"$server_log" &
    local server_pid=$!
    log_info "DPDK TCP echo server started with PID $server_pid"

    local waited=0
    while ! grep -q "listening on" "$server_log" 2>/dev/null; do
        sleep 1
        waited=$((waited + 1))
        if [[ $waited -ge 30 ]]; then
            log_error "DPDK TCP echo server did not become ready within 30s"
            cat "$server_log" >&2 || true
            kill "$server_pid" 2>/dev/null || true
            return 1
        fi
        if ! kill -0 "$server_pid" 2>/dev/null; then
            log_error "DPDK TCP echo server died during startup"
            cat "$server_log" >&2 || true
            check_process_crash "$server_pid" "tcp-echo" || true
            return 1
        fi
    done

    log_info "DPDK TCP echo server ready, waiting for kernel client..."
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

# ── Sender role (pure-kernel client) ─────────────────────────────────────────

run_sender() {
    log_info "Starting Tier 2 kernel TCP client -> ${PEER_IP}:${PORT}"

    local client_log="/tmp/tcp-kernel-client.log"
    exec > >(tee -a "$client_log") 2>&1

    junit_start_suite "tier2-tcp-echo" 1

    # Give the listener time to bind.
    sleep 5

    # ── Test: kernel client -> DPDK server echo ──────────────────────────
    log_info "Test: kernel_to_dpdk_echo"
    local start end elapsed out ok err
    start=$(_timer_now)
    ok=true; err=""
    out=$(run_with_timeout "$TEST_TIMEOUT" "$KERNEL_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --count 20 --payload-size 64 2>&1) || { ok=false; err="kernel client exited non-zero"; }
    if [[ "$ok" == "true" ]] && ! echo "$out" | grep -q "TCP_KERNEL_OK"; then
        ok=false; err="no TCP_KERNEL_OK marker"
    fi
    end=$(_timer_now); elapsed=$(_timer_elapsed "$start" "$end")
    if [[ "$ok" == "true" ]]; then
        log_info "PASS: kernel client -> DPDK server echo"
        junit_add_pass "kernel_to_dpdk_echo" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: kernel_to_dpdk_echo — $err"
        junit_add_failure "kernel_to_dpdk_echo" "$CLASSNAME" "$elapsed" "$err" "$out"
    fi

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
