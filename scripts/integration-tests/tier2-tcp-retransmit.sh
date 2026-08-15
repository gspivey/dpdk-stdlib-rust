#!/usr/bin/env bash
# tier2-tcp-retransmit.sh - Tier 2: TCP retransmission under packet loss
#
# Tests TCP retransmission behavior by injecting packet loss via tc/netem on
# the DPDK ENI's underlying interface. Verifies:
#   - Complete data delivery despite loss
#   - Retransmission occurs (transfer succeeds under conditions that would
#     fail without retransmission)
#   - Recovery time is bounded by a reasonable multiple of RTT
#
# The server runs tcp-echo on DPDK. The client applies tc netem loss before
# running the test, then removes it after.
#
# Usage:
#   # On Instance B (server):
#   ./tier2-tcp-retransmit.sh --role server --bind-ip 10.0.1.100 --port 9000 \
#       --gateway-mac AA:BB:CC:DD:EE:FF
#
#   # On Instance A (client):
#   ./tier2-tcp-retransmit.sh --role client --bind-ip 10.0.1.50 --peer-ip 10.0.1.100 \
#       --port 9000 --output /tmp/test-results/tier2-tcp-retransmit.xml \
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
CLASSNAME="tier2.tcp_retransmit"

# Loss parameters
LOSS_PERCENT=10       # Percentage of packets to drop
LOSS_INTERFACE=""     # Auto-detected from routing table

# ── Argument parsing ─────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --role)        ROLE="$2";        shift 2 ;;
        --bind-ip)     BIND_IP="$2";     shift 2 ;;
        --peer-ip)     PEER_IP="$2";     shift 2 ;;
        --port)        PORT="$2";        shift 2 ;;
        --output)      OUTPUT="$2";      shift 2 ;;
        --gateway-mac) GATEWAY_MAC="$2"; shift 2 ;;
        --loss-percent) LOSS_PERCENT="$2"; shift 2 ;;
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
    OUTPUT=$(result_path "tier2" "tcp-retransmit")
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

# ── Loss injection helpers ───────────────────────────────────────────────────

# Detect the network interface used to reach the peer IP.
detect_loss_interface() {
    if [[ -n "$LOSS_INTERFACE" ]]; then
        return
    fi
    # Use ip route to find the output interface for the peer
    LOSS_INTERFACE=$(ip route get "$PEER_IP" 2>/dev/null | grep -oP 'dev \K\S+' | head -1)
    if [[ -z "$LOSS_INTERFACE" ]]; then
        # Fallback: use the interface associated with BIND_IP
        LOSS_INTERFACE=$(ip -o addr show | grep "$BIND_IP" | awk '{print $2}' | head -1)
    fi
    if [[ -z "$LOSS_INTERFACE" ]]; then
        # Last resort: use eth0
        LOSS_INTERFACE="eth0"
    fi
    log_info "Loss injection interface: $LOSS_INTERFACE"
}

# Add packet loss via tc netem
inject_loss() {
    local percent="$1"
    log_info "Injecting ${percent}% packet loss on $LOSS_INTERFACE"
    # Remove any existing qdisc first (ignore errors if none exists)
    tc qdisc del dev "$LOSS_INTERFACE" root 2>/dev/null || true
    tc qdisc add dev "$LOSS_INTERFACE" root netem loss "${percent}%"
    log_info "Loss injection active: ${percent}% on $LOSS_INTERFACE"
}

# Remove packet loss
remove_loss() {
    log_info "Removing packet loss from $LOSS_INTERFACE"
    tc qdisc del dev "$LOSS_INTERFACE" root 2>/dev/null || true
    log_info "Loss injection removed"
}

# Ensure loss is cleaned up on exit
cleanup_loss() {
    remove_loss 2>/dev/null || true
}

# ── Server role ──────────────────────────────────────────────────────────────

run_server() {
    log_info "Starting TCP echo server (retransmit test) on ${BIND_IP}:${PORT}"
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
    log_info "Starting TCP retransmission tests against ${PEER_IP}:${PORT}"
    discover_gateway_mac
    detect_loss_interface

    # Set up cleanup trap
    trap cleanup_loss EXIT

    mkdir -p "$(dirname "$OUTPUT")"
    junit_start_suite "tier2-tcp-retransmit" 3

    # Give the server time to start
    sleep 5

    # ── Test 1: Baseline — data transfer without loss ────────────────────
    # This establishes that our test setup works before adding loss.
    log_info "Test 1: baseline_no_loss (bidir 512B x 20, no loss)"
    local test_start test_end elapsed
    test_start=$(_timer_now)
    local baseline_output=""
    local baseline_ok=true

    baseline_output=$(run_with_timeout "$TEST_TIMEOUT" \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 20 --payload-size 512 2>&1) || {
        baseline_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")
    local baseline_elapsed="$elapsed"

    if [[ "$baseline_ok" == "true" ]] && echo "$baseline_output" | grep -q "round-trips\|rtt/s"; then
        log_info "PASS: Baseline transfer succeeded"
        junit_add_pass "baseline_no_loss" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Baseline transfer failed (test infra problem)"
        log_error "Output: $baseline_output"
        junit_add_failure "baseline_no_loss" "$CLASSNAME" "$elapsed" \
            "Baseline transfer failed without loss — test infrastructure issue" "$baseline_output"
    fi

    # ── Test 2: Transfer under ${LOSS_PERCENT}% packet loss ──────────────
    log_info "Test 2: transfer_under_loss (bidir 512B x 20, ${LOSS_PERCENT}% loss)"
    inject_loss "$LOSS_PERCENT"
    sleep 1  # Let netem settle

    test_start=$(_timer_now)
    local loss_output=""
    local loss_ok=true

    # Use a longer timeout since retransmissions add latency
    loss_output=$(run_with_timeout 90 \
        "$TCP_CLIENT_BINARY" --target "$PEER_IP" --port "$PORT" \
        --local-ip "$BIND_IP" --gateway-mac "$GATEWAY_MAC" \
        --mode bidir --count 20 --payload-size 512 2>&1) || {
        loss_ok=false
    }
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")
    local loss_elapsed="$elapsed"

    remove_loss

    if [[ "$loss_ok" == "true" ]] && echo "$loss_output" | grep -q "round-trips\|rtt/s"; then
        log_info "PASS: Transfer succeeded despite ${LOSS_PERCENT}% packet loss (retransmission worked)"
        junit_add_pass "transfer_under_loss" "$CLASSNAME" "$elapsed"
    else
        log_error "FAIL: Transfer failed under ${LOSS_PERCENT}% loss (retransmission may be broken)"
        log_error "Output: $loss_output"
        junit_add_failure "transfer_under_loss" "$CLASSNAME" "$elapsed" \
            "Transfer failed under ${LOSS_PERCENT}% packet loss" "$loss_output"
    fi

    # ── Test 3: Recovery time bounded ────────────────────────────────────
    # Verify that the lossy transfer didn't take unreasonably long.
    # With 10% loss on a VPC (< 1ms RTT), recovery should complete within
    # 10x the baseline time. This bounds retransmission behavior.
    log_info "Test 3: bounded_recovery_time"
    test_start=$(_timer_now)
    local bounded_ok=true
    local bounded_err=""

    if [[ "$baseline_ok" == "true" && "$loss_ok" == "true" ]]; then
        # Compare elapsed times. Allow up to 10x slowdown from loss+retransmit.
        local max_allowed
        max_allowed=$(awk "BEGIN {printf \"%.3f\", $baseline_elapsed * 10 + 5}")
        if awk "BEGIN {exit ($loss_elapsed <= $max_allowed) ? 0 : 1}"; then
            log_info "PASS: Recovery time bounded (baseline=${baseline_elapsed}s, loss=${loss_elapsed}s, max=${max_allowed}s)"
        else
            bounded_ok=false
            bounded_err="Recovery time exceeded bound: loss=${loss_elapsed}s > max=${max_allowed}s (10x baseline ${baseline_elapsed}s + 5s)"
            log_error "FAIL: $bounded_err"
        fi
    elif [[ "$baseline_ok" != "true" ]]; then
        bounded_ok=false
        bounded_err="Cannot assess recovery time — baseline test failed"
    else
        bounded_ok=false
        bounded_err="Cannot assess recovery time — lossy transfer failed entirely"
    fi
    test_end=$(_timer_now)
    elapsed=$(_timer_elapsed "$test_start" "$test_end")

    if [[ "$bounded_ok" == "true" ]]; then
        junit_add_pass "bounded_recovery_time" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "bounded_recovery_time" "$CLASSNAME" "$elapsed" \
            "$bounded_err" "baseline=${baseline_elapsed}s loss=${loss_elapsed}s"
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
