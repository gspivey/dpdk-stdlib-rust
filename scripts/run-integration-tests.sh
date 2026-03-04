#!/usr/bin/env bash
# run-integration-tests.sh - Orchestrator for EC2 integration tests
#
# Drives the full lifecycle: deploy, wait for readiness, configure ENIs,
# run test tiers, collect JUnit XML results, optionally teardown.
#
# Usage:
#   ./scripts/run-integration-tests.sh [AWS_PROFILE] [--teardown] [--skip-deploy] [--tier 1|3] [--json-summary]
#
# When AWS_PROFILE is omitted or set to "default", the script relies on
# environment-variable credentials (AWS_ACCESS_KEY_ID, etc.) which is the
# norm in GitHub Actions.  A named profile is only exported when explicitly
# provided and not equal to "default".
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
ENI_BIND_TIMEOUT=45          # 45 seconds for ENI bind/unbind
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
Usage: $0 [AWS_PROFILE] [OPTIONS]

Orchestrates EC2 integration tests for dpdk-stdlib-rust.

Arguments:
  AWS_PROFILE           AWS CLI profile name (optional; ignored when "default")

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

# First positional argument is AWS_PROFILE (optional — may be a flag instead)
if [[ $# -gt 0 && "${1:-}" != --* ]]; then
    AWS_PROFILE="$1"
    shift
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --teardown)      FLAG_TEARDOWN=true;      shift ;;
        --skip-deploy)   FLAG_SKIP_DEPLOY=true;   shift ;;
        --json-summary)  FLAG_JSON_SUMMARY=true;  shift ;;
        --tier)
            TIER_FILTER="$2"
            if [[ "$TIER_FILTER" != "1" && "$TIER_FILTER" != "2" && "$TIER_FILTER" != "3" ]]; then
                echo "ERROR: --tier must be 1, 2, or 3, got: $TIER_FILTER" >&2
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

# Only export AWS_PROFILE when it's a real named profile.
# In GitHub Actions, credentials come from env vars (AWS_ACCESS_KEY_ID etc.)
# and exporting AWS_PROFILE=default causes the CLI to look for a named profile
# that doesn't exist, shadowing the env-var credentials.
if [[ -n "$AWS_PROFILE" && "$AWS_PROFILE" != "default" ]]; then
    export AWS_PROFILE
else
    # Ensure no stale AWS_PROFILE leaks into child processes
    unset AWS_PROFILE 2>/dev/null || true
    AWS_PROFILE=""
fi

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

# ── PR comment helper (for staged CI feedback to Claude Code web) ────────────
# Posts a markdown comment to the PR associated with this CI run.
# Requires GH_TOKEN and either PR_NUMBER or GITHUB_HEAD_REF to be set.
# No-op if not running in CI or no PR is found.

post_pr_comment() {
    local body="$1"
    local pr_number="${PR_NUMBER:-}"

    # Skip if gh CLI is not available
    command -v gh >/dev/null 2>&1 || return 0

    # Skip if no GH_TOKEN
    [[ -n "${GH_TOKEN:-}" ]] || return 0

    # Find PR number if not set
    if [[ -z "$pr_number" && -n "${GITHUB_HEAD_REF:-}" ]]; then
        pr_number=$(gh pr list --head "$GITHUB_HEAD_REF" --json number --jq '.[0].number' \
            --repo "${GITHUB_REPOSITORY:-gspivey/dpdk-stdlib-rust}" 2>/dev/null || echo "")
    fi

    if [[ -n "$pr_number" ]]; then
        gh pr comment "$pr_number" --body "$body" \
            --repo "${GITHUB_REPOSITORY:-gspivey/dpdk-stdlib-rust}" 2>/dev/null || true
    fi
}

# ── Process cleanup and ARP warming ──────────────────────────────────────────

# Kill all DPDK processes and clean runtime state on both instances.
# Must be called between tiers so the next tier starts from a clean slate.
cleanup_dpdk_state() {
    log_info "Cleaning DPDK state on both instances..."
    local cleanup_cmd="pkill -f 'target/release/echo' 2>/dev/null || true; pkill -f 'target/release/test-client' 2>/dev/null || true; pkill -f 'target/release/tokio-echo' 2>/dev/null || true; sleep 2; rm -rf /var/run/dpdk/ 2>/dev/null || true"
    ssm_run_command "$SENDER_INSTANCE_ID" 15 "$cleanup_cmd" || true
    ssm_run_command "$RECEIVER_INSTANCE_ID" 15 "$cleanup_cmd" || true
}

# Warm the kernel ARP cache so DPDK can seed from /proc/net/arp.
# In AWS VPC, the gateway MAC is needed for all DPDK outbound frames.
# dpdk-udp reads /proc/net/arp at bind() time, so we need the kernel to
# have resolved the gateway and peer IPs before the DPDK binaries start.
warm_arp_cache() {
    log_info "Warming kernel ARP cache on both instances..."
    # Ping both ENI IPs (proxy ARP populates gateway MAC) and the gateway
    local subnet_prefix
    subnet_prefix=$(echo "$SENDER_DPDK_ENI_IP" | sed 's/\.[0-9]*$/.1/')
    local arp_warm_cmd="ping -c 1 -W 2 ${subnet_prefix} >/dev/null 2>&1 || true; ping -c 1 -W 2 ${SENDER_DPDK_ENI_IP} >/dev/null 2>&1 || true; ping -c 1 -W 2 ${RECEIVER_DPDK_ENI_IP} >/dev/null 2>&1 || true"
    ssm_run_command "$SENDER_INSTANCE_ID" 15 "$arp_warm_cmd" || true
    ssm_run_command "$RECEIVER_INSTANCE_ID" 15 "$arp_warm_cmd" || true
}

# ── Networking diagnostics ───────────────────────────────────────────────────

run_diagnostics() {
    local label="$1"  # "baseline" or "failure"
    log_info "Running networking diagnostics ($label)..."

    for entry in "sender:${SENDER_INSTANCE_ID}" "receiver:${RECEIVER_INSTANCE_ID}"; do
        local role="${entry%%:*}"
        local instance_id="${entry##*:}"
        [[ -n "$instance_id" ]] || continue

        local diag_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/diagnose-networking.sh 2>&1"
        local diag_cmd_id
        diag_cmd_id=$(ssm_run_command_async "$instance_id" 30 "$diag_cmd")
        if [[ -n "$diag_cmd_id" ]]; then
            if ssm_wait_command "$instance_id" "$diag_cmd_id" 30; then
                local output
                output=$(ssm_get_stdout "$instance_id" "$diag_cmd_id" 2>/dev/null || echo "")
                if [[ -n "$output" ]]; then
                    mkdir -p "$LOGS_DIR"
                    echo "$output" > "$LOGS_DIR/${role}-networking-diag-${label}.txt"
                    log_info "Saved ${role} diagnostics to ${role}-networking-diag-${label}.txt"
                fi
            fi
        fi
    done
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

    # --no-rollback: on failure, leave instances running so we can collect
    # user-data.log via SSM.  The safety-net teardown step destroys the stack.
    log_info "Running cdk deploy (--no-rollback for debug)..."
    if ! npx cdk deploy "$CDK_STACK_NAME" \
        --require-approval never \
        --no-rollback \
        --outputs-file /tmp/cdk-outputs.json \
        $cdk_context_args 2>&1; then
        log_error "CDK deployment failed"

        # Query CloudFormation for failure reasons — these contain the cfn-signal
        # --reason string from the EXIT trap, showing the actual user-data error.
        log_info "CloudFormation CREATE_FAILED events:"
        aws cloudformation describe-stack-events \
            --stack-name "$CDK_STACK_NAME" \
            --query "StackEvents[?ResourceStatus=='CREATE_FAILED'].[LogicalResourceId,ResourceStatusReason]" \
            --output table 2>/dev/null || true

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
        log_info "CDK outputs file contents: $cdk_outputs"
        SENDER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('SenderInstanceId', ''))
" 2>&1) || { log_error "Python parse failed for SenderInstanceId: $SENDER_INSTANCE_ID"; SENDER_INSTANCE_ID=""; }
        RECEIVER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('ReceiverInstanceId', ''))
" 2>&1) || { log_error "Python parse failed for ReceiverInstanceId: $RECEIVER_INSTANCE_ID"; RECEIVER_INSTANCE_ID=""; }
        SENDER_DPDK_ENI_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('SenderDpdkEniId', ''))
" 2>&1) || { log_error "Python parse failed for SenderDpdkEniId: $SENDER_DPDK_ENI_ID"; SENDER_DPDK_ENI_ID=""; }
        RECEIVER_DPDK_ENI_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('ReceiverDpdkEniId', ''))
" 2>&1) || { log_error "Python parse failed for ReceiverDpdkEniId: $RECEIVER_DPDK_ENI_ID"; RECEIVER_DPDK_ENI_ID=""; }
        SENDER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('SenderDpdkEniPrivateIp', ''))
" 2>&1) || { log_error "Python parse failed for SenderDpdkEniPrivateIp: $SENDER_DPDK_ENI_IP"; SENDER_DPDK_ENI_IP=""; }
        RECEIVER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(data.get('$CDK_STACK_NAME', {}).get('ReceiverDpdkEniPrivateIp', ''))
" 2>&1) || { log_error "Python parse failed for ReceiverDpdkEniPrivateIp: $RECEIVER_DPDK_ENI_IP"; RECEIVER_DPDK_ENI_IP=""; }

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
    local cf_error
    cf_error=$(mktemp)
    cf_outputs=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs" \
        --output json 2>"$cf_error") || {
        log_error "CloudFormation describe-stacks failed:"
        log_error "  $(cat "$cf_error")"
        rm -f "$cf_error"
        return 1
    }
    rm -f "$cf_error"

    if [[ -z "$cf_outputs" || "$cf_outputs" == "null" ]]; then
        log_error "CloudFormation describe-stacks returned no outputs for $CDK_STACK_NAME"
        log_error "  Stack may not exist or may be in a failed state."
        return 1
    fi

    log_info "CloudFormation outputs JSON: $cf_outputs"

    SENDER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderInstanceId': print(o['OutputValue'])
" 2>&1) || { log_error "Python parse failed for SenderInstanceId: $SENDER_INSTANCE_ID"; SENDER_INSTANCE_ID=""; }
    RECEIVER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverInstanceId': print(o['OutputValue'])
" 2>&1) || { log_error "Python parse failed for ReceiverInstanceId: $RECEIVER_INSTANCE_ID"; RECEIVER_INSTANCE_ID=""; }
    SENDER_DPDK_ENI_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderDpdkEniId': print(o['OutputValue'])
" 2>&1) || { log_error "Python parse failed for SenderDpdkEniId: $SENDER_DPDK_ENI_ID"; SENDER_DPDK_ENI_ID=""; }
    RECEIVER_DPDK_ENI_ID=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverDpdkEniId': print(o['OutputValue'])
" 2>&1) || { log_error "Python parse failed for ReceiverDpdkEniId: $RECEIVER_DPDK_ENI_ID"; RECEIVER_DPDK_ENI_ID=""; }
    SENDER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderDpdkEniPrivateIp': print(o['OutputValue'])
" 2>&1) || { log_error "Python parse failed for SenderDpdkEniPrivateIp: $SENDER_DPDK_ENI_IP"; SENDER_DPDK_ENI_IP=""; }
    RECEIVER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'ReceiverDpdkEniPrivateIp': print(o['OutputValue'])
" 2>&1) || { log_error "Python parse failed for ReceiverDpdkEniPrivateIp: $RECEIVER_DPDK_ENI_IP"; RECEIVER_DPDK_ENI_IP=""; }

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

        # Check if both instances are registered with SSM.
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

# Cancel an in-flight SSM command (e.g., a listener that's still running).
# Usage: ssm_cancel_command <instance_id> <command_id>
ssm_cancel_command() {
    local instance_id="$1"
    local cmd_id="$2"

    log_info "Cancelling SSM command $cmd_id on $instance_id..."
    aws ssm cancel-command --command-id "$cmd_id" --instance-ids "$instance_id" 2>/dev/null || true
    # Give SSM agent a moment to process the cancellation
    sleep 2
}

# ── ENI configuration ───────────────────────────────────────────────────────

configure_eni() {
    local instance_id="$1"
    local action="$2"  # bind or unbind
    local expected_ip="${3:-}"  # optional: IP to assign after unbind

    log_info "ENI $action on $instance_id"
    if ! ssm_run_command "$instance_id" "$ENI_BIND_TIMEOUT" \
        "cd /opt/dpdk-stdlib && bash scripts/integration-tests/configure-eni.sh --action $action"; then
        log_error "ENI $action failed on $instance_id — fetching ENI status for diagnostics..."
        ssm_run_command "$instance_id" 15 \
            "cd /opt/dpdk-stdlib && bash scripts/integration-tests/configure-eni.sh --action status" || true
        return 1
    fi

    # After unbinding, ensure the kernel interface has the correct IP.
    # NetworkManager on AL2023 will detect the new interface and may run DHCP
    # or reconfigure it, removing any manually-assigned IP.  We must tell NM
    # to leave the interface alone before assigning the IP.
    if [[ "$action" == "unbind" && -n "$expected_ip" ]]; then
        log_info "Ensuring IP $expected_ip is configured on $instance_id secondary ENI..."
        if ! ssm_run_command "$instance_id" 30 "$(cat <<IPCMD
set -euo pipefail
pci_addr=\$(lspci -D | grep 'Elastic Network Adapter' | tail -1 | cut -d' ' -f1)
echo "PCI address: \$pci_addr"

# Wait for kernel to create the net interface (async after ena bind)
retries=0
iface=""
while [[ \$retries -lt 10 ]]; do
    iface=\$(ls /sys/bus/pci/devices/\$pci_addr/net/ 2>/dev/null | head -1)
    if [[ -n "\$iface" ]]; then
        break
    fi
    retries=\$((retries + 1))
    echo "Waiting for net interface to appear... (\$retries/10)"
    sleep 1
done

if [[ -z "\$iface" ]]; then
    echo "ERROR: No network interface found for \$pci_addr after 10s"
    ls -la /sys/bus/pci/devices/\$pci_addr/ 2>/dev/null || true
    exit 1
fi
echo "Interface: \$iface"

# Tell NetworkManager to ignore this interface so it doesn't remove our IP
if command -v nmcli >/dev/null 2>&1; then
    nmcli device set "\$iface" managed no 2>/dev/null || true
    echo "Set \$iface as unmanaged by NetworkManager"
fi

# Bring up the interface
ip link set "\$iface" up

# Flush any stale addresses and assign the expected IP
ip addr flush dev "\$iface" 2>/dev/null || true
echo "Assigning $expected_ip/24 to \$iface"
ip addr add '$expected_ip/24' dev "\$iface"

# Add route via subnet gateway
gw=\$(echo '$expected_ip' | sed 's/\.[0-9]*\$/.1/')
ip route add default via "\$gw" dev "\$iface" metric 200 2>/dev/null || true

# Verify
echo "Final interface state:"
ip -4 addr show "\$iface"
IPCMD
)"; then
            log_error "Failed to assign IP $expected_ip on $instance_id"
            return 1
        fi
    fi
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

    # Run baseline diagnostics
    run_diagnostics "baseline" || true

    # Warm ARP cache so DPDK can seed gateway MAC from /proc/net/arp
    warm_arp_cache

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

    # Wait for listener to finish (or time out), then cancel if still running
    if ! ssm_wait_command "$RECEIVER_INSTANCE_ID" "$listener_cmd_id" 30; then
        ssm_cancel_command "$RECEIVER_INSTANCE_ID" "$listener_cmd_id"
    fi

    # Kill any lingering echo server processes on the receiver
    ssm_run_command "$RECEIVER_INSTANCE_ID" 15 "pkill -f 'target/release/echo' || true" || true

    log_info "Tier 1 execution complete"
}

run_tier2() {
    log_section "Tier 2: Kernel -> DPDK interoperability test"

    # Bind ENI on receiver only (sender uses kernel networking)
    log_info "Configuring ENIs for Tier 2..."
    if ! configure_eni "$RECEIVER_INSTANCE_ID" "bind"; then
        log_error "Failed to bind ENI on receiver"
        generate_failure_xml "tier2-kernel-interop" "ENI bind failed on receiver instance"
        return 1
    fi
    # Ensure sender ENI is unbound (kernel networking)
    configure_eni "$SENDER_INSTANCE_ID" "unbind" "$SENDER_DPDK_ENI_IP" || true

    # Warm ARP cache so DPDK can seed gateway MAC from /proc/net/arp
    warm_arp_cache

    # Start listener on receiver (DPDK) in background
    log_info "Starting listener on receiver..."
    local listener_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier2-kernel-interop.sh --role listener --bind-ip $RECEIVER_DPDK_ENI_IP --port 9000"
    local listener_cmd_id
    listener_cmd_id=$(ssm_run_command_async "$RECEIVER_INSTANCE_ID" "$TEST_TIMEOUT" "$listener_cmd")

    if [[ -z "$listener_cmd_id" ]]; then
        log_error "Failed to start listener"
        generate_failure_xml "tier2-kernel-interop" "Failed to start listener on receiver"
        return 1
    fi

    # Give listener time to start
    sleep 10

    # Run sender on sender (kernel networking — no --bind-ip)
    log_info "Starting sender on sender (kernel networking)..."
    local sender_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier2-kernel-interop.sh --role sender --peer-ip $RECEIVER_DPDK_ENI_IP --port 9000 --output $RESULTS_REMOTE_DIR/tier2-kernel-interop.xml"
    if ! ssm_run_command "$SENDER_INSTANCE_ID" "$TEST_TIMEOUT" "$sender_cmd"; then
        log_error "Sender test execution failed"
        generate_failure_xml "tier2-kernel-interop" "Sender test execution failed or timed out"
    fi

    # Wait for listener to finish (or time out), then cancel if still running
    if ! ssm_wait_command "$RECEIVER_INSTANCE_ID" "$listener_cmd_id" 30; then
        ssm_cancel_command "$RECEIVER_INSTANCE_ID" "$listener_cmd_id"
    fi

    # Kill any lingering echo server processes on the receiver
    ssm_run_command "$RECEIVER_INSTANCE_ID" 15 "pkill -f 'target/release/echo' || true" || true

    log_info "Tier 2 execution complete"
}

run_tier3() {
    log_section "Tier 3: DPDK <-> iperf3 interoperability test"

    # Pre-bind diagnostics: check ENI state on sender before attempting bind
    log_info "Pre-bind diagnostics for sender..."
    ssm_run_command "$SENDER_INSTANCE_ID" 15 \
        "cd /opt/dpdk-stdlib && bash scripts/integration-tests/configure-eni.sh --action status" || true

    # Bind ENI on Instance A only; Instance B uses kernel networking
    log_info "Configuring ENIs for Tier 3..."
    if ! configure_eni "$SENDER_INSTANCE_ID" "bind"; then
        log_error "Failed to bind ENI on sender for Tier 3"
        # Capture detailed diagnostics instead of silently skipping
        log_error "Collecting ENI diagnostics from sender..."
        ssm_run_command "$SENDER_INSTANCE_ID" 15 \
            "lspci -D; echo '---'; ls -la /sys/bus/pci/drivers/vfio-pci/ 2>/dev/null || echo 'no vfio-pci dir'; echo '---'; lsmod | grep vfio; echo '---'; cat /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null; echo '---'; dmesg | tail -20" || true
        generate_failure_xml "tier3-iperf-interop" "ENI bind failed on sender instance — see instance logs for diagnostics"
        return 1
    fi
    # Ensure receiver ENI is unbound (kernel networking) with IP assigned
    if ! configure_eni "$RECEIVER_INSTANCE_ID" "unbind" "$RECEIVER_DPDK_ENI_IP"; then
        log_error "Failed to unbind/configure receiver ENI for Tier 3"
        generate_failure_xml "tier3-our-app-sends" "Receiver ENI unbind or IP assignment failed"
        generate_failure_xml "tier3-iperf-sends" "Receiver ENI unbind or IP assignment failed"
        return 1
    fi

    # Warm ARP cache so DPDK can seed gateway MAC from /proc/net/arp
    warm_arp_cache

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
        log_error "our-app-sends test execution failed (tier3 is non-blocking)"
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
        log_error "iperf-sends test execution failed (tier3 is non-blocking)"
        generate_failure_xml "tier3-iperf-sends" "iperf-sends test execution failed or timed out"
    fi

    ssm_wait_command "$SENDER_INSTANCE_ID" "$listener_cmd_id" 30 || true

    log_info "Tier 3 execution complete"
}

# ── Unbind ENIs between tiers ────────────────────────────────────────────────

unbind_all_enis() {
    log_info "Unbinding all ENIs..."

    # Kill DPDK processes first — they hold vfio-pci file descriptors open
    # which prevents driver unbind.
    cleanup_dpdk_state

    if ! configure_eni "$SENDER_INSTANCE_ID" "unbind" "$SENDER_DPDK_ENI_IP"; then
        log_error "Failed to unbind ENI on sender ($SENDER_INSTANCE_ID) — continuing"
    fi
    if ! configure_eni "$RECEIVER_INSTANCE_ID" "unbind" "$RECEIVER_DPDK_ENI_IP"; then
        log_error "Failed to unbind ENI on receiver ($RECEIVER_INSTANCE_ID) — continuing"
    fi
    # Verify ENI status after unbind to catch incomplete transitions
    log_info "Verifying ENI status after unbind..."
    ssm_run_command "$SENDER_INSTANCE_ID" 15 \
        "cd /opt/dpdk-stdlib && bash scripts/integration-tests/configure-eni.sh --action status" || true
    ssm_run_command "$RECEIVER_INSTANCE_ID" 15 \
        "cd /opt/dpdk-stdlib && bash scripts/integration-tests/configure-eni.sh --action status" || true
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

generate_skip_xml() {
    local suite_name="$1"
    local message="$2"
    local output_path="$RESULTS_DIR/${suite_name}.xml"

    mkdir -p "$RESULTS_DIR"
    cat > "$output_path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="${suite_name}" tests="1" failures="0" errors="0" skipped="1" time="0.000">
    <testcase name="execution" classname="${suite_name}" time="0.000">
        <skipped message="${message}">${message}</skipped>
    </testcase>
</testsuite>
EOF
    log_info "Generated synthetic skip XML: $output_path"
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
        suite_name=$(sed -n 's/.*name="\([^"]*\)".*/\1/p' "$xml_file" | head -1)
        suite_name="${suite_name:-unknown}"
        tests=$(sed -n 's/.*tests="\([^"]*\)".*/\1/p' "$xml_file" | head -1)
        tests="${tests:-0}"
        failures=$(sed -n 's/.*failures="\([^"]*\)".*/\1/p' "$xml_file" | head -1)
        failures="${failures:-0}"
        time_val=$(sed -n 's/.*time="\([^"]*\)".*/\1/p' "$xml_file" | head -1)
        time_val="${time_val:-0}"

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
    if command -v md5sum >/dev/null 2>&1; then
        run_id=$(echo "$commit_hash$timestamp" | md5sum | cut -c1-8)
    elif command -v md5 >/dev/null 2>&1; then
        run_id=$(echo "$commit_hash$timestamp" | md5 -q | cut -c1-8)
    else
        run_id="unknown"
    fi

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
            
            # Application logs from test execution
            collect_ssm_file  "$instance_id" "$label" "/tmp/echo-server.log"
            collect_ssm_file  "$instance_id" "$label" "/tmp/test-client.log"
            collect_ssm_file  "$instance_id" "$label" "/tmp/test-client-iperf.log"
            collect_ssm_file  "$instance_id" "$label" "/tmp/iperf3-server.log"
            
            collect_ssm_command "$instance_id" "$label" "build-listing" \
                "ls -la /opt/dpdk-stdlib/target/release/ 2>/dev/null || echo 'no build output directory'"
            collect_ssm_command "$instance_id" "$label" "network-interfaces" \
                "ip addr show 2>/dev/null || ifconfig -a 2>/dev/null || echo 'network info unavailable'"
            collect_ssm_command "$instance_id" "$label" "journal" \
                "journalctl --no-pager -n 500 2>/dev/null || tail -500 /var/log/messages 2>/dev/null || echo 'journal unavailable'"

            # Crash diagnostics: dmesg, coredump listing, and crash reports
            collect_ssm_command "$instance_id" "$label" "dmesg-crashes" \
                "dmesg | grep -iE 'segfault|trap|fault|oom|killed|echo|test-client' | tail -50 2>/dev/null || echo 'no crash-related dmesg entries'"
            collect_ssm_command "$instance_id" "$label" "coredump-listing" \
                "ls -lh /tmp/coredumps/ 2>/dev/null || echo 'no coredump directory'"
            collect_ssm_command "$instance_id" "$label" "crash-reports" \
                "cat /tmp/crash-report-*.txt 2>/dev/null || echo 'no crash reports'"
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

    log_info "Profile:      ${AWS_PROFILE:-<env-var credentials>}"
    log_info "Teardown:     $FLAG_TEARDOWN"
    log_info "Skip deploy:  $FLAG_SKIP_DEPLOY"
    log_info "Tier filter:  ${TIER_FILTER:-all}"
    log_info "JSON summary: $FLAG_JSON_SUMMARY"

    # Step 1: Deploy (or skip)
    if [[ "$FLAG_SKIP_DEPLOY" != "true" ]]; then
        if ! deploy_infrastructure; then
            # With --no-rollback, instances may still be running.
            # Try to fetch outputs and collect logs before giving up.
            log_info "Deploy failed — attempting to collect logs from surviving instances..."
            fetch_stack_outputs 2>/dev/null || true
            if [[ -n "${SENDER_INSTANCE_ID:-}" || -n "${RECEIVER_INSTANCE_ID:-}" ]]; then
                # Wait briefly for SSM to become available
                wait_for_ssm_readiness 2>/dev/null || true
            fi
            fail_with_logs "deploy_infrastructure" "Infrastructure deployment failed"
            teardown_infrastructure
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

    # Post deploy status to PR
    post_pr_comment "## [CI] Stage: Deploy
Infrastructure ready.
- Sender: \`$SENDER_INSTANCE_ID\` (DPDK ENI: $SENDER_DPDK_ENI_IP)
- Receiver: \`$RECEIVER_INSTANCE_ID\` (DPDK ENI: $RECEIVER_DPDK_ENI_IP)
- Both instances SSM-ready."

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

    # Unbind ENIs between tier 1 and tier 2
    if [[ -z "$TIER_FILTER" ]]; then
        unbind_all_enis
    fi

    if [[ -z "$TIER_FILTER" || "$TIER_FILTER" == "2" ]]; then
        run_tier2 || true
    fi

    # Unbind ENIs between tier 2 and tier 3
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

    # Post final summary to PR
    local summary_body="## [CI] Stage: Summary\n"
    if [[ "$TEST_EXIT_CODE" -eq 0 ]]; then
        summary_body+="All tests **PASSED**."
    else
        summary_body+="Some tests **FAILED** (exit code: $TEST_EXIT_CODE)."
    fi
    summary_body+="\n\nARP seeding: kernel /proc/net/arp (automatic)"
    # Include JUnit results summary
    for xml_file in "$RESULTS_DIR"/*.xml; do
        [[ -f "$xml_file" ]] || continue
        local suite_name tests failures
        suite_name=$(sed -n 's/.*name="\([^"]*\)".*/\1/p' "$xml_file" | head -1)
        tests=$(sed -n 's/.*tests="\([^"]*\)".*/\1/p' "$xml_file" | head -1)
        failures=$(sed -n 's/.*failures="\([^"]*\)".*/\1/p' "$xml_file" | head -1)
        summary_body+="\n- **${suite_name:-unknown}**: ${tests:-0} tests, ${failures:-0} failures"
    done
    post_pr_comment "$(echo -e "$summary_body")"

    # Step 9: Teardown
    teardown_infrastructure

    log_info "Exiting with code: $TEST_EXIT_CODE"
    exit "$TEST_EXIT_CODE"
}

main "$@"
