#!/usr/bin/env bash
# tier4-jumbo-echo.sh - Tier 4: Jumbo frame echo test harness
#
# Tests UDP echo with large (jumbo-frame) payloads between two dpdk-stdlib instances.
# Verifies: jumbo frame support, large packet integrity, MTU handling.
#
# AWS VPC supports 9001-byte MTU between instances in the same placement group
# and many instance types support jumbo frames by default.
#
# Payload sizes tested:
#   - 1400 bytes (near standard MTU limit)
#   - 4000 bytes (mid-range jumbo)
#   - 8000 bytes (near max jumbo, within 8973 UDP payload limit at 9001 MTU)
#
# Usage:
#   # On Instance B (listener):
#   ./tier4-jumbo-echo.sh --role listener --bind-ip 10.0.1.100 --port 9000
#
#   # On Instance A (sender):
#   ./tier4-jumbo-echo.sh --role sender --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier4-jumbo-echo.xml

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
TEST_TIMEOUT=60
CLASSNAME="tier4.jumbo_echo"

# Jumbo payload sizes to test (bytes)
PAYLOAD_SIZES=(1400 4000 8000)

# ── Argument parsing ─────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --role)        ROLE="$2";        shift 2 ;;
        --bind-ip)     BIND_IP="$2";     shift 2 ;;
        --peer-ip)     PEER_IP="$2";     shift 2 ;;
        --port)        PORT="$2";        shift 2 ;;
        --output)      OUTPUT="$2";      shift 2 ;;
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
    OUTPUT=$(result_path "tier4" "jumbo-echo")
fi

# ── Listener role ────────────────────────────────────────────────────────────

run_listener() {
    log_info "Starting Tier 4 listener on ${BIND_IP}:${PORT}"

    ulimit -c unlimited 2>/dev/null || true

    local echo_log="/tmp/echo-server-jumbo.log"
    log_info "Launching echo server: $ECHO_BINARY --ip $BIND_IP --port $PORT"
    $ECHO_BINARY --ip "$BIND_IP" --port "$PORT" > "$echo_log" 2>&1 &
    local echo_pid=$!
    log_info "Echo server started with PID $echo_pid"

    sleep 3

    if ! kill -0 "$echo_pid" 2>/dev/null; then
        log_error "Echo server exited prematurely"
        if check_process_crash "$echo_pid" "echo"; then
            log_error "Echo server CRASHED during startup"
        fi
        return 1
    fi

    log_info "Listener is ready and waiting for jumbo frame traffic"

    local waited=0
    local max_wait=120
    while kill -0 "$echo_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    if ! kill -0 "$echo_pid" 2>/dev/null; then
        check_process_crash "$echo_pid" "echo" || true
    else
        kill "$echo_pid" 2>/dev/null || true
        wait "$echo_pid" 2>/dev/null || true
    fi
    log_info "Listener finished"
}

# ── Sender role ──────────────────────────────────────────────────────────────

run_sender() {
    log_info "Starting Tier 4 sender: ${BIND_IP} -> ${PEER_IP}:${PORT}"
    log_info "Testing payload sizes: ${PAYLOAD_SIZES[*]}"
    log_info "Output will be written to: $OUTPUT"

    local client_log="/tmp/test-client-jumbo.log"
    exec > >(tee -a "$client_log") 2>&1

    local num_tests=$(( ${#PAYLOAD_SIZES[@]} + 1 ))  # +1 for ARP warmup
    junit_start_suite "tier4-jumbo-echo" "$num_tests"

    sleep 5

    # ── Warmup: ARP resolution with small packet ─────────────────────────
    log_info "Test: arp_warmup"
    local start end elapsed
    start=$(_timer_now)
    local warmup_ok=true
    local warmup_err=""

    if run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --bind-ip "$BIND_IP" --message "arp-warmup" --count 1 2>&1; then
        log_info "ARP warmup succeeded"
    else
        warmup_ok=false
        warmup_err="ARP warmup failed: no response from ${PEER_IP}"
        log_error "$warmup_err"
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$warmup_ok" == "true" ]]; then
        junit_add_pass "arp_warmup" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "arp_warmup" "$CLASSNAME" "$elapsed" "$warmup_err" \
            "Could not resolve MAC address for $PEER_IP"
    fi

    rm -rf /var/run/dpdk/ 2>/dev/null || true

    # ── Test each payload size ───────────────────────────────────────────
    for size in "${PAYLOAD_SIZES[@]}"; do
        local test_name="jumbo_echo_${size}b"
        log_info "Test: $test_name (payload=${size} bytes)"
        start=$(_timer_now)
        local test_ok=true
        local test_err=""
        local test_output=""

        test_output=$(run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
            --target "$PEER_IP" --port "$PORT" --bind-ip "$BIND_IP" \
            --payload-size "$size" --count 3 --delay 500 2>&1) || {
            test_ok=false
            test_err="Jumbo echo failed for ${size}-byte payload"
        }

        if [[ "$test_ok" == "true" ]]; then
            # Verify we got responses with matching sizes
            local ok_count
            ok_count=$(echo "$test_output" | grep -c "OK" || true)
            if [[ "$ok_count" -ge 2 ]]; then
                log_info "Jumbo echo ${size}B: ${ok_count}/3 responses matched"
            else
                test_ok=false
                test_err="Jumbo echo ${size}B: only ${ok_count}/3 responses matched size"
                log_error "$test_err"
            fi
        fi

        end=$(_timer_now)
        elapsed=$(_timer_elapsed "$start" "$end")

        if [[ "$test_ok" == "true" ]]; then
            junit_add_pass "$test_name" "$CLASSNAME" "$elapsed"
        else
            junit_add_failure "$test_name" "$CLASSNAME" "$elapsed" "$test_err" \
                "Echo server did not correctly echo ${size}-byte jumbo payload"
        fi

        rm -rf /var/run/dpdk/ 2>/dev/null || true
    done

    junit_end_suite "$OUTPUT"
    log_info "Tier 4 tests complete: $OUTPUT"
}

# ── Main ─────────────────────────────────────────────────────────────────────

case "$ROLE" in
    listener)   run_listener ;;
    sender)     run_sender ;;
    *)
        echo "Unknown role: $ROLE (expected 'listener' or 'sender')" >&2
        exit 1
        ;;
esac
