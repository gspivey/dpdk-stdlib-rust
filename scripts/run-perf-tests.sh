#!/usr/bin/env bash
# =============================================================================
# run-perf-tests.sh — Performance test orchestrator for dpdk-stdlib-rust
#
# Deploys a TRex generator + DUT instance, runs UDP echo benchmarks across
# 3 configurations (rust-dpdk, native-dpdk, plain-rust),
# collects structured JSON results, and posts a summary to the PR.
#
# Usage:
#   ./scripts/run-perf-tests.sh [OPTIONS]
#
# Options:
#   --teardown          Destroy CDK stack when done (default: true)
#   --skip-deploy       Skip CDK deploy (reuse existing stack)
#   --packet-sizes      Comma-separated sizes (default: 64,512,1400)
#   --duration          Seconds per rate step (default: 30)
#   --rate-steps        Comma-separated target PPS values (default: 70000,140000,350000,700000)
#   --configs           Comma-separated DUT configs (default: plain-rust,rust-dpdk,native-dpdk)
#   --json-summary      Write JSON summary file
#   -h, --help          Show help
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Defaults ──────────────────────────────────────────────────────────────────

TEARDOWN=true
SKIP_DEPLOY=false
PACKET_SIZES="64,512,1400,8500"
DURATION=30
RATE_STEPS="70000,140000,350000,700000"
# Kernel configs first (NIC starts in kernel mode from boot), then DPDK configs.
# This minimizes NIC rebinding — only one kernel→vfio-pci transition needed.
CONFIGS="plain-rust,rust-dpdk,tokio-dpdk,native-dpdk"
JSON_SUMMARY=false

CDK_STACK_NAME="${CDK_STACK_NAME:-PerfTestStack}"
CDK_DIR="$REPO_ROOT/deploy/cdk"
RESULTS_DIR="$REPO_ROOT/perf-results"
LOGS_DIR="$REPO_ROOT/instance-logs"
# Exported so the inline python in aggregate_results / generate_markdown_summary
# can find both directories without hard-coding paths.
export RESULTS_DIR LOGS_DIR

SSM_READINESS_TIMEOUT=600
TREX_START_TIMEOUT=120
BENCHMARK_TIMEOUT=600

TREX_INSTANCE_ID=""
DUT_INSTANCE_ID=""
TREX_DATA_ENI_IP=""       # TX ENI (device-number 1, PCI 0000:00:06.0)
TREX_DATA_RX_ENI_IP=""    # RX ENI (device-number 2, PCI 0000:00:07.0)
DUT_DATA_ENI_IP=""
TREX_GATEWAY_MAC=""
TREX_DATA_MAC=""          # TX ENI MAC
TREX_DATA_RX_MAC=""       # RX ENI MAC

# ── CLI Parsing ───────────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --teardown)       TEARDOWN=true; shift ;;
        --no-teardown)    TEARDOWN=false; shift ;;
        --skip-deploy)    SKIP_DEPLOY=true; shift ;;
        --packet-sizes)   PACKET_SIZES="$2"; shift 2 ;;
        --duration)       DURATION="$2"; shift 2 ;;
        --rate-steps)     RATE_STEPS="$2"; shift 2 ;;
        --configs)        CONFIGS="$2"; shift 2 ;;
        --json-summary)   JSON_SUMMARY=true; shift ;;
        -h|--help)
            head -25 "$0" | grep -E '^#' | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Logging ───────────────────────────────────────────────────────────────────

log_info()  { echo "[$(date -u +%H:%M:%S)] INFO  $*"; }
log_warn()  { echo "[$(date -u +%H:%M:%S)] WARN  $*" >&2; }
log_error() { echo "[$(date -u +%H:%M:%S)] ERROR $*" >&2; }

# ── PR Comment Helper ─────────────────────────────────────────────────────────

post_pr_comment() {
    local body="$1"
    local pr_number="${PR_NUMBER:-}"

    command -v gh >/dev/null 2>&1 || return 0
    [[ -n "${GH_TOKEN:-}" ]] || return 0

    if [[ -z "$pr_number" && -n "${GITHUB_HEAD_REF:-}" ]]; then
        pr_number=$(gh pr list --head "$GITHUB_HEAD_REF" --json number --jq '.[0].number' \
            --repo "${GITHUB_REPOSITORY:-gspivey/dpdk-stdlib-rust}" 2>/dev/null || echo "")
    fi

    if [[ -n "$pr_number" ]]; then
        gh pr comment "$pr_number" --body "$body" \
            --repo "${GITHUB_REPOSITORY:-gspivey/dpdk-stdlib-rust}" 2>/dev/null || true
    fi
}

# ── SSM Helpers ───────────────────────────────────────────────────────────────

ssm_run_command() {
    local instance_id="$1"
    local timeout_sec="$2"
    shift 2
    local command="$*"

    # JSON-escape the command string (handle double quotes and backslashes)
    local escaped_command
    escaped_command=$(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$command")

    # Retry send-command up to 3 times with backoff (handles SSM throttling)
    local cmd_id=""
    local send_err=""
    local retry
    for retry in 1 2 3; do
        send_err=$(aws ssm send-command \
            --instance-ids "$instance_id" \
            --document-name "AWS-RunShellScript" \
            --parameters "{\"commands\":[${escaped_command}]}" \
            --timeout-seconds "$timeout_sec" \
            --query "Command.CommandId" \
            --output text 2>&1)
        local send_exit=$?
        if [[ $send_exit -eq 0 && -n "$send_err" && "$send_err" != *"error"* && "$send_err" != *"Error"* ]]; then
            cmd_id="$send_err"
            break
        fi
        log_warn "SSM send-command attempt $retry failed for $instance_id: $send_err"
        sleep $((retry * 3))
    done

    if [[ -z "$cmd_id" ]]; then
        log_error "Failed to send SSM command to $instance_id after 3 attempts: $send_err"
        return 1
    fi

    # Wait for completion
    local elapsed=0
    local completed=false
    while [[ $elapsed -lt $timeout_sec ]]; do
        local status
        status=$(aws ssm get-command-invocation \
            --command-id "$cmd_id" \
            --instance-id "$instance_id" \
            --query "Status" \
            --output text 2>/dev/null || echo "Pending")

        case "$status" in
            Success)
                completed=true
                break
                ;;
            Failed|Cancelled|TimedOut)
                log_error "SSM command $cmd_id on $instance_id: $status"
                # Output both stdout and stderr for diagnostics
                local ssm_stdout ssm_stderr
                ssm_stdout=$(aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "StandardOutputContent" \
                    --output text 2>/dev/null || echo "(no stdout)")
                ssm_stderr=$(aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "StandardErrorContent" \
                    --output text 2>/dev/null || echo "(no stderr)")
                log_error "SSM stdout: $ssm_stdout"
                log_error "SSM stderr: $ssm_stderr"
                # Also output stdout so callers in $() can see it
                echo "$ssm_stdout"
                return 1
                ;;
        esac
        sleep 5
        elapsed=$((elapsed + 5))
    done

    if [[ "$completed" != "true" ]]; then
        log_error "SSM command $cmd_id timed out after ${timeout_sec}s (polling)"
        return 1
    fi

    # Return stdout
    aws ssm get-command-invocation \
        --command-id "$cmd_id" \
        --instance-id "$instance_id" \
        --query "StandardOutputContent" \
        --output text 2>/dev/null
}

ssm_run_command_fire_and_forget() {
    local instance_id="$1"
    local timeout_sec="$2"
    shift 2
    local command="$*"

    local escaped_command
    escaped_command=$(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$command")

    aws ssm send-command \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters "{\"commands\":[${escaped_command}]}" \
        --timeout-seconds "$timeout_sec" \
        --query "Command.CommandId" \
        --output text 2>/dev/null
}

wait_ssm_ready() {
    local instance_id="$1"
    local label="$2"
    local elapsed=0

    log_info "Waiting for $label ($instance_id) SSM readiness..."
    while [[ $elapsed -lt $SSM_READINESS_TIMEOUT ]]; do
        local status
        status=$(aws ssm describe-instance-information \
            --filters "Key=InstanceIds,Values=$instance_id" \
            --query "InstanceInformationList[0].PingStatus" \
            --output text 2>/dev/null || echo "None")

        if [[ "$status" == "Online" ]]; then
            log_info "$label SSM ready (${elapsed}s)"
            return 0
        fi
        sleep 15
        elapsed=$((elapsed + 15))
    done

    log_error "$label SSM not ready after ${SSM_READINESS_TIMEOUT}s"
    return 1
}

# ── Failure JSON ──────────────────────────────────────────────────────────────

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
    "trex_instance_id": """${TREX_INSTANCE_ID:-}""",
    "dut_instance_id": """${DUT_INSTANCE_ID:-}""",
    "commit": """${GITHUB_SHA:-unknown}""",
    "run_url": "${GITHUB_SERVER_URL:-}/${GITHUB_REPOSITORY:-}/actions/runs/${GITHUB_RUN_ID:-}",
}
with open("$LOGS_DIR/failure-summary.json", "w") as f:
    json.dump(data, f, indent=2)
PYEOF
}

# ── Environment & Diagnostics Collection ──────────────────────────────────────

collect_environment_info() {
    local instance_id="$1"
    local label="$2"

    log_info "Collecting environment info from $label..."
    local env_cmd="echo '=== System Info ===';
echo \"Hostname: \$(hostname)\";
echo \"Instance type: \$(curl -s -H \"X-aws-ec2-metadata-token: \$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600)\" http://169.254.169.254/latest/meta-data/instance-type)\";
echo \"AZ: \$(curl -s -H \"X-aws-ec2-metadata-token: \$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600)\" http://169.254.169.254/latest/meta-data/placement/availability-zone)\";
echo \"Kernel: \$(uname -r)\";
echo \"CPUs: \$(nproc)\";
echo \"Memory: \$(free -h | grep Mem | awk '{print \$2}')\";
echo \"Hugepages: \$(cat /proc/meminfo | grep HugePages_Total)\";
echo '=== PCI Devices ===';
lspci | grep -i eth 2>/dev/null || echo 'none';
echo '=== DPDK Bind Status ===';
/usr/local/bin/dpdk-devbind.py --status 2>/dev/null || echo 'devbind not available';
echo '=== Network Interfaces ===';
ip addr show 2>/dev/null || echo 'unavailable';
echo '=== Loaded Modules ===';
lsmod | grep -E '(vfio|uio|ena)' 2>/dev/null || echo 'none';
echo '=== NUMA Info ===';
numactl --hardware 2>/dev/null || echo 'numactl not available'"

    local output
    output=$(ssm_run_command "$instance_id" 30 "$env_cmd" 2>/dev/null || echo "(failed to collect)")
    mkdir -p "$LOGS_DIR"
    echo "$output" > "$LOGS_DIR/${label}-environment.txt"
    log_info "Saved $label environment info"
}

collect_instance_logs() {
    local instance_id="$1"
    local label="$2"

    log_info "Collecting logs from $label..."
    mkdir -p "$LOGS_DIR"

    # Console output first (no SSM needed, survives instance termination)
    aws ec2 get-console-output \
        --instance-id "$instance_id" \
        --latest \
        --query "Output" \
        --output text > "$LOGS_DIR/${label}-console-output.log" 2>/dev/null || true

    # Check SSM availability
    local ssm_ready
    ssm_ready=$(aws ssm describe-instance-information \
        --filters "Key=InstanceIds,Values=${instance_id}" \
        --query "InstanceInformationList[0].InstanceId" \
        --output text 2>/dev/null || echo "")

    if [[ -z "$ssm_ready" || "$ssm_ready" == "None" ]]; then
        log_info "  SSM not available for ${label} — relying on console output only"
        return 0
    fi

    # Batch 1: user-data log + app logs
    local batch1_cmd="echo '===FILE:user-data.log==='; tail -200 /var/log/user-data.log 2>/dev/null || echo '(not found)'; echo '===FILE:trex-server.log==='; tail -100 /var/log/trex-server.log 2>/dev/null || echo '(not found)'; echo '===FILE:echo-rust-dpdk.log==='; tail -80 /var/log/echo-rust-dpdk.log 2>/dev/null || echo '(not found)'; echo '===FILE:echo-rust-stdlib.log==='; tail -80 /var/log/echo-rust-stdlib.log 2>/dev/null || echo '(not found)'; echo '===FILE:testpmd.log==='; tail -80 /var/log/testpmd.log 2>/dev/null || echo '(not found)'; echo '===FILE:plain-echo.log==='; tail -80 /var/log/plain-echo.log 2>/dev/null || echo '(not found)'"

    local batch1_output
    batch1_output=$(ssm_run_command "$instance_id" 30 "$batch1_cmd" 2>/dev/null || echo "(failed)")
    if [[ -n "$batch1_output" && "$batch1_output" != "(failed)" ]]; then
        local current_file="" current_content=""
        while IFS= read -r line; do
            if [[ "$line" =~ ^===FILE:(.+)=== ]]; then
                if [[ -n "$current_file" && -n "$current_content" && "$current_content" != "(not found)" ]]; then
                    echo "$current_content" > "$LOGS_DIR/${label}-${current_file}"
                    log_info "  Saved: ${label}-${current_file}"
                fi
                current_file="${BASH_REMATCH[1]}"
                current_content=""
            else
                [[ -n "$current_content" ]] && current_content+=$'\n'"$line" || current_content="$line"
            fi
        done <<< "$batch1_output"
        if [[ -n "$current_file" && -n "$current_content" && "$current_content" != "(not found)" ]]; then
            echo "$current_content" > "$LOGS_DIR/${label}-${current_file}"
            log_info "  Saved: ${label}-${current_file}"
        fi
    fi

    sleep 2

    # Batch 2: network state + crash diagnostics + build listing
    local batch2_cmd="echo '===FILE:network-interfaces.log==='; ip addr show 2>/dev/null || echo unavailable; echo '===FILE:dmesg-crashes.log==='; dmesg | grep -iE 'segfault|page.fault|general.protection|trap |panic|oom|killed process|echo-server|t-rex|testpmd|plain-echo' | tail -50 2>/dev/null || echo 'no crash entries'; echo '===FILE:build-listing.log==='; ls -la /opt/dpdk-stdlib/target/release/ 2>/dev/null || echo 'no build dir'; echo '===FILE:crash-reports.log==='; find /var/crash /var/lib/systemd/coredump -type f -newer /proc/1/fd/0 2>/dev/null | head -20 || echo 'no crash reports'; echo '===FILE:coredump-listing.log==='; coredumpctl list 2>/dev/null | tail -10 || echo 'no coredumps'"

    local batch2_output
    batch2_output=$(ssm_run_command "$instance_id" 30 "$batch2_cmd" 2>/dev/null || echo "(failed)")
    if [[ -n "$batch2_output" && "$batch2_output" != "(failed)" ]]; then
        local current_file="" current_content=""
        while IFS= read -r line; do
            if [[ "$line" =~ ^===FILE:(.+)=== ]]; then
                if [[ -n "$current_file" && -n "$current_content" ]]; then
                    echo "$current_content" > "$LOGS_DIR/${label}-${current_file}"
                    log_info "  Saved: ${label}-${current_file}"
                fi
                current_file="${BASH_REMATCH[1]}"
                current_content=""
            else
                [[ -n "$current_content" ]] && current_content+=$'\n'"$line" || current_content="$line"
            fi
        done <<< "$batch2_output"
        if [[ -n "$current_file" && -n "$current_content" ]]; then
            echo "$current_content" > "$LOGS_DIR/${label}-${current_file}"
            log_info "  Saved: ${label}-${current_file}"
        fi
    fi

    log_info "Instance logs collected for ${label}"
}

collect_networking_diagnostics() {
    local instance_id="$1"
    local label="$2"
    local phase="$3"  # "baseline" or "failure"

    log_info "Collecting $phase networking diagnostics from $label..."
    local diag_cmd="echo '=== Interface State ===';
ip addr show 2>/dev/null;
echo '=== ARP Table ===';
ip neigh show 2>/dev/null;
echo '=== Routes ===';
ip route show 2>/dev/null;
echo '=== DPDK Bind ===';
/usr/local/bin/dpdk-devbind.py --status 2>/dev/null || echo 'unavailable';
echo '=== Ethtool Stats (ens6) ===';
ethtool -S ens6 2>/dev/null | head -30 || echo 'unavailable';
echo '=== Processes ===';
ps aux | grep -E '(echo|testpmd|t-rex|plain-echo)' | grep -v grep || echo 'none'"

    local output
    output=$(ssm_run_command "$instance_id" 30 "$diag_cmd" 2>/dev/null || echo "(failed)")
    mkdir -p "$LOGS_DIR"
    echo "$output" > "$LOGS_DIR/${label}-networking-diag-${phase}.txt"
}

# ── ENI Binding (post-SSM) ────────────────────────────────────────────────────

wait_and_bind_eni() {
    local instance_id="$1"
    local label="$2"
    local driver="$3"  # "vfio-pci" or "ena"

    log_info "Ensuring secondary ENI is attached and bound to $driver on $label..."
    local output
    output=$(ssm_run_command "$instance_id" 120 \
        "for i in \$(seq 1 60); do TOKEN=\$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); MACS=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/); for mac in \$MACS; do DN=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/\${mac}device-number); if [ \"\$DN\" = \"1\" ]; then echo \"ENI_FOUND mac=\${mac%/}\"; if [ \"$driver\" = \"vfio-pci\" ]; then ip link set ens6 down 2>/dev/null || true; /usr/local/bin/dpdk-devbind.py --bind=vfio-pci 0000:00:06.0 2>/dev/null || dpdk-devbind.py --bind=vfio-pci 0000:00:06.0 2>/dev/null || echo BIND_SKIP; else /usr/local/bin/dpdk-devbind.py --bind=ena 0000:00:06.0 2>/dev/null || true; sleep 1; ip link set ens6 up 2>/dev/null || true; fi; echo DONE; exit 0; fi; done; sleep 2; done; echo ENI_TIMEOUT" 2>/dev/null || echo "SSM_FAILED")

    if [[ "$output" == *"ENI_TIMEOUT"* ]]; then
        log_error "$label secondary ENI not found after 120s"
        return 1
    fi
    if [[ "$output" == *"SSM_FAILED"* ]]; then
        log_error "SSM command failed on $label"
        return 1
    fi
    log_info "$label ENI binding result: $(echo "$output" | head -3)"
}

wait_for_trex_rx_eni() {
    # Wait for TRex RX ENI (device-number 2, PCI 0000:00:07.0) to be attached.
    # This is a separate CloudFormation resource and may attach after the TX ENI.
    local instance_id="$1"
    log_info "Waiting for TRex RX ENI (device-number 2) to attach..."
    local output
    output=$(ssm_run_command "$instance_id" 120 \
        "for i in \$(seq 1 60); do TOKEN=\$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); MACS=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/); for mac in \$MACS; do DN=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/\${mac}device-number); if [ \"\$DN\" = \"2\" ]; then echo \"RX_ENI_FOUND mac=\${mac%/}\"; echo DONE; exit 0; fi; done; if [ \$((i % 10)) -eq 0 ]; then echo \"Attempt \$i: waiting for RX ENI (device-number 2)...\"; fi; sleep 2; done; echo RX_ENI_TIMEOUT") || echo "SSM_FAILED"

    if [[ "$output" == *"RX_ENI_TIMEOUT"* ]]; then
        log_error "TRex RX ENI (device-number 2) not found after 120s"
        return 1
    fi
    if [[ "$output" == *"SSM_FAILED"* ]]; then
        log_error "SSM command failed waiting for TRex RX ENI"
        return 1
    fi
    log_info "TRex RX ENI found: $(echo "$output" | grep RX_ENI_FOUND | head -1)"
}

# ── DUT NIC Management ────────────────────────────────────────────────────────

dut_bind_dpdk() {
    log_info "Binding DUT secondary ENI to vfio-pci (DPDK mode)..."
    # Gracefully stop any running DPDK/echo apps first — SIGTERM lets DPDK run
    # rte_eal_cleanup() so vfio-pci devices are properly released.
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "set +e; pkill -TERM -f 'target/release/echo' 2>/dev/null; pkill -TERM -f 'target/release/plain-echo' 2>/dev/null; pkill -TERM -f testpmd 2>/dev/null; pkill -TERM -f dpdk-testpmd 2>/dev/null; for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f 'target/release/echo|target/release/plain-echo|testpmd' >/dev/null 2>&1 || break; sleep 1; done; if pgrep -f 'target/release/echo|target/release/plain-echo|testpmd' >/dev/null 2>&1; then pkill -9 -f 'target/release/echo' 2>/dev/null; pkill -9 -f 'target/release/plain-echo' 2>/dev/null; pkill -9 -f testpmd 2>/dev/null; pkill -9 -f dpdk-testpmd 2>/dev/null; sleep 3; fi; echo CLEANUP_DONE" || true

    # Use sysfs driver_override — same method that works for TRex binding.
    # This avoids dpdk-devbind.py which can fail in edge cases.
    local bind_out
    bind_out=$(ssm_run_command "$DUT_INSTANCE_ID" 60 \
        "set +e; CUR_DRV=\$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); echo PRE_STATE: driver=\$CUR_DRV; if [ \"\$CUR_DRV\" = 'vfio-pci' ]; then echo ALREADY_BOUND_TO_VFIO; echo BIND_OK; exit 0; fi; modprobe vfio-pci 2>/dev/null; echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null; IFACE=\$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); if [ -n \"\$IFACE\" ]; then echo BRINGING_DOWN: \$IFACE; ip link set \$IFACE down 2>/dev/null; fi; echo UNBINDING...; echo 0000:00:06.0 > /sys/bus/pci/devices/0000:00:06.0/driver/unbind 2>&1 || echo UNBIND_RESULT: \$?; sleep 2; echo SETTING_OVERRIDE...; echo vfio-pci > /sys/bus/pci/devices/0000:00:06.0/driver_override 2>&1 || echo OVERRIDE_RESULT: \$?; echo BINDING...; echo 0000:00:06.0 > /sys/bus/pci/drivers/vfio-pci/bind 2>&1 || echo BIND_RESULT: \$?; sleep 1; DRV=\$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); echo DRIVER: \$DRV; if [ \"\$DRV\" = 'vfio-pci' ]; then echo BIND_OK; exit 0; else echo BIND_FAILED; exit 1; fi" 2>&1)
    local bind_exit=$?
    log_info "dut_bind_dpdk result (exit=$bind_exit): $bind_out"
    if [[ $bind_exit -ne 0 ]]; then
        log_error "Failed to bind DUT ENI to vfio-pci: $bind_out"
        return 1
    fi
}

dut_bind_kernel() {
    log_info "Binding DUT secondary ENI to kernel driver (kernel mode)..."
    # Gracefully stop any running DPDK/echo apps — SIGTERM lets DPDK cleanup run.
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "set +e; pkill -TERM -f 'target/release/echo' 2>/dev/null; pkill -TERM -f 'target/release/plain-echo' 2>/dev/null; pkill -TERM -f testpmd 2>/dev/null; pkill -TERM -f dpdk-testpmd 2>/dev/null; for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f 'target/release/echo|target/release/plain-echo|testpmd' >/dev/null 2>&1 || break; sleep 1; done; if pgrep -f 'target/release/echo|target/release/plain-echo|testpmd' >/dev/null 2>&1; then pkill -9 -f 'target/release/echo' 2>/dev/null; pkill -9 -f 'target/release/plain-echo' 2>/dev/null; pkill -9 -f testpmd 2>/dev/null; pkill -9 -f dpdk-testpmd 2>/dev/null; sleep 3; fi; rm -rf /var/run/dpdk/ 2>/dev/null; echo CLEANUP_DONE" || true

    local bind_out
    bind_out=$(ssm_run_command "$DUT_INSTANCE_ID" 60 \
        "set +e; CUR_DRV=\$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); echo PRE_STATE: driver=\$CUR_DRV; if [ \"\$CUR_DRV\" = 'ena' ]; then echo ALREADY_BOUND_TO_ENA; IFACE=\$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); echo IFACE: \$IFACE; if [ -n \"\$IFACE\" ]; then ip link set \$IFACE up 2>/dev/null; ip link set \$IFACE mtu 9001 2>/dev/null; echo MTU: \$(cat /sys/class/net/\$IFACE/mtu 2>/dev/null); ip addr add ${DUT_DATA_ENI_IP}/24 dev \$IFACE 2>/dev/null; ip addr show \$IFACE 2>/dev/null; fi; echo BIND_OK; exit 0; fi; echo UNBINDING...; echo 0000:00:06.0 > /sys/bus/pci/devices/0000:00:06.0/driver/unbind 2>&1 || echo UNBIND_RESULT: \$?; sleep 2; echo CLEARING_OVERRIDE...; echo '' > /sys/bus/pci/devices/0000:00:06.0/driver_override 2>&1 || echo OVERRIDE_RESULT: \$?; echo BINDING_ENA...; echo 0000:00:06.0 > /sys/bus/pci/drivers/ena/bind 2>&1 || echo BIND_RESULT: \$?; sleep 3; IFACE=\$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); echo IFACE: \$IFACE; if [ -n \"\$IFACE\" ]; then ip link set \$IFACE up 2>/dev/null; ip link set \$IFACE mtu 9001 2>/dev/null; echo MTU: \$(cat /sys/class/net/\$IFACE/mtu 2>/dev/null); sleep 2; ip addr add ${DUT_DATA_ENI_IP}/24 dev \$IFACE 2>/dev/null; ip route del default dev \$IFACE 2>/dev/null; ip addr show \$IFACE 2>/dev/null; fi; DRV=\$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); echo DRIVER: \$DRV; if [ \"\$DRV\" = 'ena' ]; then echo BIND_OK; exit 0; else echo BIND_FAILED; exit 1; fi" 2>&1)
    local bind_exit=$?
    log_info "dut_bind_kernel result (exit=$bind_exit): $bind_out"
    if [[ $bind_exit -ne 0 ]]; then
        log_error "Failed to bind DUT ENI to kernel: $bind_out"
        return 1
    fi
}

dut_stop_all_apps() {
    log_info "Stopping all DUT applications..."
    local stop_result
    # Use SIGTERM first for graceful shutdown — DPDK apps need to run their
    # cleanup (rte_eal_cleanup via Drop) so vfio-pci devices are properly released.
    # Only escalate to SIGKILL after 10s if the process won't die.
    stop_result=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "set +e; pkill -TERM -f 'target/release/echo' 2>/dev/null; pkill -TERM -f 'target/release/tokio-echo' 2>/dev/null; pkill -TERM -f 'target/release/plain-echo' 2>/dev/null; pkill -TERM -f testpmd 2>/dev/null; pkill -TERM -f dpdk-testpmd 2>/dev/null; for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f 'target/release/echo|target/release/tokio-echo|target/release/plain-echo|testpmd' >/dev/null 2>&1 || break; sleep 1; done; if pgrep -f 'target/release/echo|target/release/tokio-echo|target/release/plain-echo|testpmd' >/dev/null 2>&1; then echo 'SIGTERM did not work, escalating to SIGKILL'; pkill -9 -f 'target/release/echo' 2>/dev/null; pkill -9 -f 'target/release/tokio-echo' 2>/dev/null; pkill -9 -f 'target/release/plain-echo' 2>/dev/null; pkill -9 -f testpmd 2>/dev/null; pkill -9 -f dpdk-testpmd 2>/dev/null; sleep 3; fi; echo 'All apps stopped'") || true
    log_info "Stop result: $stop_result"
    # Clean up stale DPDK shared memory — if the previous app was SIGKILL'd,
    # the shared memory files persist and can cause the next DPDK app to fail
    # or silently malfunction (e.g., attach as secondary process).
    ssm_run_command "$DUT_INSTANCE_ID" 15 \
        "rm -rf /var/run/dpdk/ 2>/dev/null; echo DPDK_STATE_CLEANED" || true
    # Give the system time to finish DPDK cleanup after process exit
    sleep 5
}

# ── TRex Management ──────────────────────────────────────────────────────────

generate_trex_config() {
    log_info "Generating TRex configuration..."

    # TRex has 3 ENIs:
    #   device 0 = ens5 (0000:00:05.0) — Management (kernel, SSM)
    #   device 1 = ens6 (0000:00:06.0) — Data TX (DPDK)
    #   device 2 = ens7 (0000:00:07.0) — Data RX (DPDK)
    local TX_PCI="0000:00:06.0"
    local RX_PCI="0000:00:07.0"
    local TX_BDF="00:06.0"
    local RX_BDF="00:07.0"
    TREX_PCI_ADDR="$TX_PCI"
    TREX_PCI_BDF="$TX_BDF"

    # Step 1: Discover TX and RX ENI MACs via IMDS
    # device-number 1 = TX ENI, device-number 2 = RX ENI
    log_info "Step 1: Discovering TRex data ENI MACs via IMDS..."
    TREX_DATA_MAC=""
    TREX_DATA_RX_MAC=""

    local imds_result
    imds_result=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "TOKEN=\$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); MACS=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/); echo \"ALL_MACS: \$MACS\"; for mac in \$MACS; do mac=\${mac%/}; dn=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/\${mac}/device-number); echo \"MAC=\${mac} DN=\${dn}\"; if [ \"\$dn\" = \"1\" ]; then echo \"TX_MAC: \${mac}\"; fi; if [ \"\$dn\" = \"2\" ]; then echo \"RX_MAC: \${mac}\"; fi; done" || echo "SSM_FAILED")
    log_info "IMDS MAC discovery output: $(echo "$imds_result" | head -10)"

    TREX_DATA_MAC=$(echo "$imds_result" | grep "^TX_MAC:" | head -1 | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' || echo "")
    TREX_DATA_RX_MAC=$(echo "$imds_result" | grep "^RX_MAC:" | head -1 | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' || echo "")

    if [[ -z "$TREX_DATA_MAC" || ! "$TREX_DATA_MAC" =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}$ ]]; then
        log_error "Could not discover TRex TX ENI MAC (got: '$TREX_DATA_MAC')"
        log_error "IMDS output was: $(echo "$imds_result" | head -10)"
        return 1
    fi
    if [[ -z "$TREX_DATA_RX_MAC" || ! "$TREX_DATA_RX_MAC" =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}$ ]]; then
        log_error "Could not discover TRex RX ENI MAC (got: '$TREX_DATA_RX_MAC')"
        log_error "IMDS output was: $(echo "$imds_result" | head -10)"
        return 1
    fi
    log_info "TRex TX MAC: $TREX_DATA_MAC, RX MAC: $TREX_DATA_RX_MAC"

    # Step 2: Discover gateway MAC while TX ENI is still in kernel mode.
    # TRex uses DPDK (raw Ethernet frames), so it needs the L2 destination MAC.
    # AWS VPC is L3-routed: all outbound frames must use the gateway MAC.
    # The kernel already has this from boot DHCP — just read it.
    local subnet_gw
    subnet_gw=$(echo "$TREX_DATA_ENI_IP" | sed 's/\.[0-9]*$/.1/')
    log_info "Step 2: Reading gateway MAC from kernel ARP cache (gw=$subnet_gw)..."

    local tx_iface
    tx_iface=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "ls /sys/bus/pci/devices/$TX_PCI/net/ 2>/dev/null | head -1 || echo ens6" || echo "ens6")
    tx_iface=$(echo "$tx_iface" | tr -d '[:space:]')
    if [[ -z "$tx_iface" ]]; then tx_iface="ens6"; fi

    # Get TX ENI's own MAC so we can reject it if ARP returns it
    local tx_own_mac
    tx_own_mac=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "cat /sys/class/net/$tx_iface/address 2>/dev/null" || echo "")
    tx_own_mac=$(echo "$tx_own_mac" | tr -d '[:space:]')

    TREX_GATEWAY_MAC=""
    local gw_attempt
    for gw_attempt in 1 2 3 4 5; do
        local gw_raw
        gw_raw=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
            "set +e; ip link set $tx_iface up 2>/dev/null; ping -c 2 -W 2 $subnet_gw 2>/dev/null; ip neigh show ${subnet_gw} dev $tx_iface 2>/dev/null" || echo "")

        TREX_GATEWAY_MAC=$(echo "$gw_raw" | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' | head -1)

        # Reject if ARP returned our own MAC (broken ARP cache)
        if [[ -n "$TREX_GATEWAY_MAC" && -n "$tx_own_mac" && "$TREX_GATEWAY_MAC" == "$tx_own_mac" ]]; then
            log_warn "Attempt $gw_attempt: got own MAC, not gateway — retrying..."
            TREX_GATEWAY_MAC=""
        fi

        if [[ -n "$TREX_GATEWAY_MAC" ]]; then break; fi

        # Interface may not have an IP yet — run dhclient as fallback
        log_warn "Gateway MAC not found (attempt $gw_attempt), ensuring interface has IP..."
        ssm_run_command "$TREX_INSTANCE_ID" 30 \
            "set +e; dhclient $tx_iface 2>/dev/null; ip addr add ${TREX_DATA_ENI_IP}/24 dev $tx_iface 2>/dev/null; sleep 3; ping -c 3 -W 2 $subnet_gw 2>/dev/null" || true
        sleep 5
    done

    if [[ -z "$TREX_GATEWAY_MAC" ]]; then
        log_error "Could not discover gateway MAC — packets will be dropped by VPC"
        return 1
    fi
    log_info "Gateway MAC: $TREX_GATEWAY_MAC (own: ${tx_own_mac:-unknown})"

    # Step 3: Bind BOTH data ENIs to vfio-pci
    # Use sysfs driver_override — works without any DPDK tools installed.
    # First verify both PCI devices exist (ENIs must be attached).
    log_info "Step 3: Binding both data ENIs to vfio-pci..."
    local bind_result
    bind_result=$(ssm_run_command "$TREX_INSTANCE_ID" 60 \
        "echo '=== Pre-bind PCI check ==='; ls /sys/bus/pci/devices/$TX_PCI 2>/dev/null && echo PCI_TX_EXISTS || echo PCI_TX_MISSING; ls /sys/bus/pci/devices/$RX_PCI 2>/dev/null && echo PCI_RX_EXISTS || echo PCI_RX_MISSING; modprobe vfio-pci 2>/dev/null || true; echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || true; for PCI in $TX_PCI $RX_PCI; do IFACE=\$(ls /sys/bus/pci/devices/\$PCI/net/ 2>/dev/null | head -1); if [ -n \"\$IFACE\" ]; then ip link set \$IFACE down 2>/dev/null || true; fi; echo \$PCI > /sys/bus/pci/devices/\$PCI/driver/unbind 2>/dev/null || true; sleep 1; echo vfio-pci > /sys/bus/pci/devices/\$PCI/driver_override; echo \$PCI > /sys/bus/pci/drivers/vfio-pci/bind && echo BIND_OK_\$PCI || echo BIND_FAIL_\$PCI; done; echo '=== Post-bind vfio-pci devices ==='; ls /sys/bus/pci/drivers/vfio-pci/ 2>/dev/null | grep -E '00:0[67]' || echo 'NO_VFIO_DEVICES'" || echo "SSM_FAILED")
    log_info "NIC bind result: $bind_result"

    # Verify both ENIs bound successfully
    if [[ "$bind_result" == *"PCI_RX_MISSING"* ]]; then
        log_error "TRex RX ENI PCI device $RX_PCI not found — ENI may not be attached"
        return 1
    fi
    if [[ "$bind_result" != *"BIND_OK_${TX_PCI}"* ]]; then
        log_error "Failed to bind TX ENI $TX_PCI to vfio-pci"
        return 1
    fi
    if [[ "$bind_result" != *"BIND_OK_${RX_PCI}"* ]]; then
        log_error "Failed to bind RX ENI $RX_PCI to vfio-pci"
        return 1
    fi
    log_info "Both ENIs bound to vfio-pci successfully"

    # Step 4: Write /etc/trex_cfg.yaml via SSM
    log_info "Step 4: Writing TRex config (TX: $TX_BDF, RX: $RX_BDF)..."
    local yaml_content
    yaml_content=$(cat <<YAMLEOF
- port_limit: 2
  version: 2
  interfaces: ['${TX_BDF}', '${RX_BDF}']
  port_info:
    - dest_mac: '${TREX_GATEWAY_MAC}'
      src_mac:  '${TREX_DATA_MAC}'
    - dest_mac: '${TREX_GATEWAY_MAC}'
      src_mac:  '${TREX_DATA_RX_MAC}'
  memory:
    mbuf_9k: 4096
YAMLEOF
)
    local yaml_b64
    yaml_b64=$(echo "$yaml_content" | base64 -w0)

    local write_result
    write_result=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "echo $yaml_b64 | base64 -d > /etc/trex_cfg.yaml && echo WROTE || echo WRITE_ERR; cat /etc/trex_cfg.yaml 2>/dev/null || true" || echo "SSM_WRITE_FAILED")
    log_info "Config write result: $(echo "$write_result" | head -10)"

    if [[ "$write_result" == *"SSM_WRITE_FAILED"* ]]; then
        log_error "SSM command to write TRex config failed"
        return 1
    fi
    if [[ "$write_result" != *"WROTE"* ]]; then
        log_error "TRex config write did not succeed (no WROTE marker in output)"
        log_error "Write output: $(echo "$write_result" | head -5)"
        return 1
    fi
}

start_trex_server() {
    log_info "Starting TRex server..."
    local TX_PCI="0000:00:06.0"
    local RX_PCI="0000:00:07.0"

    # Verify both NICs are bound to vfio-pci before starting TRex
    local nic_state
    nic_state=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "echo NIC_STATE:; for p in $TX_PCI $RX_PCI; do readlink /sys/bus/pci/devices/\$p/driver 2>/dev/null | xargs -I{} echo \$p: {}; done; cat /etc/trex_cfg.yaml 2>/dev/null; ls /opt/trex/t-rex-64 2>/dev/null && echo TREX_BINARY_OK || echo TREX_BINARY_MISSING; echo HUGEPAGES:; grep HugePages /proc/meminfo 2>/dev/null; ls /dev/vfio/ 2>/dev/null || echo NO_VFIO_DEV" || echo "SSM_FAILED")
    log_info "Pre-start NIC state: $(echo "$nic_state" | grep -E 'vfio|BINARY|port_limit|src_mac|dest_mac|Huge|NO_VFIO' | head -10)"

    # Verify both NICs are actually on vfio-pci before starting TRex
    if ! echo "$nic_state" | grep -q "$TX_PCI.*vfio-pci"; then
        log_error "TX NIC $TX_PCI is NOT bound to vfio-pci — TRex will fail"
        log_error "Full NIC state: $nic_state"
        return 1
    fi
    if ! echo "$nic_state" | grep -q "$RX_PCI.*vfio-pci"; then
        log_error "RX NIC $RX_PCI is NOT bound to vfio-pci — TRex will fail"
        log_error "Full NIC state: $nic_state"
        return 1
    fi
    log_info "Both NICs confirmed bound to vfio-pci"

    # Ensure hugepages are allocated and mounted (TRex/DPDK requires them)
    log_info "Ensuring hugepages are allocated..."
    ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null || true; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null || true; grep -i huge /proc/meminfo" || true

    # Start TRex via fire-and-forget SSM command.
    # We don't wait for SSM completion because SSM timeouts are too short for
    # TRex DPDK initialization (~20s). Instead we fire the command and then
    # verify TRex is running after a fixed wait.
    log_info "Starting TRex server..."
    local start_cmd_id
    start_cmd_id=$(ssm_run_command_fire_and_forget "$TREX_INSTANCE_ID" 120 \
        "pkill -f t-rex-64 2>/dev/null || true; sleep 1; rm -f /var/run/dpdk/ 2>/dev/null || true; cd /opt/trex && nohup /opt/trex/t-rex-64 -i --cfg /etc/trex_cfg.yaml -c 2 </dev/null >/var/log/trex-server.log 2>&1 & disown")
    log_info "TRex start command sent (cmd_id: ${start_cmd_id:-none})"

    # Wait for TRex to initialize DPDK and start its API server.
    # TRex takes ~15-20s to probe ENA NICs via DPDK.
    log_info "Waiting 45s for TRex to initialize..."
    sleep 45

    # Single verification check
    local check
    check=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "LOG_SIZE=\$(wc -c < /var/log/trex-server.log 2>/dev/null || echo 0); echo LOG_SIZE:\$LOG_SIZE; pgrep -f t-rex >/dev/null 2>&1 && echo PROCESS_FOUND; ss -tlnp 2>/dev/null | grep 4501 && echo API_PORT; if [ \$LOG_SIZE -gt 100 ]; then echo LOG_GROWING; fi; tail -10 /var/log/trex-server.log 2>/dev/null" || echo "SSM_CHECK_FAILED")
    log_info "TRex check: $(echo "$check" | tr '\n' ' ' | head -c 500)"

    if [[ "$check" == *"PROCESS_FOUND"* || "$check" == *"API_PORT"* || "$check" == *"LOG_GROWING"* ]]; then
        log_info "TRex server is running"
        return 0
    fi

    # Retry once after 30s more
    log_info "TRex not yet detected, waiting 30s more..."
    sleep 30
    check=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "LOG_SIZE=\$(wc -c < /var/log/trex-server.log 2>/dev/null || echo 0); echo LOG_SIZE:\$LOG_SIZE; pgrep -f t-rex >/dev/null 2>&1 && echo PROCESS_FOUND; ss -tlnp 2>/dev/null | grep 4501 && echo API_PORT; if [ \$LOG_SIZE -gt 100 ]; then echo LOG_GROWING; fi; tail -10 /var/log/trex-server.log 2>/dev/null" || echo "SSM_CHECK_FAILED")
    log_info "TRex retry check: $(echo "$check" | tr '\n' ' ' | head -c 500)"

    if [[ "$check" == *"PROCESS_FOUND"* || "$check" == *"API_PORT"* || "$check" == *"LOG_GROWING"* ]]; then
        log_info "TRex server is running (after retry)"
        return 0
    fi

    log_error "TRex server failed to start — check: $(echo "$check" | tr '\n' ' ')"
    return 1
}

stop_trex_server() {
    log_info "Stopping TRex server..."
    ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "pkill -9 -f t-rex-64 2>/dev/null || true; sleep 2; pgrep -f t-rex-64 >/dev/null && echo 'WARNING: TRex still running' || echo 'TRex stopped'" 2>/dev/null || true
}

# ── Benchmark Runner ──────────────────────────────────────────────────────────

run_benchmark_for_config() {
    local config_name="$1"
    local dst_port="${2:-9000}"

    log_info "Running TRex benchmark for config: $config_name"

    # Copy benchmark script to TRex instance using base64 encoding.
    # Heredoc deployment was silently deploying stale content; base64
    # avoids shell quoting/escaping issues entirely.
    local benchmark_b64
    benchmark_b64=$(base64 -w0 "$SCRIPT_DIR/perf-tests/trex/run_benchmark.py")

    local deploy_out
    deploy_out=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "set +e; mkdir -p /opt/perf-tests; echo '$benchmark_b64' | base64 -d > /opt/perf-tests/run_benchmark.py; chmod +x /opt/perf-tests/run_benchmark.py; echo DEPLOYED_SIZE=\$(wc -c < /opt/perf-tests/run_benchmark.py) LINES=\$(wc -l < /opt/perf-tests/run_benchmark.py); echo WAIT_ON_TRAFFIC_COUNT=\$(grep -c 'wait_on_traffic' /opt/perf-tests/run_benchmark.py); echo LINE77=\$(sed -n '77p' /opt/perf-tests/run_benchmark.py); echo DEPLOY_OK") || {
        log_error "Failed to copy benchmark script to TRex"
        return 1
    }
    log_info "Script deploy result: $deploy_out"

    # Use the gateway MAC discovered during generate_trex_config
    log_info "Using gateway MAC: ${TREX_GATEWAY_MAC:-unknown}"
    log_info "Benchmark params: src=${TREX_DATA_ENI_IP} dst=${DUT_DATA_ENI_IP} port=${dst_port} sizes=${PACKET_SIZES} rates=${RATE_STEPS} duration=${DURATION}"

    # Pre-flight: verify TRex API is accessible and benchmark script can import dependencies
    local preflight
    preflight=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "cd /opt/trex && python3 -c \"
import sys
sys.path.insert(0, '/opt/trex/automation/trex_control_plane/interactive')
from trex.stl.api import STLClient
c = STLClient(server='localhost')
c.connect()
info = c.get_server_system_info()
ports = c.get_port_info()
print('TRex API OK: %d ports' % len(ports))
for i, p in enumerate(ports):
    print('  Port %d: %s' % (i, p.get('hw_mac', 'unknown')))
c.disconnect()
print('PREFLIGHT_OK')
\" 2>&1" 2>&1) || true
    log_info "TRex preflight check: $preflight"

    # Post preflight results to PR for visibility (we can't access CI runner logs)
    post_pr_comment "## [Perf] Benchmark Diag: \`$config_name\` preflight
\`\`\`
$preflight
\`\`\`"

    if [[ "$preflight" != *"PREFLIGHT_OK"* ]]; then
        log_error "TRex API preflight failed — benchmark will likely fail"
        return 1
    fi

    # Run benchmark via SSM — capture both stdout and stderr
    local bench_cmd="cd /opt/trex && python3 /opt/perf-tests/run_benchmark.py \
        --server localhost \
        --config-name '$config_name' \
        --src-ip '$TREX_DATA_ENI_IP' \
        --dst-ip '$DUT_DATA_ENI_IP' \
        --dst-mac '${TREX_GATEWAY_MAC}' \
        --dst-port $dst_port \
        --packet-sizes '$PACKET_SIZES' \
        --rate-steps '$RATE_STEPS' \
        --duration $DURATION \
        --output '/tmp/perf-results/${config_name}.json' 2>&1; echo EXIT_CODE=\$?"

    log_info "Starting benchmark SSM command (timeout=${BENCHMARK_TIMEOUT}s)..."
    local output
    output=$(ssm_run_command "$TREX_INSTANCE_ID" "$BENCHMARK_TIMEOUT" "$bench_cmd")
    local ssm_exit_code=$?

    mkdir -p "$LOGS_DIR"
    echo "$output" > "$LOGS_DIR/trex-benchmark-${config_name}.log"
    log_info "Benchmark SSM exit code: $ssm_exit_code"

    # Post benchmark output to PR for visibility
    local output_tail
    output_tail=$(echo "$output" | tail -30)
    post_pr_comment "## [Perf] Benchmark Diag: \`$config_name\` result
SSM exit: $ssm_exit_code
<details><summary>Output (last 30 lines)</summary>

\`\`\`
${output_tail}
\`\`\`
</details>"

    if [[ $ssm_exit_code -ne 0 ]]; then
        log_error "Benchmark SSM command failed for $config_name (exit=$ssm_exit_code)"
        return 1
    fi

    # Check the Python script's exit code from the captured output
    # Use sed instead of grep -P for portability
    local py_exit
    py_exit=$(echo "$output" | sed -n 's/.*EXIT_CODE=\([0-9]*\).*/\1/p' | tail -1)
    log_info "Python exit code for $config_name: '${py_exit:-not found}'"
    if [[ -n "$py_exit" && "$py_exit" != "0" ]]; then
        log_error "Benchmark Python script failed for $config_name (exit=$py_exit)"
        return 1
    fi

    # Download results from TRex instance
    log_info "Downloading results for $config_name..."
    local results_json
    results_json=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "ls -la /tmp/perf-results/ 2>/dev/null; echo '---JSON_START---'; cat /tmp/perf-results/${config_name}.json 2>/dev/null || echo 'FILE_NOT_FOUND'")
    local dl_exit=$?

    log_info "Results download SSM exit: $dl_exit"

    if [[ $dl_exit -ne 0 ]]; then
        log_error "SSM download command failed for $config_name (dl_exit=$dl_exit)"
        return 1
    fi

    if [[ "$results_json" == *"FILE_NOT_FOUND"* ]]; then
        log_error "Results file not found on TRex for $config_name"
        post_pr_comment "## [Perf] Benchmark Diag: \`$config_name\` download
Results file NOT FOUND on TRex instance.
\`\`\`
$(echo "$results_json" | head -10)
\`\`\`"
        return 1
    fi

    # Extract just the JSON part (after the directory listing separator)
    local json_content
    json_content=$(echo "$results_json" | sed -n '/^---JSON_START---$/,$ p' | tail -n +2)

    mkdir -p "$RESULTS_DIR"
    echo "$json_content" > "$RESULTS_DIR/${config_name}.json"

    # Verify the JSON is valid and has results
    local json_check
    json_check=$(python3 -c "
import json, sys
try:
    d = json.load(open('$RESULTS_DIR/${config_name}.json'))
    n = len(d.get('results', {}))
    print(f'Valid JSON: {n} packet sizes, config={d.get(\"config_name\", \"missing\")}')
    if n == 0:
        print('WARNING: No results in JSON')
        sys.exit(1)
except Exception as e:
    print(f'JSON error: {e}')
    sys.exit(1)
" 2>&1)
    local json_valid=$?
    log_info "JSON validation for $config_name: $json_check (exit=$json_valid)"

    if [[ $json_valid -ne 0 ]]; then
        log_error "Invalid/empty JSON results for $config_name"
        post_pr_comment "## [Perf] Benchmark Diag: \`$config_name\` JSON validation
**FAILED**: $json_check
\`\`\`
$(head -5 "$RESULTS_DIR/${config_name}.json")
\`\`\`"
        return 1
    fi

    log_info "Results saved to $RESULTS_DIR/${config_name}.json"
}

# ── DUT Config Runners ────────────────────────────────────────────────────────

start_dut_rust_dpdk() {
    log_info "Starting DUT: rust-dpdk (echo server with DPDK backend)"
    dut_bind_dpdk || return 1

    # Ensure hugepages are set up (process cleanup already done by dut_bind_dpdk)
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "set +e; echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null; echo HUGEPAGES_SETUP_DONE" || true

    # --perf-interval 10 enables PerfReporter so [PERF] lines (rx_pps, rx_drops,
    # rx_buf_drops, latencies) appear in the app log every 10s. The harness tails
    # this log into the perf PR comment so the numbers can be compared to TRex.
    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/echo --ip ${DUT_DATA_ENI_IP} --port 9000 --perf-interval 10 > /var/log/echo-rust-dpdk.log 2>&1 &"
    sleep 15

    # Verify it's running (retry up to 3 times — SSM can be slow)
    local status=""
    local verify_attempt
    for verify_attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            "pgrep -f 'target/release/echo' >/dev/null && echo 'running' || echo 'not running'") || true
        if [[ "$status" == *"running"* ]]; then
            break
        fi
        log_warn "rust-dpdk verify attempt $verify_attempt: status='$status'"
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "rust-dpdk echo server failed to start (status='$status')"
        ssm_run_command "$DUT_INSTANCE_ID" 30 "tail -30 /var/log/echo-rust-dpdk.log 2>/dev/null" || true
        return 1
    fi
    log_info "rust-dpdk echo server running"
}

start_dut_rust_dpdk_multicore() {
    log_info "Starting DUT: rust-dpdk-multicore (echo server with DPDK backend + multi-core pipeline)"
    dut_bind_dpdk || return 1

    # Ensure hugepages are set up (process cleanup already done by dut_bind_dpdk)
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "set +e; echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null; echo HUGEPAGES_SETUP_DONE" || true

    # Launch with --workers 2 to enable multi-core pipeline (2 workers per RX queue)
    # --perf-interval 10 enables instrumentation output every 10s to the log file
    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/echo --ip ${DUT_DATA_ENI_IP} --port 9000 --workers 2 --perf-interval 10 > /var/log/echo-rust-dpdk-multicore.log 2>&1 &"
    sleep 15

    # Verify it's running (retry up to 3 times — SSM can be slow)
    local status=""
    local verify_attempt
    for verify_attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            "pgrep -f 'target/release/echo' >/dev/null && echo 'running' || echo 'not running'") || true
        if [[ "$status" == *"running"* ]]; then
            break
        fi
        log_warn "rust-dpdk-multicore verify attempt $verify_attempt: status='$status'"
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "rust-dpdk-multicore echo server failed to start (status='$status')"
        ssm_run_command "$DUT_INSTANCE_ID" 30 "tail -30 /var/log/echo-rust-dpdk-multicore.log 2>/dev/null" || true
        return 1
    fi
    log_info "rust-dpdk-multicore echo server running"
}

start_dut_tokio_dpdk() {
    log_info "Starting DUT: tokio-dpdk (async tokio-echo with DPDK backend)"
    dut_bind_dpdk || return 1

    # Ensure hugepages are set up (process cleanup already done by dut_bind_dpdk)
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "set +e; echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null; echo HUGEPAGES_SETUP_DONE" || true

    # tokio-echo binary is built with --features dpdk in perf-test-stack.ts so
    # the DPDK backend is selected automatically when DPDK is available.
    # --perf-interval 10 enables PerfReporter so [PERF] lines (rx_pps, rx_drops,
    # rx_buf_drops, latencies) appear in the app log every 10s — same as the
    # rust-dpdk config.
    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/tokio-echo --ip ${DUT_DATA_ENI_IP} --port 9000 --perf-interval 10 > /var/log/echo-tokio-dpdk.log 2>&1 &"
    sleep 15

    # Verify it's running (retry up to 3 times — SSM can be slow)
    local status=""
    local verify_attempt
    for verify_attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            "pgrep -f 'target/release/tokio-echo' >/dev/null && echo 'running' || echo 'not running'") || true
        if [[ "$status" == *"running"* ]]; then
            break
        fi
        log_warn "tokio-dpdk verify attempt $verify_attempt: status='$status'"
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "tokio-dpdk echo server failed to start (status='$status')"
        ssm_run_command "$DUT_INSTANCE_ID" 30 "tail -30 /var/log/echo-tokio-dpdk.log 2>/dev/null" || true
        return 1
    fi
    log_info "tokio-dpdk echo server running"
}

start_dut_native_dpdk() {
    log_info "Starting DUT: native-dpdk (testpmd 5tswap)"
    dut_bind_dpdk || return 1

    # Ensure hugepages are set up (process cleanup already done by dut_bind_dpdk)
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "set +e; echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null; echo HUGEPAGES_SETUP_DONE" || true

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "nohup /usr/local/bin/dpdk-testpmd -l 0-1 -n 4 --file-prefix testpmd -a 0000:00:06.0 -- --forward-mode=5tswap --port-topology=chained --stats-period 10 --auto-start --max-pkt-len=9100 --mbuf-size=10240 > /var/log/testpmd.log 2>&1 &"
    sleep 15

    local status=""
    local verify_attempt
    for verify_attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            "pgrep -f testpmd >/dev/null && echo 'running' || echo 'not running'") || true
        if [[ "$status" == *"running"* ]]; then
            break
        fi
        log_warn "native-dpdk verify attempt $verify_attempt: status='$status'"
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "testpmd failed to start (status='$status')"
        ssm_run_command "$DUT_INSTANCE_ID" 30 "tail -30 /var/log/testpmd.log 2>/dev/null" || true
        return 1
    fi
    log_info "testpmd 5tswap running"
}

start_dut_rust_stdlib() {
    log_info "Starting DUT: rust-stdlib (plain-echo server with kernel backend)"
    dut_bind_kernel || return 1

    # plain-echo uses std::net::UdpSocket (the kernel baseline)
    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/plain-echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/echo-rust-stdlib.log 2>&1 &"
    sleep 10

    local status=""
    local verify_attempt
    for verify_attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            "pgrep -f 'target/release/plain-echo' >/dev/null && echo 'running' || echo 'not running'") || true
        if [[ "$status" == *"running"* ]]; then
            break
        fi
        log_warn "rust-stdlib verify attempt $verify_attempt: status='$status'"
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "rust-stdlib echo server failed to start (status='$status')"
        ssm_run_command "$DUT_INSTANCE_ID" 30 "tail -30 /var/log/echo-rust-stdlib.log 2>/dev/null" || true
        return 1
    fi
    log_info "rust-stdlib echo server running"
}

start_dut_plain_rust() {
    log_info "Starting DUT: plain-rust (minimal std::net echo server)"
    dut_bind_kernel || return 1

    # Capture kernel NIC counters BEFORE starting the echo server. For the
    # DPDK configs we get per-tick NIC drop deltas straight from the
    # PerfReporter, but plain-rust doesn't embed that instrumentation — the
    # closest equivalent is ethtool -S from the kernel side. Baseline now,
    # finalize after run_benchmark_for_config returns, so (final - baseline)
    # gives a kernel-level NIC drop total that we can compare against the
    # TRex observed wire loss for plain-rust. Without this, plain-rust's
    # "NIC drops" column is always "—" and we can't tell whether kernel
    # packet loss matches what DPDK sees.
    # Give the kernel a moment to bring up the freshly-rebound interface —
    # right after dut_bind_kernel the link may not be fully settled, so
    # retry ethtool up to 3 times waiting for a result with numeric stats.
    # The previous implementation had a subtle `[ -n ] && A || B` bash
    # precedence bug that silently wrote "ethtool unavailable" to the file
    # on failure, which the Python aggregator then counted as "no data".
    local ethtool_baseline
    ethtool_baseline=$(ssm_run_command "$DUT_INSTANCE_ID" 60 \
        "set -e; for retry in 1 2 3; do IFACE=\$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); if [ -n \"\$IFACE\" ]; then OUT=\$(ethtool -S \$IFACE 2>&1); if echo \"\$OUT\" | grep -q ': [0-9]'; then echo \"\$OUT\"; exit 0; fi; fi; sleep 2; done; echo 'ETHTOOL_BASELINE_FAILED iface=\$IFACE'; exit 0" 2>/dev/null || echo "(SSM failed)")
    echo "$ethtool_baseline" > "$LOGS_DIR/dut-plain-rust-ethtool-baseline.txt"
    local base_lines
    base_lines=$(echo "$ethtool_baseline" | wc -l)
    log_info "plain-rust ethtool baseline captured ($base_lines lines, head: $(echo "$ethtool_baseline" | head -2 | tr '\n' ' '))"

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/plain-echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/plain-echo.log 2>&1 &"
    sleep 10

    local status=""
    local verify_attempt
    for verify_attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            "pgrep -f 'target/release/plain-echo' >/dev/null && echo 'running' || echo 'not running'") || true
        if [[ "$status" == *"running"* ]]; then
            break
        fi
        log_warn "plain-rust verify attempt $verify_attempt: status='$status'"
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "plain-rust echo server failed to start (status='$status')"
        ssm_run_command "$DUT_INSTANCE_ID" 30 "tail -30 /var/log/plain-echo.log 2>/dev/null" || true
        return 1
    fi
    log_info "plain-rust echo server running"
}

# ── Results Aggregation ───────────────────────────────────────────────────────

aggregate_results() {
    log_info "Aggregating performance results..."
    mkdir -p "$RESULTS_DIR"

    python3 - <<'PYEOF'
import json, glob, os, re, sys
from datetime import datetime, timezone

results_dir = os.environ.get("RESULTS_DIR", "perf-results")
logs_dir = os.environ.get("LOGS_DIR", "perf-logs")
output_file = os.path.join(results_dir, "perf-report.json")

# Regex pulls fields out of:
#   [PERF] ts_unix=1712.345 interval=10s rx_pps=... rx_drops=N rx_ring_drops=N rx_buf_drops=N
#          nic_imissed=N nic_ierrors=N nic_rx_nombuf=N ...
# nic_* fields are emitted as "-" on non-DPDK backends, so we parse them as
# an optional string and treat non-digits as "unavailable".
PERF_RE = re.compile(
    r"^\[PERF\]\s+ts_unix=(?P<ts>[\d.]+).*?\s"
    r"rx_drops=(?P<rx_drops>\d+).*?\s"
    r"rx_ring_drops=(?P<rx_ring>\d+).*?\s"
    r"rx_buf_drops=(?P<rx_buf>\d+)"
)
NIC_IMISSED_RE = re.compile(r"nic_imissed=(\d+|-)")
NIC_IERRORS_RE = re.compile(r"nic_ierrors=(\d+|-)")
NIC_RX_NOMBUF_RE = re.compile(r"nic_rx_nombuf=(\d+|-)")
INTERVAL_RE = re.compile(r"interval=(\d+)s")

# [NIC-BASELINE] / [NIC-FINAL] are one-shot lines emitted by PerfReporter at
# startup and at clean shutdown. They capture the RAW cumulative rte_eth_stats
# counters, so (FINAL - BASELINE) is the total NIC drops that happened across
# the reporter's entire lifetime. The harness cross-checks that against the
# sum of per-tick deltas carried in [PERF] lines. A mismatch means either:
#   1. the tick-loop delta computation is losing data (reporter bug), or
#   2. the window-overlap aggregator below is double-counting or skipping
#      samples, or
#   3. the NIC counters moved outside the [BASELINE, FINAL] window (e.g. a
#      race where the nic_stats_fn callback observed stale data).
# This is our end-to-end self-consistency check on the instrumentation.
NIC_BASELINE_RE = re.compile(
    r"^\[NIC-BASELINE\]\s+ts_unix=(?P<ts>[\d.]+)\s+"
    r"imissed=(?P<imissed>\d+)\s+"
    r"ierrors=(?P<ierrors>\d+)\s+"
    r"rx_nombuf=(?P<rx_nombuf>\d+)"
)
NIC_FINAL_RE = re.compile(
    r"^\[NIC-FINAL\]\s+ts_unix=(?P<ts>[\d.]+)\s+"
    r"imissed=(?P<imissed>\d+)\s+"
    r"ierrors=(?P<ierrors>\d+)\s+"
    r"rx_nombuf=(?P<rx_nombuf>\d+)"
)

def _parse_nic_field(match):
    """Return int for numeric fields, None for "-" / missing."""
    if match is None:
        return None
    v = match.group(1)
    if v == "-":
        return None
    try:
        return int(v)
    except ValueError:
        return None

def load_perf_lines(config_name):
    """Return list of dicts with software + NIC drop fields.
    `interval` is the reporter window in seconds — used to figure out which
    rate-step a [PERF] sample belongs to (the line's ts_unix marks the END of
    the window, so the sample covers [ts - interval, ts]).

    NIC fields (nic_imissed/nic_ierrors/nic_rx_nombuf) are None on non-DPDK
    backends; the harness propagates None to the report so downstream
    consumers can distinguish "backend doesn't expose NIC stats" from
    "zero NIC drops"."""
    perf_path = os.path.join(logs_dir, f"dut-{config_name}-perf.log")
    samples = []
    if not os.path.exists(perf_path):
        return samples
    try:
        with open(perf_path) as fh:
            for line in fh:
                m = PERF_RE.search(line)
                if not m:
                    continue
                im = INTERVAL_RE.search(line)
                interval = int(im.group(1)) if im else 10
                samples.append({
                    "ts": float(m.group("ts")),
                    "interval": interval,
                    "rx_drops": int(m.group("rx_drops")),
                    "rx_ring": int(m.group("rx_ring")),
                    "rx_buf": int(m.group("rx_buf")),
                    "nic_imissed": _parse_nic_field(NIC_IMISSED_RE.search(line)),
                    "nic_ierrors": _parse_nic_field(NIC_IERRORS_RE.search(line)),
                    "nic_rx_nombuf": _parse_nic_field(NIC_RX_NOMBUF_RE.search(line)),
                })
    except Exception as e:
        print(f"Warning: failed to parse {perf_path}: {e}", file=sys.stderr)
    return samples

def load_nic_baseline_final(config_name):
    """Parse [NIC-BASELINE] and [NIC-FINAL] from the config's perf log.
    Returns (baseline_dict, final_dict) where each is None if the
    corresponding line wasn't found (e.g. non-DPDK backend or abnormal
    shutdown). PerfReporter emits exactly one BASELINE at startup and one
    FINAL at clean shutdown, so in a healthy run we expect one of each."""
    perf_path = os.path.join(logs_dir, f"dut-{config_name}-perf.log")
    baseline = None
    final = None
    if not os.path.exists(perf_path):
        return baseline, final
    try:
        with open(perf_path) as fh:
            for line in fh:
                m = NIC_BASELINE_RE.search(line)
                if m:
                    baseline = {
                        "ts": float(m.group("ts")),
                        "imissed": int(m.group("imissed")),
                        "ierrors": int(m.group("ierrors")),
                        "rx_nombuf": int(m.group("rx_nombuf")),
                    }
                    continue
                m = NIC_FINAL_RE.search(line)
                if m:
                    final = {
                        "ts": float(m.group("ts")),
                        "imissed": int(m.group("imissed")),
                        "ierrors": int(m.group("ierrors")),
                        "rx_nombuf": int(m.group("rx_nombuf")),
                    }
    except Exception as e:
        print(f"Warning: failed to parse baseline/final from {perf_path}: {e}",
              file=sys.stderr)
    return baseline, final


def load_ethtool_stats(path):
    """Parse `ethtool -S <iface>` output into a dict of name→int.

    The output format is one stat per line:
        NIC statistics:
             rx_packets: 12345
             tx_packets: 67890
             rx_missed_errors: 0
             ...
    Leading spaces are stripped. Non-integer values are skipped."""
    stats = {}
    if not os.path.exists(path):
        return stats
    try:
        with open(path) as fh:
            for line in fh:
                line = line.strip()
                if ":" not in line:
                    continue
                key, _, val = line.partition(":")
                val = val.strip()
                if not val:
                    continue
                try:
                    stats[key.strip()] = int(val)
                except ValueError:
                    # Some ethtool fields are strings (driver name, etc.) —
                    # we only care about numeric counters for diffing.
                    pass
    except Exception as e:
        print(f"Warning: failed to parse ethtool stats {path}: {e}",
              file=sys.stderr)
    return stats


def compute_kernel_nic_delta(config_name):
    """For plain-rust, compute kernel NIC drop delta from pre/post
    `ethtool -S` snapshots. Returns a dict with selected drop counters
    (rx_missed_errors, rx_dropped, rx_errors, rx_no_buffer_count) or
    None if either snapshot is missing.

    On AWS ENA these specific counters are the kernel-visible equivalent
    of what rte_eth_stats.imissed/ierrors/rx_nombuf expose to DPDK.
    Using ethtool gives plain-rust parity with the DPDK self-check and
    lets us cross-reference kernel drops against TRex observed wire loss."""
    base_path = os.path.join(logs_dir,
                             f"dut-{config_name}-ethtool-baseline.txt")
    final_path = os.path.join(logs_dir,
                              f"dut-{config_name}-ethtool-final.txt")
    baseline = load_ethtool_stats(base_path)
    final = load_ethtool_stats(final_path)
    if not baseline or not final:
        return None

    # ENA kernel driver exposes these; names may vary slightly across
    # kernel versions so we look for the most common forms.
    counters = [
        "rx_missed_errors",     # HW RX ring full (ENA equivalent of imissed)
        "rx_dropped",           # Generic RX drops
        "rx_errors",            # Total rx errors (ENA equivalent of ierrors)
        "rx_no_buffer_count",   # No sk_buff available (equivalent of rx_nombuf)
    ]
    delta = {}
    for c in counters:
        if c in baseline and c in final:
            delta[c] = final[c] - baseline[c]
    return delta if delta else None


def compute_nic_consistency_check(config_name, samples):
    """Compare (FINAL - BASELINE) against the sum of per-tick [PERF] deltas.

    Returns a dict with:
      - status: "ok" | "mismatch" | "no_data" | "no_shutdown"
      - expected: dict with imissed/ierrors/rx_nombuf from FINAL-BASELINE
      - actual:   dict with the sum of all [PERF] per-tick deltas
      - delta:    actual - expected (signed; positive = over-count,
                  negative = under-count / lost data)

    "no_data" means no BASELINE was parsed (non-DPDK backend).
    "no_shutdown" means BASELINE was found but FINAL was not (reporter
    probably crashed or the process was killed before clean shutdown).
    """
    baseline, final = load_nic_baseline_final(config_name)
    if baseline is None:
        return {"status": "no_data"}
    if final is None:
        return {"status": "no_shutdown", "baseline": baseline}

    expected = {
        "imissed": final["imissed"] - baseline["imissed"],
        "ierrors": final["ierrors"] - baseline["ierrors"],
        "rx_nombuf": final["rx_nombuf"] - baseline["rx_nombuf"],
    }

    # Sum per-tick deltas across the ENTIRE run — deliberately no window
    # filtering; BASELINE→FINAL covers the reporter's whole lifetime. Only
    # count samples that actually reported numeric values (non-DPDK ticks
    # would have None, but a DPDK-backed reporter should return numbers
    # every tick).
    actual = {"imissed": 0, "ierrors": 0, "rx_nombuf": 0}
    for s in samples:
        if s["nic_imissed"] is not None:
            actual["imissed"] += s["nic_imissed"]
        if s["nic_ierrors"] is not None:
            actual["ierrors"] += s["nic_ierrors"]
        if s["nic_rx_nombuf"] is not None:
            actual["rx_nombuf"] += s["nic_rx_nombuf"]

    delta = {k: actual[k] - expected[k] for k in expected}
    # Match when all three counters agree exactly. Any mismatch — even by 1
    # — is worth flagging, since the deltas are derived from the same
    # counter reads so drift here means our tick bookkeeping is broken.
    status = "ok" if all(v == 0 for v in delta.values()) else "mismatch"
    return {
        "status": status,
        "baseline": baseline,
        "final": final,
        "expected": expected,
        "actual": actual,
        "delta": delta,
    }


def annotate_with_app_drops(cfg_data):
    """Inject software-layer drop fields (`app_drops`, `app_ring_drops`,
    `app_buf_drops`) AND NIC-layer drop fields (`nic_imissed`, `nic_ierrors`,
    `nic_rx_nombuf`) into each step in cfg_data['results'].

    Sums [PERF] samples whose covered window [ts-interval, ts] overlaps the
    step's [ts_start_unix, ts_end_unix]. NIC fields are only set if at least
    one overlapping sample reported a numeric value (i.e. DPDK backend);
    otherwise they stay absent so the markdown generator can render "—".

    Note: we deliberately do NOT compute a combined `imissed + ierrors +
    rx_nombuf` total here. On AWS ENA, `ierrors` is dominated by background
    noise unrelated to test traffic (Run #10 showed ~50K ierrors/step in the
    C reference testpmd binary while imissed/rx_nombuf stayed at 0). Summing
    the three into one column masks the real drop signal; downstream
    consumers should display the three sub-columns separately."""
    name = cfg_data.get("config_name", "")
    samples = load_perf_lines(name)
    # Always run the consistency check — even with zero samples, the check
    # will return "no_data" / "no_shutdown" which the markdown generator
    # can render as "—" to make it clear which configs lack instrumentation.
    cfg_data["nic_consistency"] = compute_nic_consistency_check(name, samples)
    # For plain-rust we also attach kernel ethtool -S deltas captured by
    # the test harness pre/post. This is the only NIC-level instrumentation
    # available for the kernel echo path and gives plain-rust parity with
    # the DPDK configs in the self-check table.
    kernel_delta = compute_kernel_nic_delta(name)
    if kernel_delta is not None:
        cfg_data["kernel_ethtool_delta"] = kernel_delta
    if not samples:
        return
    for size_results in cfg_data.get("results", {}).values():
        for step in size_results:
            ts_start = step.get("ts_start_unix")
            ts_end = step.get("ts_end_unix")
            if ts_start is None or ts_end is None:
                continue
            buf_total = 0
            ring_total = 0
            drops_total = 0
            nic_imissed_total = 0
            nic_ierrors_total = 0
            nic_nombuf_total = 0
            nic_has_data = False
            for s in samples:
                window_start = s["ts"] - s["interval"]
                window_end = s["ts"]
                # Attribute the sample if its window overlaps the step window
                if window_end < ts_start or window_start > ts_end:
                    continue
                buf_total += s["rx_buf"]
                ring_total += s["rx_ring"]
                drops_total += s["rx_drops"]
                if s["nic_imissed"] is not None:
                    nic_imissed_total += s["nic_imissed"]
                    nic_has_data = True
                if s["nic_ierrors"] is not None:
                    nic_ierrors_total += s["nic_ierrors"]
                    nic_has_data = True
                if s["nic_rx_nombuf"] is not None:
                    nic_nombuf_total += s["nic_rx_nombuf"]
                    nic_has_data = True
            step["app_drops"] = drops_total
            step["app_ring_drops"] = ring_total
            step["app_buf_drops"] = buf_total
            if nic_has_data:
                step["nic_imissed"] = nic_imissed_total
                step["nic_ierrors"] = nic_ierrors_total
                step["nic_rx_nombuf"] = nic_nombuf_total

configs = {}
for f in sorted(glob.glob(os.path.join(results_dir, "*.json"))):
    if os.path.basename(f) == "perf-report.json":
        continue
    try:
        with open(f) as fh:
            data = json.load(fh)
            name = data.get("config_name", os.path.basename(f).replace(".json", ""))
            annotate_with_app_drops(data)
            configs[name] = data
    except Exception as e:
        print(f"Warning: failed to read {f}: {e}", file=sys.stderr)

report = {
    "timestamp": datetime.now(timezone.utc).isoformat(),
    "commit": os.environ.get("GITHUB_SHA", "unknown"),
    "instance_type": os.environ.get("DUT_INSTANCE_TYPE", "unknown"),
    "configs": configs,
}

with open(output_file, "w") as f:
    json.dump(report, f, indent=2)
print(f"Aggregated report written to {output_file}")
PYEOF
}

generate_markdown_summary() {
    log_info "Generating markdown summary..."

    python3 - <<'PYEOF'
import json, os, sys

results_dir = os.environ.get("RESULTS_DIR", "perf-results")
report_file = os.path.join(results_dir, "perf-report.json")
md_file = os.path.join(results_dir, "perf-summary.md")

if not os.path.exists(report_file):
    print("No report file found, skipping summary generation", file=sys.stderr)
    sys.exit(0)

with open(report_file) as f:
    report = json.load(f)

lines = []
lines.append(f"## Performance Test Results — {report.get('instance_type', 'unknown')}")
lines.append("")
lines.append(f"Commit: `{report.get('commit', 'unknown')[:8]}`")
lines.append(f"Timestamp: {report.get('timestamp', 'unknown')}")
lines.append("")

configs = report.get("configs", {})
if not configs:
    lines.append("*No results collected*")
else:
    # Collect all packet sizes across configs
    all_sizes = set()
    for cfg_data in configs.values():
        for size_key in cfg_data.get("results", {}).keys():
            all_sizes.add(size_key)

    for pkt_size in sorted(all_sizes, key=lambda s: int(s.rstrip('B'))):
        lines.append(f"### {pkt_size} packets")
        lines.append("")
        # Drop columns walk the receive path from hardware to app. The three
        # NIC sub-columns are split out from rte_eth_stats because they mean
        # different things and should not be summed — in particular on AWS
        # ENA, `ierrors` is dominated by background NIC events (bad CRCs,
        # management frames, etc.) that are NOT test-traffic loss. Lumping
        # ierrors into a single "NIC Drops" total masks the real signal.
        # Run #10 proved this — see docs/perf-test-log.md for details.
        #
        #   NIC imissed  = rte_eth_stats.imissed — the DPDK SW polled too
        #                  slowly and the HW RX descriptor ring filled up,
        #                  so the NIC discarded incoming frames. This is
        #                  the REAL "app can't keep up" signal.
        #   NIC ierrors  = rte_eth_stats.ierrors — generic NIC-level errors
        #                  (CRC, framing, oversized, etc.). On ENA this is
        #                  mostly AWS background noise and should be treated
        #                  as such unless it changes between runs.
        #   NIC nombuf   = rte_eth_stats.rx_nombuf — no mempool buffer was
        #                  available to place an incoming packet; the NIC
        #                  dropped it at the refill path. Indicates the
        #                  mempool is sized too small or being drained.
        #   App Drops    = dpdk-udp software-layer drops (rx_ring_drops +
        #                  rx_buf_drops). Packets that made it through the
        #                  NIC into the socket path but got dropped because
        #                  the worker SpscRing or the per-socket recv_queue
        #                  was full (consumer too slow).
        #
        # All four are only available for backends that emit [PERF] lines.
        # Non-zero wire drop (TX/RX delta) with imissed/nombuf/App all ≈ 0
        # points at wire loss (AWS ENA / VPC rate limiter / bad cabling).
        lines.append("| Config | Target PPS | TX pps | RX pps | Drop % | NIC imissed | NIC ierrors | NIC nombuf | App Drops | Lat Avg (us) | Lat Max (us) | TX Mbps | RX Mbps |")
        lines.append("|--------|-----------|--------|--------|--------|-------------|-------------|-----------|-----------|-------------|-------------|---------|---------|")

        for cfg_name in ["native-dpdk", "rust-dpdk", "tokio-dpdk", "plain-rust"]:
            cfg_data = configs.get(cfg_name, {})
            size_results = cfg_data.get("results", {}).get(pkt_size, [])

            for r in size_results:
                tx_pps = f"{r.get('tx_pps', 0):,}"
                rx_pps = f"{r.get('rx_pps', 0):,}"
                drop = f"{r.get('drop_pct', 0):.2f}%"
                lat_avg = r.get('lat_avg_us', -1)
                lat_max = r.get('lat_max_us', -1)
                lat_avg_s = f"{lat_avg:.1f}" if lat_avg >= 0 else "N/A"
                lat_max_s = f"{lat_max:.1f}" if lat_max >= 0 else "N/A"
                tx_mbps = f"{r.get('tx_mbps', 0):.1f}"
                rx_mbps = f"{r.get('rx_mbps', 0):.1f}"
                target = f"{r.get('target_pps', 0):,}"
                # `app_drops` / `nic_imissed` / `nic_ierrors` / `nic_rx_nombuf`
                # are only present for DPDK-backed configs that emit [PERF]
                # lines. Show "—" for plain-rust / native-dpdk (native-dpdk
                # is a C reference binary and does not emit [PERF] either).
                if 'app_drops' in r:
                    app_drops = f"{r['app_drops']:,}"
                else:
                    app_drops = "—"
                if 'nic_imissed' in r:
                    nic_imissed = f"{r['nic_imissed']:,}"
                    nic_ierrors = f"{r['nic_ierrors']:,}"
                    nic_nombuf = f"{r['nic_rx_nombuf']:,}"
                else:
                    nic_imissed = "—"
                    nic_ierrors = "—"
                    nic_nombuf = "—"

                lines.append(f"| {cfg_name} | {target} | {tx_pps} | {rx_pps} | {drop} | {nic_imissed} | {nic_ierrors} | {nic_nombuf} | {app_drops} | {lat_avg_s} | {lat_max_s} | {tx_mbps} | {rx_mbps} |")

        lines.append("")

    # ── NIC drops instrumentation self-check ────────────────────────────
    # For every config that emitted [NIC-BASELINE]/[NIC-FINAL], compare
    # (FINAL - BASELINE) to the sum of per-tick [PERF] deltas. If the
    # reporter pipeline is healthy they MUST match exactly, because both
    # numbers come from the same counter reads (FINAL-BASELINE is just a
    # telescoping sum of the tick deltas). A mismatch here is the signal
    # that our NIC drop instrumentation is losing data — which is the
    # question that motivated adding this check in the first place.
    lines.append("### NIC Drops Instrumentation Self-Check")
    lines.append("")
    lines.append(
        "Compares `(NIC-FINAL − NIC-BASELINE)` one-shot snapshots "
        "against the sum of per-tick `[PERF]` deltas over the reporter's "
        "lifetime. These MUST match exactly — a mismatch means per-tick "
        "delta bookkeeping is losing data."
    )
    lines.append("")
    lines.append("| Config | Status | imissed (expected / actual / Δ) | ierrors (expected / actual / Δ) | rx_nombuf (expected / actual / Δ) |")
    lines.append("|--------|--------|--------------------------------|----------------------------------|-----------------------------------|")
    for cfg_name in ["native-dpdk", "rust-dpdk", "tokio-dpdk", "plain-rust"]:
        cfg_data = configs.get(cfg_name, {})
        check = cfg_data.get("nic_consistency")
        if check is None:
            lines.append(f"| {cfg_name} | no data | — | — | — |")
            continue
        status = check["status"]
        if status == "no_data":
            lines.append(f"| {cfg_name} | no instrumentation | — | — | — |")
            continue
        if status == "no_shutdown":
            # Reporter started but never got a FINAL — abnormal exit.
            lines.append(
                f"| {cfg_name} | no FINAL (abnormal shutdown) | — | — | — |"
            )
            continue
        exp = check["expected"]
        act = check["actual"]
        delta = check["delta"]
        status_cell = "**OK**" if status == "ok" else "**MISMATCH**"

        def _fmt(counter):
            d = delta[counter]
            d_str = f"+{d:,}" if d > 0 else f"{d:,}"
            return f"{exp[counter]:,} / {act[counter]:,} / {d_str}"

        lines.append(
            f"| {cfg_name} | {status_cell} | "
            f"{_fmt('imissed')} | {_fmt('ierrors')} | {_fmt('rx_nombuf')} |"
        )
    lines.append("")
    lines.append(
        "*`expected = FINAL − BASELINE` (raw NIC counter delta across reporter "
        "lifetime). `actual = sum of per-tick [PERF] delta fields`. Any Δ ≠ 0 "
        "is a bug in the tick loop's bookkeeping.*"
    )
    lines.append("")

    # ── Kernel ethtool deltas for plain-rust ────────────────────────────
    # The self-check above only covers DPDK configs (they embed
    # PerfReporter). For plain-rust we don't have per-tick NIC counters,
    # but the harness captures `ethtool -S <iface>` pre- and post-run so
    # we can at least expose kernel-visible NIC drop totals side-by-side
    # with the DPDK numbers. This is the non-DPDK baseline the user asked
    # for: "we could probably get kernel data for the rust native app too".
    plain_rust_data = configs.get("plain-rust", {})
    kernel_delta = plain_rust_data.get("kernel_ethtool_delta")
    if kernel_delta is not None:
        lines.append("### plain-rust Kernel NIC Drops (ethtool -S delta)")
        lines.append("")
        lines.append(
            "Kernel-visible NIC counters from the ena driver, captured via "
            "`ethtool -S` before and after the plain-rust rate sweep. "
            "These are the non-DPDK analogue of the DPDK self-check above "
            "and let us compare what the kernel sees against what TRex "
            "observes on the wire."
        )
        lines.append("")
        lines.append("| Counter | Delta | Kernel Meaning |")
        lines.append("|---------|------:|----------------|")
        # Human-readable descriptions of each ena counter.
        descriptions = {
            "rx_missed_errors":
                "HW RX ring overflowed — driver didn't drain fast enough "
                "(equivalent of DPDK `rte_eth_stats.imissed`)",
            "rx_dropped":
                "Packets dropped somewhere in the RX path (generic)",
            "rx_errors":
                "Total RX errors — CRC, framing, truncation, etc. "
                "(equivalent of DPDK `rte_eth_stats.ierrors`)",
            "rx_no_buffer_count":
                "No sk_buff available to receive incoming packet "
                "(equivalent of DPDK `rte_eth_stats.rx_nombuf`)",
        }
        for counter in ["rx_missed_errors", "rx_dropped",
                        "rx_errors", "rx_no_buffer_count"]:
            if counter in kernel_delta:
                desc = descriptions.get(counter, "")
                lines.append(
                    f"| `{counter}` | {kernel_delta[counter]:,} | {desc} |"
                )
        lines.append("")
    elif "plain-rust" in configs:
        lines.append("### plain-rust Kernel NIC Drops (ethtool -S delta)")
        lines.append("")
        lines.append(
            "*ethtool snapshots not available — baseline or final file "
            "missing in `$LOGS_DIR/dut-plain-rust-ethtool-*.txt`.*"
        )
        lines.append("")

md_content = "\n".join(lines)
with open(md_file, "w") as f:
    f.write(md_content)

# Also print for step summary
print(md_content)
PYEOF
}

# ── Cleanup ───────────────────────────────────────────────────────────────────

cleanup() {
    local exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        log_warn "Script exiting with code $exit_code, collecting failure diagnostics..."

        if [[ -n "$DUT_INSTANCE_ID" ]]; then
            collect_instance_logs "$DUT_INSTANCE_ID" "dut" || true
            collect_networking_diagnostics "$DUT_INSTANCE_ID" "dut" "failure" || true
        fi
        if [[ -n "$TREX_INSTANCE_ID" ]]; then
            collect_instance_logs "$TREX_INSTANCE_ID" "trex" || true
            collect_networking_diagnostics "$TREX_INSTANCE_ID" "trex" "failure" || true
        fi

        write_failure_json "perf-test" "Script exited with code $exit_code"
    fi

    if [[ "$TEARDOWN" == "true" && "$SKIP_DEPLOY" == "false" ]]; then
        log_info "Tearing down PerfTestStack..."
        # Use non-blocking delete to avoid consuming OIDC token time.
        # The safety-net teardown step in the workflow will also attempt cleanup.
        aws cloudformation delete-stack --stack-name "$CDK_STACK_NAME" 2>/dev/null || log_warn "Teardown failed"
    fi
}

trap cleanup EXIT

# ── Main Flow ─────────────────────────────────────────────────────────────────

main() {
    log_info "=== Performance Test Suite ==="
    log_info "Configs: $CONFIGS"
    log_info "Packet sizes: $PACKET_SIZES"
    log_info "Rate steps: $RATE_STEPS"
    log_info "Duration per step: ${DURATION}s"

    mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

    # ── Phase 1: Deploy ──────────────────────────────────────────────────────

    if [[ "$SKIP_DEPLOY" == "false" ]]; then
        log_info "Phase 1: Deploying $CDK_STACK_NAME..."
        post_pr_comment "## [Perf] Stage: Deploy
Deploying \`$CDK_STACK_NAME\`...
Configs: \`$CONFIGS\`
Packet sizes: \`$PACKET_SIZES\`"

        cd "$CDK_DIR"

        # Fetch AMI IDs from SSM if available
        local dpdk_ami trex_ami context_args=""
        dpdk_ami=$(aws ssm get-parameter --name /dpdk-stdlib-rust/ami/latest \
            --query "Parameter.Value" --output text 2>/dev/null || echo "")
        trex_ami=$(aws ssm get-parameter --name /dpdk-stdlib-rust/ami/trex-latest \
            --query "Parameter.Value" --output text 2>/dev/null || echo "")

        if [[ -n "${DPDK_AMI_ID:-}" ]]; then dpdk_ami="$DPDK_AMI_ID"; fi
        if [[ -n "${TREX_AMI_ID:-}" ]]; then trex_ami="$TREX_AMI_ID"; fi

        if [[ -n "$dpdk_ami" ]]; then
            context_args="$context_args -c ${DPDK_AMI_CDK_CONTEXT_KEY:-dpdkAmiId}=$dpdk_ami"
            log_info "Using pre-built DPDK AMI: $dpdk_ami"
        fi
        if [[ -n "$trex_ami" ]]; then
            context_args="$context_args -c trexAmiId=$trex_ami"
            log_info "Using pre-built TRex AMI: $trex_ami"
        fi

        # Destroy any leftover stack first (from a previous failed run).
        # IMPORTANT: Do NOT use `npx cdk destroy --force` here — it blocks
        # and monitors CloudFormation events, which can consume the entire
        # 1-hour OIDC token if a previous stack is stuck in DELETE_IN_PROGRESS.
        # Use non-blocking `aws cloudformation delete-stack` + polling instead.
        log_info "Cleaning up any leftover stack..."
        local stack_status
        stack_status=$(aws cloudformation describe-stacks \
            --stack-name "$CDK_STACK_NAME" \
            --query "Stacks[0].StackStatus" \
            --output text 2>/dev/null || echo "GONE")
        log_info "Current stack status: $stack_status"

        if [[ "$stack_status" != "GONE" && "$stack_status" != "DELETE_COMPLETE" && "$stack_status" != "DELETE_IN_PROGRESS" ]]; then
            log_info "Requesting stack deletion..."
            aws cloudformation delete-stack --stack-name "$CDK_STACK_NAME" 2>&1 || true
        fi

        # Wait for stack to fully delete, retrying on DELETE_FAILED
        local stack_wait=0
        local destroy_retries=0
        local stack_deleted=false
        while [[ $stack_wait -lt 900 ]]; do
            stack_status=$(aws cloudformation describe-stacks \
                --stack-name "$CDK_STACK_NAME" \
                --query "Stacks[0].StackStatus" \
                --output text 2>/dev/null || echo "GONE")
            if [[ "$stack_status" == "GONE" || "$stack_status" == "DELETE_COMPLETE" ]]; then
                log_info "Stack fully cleaned up (status: $stack_status)"
                stack_deleted=true
                break
            fi
            if [[ "$stack_status" == "DELETE_FAILED" && $destroy_retries -lt 3 ]]; then
                destroy_retries=$((destroy_retries + 1))
                log_warn "Stack in DELETE_FAILED — retrying destroy (attempt $destroy_retries/3)..."
                if [[ $destroy_retries -le 2 ]]; then
                    aws cloudformation delete-stack \
                        --stack-name "$CDK_STACK_NAME" 2>&1 || true
                else
                    # Final attempt: delete stack retaining the stuck resources
                    log_warn "Final retry: deleting stack with --retain-resources for stuck resources..."
                    local stuck_resources
                    stuck_resources=$(aws cloudformation describe-stack-events \
                        --stack-name "$CDK_STACK_NAME" \
                        --query "StackEvents[?ResourceStatus=='DELETE_FAILED'].LogicalResourceId" \
                        --output text 2>/dev/null | tr '\t' ' ')
                    if [[ -n "$stuck_resources" ]]; then
                        log_info "Retaining stuck resources: $stuck_resources"
                        # shellcheck disable=SC2086
                        aws cloudformation delete-stack \
                            --stack-name "$CDK_STACK_NAME" \
                            --retain-resources $stuck_resources 2>&1 || true
                    else
                        aws cloudformation delete-stack \
                            --stack-name "$CDK_STACK_NAME" 2>&1 || true
                    fi
                fi
                sleep 30
                stack_wait=$((stack_wait + 30))
                continue
            fi
            log_info "Waiting for stack deletion (status: $stack_status, ${stack_wait}s elapsed)..."
            sleep 15
            stack_wait=$((stack_wait + 15))
        done

        # If the wait loop timed out, the stack is still not deleted.
        # Try force-delete with --retain-resources as a last resort before giving up.
        if [[ "$stack_deleted" == "false" ]]; then
            stack_status=$(aws cloudformation describe-stacks \
                --stack-name "$CDK_STACK_NAME" \
                --query "Stacks[0].StackStatus" \
                --output text 2>/dev/null || echo "GONE")

            if [[ "$stack_status" == "GONE" || "$stack_status" == "DELETE_COMPLETE" ]]; then
                log_info "Stack deleted just after timeout (status: $stack_status)"
            elif [[ "$stack_status" == "DELETE_IN_PROGRESS" ]]; then
                # Stack is still deleting after 15 min — something is stuck.
                # Cancel the in-progress delete by requesting a new delete with
                # --retain-resources for everything that's blocking deletion.
                log_warn "Stack still DELETE_IN_PROGRESS after 900s — escalating with --retain-resources..."
                local all_resources
                all_resources=$(aws cloudformation list-stack-resources \
                    --stack-name "$CDK_STACK_NAME" \
                    --query "StackResourceSummaries[?ResourceStatus!='DELETE_COMPLETE'].LogicalResourceId" \
                    --output text 2>/dev/null | tr '\t' ' ')
                if [[ -n "$all_resources" ]]; then
                    log_info "Retaining undeletable resources: $all_resources"
                    # shellcheck disable=SC2086
                    aws cloudformation delete-stack \
                        --stack-name "$CDK_STACK_NAME" \
                        --retain-resources $all_resources 2>&1 || true
                fi
                # Wait up to 120s more for the retain-resources delete to complete
                local extra_wait=0
                while [[ $extra_wait -lt 120 ]]; do
                    sleep 15
                    extra_wait=$((extra_wait + 15))
                    stack_status=$(aws cloudformation describe-stacks \
                        --stack-name "$CDK_STACK_NAME" \
                        --query "Stacks[0].StackStatus" \
                        --output text 2>/dev/null || echo "GONE")
                    if [[ "$stack_status" == "GONE" || "$stack_status" == "DELETE_COMPLETE" ]]; then
                        log_info "Stack cleaned up after retain-resources escalation"
                        break
                    fi
                    log_info "Waiting for retain-resources delete (status: $stack_status, ${extra_wait}s)..."
                done
                # Final check
                stack_status=$(aws cloudformation describe-stacks \
                    --stack-name "$CDK_STACK_NAME" \
                    --query "Stacks[0].StackStatus" \
                    --output text 2>/dev/null || echo "GONE")
                if [[ "$stack_status" != "GONE" && "$stack_status" != "DELETE_COMPLETE" ]]; then
                    log_error "Stack still not deleted after all retries (status: $stack_status). Cannot deploy."
                    exit 2
                fi
            elif [[ "$stack_status" == "DELETE_FAILED" ]]; then
                # One more try with --retain-resources
                log_warn "Stack in DELETE_FAILED after timeout — final retain-resources attempt..."
                local stuck_resources
                stuck_resources=$(aws cloudformation describe-stack-events \
                    --stack-name "$CDK_STACK_NAME" \
                    --query "StackEvents[?ResourceStatus=='DELETE_FAILED'].LogicalResourceId" \
                    --output text 2>/dev/null | tr '\t' ' ')
                if [[ -n "$stuck_resources" ]]; then
                    # shellcheck disable=SC2086
                    aws cloudformation delete-stack \
                        --stack-name "$CDK_STACK_NAME" \
                        --retain-resources $stuck_resources 2>&1 || true
                else
                    aws cloudformation delete-stack \
                        --stack-name "$CDK_STACK_NAME" 2>&1 || true
                fi
                sleep 30
                stack_status=$(aws cloudformation describe-stacks \
                    --stack-name "$CDK_STACK_NAME" \
                    --query "Stacks[0].StackStatus" \
                    --output text 2>/dev/null || echo "GONE")
                if [[ "$stack_status" != "GONE" && "$stack_status" != "DELETE_COMPLETE" ]]; then
                    log_error "Stack still not deleted after all retries (status: $stack_status). Cannot deploy."
                    exit 2
                fi
            else
                log_error "Stack in unexpected state after cleanup timeout: $stack_status"
                exit 2
            fi
        fi

        npx cdk deploy "$CDK_STACK_NAME" --require-approval never $context_args \
            || {
                log_error "CDK deploy failed — dumping CloudFormation events..."
                # Capture CFN events so we can diagnose what resource failed
                aws cloudformation describe-stack-events \
                    --stack-name "$CDK_STACK_NAME" \
                    --query "StackEvents[?contains(ResourceStatus,'FAILED') || contains(ResourceStatus,'ROLLBACK')].[Timestamp,LogicalResourceId,ResourceStatus,ResourceStatusReason]" \
                    --output table 2>/dev/null || true
                # Also post to PR so we can read it remotely
                local cfn_events
                cfn_events=$(aws cloudformation describe-stack-events \
                    --stack-name "$CDK_STACK_NAME" \
                    --query "StackEvents[?contains(ResourceStatus,'FAILED') || contains(ResourceStatus,'ROLLBACK')].[Timestamp,LogicalResourceId,ResourceStatus,ResourceStatusReason]" \
                    --output text 2>/dev/null | head -20 || echo "(no events)")
                post_pr_comment "## [Perf] Stage: Deploy FAILED
CDK deploy failed. CloudFormation events:
\`\`\`
$cfn_events
\`\`\`"
                exit 2
            }

        cd "$REPO_ROOT"
    fi

    # ── Phase 2: Get stack outputs & wait for SSM ────────────────────────────

    log_info "Phase 2: Resolving stack outputs..."

    TREX_INSTANCE_ID=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs[?OutputKey=='TrexInstanceId'].OutputValue" \
        --output text)
    DUT_INSTANCE_ID=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs[?OutputKey=='DutInstanceId'].OutputValue" \
        --output text)
    TREX_DATA_ENI_IP=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs[?OutputKey=='TrexDataEniPrivateIp'].OutputValue" \
        --output text)
    TREX_DATA_RX_ENI_IP=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs[?OutputKey=='TrexDataEniRxPrivateIp'].OutputValue" \
        --output text)
    DUT_DATA_ENI_IP=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs[?OutputKey=='DutDataEniPrivateIp'].OutputValue" \
        --output text)

    log_info "TRex: $TREX_INSTANCE_ID (TX: $TREX_DATA_ENI_IP, RX: $TREX_DATA_RX_ENI_IP)"
    log_info "DUT:  $DUT_INSTANCE_ID (data IP: $DUT_DATA_ENI_IP)"

    # Wait for both instances
    wait_ssm_ready "$TREX_INSTANCE_ID" "TRex" &
    local trex_wait_pid=$!
    wait_ssm_ready "$DUT_INSTANCE_ID" "DUT" &
    local dut_wait_pid=$!

    wait "$trex_wait_pid" || { log_error "TRex SSM not ready"; exit 2; }
    wait "$dut_wait_pid"  || { log_error "DUT SSM not ready"; exit 2; }

    # Query actual instance type from DUT via IMDS
    export DUT_INSTANCE_TYPE
    DUT_INSTANCE_TYPE=$(ssm_run_command "$DUT_INSTANCE_ID" 15 \
        "TOKEN=\$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/instance-type" 2>/dev/null || echo "unknown")
    log_info "DUT instance type: $DUT_INSTANCE_TYPE"

    post_pr_comment "## [Perf] Stage: Instances Ready
- TRex: \`$TREX_INSTANCE_ID\` (${TREX_DATA_ENI_IP})
- DUT: \`$DUT_INSTANCE_ID\` (${DUT_DATA_ENI_IP})
- Instance type: \`$DUT_INSTANCE_TYPE\`"

    # ── Phase 2b: Ensure secondary ENIs are attached and bound ────────────────
    # The ENI attachments are separate CloudFormation resources that may complete
    # after the instance boots. Wait for them and bind via SSM.

    log_info "Phase 2b: Ensuring secondary ENIs are attached..."
    # TRex has 2 data ENIs: device-number 1 (TX) and device-number 2 (RX).
    # Wait for BOTH before proceeding. They stay in kernel mode for now —
    # trex_configure_and_bind() will bind them to vfio-pci after gateway MAC discovery.
    wait_and_bind_eni "$TREX_INSTANCE_ID" "TRex" "ena" \
        || { log_error "TRex TX ENI attachment failed"; exit 2; }
    wait_for_trex_rx_eni "$TREX_INSTANCE_ID" \
        || { log_error "TRex RX ENI attachment failed"; exit 2; }
    # DUT ENI starts in kernel mode — orchestrator binds as needed per config.
    wait_and_bind_eni "$DUT_INSTANCE_ID" "DUT" "ena" \
        || { log_error "DUT ENI attachment failed"; exit 2; }

    # ── Phase 3: Collect baseline environment info ───────────────────────────

    log_info "Phase 3: Collecting baseline environment info..."
    collect_environment_info "$TREX_INSTANCE_ID" "trex"
    collect_environment_info "$DUT_INSTANCE_ID" "dut"
    collect_networking_diagnostics "$TREX_INSTANCE_ID" "trex" "baseline"
    collect_networking_diagnostics "$DUT_INSTANCE_ID" "dut" "baseline"

    # ── Phase 4: Configure and start TRex ────────────────────────────────────

    log_info "Phase 4: Configuring TRex..."
    post_pr_comment "## [Perf] Stage: TRex Config
Starting TRex configuration (MAC discovery + NIC binding)..."

    if ! generate_trex_config; then
        post_pr_comment "## [Perf] Stage: TRex Config FAILED
\`generate_trex_config\` returned non-zero.
- TREX_PCI_ADDR: \`${TREX_PCI_ADDR:-unset}\`
- TREX_DATA_MAC: \`${TREX_DATA_MAC:-unset}\`
- TREX_GATEWAY_MAC: \`${TREX_GATEWAY_MAC:-unset}\`"
        log_error "TRex config failed"
        exit 2
    fi

    post_pr_comment "## [Perf] Stage: TRex Config OK
- TX: \`0000:00:06.0\` MAC: \`$TREX_DATA_MAC\`
- RX: \`0000:00:07.0\` MAC: \`${TREX_DATA_RX_MAC:-unset}\`
- Gateway MAC: \`$TREX_GATEWAY_MAC\`
Starting TRex server..."

    if ! start_trex_server; then
        # Grab TRex log and NIC state for diagnostics
        local trex_log
        local diag_pci="${TREX_PCI_ADDR:-0000:00:06.0}"
        trex_log=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
            "echo '=== TRex Log ==='; cat /var/log/trex-server.log 2>/dev/null | tail -80 || echo '(no log file)'; echo; echo '=== NIC State ==='; readlink /sys/bus/pci/devices/$diag_pci/driver 2>/dev/null || echo 'no driver'; ls /sys/bus/pci/drivers/vfio-pci/$diag_pci 2>/dev/null && echo 'vfio-pci: YES' || echo 'vfio-pci: NO'; echo '=== vfio modules ==='; lsmod 2>/dev/null | grep vfio || echo 'none'; echo '=== noiommu ==='; cat /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || echo 'N/A'; echo '=== /dev/vfio ==='; ls -la /dev/vfio/ 2>/dev/null || echo 'none'; echo '=== hugepages ==='; grep -i huge /proc/meminfo 2>/dev/null | head -3; echo '=== TRex config ==='; cat /etc/trex_cfg.yaml 2>/dev/null || echo 'missing'" 2>/dev/null || echo "(SSM failed)")
        post_pr_comment "## [Perf] Stage: TRex Start FAILED
TRex server failed to start within ${TREX_START_TIMEOUT}s.
<details><summary>TRex server log + NIC diagnostics</summary>

\`\`\`
${trex_log}
\`\`\`
</details>"
        log_error "TRex start failed"
        exit 2
    fi

    post_pr_comment "## [Perf] Stage: TRex Started
TRex server running. Beginning benchmarks..."

    # ── Phase 5: Run benchmarks for each config ─────────────────────────────

    log_info "Phase 5: Running benchmarks..."
    log_info "DUT_INSTANCE_ID=$DUT_INSTANCE_ID TREX_INSTANCE_ID=$TREX_INSTANCE_ID"

    # Verify DUT SSM connectivity before starting benchmarks.
    # The DUT may still be building from user-data. Wait up to 120s for the build to finish.
    log_info "Verifying DUT SSM connectivity and build completion..."
    local dut_ready=false
    for attempt in $(seq 1 12); do
        local dut_check
        dut_check=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            "echo DUT_SSM_OK; ls /opt/dpdk-stdlib/target/release/echo 2>/dev/null && echo BUILD_DONE || echo BUILD_PENDING" 2>&1) || true
        log_info "DUT check attempt $attempt: $dut_check"
        if [[ "$dut_check" == *"DUT_SSM_OK"* && "$dut_check" == *"BUILD_DONE"* ]]; then
            dut_ready=true
            break
        elif [[ "$dut_check" == *"DUT_SSM_OK"* ]]; then
            log_info "DUT SSM works but build not done yet, waiting 10s..."
            sleep 10
        else
            log_warn "DUT SSM failed (attempt $attempt), waiting 10s..."
            sleep 10
        fi
    done

    if [[ "$dut_ready" != "true" ]]; then
        post_pr_comment "## [Perf] DUT Not Ready
DUT instance \`$DUT_INSTANCE_ID\` SSM connectivity or build not ready after 120s.
Last check output:
\`\`\`
${dut_check:-empty}
\`\`\`"
        log_error "DUT not ready, aborting benchmarks"
        exit 2
    fi

    post_pr_comment "## [Perf] DUT Ready
DUT instance \`$DUT_INSTANCE_ID\` SSM working, build complete."

    IFS=',' read -ra CONFIG_LIST <<< "$CONFIGS"
    local total_configs=${#CONFIG_LIST[@]}
    local config_idx=0
    local failed_configs=()

    for config in "${CONFIG_LIST[@]}"; do
        config_idx=$((config_idx + 1))
        log_info "=== Config $config_idx/$total_configs: $config ==="

        post_pr_comment "## [Perf] Stage: Benchmark ($config_idx/$total_configs)
Running \`$config\` benchmark...
Packet sizes: \`$PACKET_SIZES\` | Duration: ${DURATION}s/step | Target PPS: \`$RATE_STEPS\`"

        # Only run dut_stop_all_apps at the top of the FIRST iteration —
        # this is defensive in case a previous aborted run left echo/testpmd
        # processes alive. For iterations 2+, the previous iteration already
        # called dut_stop_all_apps at its tail (so [NIC-FINAL] was captured
        # in the log) and we don't need to do it again here.
        if [[ $config_idx -eq 1 ]]; then
            dut_stop_all_apps
        fi
        if [[ $config_idx -gt 1 ]]; then
            # Give the DUT time to settle between configs.
            # High-bandwidth benchmarks can overwhelm the kernel network stack
            # and make SSM temporarily unresponsive.
            log_info "Waiting 30s for DUT to settle between configs..."
            sleep 30
            local ssm_ok=false
            local ssm_retry
            for ssm_retry in 1 2 3 4 5; do
                local ssm_check
                ssm_check=$(ssm_run_command "$DUT_INSTANCE_ID" 30 "echo SSM_OK" 2>/dev/null) || true
                if [[ "$ssm_check" == *"SSM_OK"* ]]; then
                    ssm_ok=true
                    break
                fi
                log_warn "DUT SSM not responsive (attempt $ssm_retry), waiting 15s..."
                sleep 15
            done
            if [[ "$ssm_ok" == "false" ]]; then
                log_error "DUT SSM agent not responding after 5 attempts — skipping remaining configs"
                failed_configs+=("$config")
                break
            fi
            log_info "DUT SSM agent responsive, proceeding with $config"
        fi

        # Start the appropriate DUT config
        local start_ok=true
        case "$config" in
            rust-dpdk)           start_dut_rust_dpdk           || start_ok=false ;;
            rust-dpdk-multicore) start_dut_rust_dpdk_multicore || start_ok=false ;;
            tokio-dpdk)          start_dut_tokio_dpdk          || start_ok=false ;;
            native-dpdk)         start_dut_native_dpdk         || start_ok=false ;;
            rust-stdlib)         start_dut_rust_stdlib          || start_ok=false ;;
            plain-rust)          start_dut_plain_rust           || start_ok=false ;;
            *)
                log_error "Unknown config: $config"
                failed_configs+=("$config")
                continue
                ;;
        esac

        if [[ "$start_ok" == "false" ]]; then
            log_error "Failed to start DUT for config: $config"
            failed_configs+=("$config")
            # Post diagnostic info about the DUT start failure — use set +e to
            # ensure the diagnostic command itself doesn't fail due to set -e.
            local dut_diag
            dut_diag=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
                "set +e; echo '=== PCI State ==='; readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null || echo 'no driver'; ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null || echo 'no net iface'; echo '=== Processes ==='; ps aux | grep -E 'echo|testpmd|plain' | grep -v grep || echo 'none'; echo '=== DPDK bind ==='; /usr/local/bin/dpdk-devbind.py --status 2>/dev/null | head -15 || echo 'N/A'; echo '=== DPDK state ==='; ls -la /var/run/dpdk/ 2>/dev/null || echo 'no /var/run/dpdk'; echo '=== Last app logs ==='; for f in /var/log/echo-*.log /var/log/testpmd.log /var/log/plain-echo.log; do if [ -f \"\$f\" ]; then echo \"--- \$f ---\"; tail -10 \"\$f\"; fi; done; echo '=== Network ==='; ip addr show ens6 2>/dev/null || echo 'ens6 not found'; echo '=== vfio ==='; ls /dev/vfio/ 2>/dev/null || echo 'no /dev/vfio'; echo DIAG_DONE" 2>&1 || echo "(SSM failed)")
            post_pr_comment "## [Perf] DUT Start Failed: \`$config\`
DUT instance: \`$DUT_INSTANCE_ID\`
<details><summary>DUT diagnostics</summary>

\`\`\`
${dut_diag}
\`\`\`
</details>"
            # Collect diagnostics for this failure
            collect_networking_diagnostics "$DUT_INSTANCE_ID" "dut" "failure-${config}"
            continue
        fi

        # Run the benchmark
        if ! run_benchmark_for_config "$config"; then
            log_error "Benchmark failed for config: $config"
            failed_configs+=("$config")
            collect_networking_diagnostics "$DUT_INSTANCE_ID" "dut" "failure-${config}"
            collect_networking_diagnostics "$TREX_INSTANCE_ID" "trex" "failure-${config}"
        fi

        # For plain-rust, capture kernel NIC counters NOW — before the
        # process is killed — because ethtool counters live in the kernel
        # and are independent of the echo process. For DPDK configs we
        # must defer log collection until AFTER dut_stop_all_apps (see
        # below) so the `[NIC-FINAL]` line has a chance to be emitted.
        if [[ "$config" == "plain-rust" ]]; then
            local ethtool_final
            ethtool_final=$(ssm_run_command "$DUT_INSTANCE_ID" 60 \
                "set -e; for retry in 1 2 3; do IFACE=\$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); if [ -n \"\$IFACE\" ]; then OUT=\$(ethtool -S \$IFACE 2>&1); if echo \"\$OUT\" | grep -q ': [0-9]'; then echo \"\$OUT\"; exit 0; fi; fi; sleep 2; done; echo 'ETHTOOL_FINAL_FAILED iface=\$IFACE'; exit 0" 2>/dev/null || echo "(SSM failed)")
            echo "$ethtool_final" > "$LOGS_DIR/dut-plain-rust-ethtool-final.txt"
            local final_lines
            final_lines=$(echo "$ethtool_final" | wc -l)
            log_info "plain-rust ethtool final captured ($final_lines lines, head: $(echo "$ethtool_final" | head -2 | tr '\n' ' '))"
        fi

        # Stop the DUT BEFORE collecting the app log. This is essential
        # for DPDK configs: PerfReporter's one-shot `[NIC-FINAL]` line is
        # only emitted when the reporter thread is joined (either via
        # `disable_perf_reporting` on clean shutdown or via `Drop` when
        # the socket is dropped). If we grep the log while the DUT is
        # still running, `[NIC-FINAL]` won't be there yet and the
        # instrumentation self-check reports "no FINAL (abnormal shutdown)".
        # We used to call dut_stop_all_apps at the top of the next
        # iteration, but that was too late — the current iteration had
        # already scraped the log.
        dut_stop_all_apps

        # Collect DUT app log for this config
        local log_file
        case "$config" in
            rust-dpdk)           log_file="/var/log/echo-rust-dpdk.log" ;;
            rust-dpdk-multicore) log_file="/var/log/echo-rust-dpdk-multicore.log" ;;
            tokio-dpdk)          log_file="/var/log/echo-tokio-dpdk.log" ;;
            native-dpdk)         log_file="/var/log/testpmd.log" ;;
            rust-stdlib)         log_file="/var/log/echo-rust-stdlib.log" ;;
            plain-rust)          log_file="/var/log/plain-echo.log" ;;
        esac
        if [[ -n "${log_file:-}" ]]; then
            local app_log
            app_log=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
                "tail -50 $log_file 2>/dev/null || echo '(no log)'" 2>/dev/null || echo "(failed)")
            echo "$app_log" > "$LOGS_DIR/dut-${config}-app.log"

            # Also pull every [PERF] line from the full log so we can compute
            # per-step App Drops in aggregate_results. Each [PERF] line is ~300
            # bytes; even at 30+ lines per config the total is well under SSM's
            # output cap. Save to a separate file the aggregator reads.
            # Also capture [NIC-BASELINE] / [NIC-FINAL] — one-shot lines the
            # PerfReporter emits at startup and clean shutdown. They let the
            # aggregator cross-check sum-of-tick-deltas against the end-minus-
            # start NIC counter delta (instrumentation self-check).
            local perf_lines
            perf_lines=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
                "grep -E '^\[PERF\]|^\[NIC-BASELINE\]|^\[NIC-FINAL\]' $log_file 2>/dev/null || echo ''" 2>/dev/null || echo "")
            echo "$perf_lines" > "$LOGS_DIR/dut-${config}-perf.log"
            # Post testpmd log to PR for visibility (stats-period output shows RX/TX counters)
            if [[ "$config" == "native-dpdk" ]]; then
                local testpmd_diag
                testpmd_diag=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
                    "echo '=== testpmd stats (last 30 lines) ==='; tail -30 /var/log/testpmd.log 2>/dev/null || echo '(no log)'; echo '=== ENA port stats ==='; cat /sys/bus/pci/devices/0000:00:06.0/net/*/statistics/rx_packets 2>/dev/null || echo 'N/A (vfio-pci)'" 2>/dev/null || echo "(failed)")
                post_pr_comment "## [Perf] Diag: testpmd log
<details><summary>testpmd output (last 30 lines)</summary>

\`\`\`
${testpmd_diag}
\`\`\`
</details>"
            fi
        fi
    done

    # Stop TRex and DUT
    dut_stop_all_apps || true
    stop_trex_server || true

    # ── Phase 6: Aggregate results and post summary ──────────────────────────
    # Disable set -e for the reporting phase — failures here should not mask
    # the actual test outcome.
    set +e

    log_info "Phase 6: Aggregating results..."
    aggregate_results
    local summary
    summary=$(generate_markdown_summary)

    # Add failure info if any
    if [[ ${#failed_configs[@]} -gt 0 ]]; then
        summary="$summary

### Failed Configs
$(printf -- '- `%s`\n' "${failed_configs[@]}")"
    fi

    # Post to PR
    post_pr_comment "## [Perf] Stage: Results

$summary

[Full results artifact](${GITHUB_SERVER_URL:-}/${GITHUB_REPOSITORY:-}/actions/runs/${GITHUB_RUN_ID:-})"

    # Write to GitHub Actions step summary
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "$summary" >> "$GITHUB_STEP_SUMMARY"
    fi

    # Collect final logs
    collect_instance_logs "$DUT_INSTANCE_ID" "dut"
    collect_instance_logs "$TREX_INSTANCE_ID" "trex"

    set -e

    # Exit with failure if any configs failed
    if [[ ${#failed_configs[@]} -gt 0 ]]; then
        log_error "${#failed_configs[@]} config(s) failed: ${failed_configs[*]}"
        exit 1
    fi

    log_info "=== All performance tests completed successfully ==="
}

main "$@"
