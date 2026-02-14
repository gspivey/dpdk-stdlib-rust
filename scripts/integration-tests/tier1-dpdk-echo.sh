#!/usr/bin/env bash
# tier1-dpdk-echo.sh - Tier 1: DPDK <-> DPDK echo test harness
#
# Tests bidirectional UDP communication between two dpdk-stdlib instances.
# Verifies: ARP resolution, UDP send/receive, echo roundtrip, payload integrity.
#
# Usage:
#   # On Instance B (listener):
#   ./tier1-dpdk-echo.sh --role listener --bind-ip 10.0.1.100 --port 9000
#
#   # On Instance A (sender):
#   ./tier1-dpdk-echo.sh --role sender --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier1-dpdk-echo.xml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/harness-common.sh"

# ── Defaults ─────────────────────────────────────────────────────────────────

PROJECT_DIR="/opt/dpdk-stdlib"
ECHO_BINARY="$PROJECT_DIR/target/release/echo"
TEST_CLIENT_BINARY="$PROJECT_DIR/target/release/test-client"
ROLE=""
BIND_IP=""
PEER_IP=""
PORT=9000
OUTPUT=""
TEST_TIMEOUT=60  # Per-test timeout in seconds
CLASSNAME="tier1.dpdk_echo"

# ── Argument parsing ─────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --role)       ROLE="$2";    shift 2 ;;
        --bind-ip)    BIND_IP="$2"; shift 2 ;;
        --peer-ip)    PEER_IP="$2"; shift 2 ;;
        --port)       PORT="$2";    shift 2 ;;
        --output)     OUTPUT="$2";  shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 --role <listener|sender> --bind-ip <IP> [--peer-ip <IP>] --port <PORT> [--output <PATH>]" >&2
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
    OUTPUT=$(result_path "tier1" "dpdk-echo")
fi

# ── Listener role ────────────────────────────────────────────────────────────

run_listener() {
    log_info "Starting Tier 1 listener on ${BIND_IP}:${PORT}"

    # Start the echo server in the background
    log_info "Launching echo server: $ECHO_BINARY --ip $BIND_IP --port $PORT"
    "$ECHO_BINARY" --ip "$BIND_IP" --port "$PORT" &
    local echo_pid=$!
    log_info "Echo server started with PID $echo_pid"

    # Wait for the echo server to be ready (give it a moment to bind)
    sleep 3

    # Verify the echo server is still running
    if ! kill -0 "$echo_pid" 2>/dev/null; then
        log_error "Echo server exited prematurely"
        wait "$echo_pid" || true
        return 1
    fi

    log_info "Listener is ready and waiting for traffic"

    # Keep running until killed by the orchestrator or sender finishes
    # The listener stays up for the duration of the test
    local waited=0
    local max_wait=120
    while kill -0 "$echo_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    # Clean up
    kill "$echo_pid" 2>/dev/null || true
    wait "$echo_pid" 2>/dev/null || true
    log_info "Listener finished"
}

# ── Sender role ──────────────────────────────────────────────────────────────

run_sender() {
    log_info "Starting Tier 1 sender: ${BIND_IP} -> ${PEER_IP}:${PORT}"
    log_info "Output will be written to: $OUTPUT"

    junit_start_suite "tier1-dpdk-echo" 4

    # Give the listener time to start
    sleep 5

    # ── Test 1: ARP resolution ───────────────────────────────────────────
    log_info "Test: arp_resolution"
    local start end elapsed
    start=$(_timer_now)
    local arp_ok=true
    local arp_err=""

    # Send a single packet to trigger ARP resolution, then check if we get a response
    if run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --message "arp-probe" --count 1 2>&1; then
        log_info "ARP resolution succeeded (got response from peer)"
    else
        arp_ok=false
        arp_err="ARP resolution failed: no response from ${PEER_IP}"
        log_error "$arp_err"
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$arp_ok" == "true" ]]; then
        junit_add_pass "arp_resolution" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "arp_resolution" "$CLASSNAME" "$elapsed" "$arp_err" "Could not resolve MAC address for $PEER_IP"
    fi

    # ── Test 2: UDP send/receive ─────────────────────────────────────────
    log_info "Test: udp_send_receive"
    start=$(_timer_now)
    local send_ok=true
    local send_err=""
    local send_output=""

    send_output=$(run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --message "hello-dpdk" --count 3 --delay 500 2>&1) || {
        send_ok=false
        send_err="UDP send/receive failed"
    }

    # Verify we got responses
    if [[ "$send_ok" == "true" ]]; then
        if echo "$send_output" | grep -q "Received"; then
            log_info "UDP send/receive succeeded"
        else
            send_ok=false
            send_err="No response received from peer"
        fi
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$send_ok" == "true" ]]; then
        junit_add_pass "udp_send_receive" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "udp_send_receive" "$CLASSNAME" "$elapsed" "$send_err" "$send_output"
    fi

    # ── Test 3: Echo roundtrip ───────────────────────────────────────────
    log_info "Test: echo_roundtrip"
    start=$(_timer_now)
    local echo_ok=true
    local echo_err=""
    local echo_output=""

    echo_output=$(run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --message "roundtrip-test" --count 5 --delay 200 2>&1) || {
        echo_ok=false
        echo_err="Echo roundtrip timed out or failed"
    }

    # Count how many responses came back
    if [[ "$echo_ok" == "true" ]]; then
        local response_count
        response_count=$(echo "$echo_output" | grep -c "Received" || true)
        if [[ "$response_count" -ge 3 ]]; then
            log_info "Echo roundtrip succeeded: $response_count/5 responses received"
        else
            echo_ok=false
            echo_err="Echo roundtrip: only $response_count/5 responses received (need >= 3)"
        fi
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$echo_ok" == "true" ]]; then
        junit_add_pass "echo_roundtrip" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "echo_roundtrip" "$CLASSNAME" "$elapsed" "$echo_err" "$echo_output"
    fi

    # ── Test 4: Payload integrity ────────────────────────────────────────
    log_info "Test: payload_integrity"
    start=$(_timer_now)
    local payload_ok=true
    local payload_err=""
    local payload_output=""
    local test_payload="Hello DPDK payload integrity check 12345"

    payload_output=$(run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --message "$test_payload" --count 1 2>&1) || {
        payload_ok=false
        payload_err="Payload integrity test timed out or failed"
    }

    # Verify the echoed payload contains our message
    if [[ "$payload_ok" == "true" ]]; then
        if echo "$payload_output" | grep -q "echo: $test_payload"; then
            log_info "Payload integrity verified"
        elif echo "$payload_output" | grep -q "Received"; then
            # Got a response but payload may differ - still check
            log_info "Response received, checking payload match..."
            if echo "$payload_output" | grep -q "$test_payload"; then
                log_info "Payload integrity verified (found in response)"
            else
                payload_ok=false
                payload_err="Payload mismatch: response did not contain expected payload"
            fi
        else
            payload_ok=false
            payload_err="No response received for payload integrity test"
        fi
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$payload_ok" == "true" ]]; then
        junit_add_pass "payload_integrity" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "payload_integrity" "$CLASSNAME" "$elapsed" "$payload_err" "$payload_output"
    fi

    # ── Finalize ─────────────────────────────────────────────────────────
    junit_end_suite
    junit_write "$OUTPUT"

    log_info "Tier 1 sender tests complete. Results: $OUTPUT"
}

# ── Main dispatch ────────────────────────────────────────────────────────────

case "$ROLE" in
    listener)
        run_listener
        ;;
    sender)
        run_sender
        ;;
    *)
        echo "Unknown role: $ROLE" >&2
        echo "Usage: $0 --role <listener|sender> ..." >&2
        exit 1
        ;;
esac
