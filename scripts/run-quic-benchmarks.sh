#!/usr/bin/env bash
# run-quic-benchmarks.sh - Run QUIC benchmarks on EC2 with DPDK
#
# Deploys an EC2 instance, builds the quic-bench binary, runs it with both
# --provider=stock and --provider=native-dpdk, and collects results.
#
# Usage:
#   ./scripts/run-quic-benchmarks.sh [--teardown] [--skip-deploy]
#
# Exit codes:
#   0 = benchmarks completed
#   1 = benchmark execution failure
#   2 = infrastructure failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CDK_DIR="$REPO_ROOT/deploy/cdk"

# ── Configuration ────────────────────────────────────────────────────────────

SSM_READINESS_TIMEOUT=600
CDK_STACK_NAME="${CDK_STACK_NAME:-DpdkTestStack}"
SSM_POLL_INTERVAL=15
RESULTS_DIR="$REPO_ROOT/perf-results"
LOGS_DIR="$REPO_ROOT/instance-logs"

BENCH_DURATION=10
BENCH_STREAMS=4
BENCH_PAYLOAD=1048576  # 1 MB

# ── CLI parsing ──────────────────────────────────────────────────────────────

FLAG_TEARDOWN=false
FLAG_SKIP_DEPLOY=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --teardown)      FLAG_TEARDOWN=true;      shift ;;
        --skip-deploy)   FLAG_SKIP_DEPLOY=true;   shift ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done

# ── Logging ──────────────────────────────────────────────────────────────────

log_info() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] INFO: $*"; }
log_error() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ERROR: $*" >&2; }

# ── State ────────────────────────────────────────────────────────────────────

# We use the sender instance for benchmarks (only one instance needed)
BENCH_INSTANCE_ID=""

# ── Deploy ───────────────────────────────────────────────────────────────────

deploy_stack() {
    log_info "Deploying CDK stack: $CDK_STACK_NAME"
    cd "$CDK_DIR"

    local cdk_args=("--require-approval" "never")
    if [[ -n "${DPDK_AMI_ID:-}" ]]; then
        cdk_args+=("-c" "amiId=${DPDK_AMI_ID}")
    fi

    npx cdk deploy "$CDK_STACK_NAME" "${cdk_args[@]}" 2>&1 | tail -20

    local cdk_outputs
    cdk_outputs=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs" --output json 2>/dev/null || echo "[]")

    BENCH_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "
import json, sys
outputs = json.load(sys.stdin)
for o in outputs:
    if o['OutputKey'] == 'SenderInstanceId':
        print(o['OutputValue']); break
" 2>/dev/null || echo "")

    if [[ -z "$BENCH_INSTANCE_ID" ]]; then
        log_error "Could not extract SenderInstanceId from CDK outputs"
        exit 2
    fi

    log_info "Benchmark instance: $BENCH_INSTANCE_ID"
    cd "$REPO_ROOT"
}

# ── SSM helpers ──────────────────────────────────────────────────────────────

wait_for_ssm() {
    local instance_id="$1"
    log_info "Waiting for SSM readiness on $instance_id..."
    local waited=0
    while true; do
        local status
        status=$(aws ssm describe-instance-information \
            --filters "Key=InstanceIds,Values=$instance_id" \
            --query "InstanceInformationList[0].PingStatus" \
            --output text 2>/dev/null || echo "")
        if [[ "$status" == "Online" ]]; then
            log_info "SSM is online"
            return 0
        fi
        sleep "$SSM_POLL_INTERVAL"
        waited=$((waited + SSM_POLL_INTERVAL))
        if [[ $waited -ge $SSM_READINESS_TIMEOUT ]]; then
            log_error "Instance did not become SSM-ready within ${SSM_READINESS_TIMEOUT}s"
            exit 2
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

    [[ -n "$cmd_id" ]] || return 1

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
                aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "StandardErrorContent" --output text 2>/dev/null >&2 || true
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

# ── Run benchmarks ───────────────────────────────────────────────────────────

run_benchmarks() {
    mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

    local bench_binary="/opt/dpdk-stdlib/target/release/quic-bench"

    # Run stock provider benchmark
    log_info "Running QUIC benchmark: --provider=stock"
    local stock_output
    stock_output=$(ssm_run_get_output "$BENCH_INSTANCE_ID" \
        "$bench_binary --provider=stock --duration=$BENCH_DURATION --streams=$BENCH_STREAMS --payload-size=$BENCH_PAYLOAD" \
        120) || {
        log_error "Stock benchmark failed"
        echo "BENCHMARK FAILED" > "$RESULTS_DIR/quic-bench-stock.txt"
        stock_output=""
    }

    if [[ -n "$stock_output" ]]; then
        echo "$stock_output" > "$RESULTS_DIR/quic-bench-stock.txt"
        log_info "Stock benchmark results saved"
    fi

    # Run native-dpdk provider benchmark
    log_info "Running QUIC benchmark: --provider=native-dpdk"
    local dpdk_output
    dpdk_output=$(ssm_run_get_output "$BENCH_INSTANCE_ID" \
        "$bench_binary --provider=native-dpdk --duration=$BENCH_DURATION --streams=$BENCH_STREAMS --payload-size=$BENCH_PAYLOAD" \
        120) || {
        log_error "Native DPDK benchmark failed"
        echo "BENCHMARK FAILED" > "$RESULTS_DIR/quic-bench-native-dpdk.txt"
        dpdk_output=""
    }

    if [[ -n "$dpdk_output" ]]; then
        echo "$dpdk_output" > "$RESULTS_DIR/quic-bench-native-dpdk.txt"
        log_info "Native DPDK benchmark results saved"
    fi

    # Post comparative summary to PR
    if [[ -n "${PR_NUMBER:-}" && -n "$stock_output" && -n "$dpdk_output" ]]; then
        local report="/tmp/quic-bench-summary.md"
        cat > "$report" <<EOF
## QUIC Benchmark Comparison

**Config**: duration=${BENCH_DURATION}s, streams=${BENCH_STREAMS}, payload=${BENCH_PAYLOAD}B

### Stock Provider (Tokio I/O)
\`\`\`
$stock_output
\`\`\`

### Native DPDK Provider
\`\`\`
$dpdk_output
\`\`\`
EOF
        gh pr comment "$PR_NUMBER" --body-file "$report" --repo "${GITHUB_REPOSITORY:-}" 2>/dev/null || true
    fi
}

# ── Teardown ─────────────────────────────────────────────────────────────────

teardown() {
    if [[ "$FLAG_TEARDOWN" == "true" && -n "$BENCH_INSTANCE_ID" ]]; then
        log_info "Tearing down CDK stack..."
        cd "$CDK_DIR"
        npx cdk destroy "$CDK_STACK_NAME" --force 2>&1 | tail -5 || true
        cd "$REPO_ROOT"
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    if [[ "$FLAG_SKIP_DEPLOY" != "true" ]]; then
        deploy_stack
    else
        local cf_outputs
        cf_outputs=$(aws cloudformation describe-stacks \
            --stack-name "$CDK_STACK_NAME" \
            --query "Stacks[0].Outputs" --output json 2>/dev/null || echo "[]")
        BENCH_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='SenderInstanceId']" 2>/dev/null || echo "")
    fi

    wait_for_ssm "$BENCH_INSTANCE_ID"
    run_benchmarks
    teardown
    log_info "QUIC benchmarks complete"
}

trap teardown EXIT
main
