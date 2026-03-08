#!/usr/bin/env bash
# =============================================================================
# run-perf-tests.sh — Performance test orchestrator for dpdk-stdlib-rust
#
# Deploys a TRex generator + DUT instance, runs UDP echo benchmarks across
# 4 configurations (rust-dpdk, native-dpdk, rust-stdlib, plain-rust),
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
#   --rate-steps        Comma-separated rate percentages (default: 10,25,50,75,100)
#   --configs           Comma-separated DUT configs (default: rust-dpdk,native-dpdk,rust-stdlib,plain-rust)
#   --json-summary      Write JSON summary file
#   -h, --help          Show help
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Defaults ──────────────────────────────────────────────────────────────────

TEARDOWN=true
SKIP_DEPLOY=false
PACKET_SIZES="64,512,1400"
DURATION=30
RATE_STEPS="10,25,50,75,100"
CONFIGS="rust-dpdk,native-dpdk,rust-stdlib,plain-rust"
JSON_SUMMARY=false

CDK_STACK_NAME="PerfTestStack"
CDK_DIR="$REPO_ROOT/deploy/cdk"
RESULTS_DIR="$REPO_ROOT/perf-results"
LOGS_DIR="$REPO_ROOT/instance-logs"

SSM_READINESS_TIMEOUT=600
TREX_START_TIMEOUT=120
BENCHMARK_TIMEOUT=600

TREX_INSTANCE_ID=""
DUT_INSTANCE_ID=""
TREX_DATA_ENI_IP=""
DUT_DATA_ENI_IP=""
TREX_GATEWAY_MAC=""
TREX_DATA_MAC=""

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

    local cmd_id
    cmd_id=$(aws ssm send-command \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters "{\"commands\":[${escaped_command}]}" \
        --timeout-seconds "$timeout_sec" \
        --query "Command.CommandId" \
        --output text 2>/dev/null)

    if [[ -z "$cmd_id" ]]; then
        log_error "Failed to send SSM command to $instance_id"
        return 1
    fi

    # Wait for completion
    local elapsed=0
    while [[ $elapsed -lt $timeout_sec ]]; do
        local status
        status=$(aws ssm get-command-invocation \
            --command-id "$cmd_id" \
            --instance-id "$instance_id" \
            --query "Status" \
            --output text 2>/dev/null || echo "Pending")

        case "$status" in
            Success) break ;;
            Failed|Cancelled|TimedOut)
                log_error "SSM command $cmd_id on $instance_id: $status"
                # Save stderr for diagnostics
                aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "StandardErrorContent" \
                    --output text 2>/dev/null || true
                return 1
                ;;
        esac
        sleep 5
        elapsed=$((elapsed + 5))
    done

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

# ── DUT NIC Management ────────────────────────────────────────────────────────

dut_bind_dpdk() {
    log_info "Binding DUT secondary ENI to vfio-pci (DPDK mode)..."
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "ip link set ens6 down 2>/dev/null || true; /usr/local/bin/dpdk-devbind.py --bind=vfio-pci 0000:00:06.0 && echo 'Bound to vfio-pci'" \
        || { log_error "Failed to bind DUT ENI to vfio-pci"; return 1; }
}

dut_bind_kernel() {
    log_info "Binding DUT secondary ENI to kernel driver (kernel mode)..."
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        "/usr/local/bin/dpdk-devbind.py --bind=ena 0000:00:06.0 2>/dev/null || true; sleep 2; ip link set ens6 up 2>/dev/null || true; ip addr add ${DUT_DATA_ENI_IP}/24 dev ens6 2>/dev/null || true; echo 'Bound to kernel (ena)'" \
        || { log_error "Failed to bind DUT ENI to kernel"; return 1; }
}

dut_stop_all_apps() {
    log_info "Stopping all DUT applications..."
    ssm_run_command "$DUT_INSTANCE_ID" 15 \
        "pkill -f 'target/release/echo' 2>/dev/null || true; pkill -f 'target/release/plain-echo' 2>/dev/null || true; pkill -f testpmd 2>/dev/null || true; rm -rf /var/run/dpdk/ 2>/dev/null || true; sleep 2; echo 'All apps stopped'" \
        2>/dev/null || true
}

# ── TRex Management ──────────────────────────────────────────────────────────

generate_trex_config() {
    log_info "Generating TRex configuration..."

    # Step 1: Discover TRex data ENI source MAC via IMDS (most reliable)
    # IMDS is authoritative and doesn't depend on sysfs timing or interface naming.
    # We query all MACs, find the one with device-number=1 (secondary ENI).
    log_info "Step 1: Discovering TRex data ENI MAC via IMDS..."
    TREX_DATA_MAC=""

    # Single SSM command: get token, list MACs, find device-number 1, return its MAC
    local imds_result
    imds_result=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "TOKEN=\$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); MACS=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/); echo \"ALL_MACS: \$MACS\"; for mac in \$MACS; do mac=\${mac%/}; dn=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/\${mac}/device-number); echo \"MAC=\${mac} DN=\${dn}\"; if [ \"\$dn\" = \"1\" ]; then echo \"FOUND_MAC: \${mac}\"; fi; done" 2>/dev/null || echo "SSM_FAILED")
    log_info "IMDS MAC discovery output: $(echo "$imds_result" | head -10)"

    # Extract the MAC from the FOUND_MAC line
    TREX_DATA_MAC=$(echo "$imds_result" | grep "^FOUND_MAC:" | head -1 | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' || echo "")

    # Fallback: try sysfs if IMDS didn't work
    if [[ -z "$TREX_DATA_MAC" ]]; then
        log_warn "IMDS discovery failed, falling back to sysfs..."
        local sysfs_result
        sysfs_result=$(ssm_run_command "$TREX_INSTANCE_ID" 15 \
            "for iface in ens6 eth1; do if [ -f /sys/class/net/\$iface/address ]; then echo \"SYSFS_MAC: \$(cat /sys/class/net/\$iface/address)\"; break; fi; done" 2>/dev/null || echo "")
        TREX_DATA_MAC=$(echo "$sysfs_result" | grep "^SYSFS_MAC:" | head -1 | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' || echo "")
    fi

    if [[ -z "$TREX_DATA_MAC" || ! "$TREX_DATA_MAC" =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}$ ]]; then
        log_error "Could not discover TRex data ENI MAC (got: '$TREX_DATA_MAC')"
        log_error "IMDS output was: $(echo "$imds_result" | head -10)"
        return 1
    fi
    log_info "TRex data ENI MAC: $TREX_DATA_MAC"

    # Step 2: Discover gateway MAC while ENI is still in kernel mode
    # In AWS VPC, all frames must use gateway MAC (L3-routed, not L2-switched)
    local subnet_gw
    subnet_gw=$(echo "$TREX_DATA_ENI_IP" | sed 's/\.[0-9]*$/.1/')
    log_info "Step 2: Discovering gateway MAC (subnet gateway: $subnet_gw)..."

    local gw_raw
    gw_raw=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "ip link set ens6 up 2>/dev/null || true; ip addr add ${TREX_DATA_ENI_IP}/24 dev ens6 2>/dev/null || true; sleep 1; ping -c 2 -W 2 $subnet_gw 2>/dev/null || true; ping -c 2 -W 2 ${DUT_DATA_ENI_IP} 2>/dev/null || true; sleep 2; echo NEIGH:; ip neigh show dev ens6 2>/dev/null || echo none; echo GW_ENTRY:; ip neigh show ${subnet_gw} dev ens6 2>/dev/null || echo none" 2>/dev/null || echo "SSM_FAILED")
    log_info "Gateway discovery raw: $(echo "$gw_raw" | tail -8)"

    # Extract gateway MAC specifically from the GW_ENTRY line (targeted at subnet .1)
    TREX_GATEWAY_MAC=$(echo "$gw_raw" | grep -A1 "^GW_ENTRY:" | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' | head -1)
    # Fallback: any MAC from the neighbor table
    if [[ -z "$TREX_GATEWAY_MAC" ]]; then
        TREX_GATEWAY_MAC=$(echo "$gw_raw" | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' | head -1)
    fi

    if [[ -z "$TREX_GATEWAY_MAC" ]]; then
        log_error "Could not discover gateway MAC on TRex data ENI — packets will be dropped by VPC"
        return 1
    fi
    log_info "Gateway MAC: $TREX_GATEWAY_MAC"

    # Step 3: Unbind ens6 from kernel driver so TRex can bind it via vfio-pci
    # Use sysfs driver_override — works without any DPDK tools installed.
    log_info "Step 3: Unbinding TRex data ENI from kernel driver..."
    local bind_result
    bind_result=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "set -x; PCI=0000:00:06.0; ip link set ens6 down 2>/dev/null || true; modprobe vfio-pci 2>/dev/null || true; echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || true; echo \$PCI > /sys/bus/pci/devices/\$PCI/driver/unbind 2>/dev/null || true; sleep 1; echo vfio-pci > /sys/bus/pci/devices/\$PCI/driver_override; echo \$PCI > /sys/bus/pci/drivers/vfio-pci/bind && echo BIND_OK || echo BIND_FAIL; ls /sys/bus/pci/drivers/vfio-pci/\$PCI 2>/dev/null && echo VERIFIED || echo NOT_VERIFIED" 2>/dev/null || echo "SSM_FAILED")
    log_info "NIC bind result: $(echo "$bind_result" | grep -E 'BIND_|VERIFIED|FAIL' | head -5)"
    if [[ "$bind_result" != *"BIND_OK"* ]]; then
        log_warn "vfio-pci binding may have failed — TRex might not start"
    fi

    # Step 4: Write /etc/trex_cfg.yaml via SSM
    # Use base64 encoding to avoid all quoting/heredoc issues through SSM JSON layer
    log_info "Step 4: Writing TRex config..."
    local yaml_content
    yaml_content=$(cat <<YAMLEOF
- port_limit: 1
  version: 2
  interfaces: ['00:06.0']
  port_info:
    - dest_mac: '${TREX_GATEWAY_MAC}'
      src_mac:  '${TREX_DATA_MAC}'
YAMLEOF
)
    local yaml_b64
    yaml_b64=$(echo "$yaml_content" | base64 -w0)

    local write_result
    write_result=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "echo $yaml_b64 | base64 -d > /etc/trex_cfg.yaml && echo WROTE || echo WRITE_ERR; cat /etc/trex_cfg.yaml 2>/dev/null || true" 2>/dev/null || echo "SSM_WRITE_FAILED")
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

    # Verify NIC is bound to vfio-pci before starting TRex
    local nic_state
    nic_state=$(ssm_run_command "$TREX_INSTANCE_ID" 15 \
        "echo NIC_STATE:; ls -la /sys/bus/pci/drivers/vfio-pci/0000:00:06.0 2>/dev/null && echo VFIO_OK || echo VFIO_NOT_BOUND; ls /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null && readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null || echo NO_DRIVER; cat /etc/trex_cfg.yaml 2>/dev/null; ls /opt/trex/t-rex-64 2>/dev/null && echo TREX_BINARY_OK || echo TREX_BINARY_MISSING" 2>/dev/null || echo "SSM_FAILED")
    log_info "Pre-start NIC state: $(echo "$nic_state" | grep -E 'VFIO|DRIVER|BINARY|port_limit|src_mac|dest_mac' | head -8)"

    if [[ "$nic_state" == *"VFIO_NOT_BOUND"* ]]; then
        log_warn "NIC not bound to vfio-pci — attempting fallback with TRex dpdk_setup_ports.py"
        ssm_run_command "$TREX_INSTANCE_ID" 30 \
            "cd /opt/trex && python3 dpdk_setup_ports.py -b vfio-pci 0000:00:06.0 2>&1 || echo SETUP_PORTS_FAILED; ls /sys/bus/pci/drivers/vfio-pci/0000:00:06.0 2>/dev/null && echo VFIO_OK || echo STILL_NOT_BOUND" 2>/dev/null || true
    fi

    # Start in background via nohup
    ssm_run_command_fire_and_forget "$TREX_INSTANCE_ID" 300 \
        "cd /opt/trex && nohup ./t-rex-64 -i --cfg /etc/trex_cfg.yaml -c 2 > /var/log/trex-server.log 2>&1 &"

    # Wait for TRex to be ready
    local elapsed=0
    while [[ $elapsed -lt $TREX_START_TIMEOUT ]]; do
        local status
        status=$(ssm_run_command "$TREX_INSTANCE_ID" 10 \
            "pgrep -f t-rex-64 >/dev/null && echo 'running' || echo 'not running'" 2>/dev/null || echo "unknown")

        if [[ "$status" == *"running"* ]]; then
            log_info "TRex server is running (${elapsed}s)"
            # Give it a few more seconds to initialize
            sleep 5
            return 0
        fi
        # If TRex exited early, check log immediately
        if [[ $elapsed -ge 15 ]]; then
            local early_log
            early_log=$(ssm_run_command "$TREX_INSTANCE_ID" 10 \
                "cat /var/log/trex-server.log 2>/dev/null | tail -20 || echo '(no log yet)'" 2>/dev/null || echo "")
            if [[ -n "$early_log" && "$early_log" != *"(no log yet)"* ]]; then
                log_info "TRex log (early check): $(echo "$early_log" | tail -5)"
            fi
        fi
        sleep 5
        elapsed=$((elapsed + 5))
    done

    log_error "TRex server failed to start within ${TREX_START_TIMEOUT}s"
    # Collect TRex log for diagnostics
    ssm_run_command "$TREX_INSTANCE_ID" 10 "tail -50 /var/log/trex-server.log 2>/dev/null || echo '(no log)'" 2>/dev/null || true
    return 1
}

stop_trex_server() {
    log_info "Stopping TRex server..."
    ssm_run_command "$TREX_INSTANCE_ID" 15 \
        "pkill -f t-rex-64 2>/dev/null || true; sleep 2; echo 'TRex stopped'" 2>/dev/null || true
}

# ── Benchmark Runner ──────────────────────────────────────────────────────────

run_benchmark_for_config() {
    local config_name="$1"
    local dst_port="${2:-9000}"

    log_info "Running TRex benchmark for config: $config_name"

    # Copy benchmark script to TRex instance
    local benchmark_script
    benchmark_script=$(cat "$SCRIPT_DIR/perf-tests/trex/run_benchmark.py")

    ssm_run_command "$TREX_INSTANCE_ID" 15 \
        "mkdir -p /opt/perf-tests && cat > /opt/perf-tests/run_benchmark.py << 'PYSCRIPT'
${benchmark_script}
PYSCRIPT
chmod +x /opt/perf-tests/run_benchmark.py" || {
        log_error "Failed to copy benchmark script to TRex"
        return 1
    }

    # Use the gateway MAC discovered during generate_trex_config
    log_info "Using gateway MAC: ${TREX_GATEWAY_MAC:-unknown}"

    # Run benchmark via SSM
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
        --output '/tmp/perf-results/${config_name}.json'"

    local output
    output=$(ssm_run_command "$TREX_INSTANCE_ID" "$BENCHMARK_TIMEOUT" "$bench_cmd" 2>/dev/null)
    local exit_code=$?

    echo "$output"

    if [[ $exit_code -ne 0 ]]; then
        log_error "Benchmark failed for $config_name"
        mkdir -p "$LOGS_DIR"
        echo "$output" > "$LOGS_DIR/trex-benchmark-${config_name}.log"
        return 1
    fi

    # Download results from TRex instance
    local results_json
    results_json=$(ssm_run_command "$TREX_INSTANCE_ID" 15 \
        "cat /tmp/perf-results/${config_name}.json 2>/dev/null || echo '{}'" 2>/dev/null)

    mkdir -p "$RESULTS_DIR"
    echo "$results_json" > "$RESULTS_DIR/${config_name}.json"
    log_info "Results saved to $RESULTS_DIR/${config_name}.json"
}

# ── DUT Config Runners ────────────────────────────────────────────────────────

start_dut_rust_dpdk() {
    log_info "Starting DUT: rust-dpdk (echo server with DPDK backend)"
    dut_bind_dpdk || return 1

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/echo-rust-dpdk.log 2>&1 &"
    sleep 5

    # Verify it's running
    local status
    status=$(ssm_run_command "$DUT_INSTANCE_ID" 10 \
        "pgrep -f 'target/release/echo' >/dev/null && echo 'running' || echo 'not running'" 2>/dev/null)
    if [[ "$status" != *"running"* ]]; then
        log_error "rust-dpdk echo server failed to start"
        ssm_run_command "$DUT_INSTANCE_ID" 10 "tail -20 /var/log/echo-rust-dpdk.log 2>/dev/null" 2>/dev/null || true
        return 1
    fi
    log_info "rust-dpdk echo server running"
}

start_dut_native_dpdk() {
    log_info "Starting DUT: native-dpdk (testpmd macswap)"
    dut_bind_dpdk || return 1

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "nohup /usr/local/bin/dpdk-testpmd -l 0-1 -n 4 --vdev=net_vfio0 -- --forward-mode=macswap --port-topology=chained --auto-start > /var/log/testpmd.log 2>&1 &"
    sleep 5

    local status
    status=$(ssm_run_command "$DUT_INSTANCE_ID" 10 \
        "pgrep -f testpmd >/dev/null && echo 'running' || echo 'not running'" 2>/dev/null)
    if [[ "$status" != *"running"* ]]; then
        log_error "testpmd failed to start"
        ssm_run_command "$DUT_INSTANCE_ID" 10 "tail -20 /var/log/testpmd.log 2>/dev/null" 2>/dev/null || true
        return 1
    fi
    log_info "testpmd macswap running"
}

start_dut_rust_stdlib() {
    log_info "Starting DUT: rust-stdlib (echo server with kernel backend)"
    dut_bind_kernel || return 1

    # The echo binary without DPDK feature falls back to std::net
    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/echo-rust-stdlib.log 2>&1 &"
    sleep 3

    local status
    status=$(ssm_run_command "$DUT_INSTANCE_ID" 10 \
        "pgrep -f 'target/release/echo' >/dev/null && echo 'running' || echo 'not running'" 2>/dev/null)
    if [[ "$status" != *"running"* ]]; then
        log_error "rust-stdlib echo server failed to start"
        ssm_run_command "$DUT_INSTANCE_ID" 10 "tail -20 /var/log/echo-rust-stdlib.log 2>/dev/null" 2>/dev/null || true
        return 1
    fi
    log_info "rust-stdlib echo server running"
}

start_dut_plain_rust() {
    log_info "Starting DUT: plain-rust (minimal std::net echo server)"
    dut_bind_kernel || return 1

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/plain-echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/plain-echo.log 2>&1 &"
    sleep 3

    local status
    status=$(ssm_run_command "$DUT_INSTANCE_ID" 10 \
        "pgrep -f 'target/release/plain-echo' >/dev/null && echo 'running' || echo 'not running'" 2>/dev/null)
    if [[ "$status" != *"running"* ]]; then
        log_error "plain-rust echo server failed to start"
        ssm_run_command "$DUT_INSTANCE_ID" 10 "tail -20 /var/log/plain-echo.log 2>/dev/null" 2>/dev/null || true
        return 1
    fi
    log_info "plain-rust echo server running"
}

# ── Results Aggregation ───────────────────────────────────────────────────────

aggregate_results() {
    log_info "Aggregating performance results..."
    mkdir -p "$RESULTS_DIR"

    python3 - <<'PYEOF'
import json, glob, os, sys
from datetime import datetime, timezone

results_dir = os.environ.get("RESULTS_DIR", "perf-results")
output_file = os.path.join(results_dir, "perf-report.json")

configs = {}
for f in sorted(glob.glob(os.path.join(results_dir, "*.json"))):
    if os.path.basename(f) == "perf-report.json":
        continue
    try:
        with open(f) as fh:
            data = json.load(fh)
            name = data.get("config_name", os.path.basename(f).replace(".json", ""))
            configs[name] = data
    except Exception as e:
        print(f"Warning: failed to read {f}: {e}", file=sys.stderr)

report = {
    "timestamp": datetime.now(timezone.utc).isoformat(),
    "commit": os.environ.get("GITHUB_SHA", "unknown"),
    "instance_type": "c5n.2xlarge",
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

    for pkt_size in sorted(all_sizes):
        lines.append(f"### {pkt_size} packets")
        lines.append("")
        lines.append("| Config | Rate | TX pps | RX pps | Drop % | Lat Avg (us) | Lat Max (us) | TX Mbps |")
        lines.append("|--------|------|--------|--------|--------|-------------|-------------|---------|")

        for cfg_name in ["native-dpdk", "rust-dpdk", "rust-stdlib", "plain-rust"]:
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
                rate = f"{r.get('offered_pct', 0)}%"

                lines.append(f"| {cfg_name} | {rate} | {tx_pps} | {rx_pps} | {drop} | {lat_avg_s} | {lat_max_s} | {tx_mbps} |")

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
        cd "$CDK_DIR"
        npx cdk destroy "$CDK_STACK_NAME" --force 2>/dev/null || log_warn "Teardown failed"
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
        log_info "Phase 1: Deploying PerfTestStack..."
        post_pr_comment "## [Perf] Stage: Deploy
Deploying \`PerfTestStack\` (TRex + DUT on c5n.2xlarge)...
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
            context_args="$context_args -c dpdkAmiId=$dpdk_ami"
            log_info "Using pre-built DPDK AMI: $dpdk_ami"
        fi
        if [[ -n "$trex_ami" ]]; then
            context_args="$context_args -c trexAmiId=$trex_ami"
            log_info "Using pre-built TRex AMI: $trex_ami"
        fi

        npx cdk deploy "$CDK_STACK_NAME" --require-approval never $context_args \
            || { log_error "CDK deploy failed"; exit 2; }

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
    DUT_DATA_ENI_IP=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs[?OutputKey=='DutDataEniPrivateIp'].OutputValue" \
        --output text)

    log_info "TRex: $TREX_INSTANCE_ID (data IP: $TREX_DATA_ENI_IP)"
    log_info "DUT:  $DUT_INSTANCE_ID (data IP: $DUT_DATA_ENI_IP)"

    # Wait for both instances
    wait_ssm_ready "$TREX_INSTANCE_ID" "TRex" &
    local trex_wait_pid=$!
    wait_ssm_ready "$DUT_INSTANCE_ID" "DUT" &
    local dut_wait_pid=$!

    wait "$trex_wait_pid" || { log_error "TRex SSM not ready"; exit 2; }
    wait "$dut_wait_pid"  || { log_error "DUT SSM not ready"; exit 2; }

    post_pr_comment "## [Perf] Stage: Instances Ready
- TRex: \`$TREX_INSTANCE_ID\` (${TREX_DATA_ENI_IP})
- DUT: \`$DUT_INSTANCE_ID\` (${DUT_DATA_ENI_IP})"

    # ── Phase 2b: Ensure secondary ENIs are attached and bound ────────────────
    # The ENI attachments are separate CloudFormation resources that may complete
    # after the instance boots. Wait for them and bind via SSM.

    log_info "Phase 2b: Ensuring secondary ENIs are attached..."
    # TRex ENI stays in kernel mode for now — we need it for gateway MAC discovery.
    # TRex will bind it to DPDK internally when started.
    wait_and_bind_eni "$TREX_INSTANCE_ID" "TRex" "ena" \
        || { log_error "TRex ENI attachment failed"; exit 2; }
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
- TREX_DATA_MAC: \`${TREX_DATA_MAC:-unset}\`
- TREX_GATEWAY_MAC: \`${TREX_GATEWAY_MAC:-unset}\`"
        log_error "TRex config failed"
        exit 2
    fi

    post_pr_comment "## [Perf] Stage: TRex Config OK
- Data MAC: \`$TREX_DATA_MAC\`
- Gateway MAC: \`$TREX_GATEWAY_MAC\`
Starting TRex server..."

    if ! start_trex_server; then
        # Grab TRex log and NIC state for diagnostics
        local trex_log
        trex_log=$(ssm_run_command "$TREX_INSTANCE_ID" 15 \
            "echo '=== TRex Log ==='; tail -30 /var/log/trex-server.log 2>/dev/null || echo '(no log)'; echo '=== NIC State ==='; readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null || echo 'no driver'; ls /sys/bus/pci/drivers/vfio-pci/0000:00:06.0 2>/dev/null && echo 'vfio-pci: YES' || echo 'vfio-pci: NO'; echo '=== vfio modules ==='; lsmod 2>/dev/null | grep vfio || echo 'none'; echo '=== noiommu ==='; cat /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || echo 'N/A'" 2>/dev/null || echo "(SSM failed)")
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
    IFS=',' read -ra CONFIG_LIST <<< "$CONFIGS"
    local total_configs=${#CONFIG_LIST[@]}
    local config_idx=0
    local failed_configs=()

    for config in "${CONFIG_LIST[@]}"; do
        config_idx=$((config_idx + 1))
        log_info "=== Config $config_idx/$total_configs: $config ==="

        post_pr_comment "## [Perf] Stage: Benchmark ($config_idx/$total_configs)
Running \`$config\` benchmark...
Packet sizes: \`$PACKET_SIZES\` | Duration: ${DURATION}s/step | Rates: \`$RATE_STEPS\`%"

        # Stop any running DUT apps
        dut_stop_all_apps

        # Start the appropriate DUT config
        local start_ok=true
        case "$config" in
            rust-dpdk)    start_dut_rust_dpdk   || start_ok=false ;;
            native-dpdk)  start_dut_native_dpdk || start_ok=false ;;
            rust-stdlib)  start_dut_rust_stdlib  || start_ok=false ;;
            plain-rust)   start_dut_plain_rust   || start_ok=false ;;
            *)
                log_error "Unknown config: $config"
                failed_configs+=("$config")
                continue
                ;;
        esac

        if [[ "$start_ok" == "false" ]]; then
            log_error "Failed to start DUT for config: $config"
            failed_configs+=("$config")
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

        # Collect DUT app log for this config
        local log_file
        case "$config" in
            rust-dpdk)    log_file="/var/log/echo-rust-dpdk.log" ;;
            native-dpdk)  log_file="/var/log/testpmd.log" ;;
            rust-stdlib)  log_file="/var/log/echo-rust-stdlib.log" ;;
            plain-rust)   log_file="/var/log/plain-echo.log" ;;
        esac
        if [[ -n "${log_file:-}" ]]; then
            local app_log
            app_log=$(ssm_run_command "$DUT_INSTANCE_ID" 10 \
                "tail -50 $log_file 2>/dev/null || echo '(no log)'" 2>/dev/null || echo "(failed)")
            echo "$app_log" > "$LOGS_DIR/dut-${config}-app.log"
        fi
    done

    # Stop TRex and DUT
    dut_stop_all_apps
    stop_trex_server

    # ── Phase 6: Aggregate results and post summary ──────────────────────────

    log_info "Phase 6: Aggregating results..."
    aggregate_results
    local summary
    summary=$(generate_markdown_summary)

    # Add failure info if any
    if [[ ${#failed_configs[@]} -gt 0 ]]; then
        summary="$summary

### Failed Configs
$(printf '- `%s`\n' "${failed_configs[@]}")"
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

    # Exit with failure if any configs failed
    if [[ ${#failed_configs[@]} -gt 0 ]]; then
        log_error "${#failed_configs[@]} config(s) failed: ${failed_configs[*]}"
        exit 1
    fi

    log_info "=== All performance tests completed successfully ==="
}

main "$@"
