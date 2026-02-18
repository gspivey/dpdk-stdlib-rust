#!/usr/bin/env bash
# run-integration-tests.sh - Orchestrator for EC2 integration tests
#
# Drives the full lifecycle: deploy, wait for readiness, configure ENIs,
# run test tiers, collect JUnit XML results, optionally teardown.
#
# Usage:
#   ./scripts/run-integration-tests.sh <AWS_PROFILE> [--teardown] [--skip-deploy] [--tier 1|3] [--json-summary]
#
# Exit codes:
#   0 = all tests passed
#   1 = one or more tests failed
#   2 = infrastructure/setup failure

set -euo pipefail

# ── Repository root detection ────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CDK_DIR="$REPO_ROOT/deploy/cdk"

# ── Configuration constants ──────────────────────────────────────────────────

SSM_READINESS_TIMEOUT=600    # 10 minutes to wait for SSM
TEST_TIMEOUT=120             # 2 minutes per test scenario
ENI_BIND_TIMEOUT=30          # 30 seconds for ENI bind/unbind
RESULTS_DIR="$REPO_ROOT/test-results"
RESULTS_REMOTE_DIR="/tmp/test-results"
CDK_STACK_NAME="DpdkTestStack"
SSM_POLL_INTERVAL=15         # seconds between SSM readiness polls
LOGS_DIR="$REPO_ROOT/instance-logs"
FAILED_STEP=""               # Set by fail_with_logs; written to step summary / failure JSON

# ── CLI argument parsing ─────────────────────────────────────────────────────

AWS_PROFILE=""
FLAG_TEARDOWN=false
FLAG_SKIP_DEPLOY=false
FLAG_JSON_SUMMARY=false
TIER_FILTER=""  # empty = run all tiers

usage() {
    cat <<EOF
Usage: $0 <AWS_PROFILE> [OPTIONS]

Orchestrates EC2 integration tests for dpdk-stdlib-rust.

Arguments:
  AWS_PROFILE           AWS CLI profile to use for deployment and SSM

Options:
  --teardown            Destroy AWS infrastructure after tests complete
  --skip-deploy         Skip CDK deployment (use existing infrastructure)
  --tier <1|3>          Run only the specified test tier
  --json-summary        Generate test-results/summary.json for agent consumption
  -h, --help            Show this help message

Exit codes:
  0  All tests passed
  1  One or more tests failed
  2  Infrastructure/setup failure
EOF
}

if [[ $# -lt 1 ]]; then
    usage
    exit 2
fi

# First positional argument is AWS_PROFILE
AWS_PROFILE="$1"
shift

while [[ $# -gt 0 ]]; do
    case "$1" in
        --teardown)      FLAG_TEARDOWN=true;      shift ;;
        --skip-deploy)   FLAG_SKIP_DEPLOY=true;   shift ;;
        --json-summary)  FLAG_JSON_SUMMARY=true;  shift ;;
        --tier)
            TIER_FILTER="$2"
            if [[ "$TIER_FILTER" != "1" && "$TIER_FILTER" != "3" ]]; then
                echo "ERROR: --tier must be 1 or 3, got: $TIER_FILTER" >&2
                exit 2
            fi
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage
            exit 2
            ;;
    esac
done

if [[ -z "$AWS_PROFILE" ]]; then
    echo "ERROR: AWS_PROFILE argument is required" >&2
    usage
    exit 2
fi

export AWS_PROFILE

# ── Logging helpers ──────────────────────────────────────────────────────────

log_info() {
    echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] INFO: $*"
}

log_error() {
    echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ERROR: $*" >&2
}

log_section() {
    echo ""
    echo "================================================================"
    echo "  $*"
    echo "================================================================"
    echo ""
}

# ── Stack output variables (populated after deploy) ──────────────────────────

SENDER_INSTANCE_ID=""
RECEIVER_INSTANCE_ID=""
SENDER_DPDK_ENI_ID=""
RECEIVER_DPDK_ENI_ID=""
SENDER_DPDK_ENI_IP=""
RECEIVER_DPDK_ENI_IP=""

# Track test results
TEST_EXIT_CODE=0

# ── Infrastructure deployment ────────────────────────────────────────────────

deploy_infrastructure() {
    log_section "Deploying infrastructure"

    cd "$CDK_DIR"

    # Pass pre-built AMI ID to CDK if available
    local cdk_context_args=""
    if [[ -n "${DPDK_AMI_ID:-}" ]]; then
        cdk_context_args="-c amiId=${DPDK_AMI_ID}"
        log_info "Using pre-built DPDK AMI: $DPDK_AMI_ID"
    else
        log_info "No pre-built AMI specified, using stock AL2023 with full bootstrap"
    fi

    log_info "Running cdk deploy..."
    if ! npx cdk deploy "$CDK_STACK_NAME" \
        --require-approval never \
        --outputs-file /tmp/cdk-outputs.json \
        $cdk_context_args 2>&1; then
        log_error "CDK deployment failed"
        return 1
    fi

    log_info "CDK deployment complete"
    cd "$REPO_ROOT"
}

fetch_stack_outputs() {
    log_info "Fetching stack outputs..."

    # ── Attempt 1: CDK outputs file written by cdk deploy ────────────────────
    # CDK writes this file when --outputs-file is passed. However, when CDK
    # cannot assume the deploy role and "proceeds anyway", it may write an
    # empty JSON object ({}) even though the stack has real outputs.
    if [[ -f /tmp/cdk-outputs.json ]]; then
        local cdk_outputs
        cdk_outputs=$(cat /tmp/cdk-outputs.json)
        SENDER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('SenderInstanceId', ''))
" 2>/dev/null || true)
        RECEIVER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('ReceiverInstanceId', ''))
" 2>/dev/null || true)
        SENDER_DPDK_ENI_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('SenderDpdkEniId', ''))
" 2>/dev/null || true)
        RECEIVER_DPDK_ENI_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('ReceiverDpdkEniId', ''))
" 2>/dev/null || true)
        SENDER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('SenderDpdkEniPrivateIp', ''))
" 2>/dev/null || true)
        RECEIVER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('ReceiverDpdkEniPrivateIp', ''))
" 2>/dev/null || true)

        if [[ -n "$SENDER_INSTANCE_ID" && -n "$RECEIVER_INSTANCE_ID" \
              && -n "$SENDER_DPDK_ENI_IP" && -n "$RECEIVER_DPDK_ENI_IP" ]]; then
            log_info "Stack outputs (from cdk-outputs.json):"
            log_info "  Sender Instance:    $SENDER_INSTANCE_ID"
            log_info "  Receiver Instance:  $RECEIVER_INSTANCE_ID"
            log_info "  Sender ENI:         $SENDER_DPDK_ENI_ID"
            log_info "  Receiver ENI:       $RECEIVER_DPDK_ENI_ID"
            log_info "  Sender ENI IP:      $SENDER_DPDK_ENI_IP"
            log_info "  Receiver ENI IP:    $RECEIVER_DPDK_ENI_IP"
            return 0
        fi

        log_info "CDK outputs file incomplete (CDK may not have had deploy-role access) — falling back to CloudFormation"
    fi

    # ── Attempt 2: CloudFormation describe-stacks ────────────────────────────
    # Authoritative source of truth. Used when the CDK outputs file is absent
    # or incomplete (e.g., CDK ran without the deploy role and wrote {}).
    local cf_outputs
    cf_outputs=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs" \
        --output json 2>/dev/null || true)

    if [[ -z "$cf_outputs" || "$cf_outputs" == "null" ]]; then
        log_error "CloudFormation describe-stacks returned no outputs for $CDK_STACK_NAME"
        log_error "  Stack may not exist or may be in a failed state."
        return 1
    fi

    SENDER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderInstanceId': print(o['OutputValue'])
" 2>/dev/null || true)
    RECEIVER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverInstanceId': print(o['OutputValue'])
" 2>/dev/null || true)
    SENDER_DPDK_ENI_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderDpdkEniId': print(o['OutputValue'])
" 2>/dev/null || true)
    RECEIVER_DPDK_ENI_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverDpdkEniId': print(o['OutputValue'])
" 2>/dev/null || true)
    SENDER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderDpdkEniPrivateIp': print(o['OutputValue'])
" 2>/dev/null || true)
    RECEIVER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverDpdkEniPrivateIp': print(o['OutputValue'])
" 2>/dev/null || true)

    log_info "Stack outputs (from CloudFormation describe-stacks):"
    log_info "  Sender Instance:    $SENDER_INSTANCE_ID"
    log_info "  Receiver Instance:  $RECEIVER_INSTANCE_ID"
    log_info "  Sender ENI:         $SENDER_DPDK_ENI_ID"
    log_info "  Receiver ENI:       $RECEIVER_DPDK_ENI_ID"
    log_info "  Sender ENI IP:      $SENDER_DPDK_ENI_IP"
    log_info "  Receiver ENI IP:    $RECEIVER_DPDK_ENI_IP"

    # ── Final validation ─────────────────────────────────────────────────────
    if [[ -z "$SENDER_INSTANCE_ID" || -z "$RECEIVER_INSTANCE_ID" ]]; then
        log_error "Missing required stack outputs (instance IDs)"
        return 1
    fi
    if [[ -z "$SENDER_DPDK_ENI_IP" || -z "$RECEIVER_DPDK_ENI_IP" ]]; then
        log_error "Missing required stack outputs (ENI private IPs)"
        return 1
    fi
}

# ── SSM readiness ────────────────────────────────────────────────────────────

wait_for_ssm_readiness() {
    log_section "Waiting for SSM readiness"

    local elapsed=0
    while [[ $elapsed -lt $SSM_READINESS_TIMEOUT ]]; do
        local ready_count=0

        # Check if both instances are registered with SSM
        local ssm_info
        ssm_info=$(aws ssm describe-instance-information \
            --filters "Key=InstanceIds,Values=${SENDER_INSTANCE_ID},${RECEIVER_INSTANCE_ID}" \
            --query "InstanceInformationList[].InstanceId" \
            --output text 2>/dev/null || true)

        for id in $SENDER_INSTANCE_ID $RECEIVER_INSTANCE_ID; do
            if echo "$ssm_info" | grep -q "$id"; then
                ready_count=$((ready_count + 1))
            fi
        done

        if [[ $ready_count -ge 2 ]]; then
            log_info "Both instances are SSM-ready"
            return 0
        fi

        log_info "Waiting for SSM readiness... ($ready_count/2 ready, ${elapsed}s elapsed)"
        sleep "$SSM_POLL_INTERVAL"
        elapsed=$((elapsed + SSM_POLL_INTERVAL))
    done

    log_error "SSM readiness timeout after ${SSM_READINESS_TIMEOUT}s"
    return 1
}

verify_build() {
    log_info "Verifying project build on instances..."

    for instance_id in "$SENDER_INSTANCE_ID" "$RECEIVER_INSTANCE_ID"; do
        log_info "Checking build on $instance_id..."

        local cmd_id
        cmd_id=$(ssm_run_command_async "$instance_id" 30 \
            "test -f /opt/dpdk-stdlib/target/release/echo && echo BUILD_OK || echo BUILD_MISSING")

        if [[ -z "$cmd_id" ]]; then
            log_error "Failed to send build verification command to $instance_id"
            return 1
        fi

        # Poll for completion rather than fixed sleep
        if ! ssm_wait_command "$instance_id" "$cmd_id" 60; then
            log_error "Build verification command timed out on $instance_id"
            return 1
        fi

        local result
        result=$(ssm_get_stdout "$instance_id" "$cmd_id")

        if echo "$result" | grep -q "BUILD_OK"; then
            log_info "Build verified on $instance_id"
        else
            log_error "Build not found on $instance_id (output: $result)"
            return 1
        fi
    done
}

# ── SSM command execution helpers ────────────────────────────────────────────

# Run a command on an instance via SSM and wait for completion.
# Usage: ssm_run_command <instance_id> <timeout_seconds> <command_string>
# Returns: exit code of the remote command
ssm_run_command() {
    local instance_id="$1"
    local timeout_secs="$2"
    local command_str="$3"

    local cmd_id
    cmd_id=$(aws ssm send-command \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters "commands=[\"$command_str\"]" \
        --timeout-seconds "$timeout_secs" \
        --query "Command.CommandId" \
        --output text 2>/dev/null)

    if [[ -z "$cmd_id" ]]; then
        log_error "Failed to send SSM command to $instance_id"
        return 1
    fi

    # Poll for completion
    local elapsed=0
    local status=""
    while [[ $elapsed -lt $timeout_secs ]]; do
        sleep 5
        elapsed=$((elapsed + 5))

        status=$(aws ssm get-command-invocation \
            --command-id "$cmd_id" \
            --instance-id "$instance_id" \
            --query "Status" \
            --output text 2>/dev/null || echo "Pending")

        case "$status" in
            Success)
                return 0
                ;;
            Failed|TimedOut|Cancelled)
                log_error "SSM command $status on $instance_id (cmd: $cmd_id)"
                # Fetch stderr for diagnostics
                aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "StandardErrorContent" \
                    --output text 2>/dev/null || true
                return 1
                ;;
        esac
    done

    log_error "SSM command timed out on $instance_id after ${timeout_secs}s"
    return 1
}

# Run a command on an instance via SSM in the background (non-blocking).
# Usage: ssm_run_command_async <instance_id> <timeout_seconds> <command_string>
# Prints the command ID to stdout.
ssm_run_command_async() {
    local instance_id="$1"
    local timeout_secs="$2"
    local command_str="$3"

    aws ssm send-command \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters "commands=[\"$command_str\"]" \
        --timeout-seconds "$timeout_secs" \
        --query "Command.CommandId" \
        --output text 2>/dev/null
}

# Wait for an async SSM command to complete.
# Usage: ssm_wait_command <instance_id> <command_id> <timeout_seconds>
ssm_wait_command() {
    local instance_id="$1"
    local cmd_id="$2"
    local timeout_secs="$3"

    local elapsed=0
    while [[ $elapsed -lt $timeout_secs ]]; do
        sleep 5
        elapsed=$((elapsed + 5))

        local status
        status=$(aws ssm get-command-invocation \
            --command-id "$cmd_id" \
            --instance-id "$instance_id" \
            --query "Status" \
            --output text 2>/dev/null || echo "Pending")

        case "$status" in
            Success)  return 0 ;;
            Failed|TimedOut|Cancelled)
                log_error "SSM command $status on $instance_id"
                return 1
                ;;
        esac
    done

    log_error "SSM command timed out waiting for $cmd_id on $instance_id"
    return 1
}

# Retrieve stdout from a completed SSM command.
ssm_get_stdout() {
    local instance_id="$1"
    local cmd_id="$2"

    aws ssm get-command-invocation \
        --command-id "$cmd_id" \
        --instance-id "$instance_id" \
        --query "StandardOutputContent" \
        --output text 2>/dev/null
}

# ── ENI configuration ───────────────────────────────────────────────────────

configure_eni() {
    local instance_id="$1"
    local action="$2"  # bind or unbind

    log_info "ENI $action on $instance_id"
    ssm_run_command "$instance_id" "$ENI_BIND_TIMEOUT" \
        "cd /opt/dpdk-stdlib && bash scripts/integration-tests/configure-eni.sh --action $action"
}

# ── Tier execution ───────────────────────────────────────────────────────────

run_tier1() {
    log_section "Tier 1: DPDK <-> DPDK echo test"

    # Bind ENIs on both instances
    log_info "Binding ENIs for Tier 1..."
    if ! configure_eni "$SENDER_INSTANCE_ID" "bind"; then
        log_error "Failed to bind ENI on sender"
        generate_failure_xml "tier1-dpdk-echo" "ENI bind failed on sender instance"
        return 1
    fi
    if ! configure_eni "$RECEIVER_INSTANCE_ID" "bind"; then
        log_error "Failed to bind ENI on receiver"
        generate_failure_xml "tier1-dpdk-echo" "ENI bind failed on receiver instance"
        return 1
    fi

    # Start listener on receiver (Instance B) in background
    log_info "Starting listener on receiver..."
    local listener_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier1-dpdk-echo.sh --role listener --bind-ip $RECEIVER_DPDK_ENI_IP --port 9000"
    local listener_cmd_id
    listener_cmd_id=$(ssm_run_command_async "$RECEIVER_INSTANCE_ID" "$TEST_TIMEOUT" "$listener_cmd")

    if [[ -z "$listener_cmd_id" ]]; then
        log_error "Failed to start listener"
        generate_failure_xml "tier1-dpdk-echo" "Failed to start listener on receiver"
        return 1
    fi

    # Give listener time to start
    sleep 10

    # Run sender on sender (Instance A) - this produces the JUnit XML
    log_info "Starting sender on sender..."
    local sender_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier1-dpdk-echo.sh --role sender --bind-ip $SENDER_DPDK_ENI_IP --peer-ip $RECEIVER_DPDK_ENI_IP --port 9000 --output $RESULTS_REMOTE_DIR/tier1-dpdk-echo.xml"
    if ! ssm_run_command "$SENDER_INSTANCE_ID" "$TEST_TIMEOUT" "$sender_cmd"; then
        log_error "Sender test execution failed"
        generate_failure_xml "tier1-dpdk-echo" "Sender test execution failed or timed out"
    fi

    # Wait for listener to finish (or time out)
    ssm_wait_command "$RECEIVER_INSTANCE_ID" "$listener_cmd_id" 30 || true

    log_info "Tier 1 execution complete"
}

run_tier3() {
    log_section "Tier 3: DPDK <-> iperf3 interoperability test"

    # Bind ENI on Instance A only; Instance B uses kernel networking
    log_info "Configuring ENIs for Tier 3..."
    if ! configure_eni "$SENDER_INSTANCE_ID" "bind"; then
        log_error "Failed to bind ENI on sender"
        generate_failure_xml "tier3-iperf-interop" "ENI bind failed on sender instance"
        return 1
    fi
    # Ensure receiver ENI is unbound (kernel networking for iperf3)
    configure_eni "$RECEIVER_INSTANCE_ID" "unbind" || true

    # ── Direction 1: our-app-sends ───────────────────────────────────────
    log_info "Direction 1: our-app-sends (dpdk-stdlib -> iperf3)"

    # Start iperf3 server on Instance B
    local iperf_server_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier3-iperf-interop.sh --role server --direction our-app-sends --local-ip $RECEIVER_DPDK_ENI_IP --peer-ip $SENDER_DPDK_ENI_IP --port 9000"
    local iperf_server_cmd_id
    iperf_server_cmd_id=$(ssm_run_command_async "$RECEIVER_INSTANCE_ID" "$TEST_TIMEOUT" "$iperf_server_cmd")

    sleep 10

    # Run dpdk-stdlib sender on Instance A
    local sender_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier3-iperf-interop.sh --role client --direction our-app-sends --local-ip $SENDER_DPDK_ENI_IP --peer-ip $RECEIVER_DPDK_ENI_IP --port 9000 --output $RESULTS_REMOTE_DIR/tier3-our-app-sends.xml"
    if ! ssm_run_command "$SENDER_INSTANCE_ID" "$TEST_TIMEOUT" "$sender_cmd"; then
        log_error "our-app-sends test execution failed"
        generate_failure_xml "tier3-our-app-sends" "our-app-sends test execution failed or timed out"
    fi

    ssm_wait_command "$RECEIVER_INSTANCE_ID" "$iperf_server_cmd_id" 30 || true

    # ── Direction 2: iperf-sends ─────────────────────────────────────────
    log_info "Direction 2: iperf-sends (iperf3 -> dpdk-stdlib)"

    # Start dpdk-stdlib listener on Instance A
    local listener_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier3-iperf-interop.sh --role server --direction iperf-sends --local-ip $SENDER_DPDK_ENI_IP --peer-ip $RECEIVER_DPDK_ENI_IP --port 9000"
    local listener_cmd_id
    listener_cmd_id=$(ssm_run_command_async "$SENDER_INSTANCE_ID" "$TEST_TIMEOUT" "$listener_cmd")

    sleep 10

    # Run iperf3 client on Instance B
    local iperf_client_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier3-iperf-interop.sh --role client --direction iperf-sends --local-ip $RECEIVER_DPDK_ENI_IP --peer-ip $SENDER_DPDK_ENI_IP --port 9000 --output $RESULTS_REMOTE_DIR/tier3-iperf-sends.xml"
    if ! ssm_run_command "$RECEIVER_INSTANCE_ID" "$TEST_TIMEOUT" "$iperf_client_cmd"; then
        log_error "iperf-sends test execution failed"
        generate_failure_xml "tier3-iperf-sends" "iperf-sends test execution failed or timed out"
    fi

    ssm_wait_command "$SENDER_INSTANCE_ID" "$listener_cmd_id" 30 || true

    log_info "Tier 3 execution complete"
}

# ── Unbind ENIs between tiers ────────────────────────────────────────────────

unbind_all_enis() {
    log_info "Unbinding all ENIs..."
    configure_eni "$SENDER_INSTANCE_ID" "unbind" || true
    configure_eni "$RECEIVER_INSTANCE_ID" "unbind" || true
}

# ── Result collection ────────────────────────────────────────────────────────

generate_failure_xml() {
    local suite_name="$1"
    local message="$2"
    local output_path="$RESULTS_DIR/${suite_name}.xml"

    mkdir -p "$RESULTS_DIR"
    cat > "$output_path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="${suite_name}" tests="1" failures="1" errors="0" time="0.000">
    <testcase name="execution" classname="${suite_name}" time="0.000">
        <failure message="${message}" type="ExecutionError">${message}</failure>
    </testcase>
</testsuite>
EOF
    log_info "Generated synthetic failure XML: $output_path"
}

collect_results() {
    log_section "Collecting test results"

    mkdir -p "$RESULTS_DIR"

    # Collect XML files from sender instance
    collect_xml_from_instance "$SENDER_INSTANCE_ID" "sender"

    # Collect XML files from receiver instance
    collect_xml_from_instance "$RECEIVER_INSTANCE_ID" "receiver"
}

collect_xml_from_instance() {
    local instance_id="$1"
    local label="$2"

    log_info "Collecting results from $label ($instance_id)..."

    # List XML files in remote results directory
    local list_cmd="ls -1 $RESULTS_REMOTE_DIR/*.xml 2>/dev/null || echo 'NO_FILES'"
    local cmd_id
    cmd_id=$(ssm_run_command_async "$instance_id" 30 "$list_cmd")

    if [[ -z "$cmd_id" ]]; then
        log_error "Failed to list results on $label"
        return 1
    fi

    sleep 5

    local file_list
    file_list=$(ssm_get_stdout "$instance_id" "$cmd_id")

    if [[ "$file_list" == "NO_FILES" || -z "$file_list" ]]; then
        log_info "No result files on $label"
        return 0
    fi

    # Download each XML file
    while IFS= read -r remote_path; do
        [[ -z "$remote_path" ]] && continue
        local filename
        filename=$(basename "$remote_path")

        log_info "Retrieving $filename from $label..."

        local cat_cmd="cat $remote_path"
        local cat_cmd_id
        cat_cmd_id=$(ssm_run_command_async "$instance_id" 30 "$cat_cmd")

        if [[ -z "$cat_cmd_id" ]]; then
            log_error "Failed to retrieve $filename from $label"
            continue
        fi

        sleep 5

        local content
        content=$(ssm_get_stdout "$instance_id" "$cat_cmd_id")

        if [[ -n "$content" ]]; then
            echo "$content" > "$RESULTS_DIR/$filename"
            log_info "Saved $filename"
        else
            log_error "Empty content for $filename from $label"
            generate_failure_xml "${filename%.xml}" "Failed to retrieve results from $label"
        fi
    done <<< "$file_list"
}

# ── Summary reporting ────────────────────────────────────────────────────────

print_summary() {
    log_section "Test Results Summary"

    local total_tests=0
    local total_failures=0
    local total_time="0"

    printf "%-40s %6s %8s %10s\n" "Suite" "Tests" "Failures" "Time (s)"
    printf "%-40s %6s %8s %10s\n" "----------------------------------------" "------" "--------" "----------"

    for xml_file in "$RESULTS_DIR"/*.xml; do
        [[ -f "$xml_file" ]] || continue

        local suite_name tests failures time_val
        suite_name=$(grep -oP 'name="\K[^"]+' "$xml_file" | head -1 || echo "unknown")
        tests=$(grep -oP 'tests="\K[^"]+' "$xml_file" | head -1 || echo "0")
        failures=$(grep -oP 'failures="\K[^"]+' "$xml_file" | head -1 || echo "0")
        time_val=$(grep -oP 'time="\K[^"]+' "$xml_file" | head -1 || echo "0")

        printf "%-40s %6s %8s %10s\n" "$suite_name" "$tests" "$failures" "$time_val"

        total_tests=$((total_tests + tests))
        total_failures=$((total_failures + failures))
        total_time=$(awk "BEGIN {printf \"%.3f\", $total_time + $time_val}")
    done

    printf "%-40s %6s %8s %10s\n" "----------------------------------------" "------" "--------" "----------"
    printf "%-40s %6d %8d %10s\n" "TOTAL" "$total_tests" "$total_failures" "$total_time"
    echo ""

    local passed=$((total_tests - total_failures))
    if [[ $total_failures -eq 0 ]]; then
        log_info "ALL TESTS PASSED ($passed/$total_tests)"
        TEST_EXIT_CODE=0
    else
        log_error "FAILURES DETECTED: $total_failures/$total_tests tests failed"
        TEST_EXIT_CODE=1
    fi
}

# ── JSON summary generation ──────────────────────────────────────────────────

generate_json_summary() {
    if [[ "$FLAG_JSON_SUMMARY" != "true" ]]; then
        return 0
    fi

    log_info "Generating JSON summary..."

    local commit_hash
    commit_hash=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo "unknown")
    local timestamp
    timestamp=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    local run_id
    run_id=$(echo "$commit_hash$timestamp" | md5sum | cut -c1-8 2>/dev/null || echo "unknown")

    # Build JSON using python3 for reliable JSON generation
    python3 - "$RESULTS_DIR" "$run_id" "$commit_hash" "$timestamp" <<'PYEOF'
import json
import sys
import os
import re

results_dir = sys.argv[1]
run_id = sys.argv[2]
commit_hash = sys.argv[3]
timestamp = sys.argv[4]

tiers = []
total_tests = 0
total_failures = 0
total_time = 0.0

for xml_file in sorted(os.listdir(results_dir)):
    if not xml_file.endswith('.xml'):
        continue
    filepath = os.path.join(results_dir, xml_file)
    with open(filepath, 'r') as f:
        content = f.read()

    # Parse testsuite attributes
    suite_match = re.search(r'<testsuite\s+([^>]+)>', content)
    if not suite_match:
        continue

    attrs = suite_match.group(1)
    suite_name = re.search(r'name="([^"]*)"', attrs)
    suite_tests = re.search(r'tests="([^"]*)"', attrs)
    suite_failures = re.search(r'failures="([^"]*)"', attrs)
    suite_time = re.search(r'time="([^"]*)"', attrs)

    suite_name = suite_name.group(1) if suite_name else xml_file
    num_tests = int(suite_tests.group(1)) if suite_tests else 0
    num_failures = int(suite_failures.group(1)) if suite_failures else 0
    suite_time_val = float(suite_time.group(1)) if suite_time else 0.0

    total_tests += num_tests
    total_failures += num_failures
    total_time += suite_time_val

    # Parse individual testcases
    tests = []
    for tc_match in re.finditer(r'<testcase\s+([^>]*)>(.*?)</testcase>', content, re.DOTALL):
        tc_attrs = tc_match.group(1)
        tc_body = tc_match.group(2)

        tc_name = re.search(r'name="([^"]*)"', tc_attrs)
        tc_class = re.search(r'classname="([^"]*)"', tc_attrs)
        tc_time = re.search(r'time="([^"]*)"', tc_attrs)

        failure_match = re.search(r'<failure\s+message="([^"]*)"[^>]*>(.*?)</failure>', tc_body, re.DOTALL)

        test_entry = {
            "name": tc_name.group(1) if tc_name else "unknown",
            "classname": tc_class.group(1) if tc_class else "unknown",
            "status": "fail" if failure_match else "pass",
            "duration_seconds": float(tc_time.group(1)) if tc_time else 0.0,
            "error": None
        }

        if failure_match:
            test_entry["error"] = {
                "message": failure_match.group(1),
                "details": failure_match.group(2).strip()
            }

        tests.append(test_entry)

    tier_status = "pass" if num_failures == 0 else "fail"
    tiers.append({
        "name": suite_name,
        "status": tier_status,
        "tests": tests
    })

summary = {
    "run_id": run_id,
    "commit": commit_hash,
    "timestamp": timestamp,
    "infrastructure": {
        "instance_type": "c5n.large",
        "dpdk_version": "22.11.6",
        "region": "us-east-1"
    },
    "tiers": tiers,
    "summary": {
        "total": total_tests,
        "passed": total_tests - total_failures,
        "failed": total_failures,
        "total_time_seconds": round(total_time, 3)
    }
}

output_path = os.path.join(results_dir, "summary.json")
with open(output_path, 'w') as f:
    json.dump(summary, f, indent=2)

print(f"JSON summary written to {output_path}")
PYEOF

    log_info "JSON summary generated: $RESULTS_DIR/summary.json"
}

# ── Teardown ─────────────────────────────────────────────────────────────────

teardown_infrastructure() {
    if [[ "$FLAG_TEARDOWN" != "true" ]]; then
        log_info "Teardown not requested. To destroy infrastructure manually:"
        log_info "  cd $CDK_DIR && npx cdk destroy $CDK_STACK_NAME"
        return 0
    fi

    log_section "Tearing down infrastructure"

    cd "$CDK_DIR"
    if ! npx cdk destroy "$CDK_STACK_NAME" --force 2>&1; then
        log_error "CDK teardown failed (test results are preserved)"
        # Don't change TEST_EXIT_CODE - preserve test result
    else
        log_info "Infrastructure destroyed"
    fi
    cd "$REPO_ROOT"
}

# ── Instance log collection ──────────────────────────────────────────────────
#
# Collects logs from EC2 instances using two methods:
#   1. EC2 console output  - works even without SSM, survives instance termination
#   2. SSM file retrieval  - richer logs when SSM is reachable
#
# Logs are written to $LOGS_DIR/ with <role>-<filename> naming.

collect_ssm_file() {
    local instance_id="$1"
    local label="$2"
    local remote_path="$3"

    local filename
    filename=$(basename "$remote_path")

    local cmd_id
    cmd_id=$(ssm_run_command_async "$instance_id" 30 \
        "test -f $remote_path && cat $remote_path || echo 'FILE_NOT_FOUND'") 2>/dev/null || return 0

    [[ -z "$cmd_id" ]] && return 0

    ssm_wait_command "$instance_id" "$cmd_id" 45 2>/dev/null || true

    local content
    content=$(ssm_get_stdout "$instance_id" "$cmd_id" 2>/dev/null || true)

    if [[ -n "$content" && "$content" != "FILE_NOT_FOUND" ]]; then
        echo "$content" > "$LOGS_DIR/${label}-${filename}"
        log_info "  Saved: ${label}-${filename} ($(echo "$content" | wc -l) lines)"
    fi
}

collect_ssm_command() {
    local instance_id="$1"
    local label="$2"
    local name="$3"
    local command="$4"

    local cmd_id
    cmd_id=$(ssm_run_command_async "$instance_id" 30 "$command") 2>/dev/null || return 0

    [[ -z "$cmd_id" ]] && return 0

    ssm_wait_command "$instance_id" "$cmd_id" 45 2>/dev/null || true

    local content
    content=$(ssm_get_stdout "$instance_id" "$cmd_id" 2>/dev/null || true)

    if [[ -n "$content" ]]; then
        echo "$content" > "$LOGS_DIR/${label}-${name}.log"
        log_info "  Saved: ${label}-${name}.log"
    fi
}

collect_instance_logs() {
    log_section "Collecting instance logs"
    mkdir -p "$LOGS_DIR"

    # Build the list of (label, instance_id) pairs to collect from
    local -a instances=()
    if [[ -n "${SENDER_INSTANCE_ID:-}" ]]; then
        instances+=("sender:${SENDER_INSTANCE_ID}")
    fi
    if [[ -n "${RECEIVER_INSTANCE_ID:-}" ]]; then
        instances+=("receiver:${RECEIVER_INSTANCE_ID}")
    fi

    # Fallback: when CDK deploy failed and we have no stack outputs, look for
    # instances in CloudFormation events (they appear even after rollback).
    if [[ ${#instances[@]} -eq 0 ]]; then
        log_info "No known instance IDs, scanning CloudFormation events..."
        local event_ids
        event_ids=$(aws cloudformation describe-stack-events \
            --stack-name "$CDK_STACK_NAME" \
            --query "StackEvents[?ResourceType=='AWS::EC2::Instance' && PhysicalResourceId!=''].PhysicalResourceId" \
            --output text 2>/dev/null | tr '\t' '\n' | grep -E '^i-' | sort -u || true)

        local idx=0
        for inst_id in $event_ids; do
            instances+=("instance-${idx}:${inst_id}")
            idx=$((idx + 1))
        done
    fi

    if [[ ${#instances[@]} -eq 0 ]]; then
        log_info "No instances found - skipping log collection"
        return 0
    fi

    for entry in "${instances[@]}"; do
        local label="${entry%%:*}"
        local instance_id="${entry##*:}"

        log_info "Collecting logs from ${label} (${instance_id})..."

        # ── EC2 console output (survives termination, no SSM needed) ─────────
        local console_output
        console_output=$(aws ec2 get-console-output \
            --instance-id "$instance_id" \
            --latest \
            --query "Output" \
            --output text 2>/dev/null || echo "(console output unavailable)")
        echo "$console_output" > "$LOGS_DIR/${label}-console-output.log"
        log_info "  Saved: ${label}-console-output.log ($(echo "$console_output" | wc -l) lines)"

        # ── SSM-based log collection (richer, needs SSM agent) ───────────────
        local ssm_ready
        ssm_ready=$(aws ssm describe-instance-information \
            --filters "Key=InstanceIds,Values=${instance_id}" \
            --query "InstanceInformationList[0].InstanceId" \
            --output text 2>/dev/null || echo "")

        if [[ -n "$ssm_ready" && "$ssm_ready" != "None" ]]; then
            log_info "  SSM available - collecting log files..."
            collect_ssm_file  "$instance_id" "$label" "/var/log/user-data.log"
            collect_ssm_file  "$instance_id" "$label" "/var/log/cloud-init-output.log"
            collect_ssm_file  "$instance_id" "$label" "/var/log/cfn-init.log"
            collect_ssm_file  "$instance_id" "$label" "/var/log/cfn-init-cmd.log"
            collect_ssm_command "$instance_id" "$label" "build-listing" \
                "ls -la /opt/dpdk-stdlib/target/release/ 2>/dev/null || echo 'no build output directory'"
            collect_ssm_command "$instance_id" "$label" "journal" \
                "journalctl --no-pager -n 500 2>/dev/null || tail -500 /var/log/messages 2>/dev/null || echo 'journal unavailable'"
        else
            log_info "  SSM not available for ${label} - relying on console output only"
        fi
    done

    log_info "Instance logs written to: $LOGS_DIR"
}

# ── GitHub Actions step summary ──────────────────────────────────────────────
#
# write_step_summary: writes a markdown digest to $GITHUB_STEP_SUMMARY so the
# failure reason and key log excerpts are visible directly in the Actions UI
# without downloading any artifacts.  No-op when not running in GitHub Actions.
#
# The goal: an agent or human reviewing a failed run should be able to see
# the root cause and last lines of user-data.log without leaving the page.

write_step_summary() {
    [[ -z "${GITHUB_STEP_SUMMARY:-}" ]] && return 0

    local result_icon status_text
    if [[ "${TEST_EXIT_CODE:-0}" -eq 0 ]]; then
        result_icon="✅"
        status_text="PASSED"
    else
        result_icon="❌"
        status_text="FAILED (exit ${TEST_EXIT_CODE:-2})"
    fi

    {
        echo "## ${result_icon} Integration Tests: ${status_text}"
        echo ""

        if [[ -n "${FAILED_STEP:-}" ]]; then
            echo "**Failed at step:** \`${FAILED_STEP}\`"
            echo ""
        fi

        if [[ -n "${SENDER_INSTANCE_ID:-}" ]]; then
            echo "**Sender:** \`${SENDER_INSTANCE_ID}\` | **Receiver:** \`${RECEIVER_INSTANCE_ID:-unknown}\`"
            echo ""
        fi

        # Per-instance log excerpts (collapsed by default — readable without downloading)
        for label in sender receiver; do
            local userdata_log="$LOGS_DIR/${label}-user-data.log"
            local console_log="$LOGS_DIR/${label}-console-output.log"
            local build_file="$LOGS_DIR/${label}-build-listing.txt"

            echo "### ${label^} instance logs"
            echo ""

            # Prefer user-data.log (richer); fall back to console output
            if [[ -f "$userdata_log" && -s "$userdata_log" ]]; then
                local line_count
                line_count=$(wc -l < "$userdata_log")
                echo "<details><summary>user-data.log — last 80 of ${line_count} lines</summary>"
                echo ""
                echo '```'
                tail -80 "$userdata_log"
                echo '```'
                echo "</details>"
                echo ""
            elif [[ -f "$console_log" && -s "$console_log" ]]; then
                local line_count
                line_count=$(wc -l < "$console_log")
                echo "<details><summary>EC2 console output — last 80 of ${line_count} lines</summary>"
                echo ""
                echo '```'
                tail -80 "$console_log"
                echo '```'
                echo "</details>"
                echo ""
            else
                echo "_No logs collected for ${label} instance._"
                echo ""
            fi

            if [[ -f "$build_file" ]]; then
                echo "**Build directory (\`/opt/dpdk-stdlib/target/release/\`):**"
                echo '```'
                cat "$build_file"
                echo '```'
                echo ""
            fi
        done

        # Test result counts if JUnit XML was generated
        if compgen -G "$RESULTS_DIR/*.xml" > /dev/null 2>&1; then
            local total=0 failures=0
            for xml in "$RESULTS_DIR"/*.xml; do
                local t f
                t=$(python3 -c "import xml.etree.ElementTree as ET; t=ET.parse('$xml').getroot(); print(t.get('tests','0'))" 2>/dev/null || echo 0)
                f=$(python3 -c "import xml.etree.ElementTree as ET; t=ET.parse('$xml').getroot(); print(t.get('failures','0'))" 2>/dev/null || echo 0)
                total=$(( total + t ))
                failures=$(( failures + f ))
            done
            echo "### Test counts"
            echo "Tests run: **${total}** | Failures: **${failures}**"
            echo ""
        fi

        echo "---"
        echo "_Artifacts: \`instance-logs\` (raw logs) · \`integration-test-results\` (JUnit XML)_"
    } >> "$GITHUB_STEP_SUMMARY"
}

# write_failure_json: writes structured failure info to instance-logs/failure-summary.json.
# An agent reading the artifact can parse this directly instead of grepping raw logs.

write_failure_json() {
    local step="$1"
    local message="$2"
    mkdir -p "$LOGS_DIR"

    python3 - <<PYEOF
import json, datetime
data = {
    "failed_step": """$step""",
    "error": """$message""",
    "exit_code": 2,
    "timestamp": datetime.datetime.utcnow().isoformat() + "Z",
    "sender_instance_id": """${SENDER_INSTANCE_ID:-}""",
    "receiver_instance_id": """${RECEIVER_INSTANCE_ID:-}""",
    "commit": """${GITHUB_SHA:-unknown}""",
    "run_url": "${GITHUB_SERVER_URL:-}/${GITHUB_REPOSITORY:-}/actions/runs/${GITHUB_RUN_ID:-}",
}
with open("$LOGS_DIR/failure-summary.json", "w") as f:
    json.dump(data, f, indent=2)
print(f"Failure summary: $LOGS_DIR/failure-summary.json")
PYEOF
}

# fail_with_logs: collect logs + write step summary + write failure JSON, then return.
# Call this before exit 2 on any infrastructure failure path.

fail_with_logs() {
    local step="$1"
    local message="$2"
    FAILED_STEP="$step"
    log_error "$message"
    collect_instance_logs || true
    write_failure_json "$step" "$message" || true
    write_step_summary || true
}

# ── Main execution ───────────────────────────────────────────────────────────

main() {
    log_section "EC2 Integration Tests for dpdk-stdlib-rust"

    log_info "Profile:      $AWS_PROFILE"
    log_info "Teardown:     $FLAG_TEARDOWN"
    log_info "Skip deploy:  $FLAG_SKIP_DEPLOY"
    log_info "Tier filter:  ${TIER_FILTER:-all}"
    log_info "JSON summary: $FLAG_JSON_SUMMARY"

    # Step 1: Deploy (or skip)
    if [[ "$FLAG_SKIP_DEPLOY" != "true" ]]; then
        if ! deploy_infrastructure; then
            # CF events preserve instance IDs even after rollback — collect what we can
            fail_with_logs "deploy_infrastructure" "Infrastructure deployment failed"
            exit 2
        fi
    fi

    # Step 2: Fetch stack outputs
    if ! fetch_stack_outputs; then
        fail_with_logs "fetch_stack_outputs" "Failed to fetch stack outputs"
        exit 2
    fi

    # Step 3: Wait for SSM readiness
    if ! wait_for_ssm_readiness; then
        fail_with_logs "wait_for_ssm_readiness" "Instances not ready within SSM timeout"
        teardown_infrastructure
        exit 2
    fi

    # Step 4: Verify build
    if ! verify_build; then
        fail_with_logs "verify_build" "Build verification failed — echo binary missing on instance"
        teardown_infrastructure
        exit 2
    fi

    # Step 5: Run tiers
    mkdir -p "$RESULTS_DIR"

    if [[ -z "$TIER_FILTER" || "$TIER_FILTER" == "1" ]]; then
        run_tier1 || true
    fi

    # Unbind ENIs between tiers
    if [[ -z "$TIER_FILTER" ]]; then
        unbind_all_enis
    fi

    if [[ -z "$TIER_FILTER" || "$TIER_FILTER" == "3" ]]; then
        run_tier3 || true
    fi

    # Step 6: Collect results
    collect_results

    # Step 7: Summary
    print_summary
    generate_json_summary

    # Step 8: Collect instance logs + write step summary (always, before teardown)
    collect_instance_logs || true
    write_step_summary || true

    # Step 9: Teardown
    teardown_infrastructure

    log_info "Exiting with code: $TEST_EXIT_CODE"
    exit "$TEST_EXIT_CODE"
}

main "$@"
