#!/usr/bin/env bash
# tier3-iperf-interop.sh - Tier 3: Cross-stack interoperability test harness
#
# Tests that dpdk-stdlib can interoperate across networking stacks:
#   - "our-app-sends": Instance A (dpdk-stdlib via DPDK) sends to Instance B (kernel echo server)
#   - "iperf-sends": Instance B (kernel test-client) sends to Instance A (dpdk-stdlib listener)
#
# This validates that DPDK packets can reach kernel sockets and vice versa
# across the VPC, verifying correct gateway MAC usage and packet format.
#
# Usage:
#   ./tier3-iperf-interop.sh --role <server|client> --direction <our-app-sends|iperf-sends> \
#       --local-ip <IP> --peer-ip <IP> --port 9000 --output /tmp/test-results/tier3-iperf-interop.xml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/harness-common.sh"

# ── Defaults ─────────────────────────────────────────────────────────────────

PROJECT_DIR="/opt/dpdk-stdlib"
ECHO_BINARY="$PROJECT_DIR/target/release/echo"
TEST_CLIENT_BINARY="$PROJECT_DIR/target/release/test-client"
ROLE=""
DIRECTION=""
LOCAL_IP=""
PEER_IP=""
PORT=9000
OUTPUT=""
TEST_TIMEOUT=60
CLASSNAME="tier3.cross_stack_interop"

# ── Argument parsing ─────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --role)       ROLE="$2";      shift 2 ;;
        --direction)  DIRECTION="$2"; shift 2 ;;
        --local-ip)   LOCAL_IP="$2";  shift 2 ;;
        --peer-ip)    PEER_IP="$2";   shift 2 ;;
        --port)       PORT="$2";      shift 2 ;;
        --output)     OUTPUT="$2";    shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 --role <server|client> --direction <our-app-sends|iperf-sends> --local-ip <IP> --peer-ip <IP> [--port PORT] [--output PATH]" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$ROLE" || -z "$DIRECTION" || -z "$LOCAL_IP" || -z "$PEER_IP" ]]; then
    echo "Missing required arguments" >&2
    echo "Usage: $0 --role <server|client> --direction <our-app-sends|iperf-sends> --local-ip <IP> --peer-ip <IP>" >&2
    exit 1
fi

if [[ -z "$OUTPUT" ]]; then
    OUTPUT=$(result_path "tier3" "iperf-interop")
fi

# ── Direction: our-app-sends ─────────────────────────────────────────────────
# Instance A (DPDK) sends UDP to Instance B (kernel networking).
# - Server role (Instance B): Start a Python UDP echo server on the kernel stack.
#   We use Python instead of iperf3 because iperf3 requires a TCP control
#   channel which DPDK doesn't support. Python's UDP socket is a real kernel
#   socket, validating true cross-stack interop.
# - Client role (Instance A): Run dpdk-stdlib test-client with --bind-ip (DPDK path).

run_our_app_sends_server() {
    log_info "Starting kernel UDP echo server on 0.0.0.0:${PORT} (our-app-sends direction)"

    # Diagnostic: verify kernel interface has the expected IP
    log_info "Receiver network state:"
    ip -4 addr show 2>/dev/null || true
    log_info "Receiver ARP table:"
    cat /proc/net/arp 2>/dev/null || true

    local echo_log="/tmp/iperf3-server.log"
    log_info "Kernel echo server output will be logged to: $echo_log"

    # Python UDP echo server — listens on all interfaces so it works
    # regardless of which kernel interface has the target IP.
    python3 -u -c "
import socket, sys, signal, time

signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))

s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', $PORT))
s.settimeout(5.0)
print(f'Kernel UDP echo server listening on 0.0.0.0:$PORT', flush=True)

count = 0
start = time.time()
while time.time() - start < 90:
    try:
        data, addr = s.recvfrom(1500)
        count += 1
        print(f'Received {len(data)} bytes from {addr}: {data[:80]}', flush=True)
        s.sendto(data, addr)
    except socket.timeout:
        continue
    except Exception as e:
        print(f'Error: {e}', flush=True)
        break

print(f'Kernel echo server finished after {count} packets', flush=True)
" > "$echo_log" 2>&1 &
    local pid=$!
    log_info "Kernel echo server started with PID $pid"

    # Wait for the test to complete (sender will drive the timing)
    local waited=0
    local max_wait=90
    while kill -0 "$pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    log_info "Kernel echo server finished"
}

run_our_app_sends_client() {
    # Instance A: run dpdk-stdlib sending UDP via DPDK to kernel echo server
    log_info "Sending UDP traffic from dpdk-stdlib (DPDK) to kernel echo server at ${PEER_IP}:${PORT}"

    # Capture test-client output to a log file
    local client_log="/tmp/test-client-iperf.log"
    log_info "Test client output will be logged to: $client_log"
    exec > >(tee -a "$client_log") 2>&1

    junit_start_suite "tier3-iperf-interop" 1

    # Give the kernel echo server time to start
    sleep 5

    # Pre-flight diagnostics: verify DPDK state and ARP cache
    log_info "Pre-flight: checking DPDK state and ARP cache..."
    log_info "Local IP: $LOCAL_IP, Peer IP: $PEER_IP, Port: $PORT"
    log_info "/proc/net/arp contents:"
    cat /proc/net/arp 2>/dev/null || true
    log_info "DPDK runtime state:"
    ls -la /var/run/dpdk/ 2>/dev/null || echo "No /var/run/dpdk/ directory"
    log_info "vfio-pci bindings:"
    ls /sys/bus/pci/drivers/vfio-pci/ 2>/dev/null || echo "No vfio-pci bindings"
    log_info "Test binary: $TEST_CLIENT_BINARY"
    ls -la "$TEST_CLIENT_BINARY" 2>/dev/null || echo "Binary not found!"

    local start end elapsed
    start=$(_timer_now)
    local test_ok=true
    local test_err=""
    local test_output=""

    # Clean DPDK runtime state so EAL can initialize fresh
    rm -rf /var/run/dpdk/ 2>/dev/null || true

    # Use test-client with --bind-ip to force DPDK path
    log_info "Launching test-client: $TEST_CLIENT_BINARY --target $PEER_IP --port $PORT --bind-ip $LOCAL_IP --count 10 --delay 200"
    test_output=$(run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" --bind-ip "$LOCAL_IP" \
        --message "dpdk-to-kernel-test-payload" \
        --count 10 --delay 200 2>&1) || {
        test_ok=false
        test_err="Failed to send UDP traffic from dpdk-stdlib to kernel echo server"
    }
    log_info "Test client output: $test_output"

    # Verify that packets were sent and responses received
    if [[ "$test_ok" == "true" ]]; then
        if echo "$test_output" | grep -q "Sent"; then
            local sent_count
            sent_count=$(echo "$test_output" | grep -c "Sent" || true)
            local recv_count
            recv_count=$(echo "$test_output" | grep -c "Received" || true)
            log_info "our-app-sends: sent $sent_count packets, received $recv_count responses"
            if [[ "$sent_count" -ge 5 ]]; then
                log_info "our-app-sends: PASS (sent >= 5 packets)"
            else
                test_ok=false
                test_err="Only sent $sent_count/10 packets (need >= 5)"
            fi
        else
            test_ok=false
            test_err="No packets were sent to kernel echo server"
        fi
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$test_ok" == "true" ]]; then
        junit_add_pass "our_app_sends" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "our_app_sends" "$CLASSNAME" "$elapsed" "$test_err" "$test_output"
    fi

    junit_end_suite
    junit_write "$OUTPUT"
    log_info "our-app-sends test complete"
}

# ── Direction: iperf-sends ───────────────────────────────────────────────────
# Instance B (kernel networking) sends UDP to Instance A (dpdk-stdlib listener).
# - Server role (Instance A): start dpdk-stdlib echo server (DPDK path)
# - Client role (Instance B): run test-client WITHOUT --bind-ip so it uses
#   the tokio/kernel fallback, testing kernel→DPDK interoperability.

run_iperf_sends_server() {
    # Instance A: run dpdk-stdlib echo server as listener (DPDK path)
    log_info "Starting dpdk-stdlib listener on ${LOCAL_IP}:${PORT} (iperf-sends direction)"

    # Enable coredumps for this shell and its children
    ulimit -c unlimited 2>/dev/null || true

    local echo_log="/tmp/echo-server.log"
    "$ECHO_BINARY" --ip "$LOCAL_IP" --port "$PORT" > "$echo_log" 2>&1 &
    local echo_pid=$!
    log_info "Echo server started with PID $echo_pid"

    sleep 3
    if ! kill -0 "$echo_pid" 2>/dev/null; then
        log_error "Echo server exited prematurely"
        if check_process_crash "$echo_pid" "echo"; then
            log_error "Echo server CRASHED during startup (see crash report above)"
        fi
        return 1
    fi

    # Wait for the test to complete
    local waited=0
    local max_wait=90
    while kill -0 "$echo_pid" 2>/dev/null && [[ $waited -lt $max_wait ]]; do
        sleep 5
        waited=$(( waited + 5 ))
    done

    # Check if the echo server crashed during the test (vs. being killed normally)
    if ! kill -0 "$echo_pid" 2>/dev/null; then
        check_process_crash "$echo_pid" "echo" || true
    else
        kill "$echo_pid" 2>/dev/null || true
        wait "$echo_pid" 2>/dev/null || true
    fi
    log_info "Echo server finished"
}

run_iperf_sends_client() {
    # Instance B: send UDP from kernel stack to dpdk-stdlib listener
    log_info "Sending kernel UDP traffic to dpdk-stdlib listener at ${PEER_IP}:${PORT}"

    # Capture output
    local client_log="/tmp/test-client-iperf.log"
    exec > >(tee -a "$client_log") 2>&1

    junit_start_suite "tier3-iperf-interop" 1

    # Give the dpdk-stdlib listener time to start
    sleep 5

    local start end elapsed
    start=$(_timer_now)
    local test_ok=true
    local test_err=""
    local test_output=""

    # Run test-client WITHOUT --bind-ip so it falls back to tokio (kernel).
    # This tests the kernel → DPDK path.
    test_output=$(run_with_timeout "$TEST_TIMEOUT" "$TEST_CLIENT_BINARY" \
        --target "$PEER_IP" --port "$PORT" \
        --message "kernel-to-dpdk-test-payload" \
        --count 10 --delay 200 2>&1) || {
        test_ok=false
        test_err="Failed to send kernel UDP traffic to dpdk-stdlib listener"
    }

    # Verify packets were sent and check for responses
    if [[ "$test_ok" == "true" ]]; then
        if echo "$test_output" | grep -q "Sent"; then
            local sent_count
            sent_count=$(echo "$test_output" | grep -c "Sent" || true)
            local recv_count
            recv_count=$(echo "$test_output" | grep -c "Received" || true)
            log_info "iperf-sends: sent $sent_count packets, received $recv_count responses"
            if [[ "$sent_count" -ge 5 ]]; then
                log_info "iperf-sends: PASS (sent >= 5 packets)"
            else
                test_ok=false
                test_err="Only sent $sent_count/10 packets (need >= 5)"
            fi
        else
            test_ok=false
            test_err="No packets were sent to dpdk-stdlib listener"
        fi
    fi

    end=$(_timer_now)
    elapsed=$(_timer_elapsed "$start" "$end")

    if [[ "$test_ok" == "true" ]]; then
        junit_add_pass "iperf_sends" "$CLASSNAME" "$elapsed"
    else
        junit_add_failure "iperf_sends" "$CLASSNAME" "$elapsed" "$test_err" "$test_output"
    fi

    junit_end_suite
    junit_write "$OUTPUT"
    log_info "iperf-sends test complete"
}

# ── Main dispatch ────────────────────────────────────────────────────────────

case "${DIRECTION}:${ROLE}" in
    our-app-sends:server)
        run_our_app_sends_server
        ;;
    our-app-sends:client)
        run_our_app_sends_client
        ;;
    iperf-sends:server)
        run_iperf_sends_server
        ;;
    iperf-sends:client)
        run_iperf_sends_client
        ;;
    *)
        echo "Invalid direction:role combination: ${DIRECTION}:${ROLE}" >&2
        echo "Valid combinations:" >&2
        echo "  --direction our-app-sends --role server   (Instance B: kernel echo server)" >&2
        echo "  --direction our-app-sends --role client   (Instance A: dpdk-stdlib sender)" >&2
        echo "  --direction iperf-sends   --role server   (Instance A: dpdk-stdlib listener)" >&2
        echo "  --direction iperf-sends   --role client   (Instance B: kernel test-client)" >&2
        exit 1
        ;;
esac
