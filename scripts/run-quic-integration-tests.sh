#!/usr/bin/env bash
# run-quic-integration-tests.sh - Orchestrator for QUIC EC2 integration tests
#
# Deploys two EC2 instances via CDK (DpdkTestStack), builds the QUIC binaries,
# runs a QUIC handshake + bidirectional throughput test between them,
# collects JUnit XML results, and optionally tears down.
#
# Usage:
#   ./scripts/run-quic-integration-tests.sh [--teardown] [--skip-deploy] [--json-summary]
#
# Exit codes:
#   0 = all tests passed
#   1 = one or more tests failed
#   2 = infrastructure/setup failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CDK_DIR="$REPO_ROOT/deploy/cdk"

# ── Configuration ────────────────────────────────────────────────────────────

SSM_READINESS_TIMEOUT=600
TEST_TIMEOUT=120
ENI_BIND_TIMEOUT=90
RESULTS_DIR="$REPO_ROOT/test-results"
RESULTS_REMOTE_DIR="/tmp/test-results"
CDK_STACK_NAME="${CDK_STACK_NAME:-DpdkTestStack}"
SSM_POLL_INTERVAL=15
LOGS_DIR="$REPO_ROOT/instance-logs"
FAILED_STEP=""

# ── CLI parsing ──────────────────────────────────────────────────────────────

FLAG_TEARDOWN=false
FLAG_SKIP_DEPLOY=false
FLAG_JSON_SUMMARY=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --teardown)      FLAG_TEARDOWN=true;      shift ;;
        --skip-deploy)   FLAG_SKIP_DEPLOY=true;   shift ;;
        --json-summary)  FLAG_JSON_SUMMARY=true;  shift ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done

# ── Logging ──────────────────────────────────────────────────────────────────

log_info() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] INFO: $*"; }
log_error() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ERROR: $*" >&2; }

# ── Failure handling ─────────────────────────────────────────────────────────

fail_with_logs() {
    local step="$1"
    local message="$2"
    FAILED_STEP="$step"
    log_error "[$step] $message"
    mkdir -p "$LOGS_DIR"
    cat > "$LOGS_DIR/failure-summary.json" <<EOF
{
  "failed_step": "$step",
  "error": "$message",
  "sender_instance_id": "${SENDER_INSTANCE_ID:-unknown}",
  "receiver_instance_id": "${RECEIVER_INSTANCE_ID:-unknown}",
  "run_url": "https://github.com/${GITHUB_REPOSITORY:-local}/actions/runs/${GITHUB_RUN_ID:-0}"
}
EOF
    exit 2
}

generate_failure_xml() {
    local test_name="$1"
    local message="$2"
    mkdir -p "$RESULTS_DIR"
    cat > "$RESULTS_DIR/${test_name}.xml" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="${test_name}" tests="1" failures="1" errors="0" time="0">
    <testcase name="infrastructure_setup" classname="tier1.${test_name}" time="0">
        <failure message="${message}" type="InfrastructureError">Setup failed before tests could run: ${message}</failure>
    </testcase>
</testsuite>
EOF
}

# ── State ────────────────────────────────────────────────────────────────────

SENDER_INSTANCE_ID=""
RECEIVER_INSTANCE_ID=""
SENDER_DPDK_ENI_IP=""
RECEIVER_DPDK_ENI_IP=""

# ── CDK deploy ───────────────────────────────────────────────────────────────

deploy_stack() {
    log_info "Deploying CDK stack: $CDK_STACK_NAME"
    cd "$CDK_DIR"

    local cdk_args=("--require-approval" "never")
    if [[ -n "${DPDK_AMI_ID:-}" ]]; then
        cdk_args+=("-c" "amiId=${DPDK_AMI_ID}")
    fi

    npx cdk deploy "$CDK_STACK_NAME" "${cdk_args[@]}" 2>&1 | tail -20

    # Extract outputs
    local cdk_outputs
    cdk_outputs=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs" --output json 2>/dev/null || echo "[]")

    SENDER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderInstanceId':
        print(o['OutputValue']); break
" 2>/dev/null || echo "")

    RECEIVER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverInstanceId':
        print(o['OutputValue']); break
" 2>/dev/null || echo "")

    SENDER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderDpdkEniPrivateIp':
        print(o['OutputValue']); break
" 2>/dev/null || echo "")

    RECEIVER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverDpdkEniPrivateIp':
        print(o['OutputValue']); break
" 2>/dev/null || echo "")

    if [[ -z "$SENDER_INSTANCE_ID" || -z "$RECEIVER_INSTANCE_ID" ]]; then
        fail_with_logs "deploy" "Could not extract instance IDs from CDK outputs"
    fi
    if [[ -z "$SENDER_DPDK_ENI_IP" || -z "$RECEIVER_DPDK_ENI_IP" ]]; then
        fail_with_logs "deploy" "Could not extract ENI IPs from CDK outputs"
    fi

    log_info "Sender:   $SENDER_INSTANCE_ID ($SENDER_DPDK_ENI_IP)"
    log_info "Receiver: $RECEIVER_INSTANCE_ID ($RECEIVER_DPDK_ENI_IP)"
    cd "$REPO_ROOT"
}

# ── Wait for SSM readiness ───────────────────────────────────────────────────

wait_for_ssm() {
    local instance_id="$1"
    local label="$2"
    log_info "Waiting for SSM readiness on $label ($instance_id)..."

    local waited=0
    while true; do
        local status
        status=$(aws ssm describe-instance-information \
            --filters "Key=InstanceIds,Values=$instance_id" \
            --query "InstanceInformationList[0].PingStatus" \
            --output text 2>/dev/null || echo "")
        if [[ "$status" == "Online" ]]; then
            log_info "$label SSM is online"
            return 0
        fi
        sleep "$SSM_POLL_INTERVAL"
        waited=$((waited + SSM_POLL_INTERVAL))
        if [[ $waited -ge $SSM_READINESS_TIMEOUT ]]; then
            fail_with_logs "ssm_readiness" "$label ($instance_id) did not become SSM-ready within ${SSM_READINESS_TIMEOUT}s"
        fi
    done
}

# ── SSM command execution ────────────────────────────────────────────────────

ssm_run() {
    local instance_id="$1"
    local command="$2"
    local timeout="${3:-120}"

    local cmd_id
    cmd_id=$(aws ssm send-command \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters "{\"commands\":[\"$command\"],\"executionTimeout\":[\"$timeout\"]}" \
        --timeout-seconds "$timeout" \
        --query "Command.CommandId" --output text 2>/dev/null)

    if [[ -z "$cmd_id" ]]; then
        return 1
    fi

    # Wait for completion
    local waited=0
    while true; do
        local status
        status=$(aws ssm get-command-invocation \
            --command-id "$cmd_id" \
            --instance-id "$instance_id" \
            --query "Status" --output text 2>/dev/null || echo "Pending")
        case "$status" in
            Success) return 0 ;;
            Failed|TimedOut|Cancelled)
                local output
                output=$(aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "[StandardOutputContent,StandardErrorContent]" \
                    --output text 2>/dev/null || echo "")
                log_error "SSM command failed ($status): $output"
                return 1
                ;;
        esac
        sleep 5
        waited=$((waited + 5))
        if [[ $waited -ge $((timeout + 30)) ]]; then
            return 1
        fi
    done
}

ssm_run_get_output() {
    local instance_id="$1"
    local command="$2"
    local timeout="${3:-120}"

    local cmd_id
    cmd_id=$(aws ssm send-command \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters "{\"commands\":[\"$command\"],\"executionTimeout\":[\"$timeout\"]}" \
        --timeout-seconds "$timeout" \
        --query "Command.CommandId" --output text 2>/dev/null)

    if [[ -z "$cmd_id" ]]; then
        return 1
    fi

    local waited=0
    while true; do
        local status
        status=$(aws ssm get-command-invocation \
            --command-id "$cmd_id" \
            --instance-id "$instance_id" \
            --query "Status" --output text 2>/dev/null || echo "Pending")
        case "$status" in
            Success)
                aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "StandardOutputContent" --output text 2>/dev/null
                return 0
                ;;
            Failed|TimedOut|Cancelled)
                return 1
                ;;
        esac
        sleep 5
        waited=$((waited + 5))
        if [[ $waited -ge $((timeout + 30)) ]]; then
            return 1
        fi
    done
}

# ── ENI configuration ────────────────────────────────────────────────────────

configure_eni() {
    local instance_id="$1"
    local action="$2"  # "bind" or "unbind"
    local eni_ip="$3"

    log_info "ENI $action on $instance_id for $eni_ip"
    local script_path="/opt/dpdk-stdlib/scripts/integration-tests/configure-eni.sh"
    ssm_run "$instance_id" "cd /opt/dpdk-stdlib && bash $script_path --$action --eni-ip $eni_ip" "$ENI_BIND_TIMEOUT"
}

# ── Collect logs ─────────────────────────────────────────────────────────────

collect_logs() {
    mkdir -p "$LOGS_DIR"

    for role_instance in "sender:$SENDER_INSTANCE_ID" "receiver:$RECEIVER_INSTANCE_ID"; do
        local role="${role_instance%%:*}"
        local iid="${role_instance##*:}"
        [[ -n "$iid" ]] || continue

        # QUIC app logs
        ssm_run_get_output "$iid" "cat /tmp/quic-echo-server.log 2>/dev/null || echo ''" 30 \
            > "$LOGS_DIR/${role}-quic-echo-server.log" 2>/dev/null || true
        ssm_run_get_output "$iid" "cat /tmp/quic-server-stdout.log 2>/dev/null || echo ''" 30 \
            > "$LOGS_DIR/${role}-quic-server-stdout.log" 2>/dev/null || true

        # Console output (survives instance termination)
        aws ec2 get-console-output --instance-id "$iid" \
            --query "Output" --output text > "$LOGS_DIR/${role}-console-output.log" 2>/dev/null || true
    done
}

# ── ARP warm ─────────────────────────────────────────────────────────────────

warm_arp_cache() {
    local instance_id="$1"
    local target_ip="$2"
    local subnet_gw
    subnet_gw=$(echo "$target_ip" | sed 's/\.[0-9]*$/.1/')
    ssm_run "$instance_id" "ping -c 2 -W 2 $subnet_gw >/dev/null 2>&1 || true; ping -c 2 -W 2 $target_ip >/dev/null 2>&1 || true; sleep 1" 30 || true
}

# ── Run QUIC integration test ────────────────────────────────────────────────

run_quic_test() {
    log_info "=== Running QUIC Integration Test ==="
    mkdir -p "$RESULTS_DIR"

    # Step 1: Bind ENIs
    log_info "Binding ENIs for DPDK..."
    if ! configure_eni "$SENDER_INSTANCE_ID" "bind" "$SENDER_DPDK_ENI_IP"; then
        generate_failure_xml "tier1-quic-handshake" "ENI bind failed on sender"
        fail_with_logs "eni_bind" "Failed to bind ENI on sender"
    fi
    if ! configure_eni "$RECEIVER_INSTANCE_ID" "bind" "$RECEIVER_DPDK_ENI_IP"; then
        generate_failure_xml "tier1-quic-handshake" "ENI bind failed on receiver"
        fail_with_logs "eni_bind" "Failed to bind ENI on receiver"
    fi

    # Step 2: Warm ARP caches
    log_info "Warming ARP caches..."
    warm_arp_cache "$SENDER_INSTANCE_ID" "$RECEIVER_DPDK_ENI_IP"
    warm_arp_cache "$RECEIVER_INSTANCE_ID" "$SENDER_DPDK_ENI_IP"

    # Step 3: Start QUIC echo server on receiver
    log_info "Starting QUIC echo server on receiver ($RECEIVER_DPDK_ENI_IP)..."
    local server_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier1-quic-handshake.sh --role server --bind-ip $RECEIVER_DPDK_ENI_IP --port 4433"
    # Start server in background (it blocks waiting for connections)
    local server_cmd_id
    server_cmd_id=$(aws ssm send-command \
        --instance-ids "$RECEIVER_INSTANCE_ID" \
        --document-name "AWS-RunShellScript" \
        --parameters "{\"commands\":[\"$server_cmd\"],\"executionTimeout\":[\"300\"]}" \
        --timeout-seconds 300 \
        --query "Command.CommandId" --output text 2>/dev/null)

    if [[ -z "$server_cmd_id" ]]; then
        generate_failure_xml "tier1-quic-handshake" "Failed to start QUIC server"
        fail_with_logs "server_start" "SSM command to start QUIC server failed"
    fi

    # Wait for server to be ready (check for cert file)
    log_info "Waiting for QUIC server to be ready..."
    sleep 10

    # Step 4: Transfer cert from receiver to sender
    log_info "Transferring server cert to sender..."
    local cert_content
    cert_content=$(ssm_run_get_output "$RECEIVER_INSTANCE_ID" "cat /tmp/quic-server-cert.pem 2>/dev/null" 30) || true

    if [[ -z "$cert_content" ]]; then
        # Retry after a few more seconds
        sleep 10
        cert_content=$(ssm_run_get_output "$RECEIVER_INSTANCE_ID" "cat /tmp/quic-server-cert.pem 2>/dev/null" 30) || true
    fi

    if [[ -z "$cert_content" ]]; then
        generate_failure_xml "tier1-quic-handshake" "Could not retrieve server certificate"
        fail_with_logs "cert_transfer" "Server cert not available after 20s"
    fi

    # Write cert to sender
    local escaped_cert
    escaped_cert=$(echo "$cert_content" | sed 's/"/\\"/g' | sed ':a;N;$!ba;s/\n/\\n/g')
    ssm_run "$SENDER_INSTANCE_ID" "printf '$escaped_cert' > /tmp/quic-server-cert.pem" 30 || {
        generate_failure_xml "tier1-quic-handshake" "Failed to write cert on sender"
        fail_with_logs "cert_transfer" "Could not write cert file on sender"
    }

    # Step 5: Run client tests on sender
    log_info "Running QUIC client tests on sender ($SENDER_DPDK_ENI_IP)..."
    local client_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier1-quic-handshake.sh --role client --bind-ip $SENDER_DPDK_ENI_IP --peer-ip $RECEIVER_DPDK_ENI_IP --port 4433 --output $RESULTS_REMOTE_DIR/tier1-quic-handshake.xml"

    if ! ssm_run "$SENDER_INSTANCE_ID" "$client_cmd" "$TEST_TIMEOUT"; then
        log_error "Client test execution failed"
        # Still try to collect results
    fi

    # Step 6: Collect test results from sender
    log_info "Collecting test results..."
    local result_xml
    result_xml=$(ssm_run_get_output "$SENDER_INSTANCE_ID" "cat $RESULTS_REMOTE_DIR/tier1-quic-handshake.xml 2>/dev/null || echo ''" 30) || true
    if [[ -n "$result_xml" && "$result_xml" != "" ]]; then
        echo "$result_xml" > "$RESULTS_DIR/tier1-quic-handshake.xml"
        log_info "Test results saved to $RESULTS_DIR/tier1-quic-handshake.xml"
    else
        generate_failure_xml "tier1-quic-handshake" "No test results collected from sender"
    fi

    # Kill the server
    ssm_run "$RECEIVER_INSTANCE_ID" "pkill -f quic-echo-server || true" 10 || true
}

# ── Teardown ─────────────────────────────────────────────────────────────────

teardown() {
    if [[ "$FLAG_TEARDOWN" == "true" ]]; then
        log_info "Tearing down CDK stack..."
        cd "$CDK_DIR"
        npx cdk destroy "$CDK_STACK_NAME" --force 2>&1 | tail -5 || true
        cd "$REPO_ROOT"
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

    # Deploy
    if [[ "$FLAG_SKIP_DEPLOY" != "true" ]]; then
        deploy_stack
    else
        # Read existing stack outputs
        log_info "Skipping deploy, reading existing stack outputs..."
        local cf_outputs
        cf_outputs=$(aws cloudformation describe-stacks \
            --stack-name "$CDK_STACK_NAME" \
            --query "Stacks[0].Outputs" --output json 2>/dev/null || echo "[]")
        SENDER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='SenderInstanceId']" 2>/dev/null || echo "")
        RECEIVER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='ReceiverInstanceId']" 2>/dev/null || echo "")
        SENDER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='SenderDpdkEniPrivateIp']" 2>/dev/null || echo "")
        RECEIVER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='ReceiverDpdkEniPrivateIp']" 2>/dev/null || echo "")
    fi

    # Wait for SSM
    wait_for_ssm "$SENDER_INSTANCE_ID" "sender"
    wait_for_ssm "$RECEIVER_INSTANCE_ID" "receiver"

    # Run test
    local test_exit=0
    run_quic_test || test_exit=$?

    # Collect logs
    collect_logs

    # JSON summary
    if [[ "$FLAG_JSON_SUMMARY" == "true" ]]; then
        local failures=0
        if [[ -f "$RESULTS_DIR/tier1-quic-handshake.xml" ]]; then
            failures=$(grep -oP 'failures="\K[^"]+' "$RESULTS_DIR/tier1-quic-handshake.xml" 2>/dev/null | head -1 || echo "0")
        fi
        cat > "$RESULTS_DIR/summary.json" <<EOF
{
  "suite": "quic-integration",
  "total": 2,
  "passed": $((2 - failures)),
  "failed": $failures,
  "exit_code": $test_exit
}
EOF
    fi

    # Teardown
    teardown

    # Exit with test result
    if [[ $test_exit -ne 0 ]]; then
        exit 1
    fi

    # Check for failures in JUnit XML
    if [[ -f "$RESULTS_DIR/tier1-quic-handshake.xml" ]]; then
        local failures
        failures=$(grep -oP 'failures="\K[^"]+' "$RESULTS_DIR/tier1-quic-handshake.xml" 2>/dev/null | head -1 || echo "0")
        if [[ "$failures" != "0" ]]; then
            log_error "Tests had $failures failure(s)"
            exit 1
        fi
    fi

    log_info "All QUIC integration tests passed"
}

trap teardown EXIT
main
