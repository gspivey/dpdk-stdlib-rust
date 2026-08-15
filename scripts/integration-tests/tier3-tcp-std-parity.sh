#!/usr/bin/env bash
# tier3-tcp-std-parity.sh - Tier 3: TCP std::net::TcpStream parity test
#
# Tests that dpdk-stdlib-tcp produces byte-for-byte identical received streams
# and identical io::ErrorKind values compared to std::net::TcpStream when
# communicating with the same server.
#
# Uses tcp-test-client --mode std-parity which:
#   1. Connects via dpdk-stdlib-tcp, sends payload, reads echo, disconnects
#   2. Connects via std::net::TcpStream, sends same payload, reads echo, disconnects
#   3. Compares received bytes — must be identical
#
# Usage:
#   # On Instance B (server):
#   ./tier3-tcp-std-parity.sh --role server --bind-ip 10.0.1.100 --port 9000 \
#       --gateway-mac AA:BB:CC:DD:EE:FF
#
#   # On Instance A (client):
#   ./tier3-tcp-std-parity.sh --role client --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier3-tcp-std-parity.xml \
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
CLASSNAME="tier3.tcp_std_parity"

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
    OUTPUT=$(result_path "tier3" "tcp-std-parity")
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
#
# The std-parity test needs a server that BOTH the DPDK client and the
# std::net client can connect to. The DPDK tcp-echo listens on the DPDK ENI,
# and the std::net::TcpStream connects through the kernel stack.
#
# For the std::net client to reach the same server, we also start a kernel
# echo server on the same port (accessible via the management interface).
# However, the simplest setup is: the DPDK tcp-echo server is reachable
# from both paths because the VPC routes to the instance.

run_server() {
    log_info "Starting TCP echo server (std-parity test) on ${BIND_IP}:${PORT}"
    discover_gateway_mac

    ulimit -c unlimited 2>/dev/null || true

    # Start DPDK tcp-echo server — handles connections from DPDK path
    local server_log="/tmp/tcp-echo-server.log"
    log_info "Launching: $TCP_ECHO_BINARY --ip $BIND_IP --port $PORT --gateway-mac $GATEWAY_MAC"
    $TCP_ECHO_BINARY --ip "$BIND_IP" --port "$PORT" --gateway-mac "$GATEWAY_MAC" \
        > "$server_log" 2>&1 &
    local dpdk_pid=$!
    log_info "DPDK tcp-echo server started with PID $dpdk_pid"

    # Also start a kernel echo server on PORT+1 for the std::net path.
    # The std-parity mode in tcp-test-client connects std::net to the same
    # target, but since DPDK owns the ENI, std::net connections route through
    # the management interface. We need a kernel listener on a known port.
    local kernel_port=$((PORT + 1))
    local kernel_log="/tmp/kernel-echo-server.log"
    if command -v socat >/dev/null 2>&1; then
        log_info "Launching kernel echo (socat) on 0.0.0.0:${kernel_port}"
        socat TCP-LISTEN:"$kernel_port",reuseaddr,fork EXEC:cat > "$kernel_log" 2>&1 &
        local kernel_pid=$!
    elif command -v ncat >/dev/null 2>&1; then
        log_info "Launching kernel echo (ncat) on 0.0.0.0:${kernel_port}"
        ncat -l -k -p "$kernel_port" --exec "/bin/cat" > "$kernel_log" 2>&1 &
        local kernel_pid=$!
    else
        log_info "No kernel echo tool available (socat/ncat) — std-parity may use DPDK path for both"
        local kernel_pid=""
    fi

    sleep 3

    if ! kill -0 "$dpdk_pid" 2>/dev/null; then
        log_error "DPDK tcp-echo server exited prematurely"
        cat "$server_log" >&2
        check_process_crash "$dpdk_pid" "tcp-echo" || true
        exit 1
    fi

    log_info "Servers ready, waiting for client tests..."

    # Keep running until killed by orchestrator
    local waited=0
    local max_wait=180
    while kill -0 "$dpdk_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    if ! kill -0 "$dpdk_pid" 2>/dev/null; then
        check_process_crash "$dpdk_pid" "tcp-echo" || true
    else
        kill "$dpdk_pid" 2>/dev/null || true
        wait "$dpdk_pid" 2>/dev/null || true
    fi
    if [[ -n "${kernel_pid:-}" ]]; then
        kill "$kernel_pid" 2>/dev/null || true
        wait "$kernel_pid" 2>/dev/null || true
    fi
    log_info "All servers finished"
}

# ── Client role ──────────────────────────────────────────────────────────────

run_client() {
    log_info "Starting TCP std-parity tests against ${PEER_IP}:${PORT}"
    discover_gateway_mac

    mkdir -p "$(dirname "$OUTPUT")"
    junit_start_suite "tier3-tcp-std-parity" 4

    # Give the server time to start
    sleep 5

    # ── Test 1: std-parity with small payload (64B) ──────────────────────
    log_info "Test 1: std_parity_small (64B)"
    local test_start test_end elapsed
    test_start=$(_timer_now)
    local small_output=""
    local small_ok=true

    small_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode std-parity --payload-size 64 2>&1) || {
        small_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$small_ok" == "true" ]] && echo "$small_output" | grep -q "PASS"; then
        log_info "PASS: std-parity 64B — byte-for-byte identical"
        junit_add_pass "std_parity_small" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: std-parity 64B failed"
        log_error "Output: $small_output"
        local failure_msg="std-parity 64B: DPDK and std::net results differ"
        if echo "$small_output" | grep -q "FAIL"; then
            failure_msg="std-parity 64B: $(echo "$small_output" | grep "FAIL" | head -1)"
        fi
        junit_add_failure "std_parity_small" "$CLASSNAME" "$elapsed" \
            "$failure_msg" "$small_output"
    fi

    # ── Test 2: std-parity with medium payload (512B) ────────────────────
    log_info "Test 2: std_parity_medium (512B)"
    test_start=$(_timer_now)
    local med_output=""
    local med_ok=true

    med_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode std-parity --payload-size 512 2>&1) || {
        med_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$med_ok" == "true" ]] && echo "$med_output" | grep -q "PASS"; then
        log_info "PASS: std-parity 512B — byte-for-byte identical"
        junit_add_pass "std_parity_medium" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: std-parity 512B failed"
        log_error "Output: $med_output"
        local failure_msg="std-parity 512B: DPDK and std::net results differ"
        if echo "$med_output" | grep -q "FAIL"; then
            failure_msg="std-parity 512B: $(echo "$med_output" | grep "FAIL" | head -1)"
        fi
        junit_add_failure "std_parity_medium" "$CLASSNAME" "$elapsed" \
            "$failure_msg" "$med_output"
    fi

    # ── Test 3: std-parity with large payload (1400B) ────────────────────
    log_info "Test 3: std_parity_large (1400B)"
    test_start=$(_timer_now)
    local large_output=""
    local large_ok=true

    large_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode std-parity --payload-size 1400 2>&1) || {
        large_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$large_ok" == "true" ]] && echo "$large_output" | grep -q "PASS"; then
        log_info "PASS: std-parity 1400B — byte-for-byte identical"
        junit_add_pass "std_parity_large" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: std-parity 1400B failed"
        log_error "Output: $large_output"
        local failure_msg="std-parity 1400B: DPDK and std::net results differ"
        if echo "$large_output" | grep -q "FAIL"; then
            failure_msg="std-parity 1400B: $(echo "$large_output" | grep "FAIL" | head -1)"
        fi
        junit_add_failure "std_parity_large" "$CLASSNAME" "$elapsed" \
            "$failure_msg" "$large_output"
    fi

    # ── Test 4: std-parity with multi-segment payload (4096B) ────────────
    #
    # Tests a payload that exceeds MSS (1460 for IPv4). This ensures that
    # segmentation and reassembly produce identical results for both paths.
    log_info "Test 4: std_parity_multi_segment (4096B)"
    test_start=$(_timer_now)
    local multi_output=""
    local multi_ok=true

    multi_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode std-parity --payload-size 4096 2>&1) || {
        multi_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$multi_ok" == "true" ]] && echo "$multi_output" | grep -q "PASS"; then
        log_info "PASS: std-parity 4096B — byte-for-byte identical (multi-segment)"
        junit_add_pass "std_parity_multi_segment" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: std-parity 4096B failed"
        log_error "Output: $multi_output"
        local failure_msg="std-parity 4096B: DPDK and std::net results differ (multi-segment)"
        if echo "$multi_output" | grep -q "FAIL"; then
            failure_msg="std-parity 4096B: $(echo "$multi_output" | grep "FAIL" | head -1)"
        fi
        junit_add_failure "std_parity_multi_segment" "$CLASSNAME" "$elapsed" \
            "$failure_msg" "$multi_output"
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
