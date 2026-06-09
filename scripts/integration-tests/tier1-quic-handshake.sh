#!/usr/bin/env bash
# tier1-quic-handshake.sh - Tier 1: QUIC handshake and bidirectional echo over DPDK
#
# Tests full QUIC connection between two dpdk-stdlib-quic instances.
# Verifies: TLS handshake, bidirectional stream send/receive, payload integrity.
#
# Usage:
#   # On Instance B (server):
#   ./tier1-quic-handshake.sh --role server --bind-ip 10.0.1.100 --port 4433
#
#   # On Instance A (client):
#   ./tier1-quic-handshake.sh --role client --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 4433 --output /tmp/test-results/tier1-quic-handshake.xml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/harness-common.sh"

# ── Defaults ─────────────────────────────────────────────────────────────────

PROJECT_DIR="/opt/dpdk-stdlib"
QUIC_SERVER_BINARY="$PROJECT_DIR/target/release/quic-echo-server"
QUIC_CLIENT_BINARY="$PROJECT_DIR/target/release/quic-test-client"
ROLE=""
BIND_IP=""
PEER_IP=""
PORT=4433
OUTPUT=""
GATEWAY_MAC=""
TEST_TIMEOUT=60
CLASSNAME="tier1.quic_handshake"

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

if [[ -z "$ROLE" || -z "$BIND_IP" ]]; then
    echo "Missing required arguments: --role and --bind-ip are required" >&2
    exit 1
fi

if [[ "$ROLE" == "client" && -z "$PEER_IP" ]]; then
    echo "Client role requires --peer-ip" >&2
    exit 1
fi

if [[ -z "$OUTPUT" ]]; then
    OUTPUT=$(result_path "tier1" "quic-handshake")
fi

# ── Discover gateway MAC if not provided ─────────────────────────────────────

discover_gateway_mac() {
    if [[ -n "$GATEWAY_MAC" ]]; then
        return
    fi
    # In AWS VPC, the gateway is always .1 of the subnet
    local subnet_gw
    subnet_gw=$(echo "$BIND_IP" | sed 's/\.[0-9]*$/.1/')
    # Warm the ARP cache
    ping -c 1 -W 2 "$subnet_gw" >/dev/null 2>&1 || true
    sleep 1
    # Read from /proc/net/arp
    GATEWAY_MAC=$(awk -v ip="$subnet_gw" '$1 == ip && $4 != "00:00:00:00:00:00" {print $4}' /proc/net/arp | head -1)
    if [[ -z "$GATEWAY_MAC" ]]; then
        log_error "Could not discover gateway MAC for $subnet_gw"
        GATEWAY_MAC="00:00:00:00:00:00"
    fi
    log_info "Discovered gateway MAC: $GATEWAY_MAC"
}

# ── Server role ──────────────────────────────────────────────────────────────

run_server() {
    log_info "Starting QUIC echo server on ${BIND_IP}:${PORT}"
    discover_gateway_mac

    ulimit -c unlimited 2>/dev/null || true

    local server_log="/tmp/quic-echo-server.log"
    local cert_file="/tmp/quic-server-cert.pem"

    log_info "Launching: $QUIC_SERVER_BINARY --ip $BIND_IP --port $PORT --gateway-mac $GATEWAY_MAC"
    $QUIC_SERVER_BINARY --ip "$BIND_IP" --port "$PORT" --gateway-mac "$GATEWAY_MAC" \
        > /tmp/quic-server-stdout.log 2>"$server_log" &
    local server_pid=$!
    log_info "QUIC server started with PID $server_pid"

    # Wait for server ready signal
    local waited=0
    while ! grep -q "QUIC_SERVER_READY" "$server_log" 2>/dev/null; do
        sleep 1
        waited=$((waited + 1))
        if [[ $waited -ge 30 ]]; then
            log_error "Server did not become ready within 30s"
            kill "$server_pid" 2>/dev/null || true
            exit 1
        fi
        if ! kill -0 "$server_pid" 2>/dev/null; then
            log_error "Server process died"
            cat "$server_log" >&2
            exit 1
        fi
    done

    # Extract cert PEM from stdout
    sed -n '/---BEGIN CERT PEM---/,/---END CERT PEM---/{//!p}' /tmp/quic-server-stdout.log > "$cert_file"
    if [[ ! -s "$cert_file" ]]; then
        log_error "Failed to extract server certificate"
        kill "$server_pid" 2>/dev/null || true
        exit 1
    fi

    log_info "QUIC server ready, cert written to $cert_file"
    log_info "Waiting for client tests to complete..."

    # Keep running until killed
    wait "$server_pid" 2>/dev/null || true
}

# ── Client role ──────────────────────────────────────────────────────────────

run_client() {
    log_info "Starting QUIC client tests against ${PEER_IP}:${PORT}"
    discover_gateway_mac

    local cert_file="/tmp/quic-server-cert.pem"

    # Wait for cert file from server (deployed by orchestrator via SSM)
    local waited=0
    while [[ ! -s "$cert_file" ]]; do
        sleep 2
        waited=$((waited + 2))
        if [[ $waited -ge 60 ]]; then
            log_error "Server cert not available within 60s"
            exit 1
        fi
    done
    log_info "Server cert available at $cert_file"

    mkdir -p "$(dirname "$OUTPUT")"
    junit_start_suite "tier1-quic-handshake" 2

    # Test 1: Handshake
    log_info "Test 1: QUIC handshake"
    local test_start
    test_start=$(_timer_now)
    local hs_output
    hs_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$QUIC_CLIENT_BINARY" --server-ip "$PEER_IP" --port "$PORT" \
        --bind-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --cert-pem "$cert_file" --mode handshake 2>&1) || true
    local test_end
    test_end=$(_timer_now)
    local elapsed
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if echo "$hs_output" | grep -q "HANDSHAKE_OK"; then
        log_info "PASS: QUIC handshake succeeded"
        local latency
        latency=$(echo "$hs_output" | grep "HANDSHAKE_OK" | sed 's/.*latency_us=\([0-9]*\).*/\1/')
        junit_add_pass "quic_handshake" "$CLASSNAME" "$elapsed"
        log_info "  Handshake latency: ${latency} µs"
    else
        log_error "FAIL: QUIC handshake failed"
        log_error "Output: $hs_output"
        junit_add_failure "quic_handshake" "$CLASSNAME" "$elapsed" \
            "Handshake failed" "$hs_output"
    fi

    # Test 2: Bidirectional data transfer
    log_info "Test 2: Bidirectional echo (1KB payload)"
    test_start=$(_timer_now)
    local bidir_output
    bidir_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$QUIC_CLIENT_BINARY" --server-ip "$PEER_IP" --port "$PORT" \
        --bind-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --cert-pem "$cert_file" --mode bidir --payload-size 1024 2>&1) || true
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if echo "$bidir_output" | grep -q "BIDIR_OK"; then
        log_info "PASS: Bidirectional echo succeeded"
        local throughput
        throughput=$(echo "$bidir_output" | grep "BIDIR_OK" | sed 's/.*throughput_mbps=\([0-9.]*\).*/\1/')
        junit_add_pass "quic_bidir_echo" "$CLASSNAME" "$elapsed"
        log_info "  Throughput: ${throughput} Mbps"
    else
        log_error "FAIL: Bidirectional echo failed"
        log_error "Output: $bidir_output"
        junit_add_failure "quic_bidir_echo" "$CLASSNAME" "$elapsed" \
            "Bidir echo failed" "$bidir_output"
    fi

    junit_end_suite
    junit_write "$OUTPUT"

    # Print summary
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
