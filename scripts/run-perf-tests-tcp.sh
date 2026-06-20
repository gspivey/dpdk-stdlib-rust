#!/usr/bin/env bash
# =============================================================================
# run-perf-tests-tcp.sh - TCP Performance test orchestrator for dpdk-stdlib-rust
#
# Deploys a TRex generator + DUT instance, runs TCP echo benchmarks (ASTF mode)
# across 3 configurations (plain-rust-tcp, rust-dpdk-tcp, tokio-dpdk-tcp),
# collects structured JSON results, and posts a summary to the PR.
#
# Usage:
#   ./scripts/run-perf-tests-tcp.sh [OPTIONS]
#
# Options:
#   --teardown          Destroy CDK stack when done (default: true)
#   --no-teardown       Keep CDK stack after tests
#   --skip-deploy       Skip CDK deploy (reuse existing stack)
#   --payload-sizes     Comma-separated sizes (default: 64,512,1400,65536)
#   --duration          Seconds per rate step (default: 30)
#   --cps-rates         Comma-separated target CPS values (default: 100,500,1000,5000)
#   --configs           Comma-separated DUT configs (default: plain-rust-tcp,rust-dpdk-tcp,tokio-dpdk-tcp)
#   --json-summary      Write JSON summary file
#   -h, --help          Show help
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# -- Defaults --

TEARDOWN=true
SKIP_DEPLOY=false
PAYLOAD_SIZES="64,512,1400,65536"
DURATION=30
CPS_RATES="100,500,1000,5000"
CONFIGS="plain-rust-tcp,rust-dpdk-tcp,tokio-dpdk-tcp"
JSON_SUMMARY=false

CDK_STACK_NAME="${CDK_STACK_NAME:-PerfTestStack}"
CDK_DIR="$REPO_ROOT/deploy/cdk"
RESULTS_DIR="$REPO_ROOT/perf-results-tcp"
LOGS_DIR="$REPO_ROOT/instance-logs-tcp"
export RESULTS_DIR LOGS_DIR

SSM_READINESS_TIMEOUT=600
TREX_START_TIMEOUT=120
BENCHMARK_TIMEOUT=600

TREX_INSTANCE_ID=""
DUT_INSTANCE_ID=""
TREX_DATA_ENI_IP=""
TREX_DATA_RX_ENI_IP=""
DUT_DATA_ENI_IP=""
TREX_GATEWAY_MAC=""
TREX_DATA_MAC=""
TREX_DATA_RX_MAC=""

# -- CLI Parsing --

while [[ $# -gt 0 ]]; do
    case "$1" in
        --teardown)       TEARDOWN=true; shift ;;
        --no-teardown)    TEARDOWN=false; shift ;;
        --skip-deploy)    SKIP_DEPLOY=true; shift ;;
        --payload-sizes)  PAYLOAD_SIZES="$2"; shift 2 ;;
        --duration)       DURATION="$2"; shift 2 ;;
        --cps-rates)      CPS_RATES="$2"; shift 2 ;;
        --configs)        CONFIGS="$2"; shift 2 ;;
        --json-summary)   JSON_SUMMARY=true; shift ;;
        -h|--help)
            head -25 "$0" | grep -E '^#' | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# -- Logging --

log_info()  { echo "[$(date -u +%H:%M:%S)] INFO  $*"; }
log_warn()  { echo "[$(date -u +%H:%M:%S)] WARN  $*" >&2; }
log_error() { echo "[$(date -u +%H:%M:%S)] ERROR $*" >&2; }

# -- PR Comment Helper --

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

# -- SSM Helpers --

ssm_run_command() {
    local instance_id="$1"
    local timeout_sec="$2"
    shift 2
    local command="$*"

    local escaped_command
    escaped_command=$(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$command")

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
        log_warn "SSM send-command attempt $retry failed: $send_err"
        sleep $((retry * 3))
    done

    if [[ -z "$cmd_id" ]]; then
        log_error "Failed to send SSM command after 3 attempts: $send_err"
        return 1
    fi

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
            Success) completed=true; break ;;
            Failed|Cancelled|TimedOut)
                log_error "SSM command $cmd_id: $status"
                aws ssm get-command-invocation \
                    --command-id "$cmd_id" \
                    --instance-id "$instance_id" \
                    --query "StandardOutputContent" \
                    --output text 2>/dev/null || true
                return 1
                ;;
        esac
        sleep 5
        elapsed=$((elapsed + 5))
    done

    if [[ "$completed" != "true" ]]; then
        log_error "SSM command $cmd_id timed out after ${timeout_sec}s"
        return 1
    fi

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

# -- Failure JSON --

write_failure_json() {
    local step="$1"
    local message="$2"
    mkdir -p "$LOGS_DIR"

    python3 -c "
import json, datetime
data = {
    'failed_step': '$step',
    'error': '$message',
    'exit_code': 2,
    'timestamp': datetime.datetime.utcnow().isoformat() + 'Z',
    'trex_instance_id': '${TREX_INSTANCE_ID:-}',
    'dut_instance_id': '${DUT_INSTANCE_ID:-}',
    'commit': '${GITHUB_SHA:-unknown}',
}
with open('$LOGS_DIR/failure-summary.json', 'w') as f:
    json.dump(data, f, indent=2)
"
}

# -- Environment & Diagnostics --

collect_environment_info() {
    local instance_id="$1"
    local label="$2"
    log_info "Collecting environment info from $label..."
    local output
    output=$(ssm_run_command "$instance_id" 30 \
        'echo "=== System Info ==="; echo "Kernel: $(uname -r)"; echo "CPUs: $(nproc)"; echo "Hugepages:"; cat /proc/meminfo | grep HugePages_Total; echo "=== PCI Devices ==="; lspci | grep -i eth 2>/dev/null || echo none; echo "=== DPDK Bind ==="; /usr/local/bin/dpdk-devbind.py --status 2>/dev/null || echo unavailable; echo "=== Interfaces ==="; ip addr show 2>/dev/null || echo unavailable' 2>/dev/null || echo "(failed)")
    mkdir -p "$LOGS_DIR"
    echo "$output" > "$LOGS_DIR/${label}-environment.txt"
}

collect_instance_logs() {
    local instance_id="$1"
    local label="$2"
    log_info "Collecting logs from $label..."
    mkdir -p "$LOGS_DIR"

    aws ec2 get-console-output \
        --instance-id "$instance_id" \
        --latest \
        --query "Output" \
        --output text > "$LOGS_DIR/${label}-console-output.log" 2>/dev/null || true

    local ssm_ready
    ssm_ready=$(aws ssm describe-instance-information \
        --filters "Key=InstanceIds,Values=${instance_id}" \
        --query "InstanceInformationList[0].InstanceId" \
        --output text 2>/dev/null || echo "")

    if [[ -z "$ssm_ready" || "$ssm_ready" == "None" ]]; then
        return 0
    fi

    ssm_run_command "$instance_id" 30 \
        'tail -100 /var/log/user-data.log 2>/dev/null; echo "---"; tail -50 /var/log/trex-server.log 2>/dev/null; echo "---"; tail -50 /var/log/tcp-echo.log 2>/dev/null; tail -50 /var/log/plain-tcp-echo.log 2>/dev/null; tail -50 /var/log/tokio-tcp-echo.log 2>/dev/null' \
        > "$LOGS_DIR/${label}-app-logs.txt" 2>/dev/null || true
}

collect_networking_diagnostics() {
    local instance_id="$1"
    local label="$2"
    local phase="$3"
    log_info "Collecting $phase networking diagnostics from $label..."
    local output
    output=$(ssm_run_command "$instance_id" 30 \
        'echo "=== Interfaces ==="; ip addr show 2>/dev/null; echo "=== ARP ==="; ip neigh show 2>/dev/null; echo "=== Routes ==="; ip route show 2>/dev/null; echo "=== DPDK ==="; /usr/local/bin/dpdk-devbind.py --status 2>/dev/null || echo unavailable; echo "=== Processes ==="; ps aux | grep -E "(echo|tcp-echo|t-rex)" | grep -v grep || echo none' 2>/dev/null || echo "(failed)")
    mkdir -p "$LOGS_DIR"
    echo "$output" > "$LOGS_DIR/${label}-networking-diag-${phase}.txt"
}

# -- DUT NIC Management --

dut_bind_dpdk() {
    log_info "Binding DUT secondary ENI to vfio-pci (DPDK mode)..."
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        'set +e; pkill -TERM -f "target/release/tcp-echo" 2>/dev/null; pkill -TERM -f "target/release/plain-tcp-echo" 2>/dev/null; pkill -TERM -f "target/release/tokio-tcp-echo" 2>/dev/null; for i in $(seq 1 10); do pgrep -f "target/release/.*tcp-echo" >/dev/null 2>&1 || break; sleep 1; done; pkill -9 -f "target/release/.*tcp-echo" 2>/dev/null; sleep 2; echo CLEANUP_DONE' || true

    local bind_cmd='set +e; CUR_DRV=$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); echo "PRE_STATE: driver=$CUR_DRV"; if [ "$CUR_DRV" = "vfio-pci" ]; then echo ALREADY_BOUND_TO_VFIO; echo BIND_OK; exit 0; fi; modprobe vfio-pci 2>/dev/null; echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null; IFACE=$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); if [ -n "$IFACE" ]; then ip link set $IFACE down 2>/dev/null; fi; echo 0000:00:06.0 > /sys/bus/pci/devices/0000:00:06.0/driver/unbind 2>/dev/null || true; sleep 2; echo vfio-pci > /sys/bus/pci/devices/0000:00:06.0/driver_override; echo 0000:00:06.0 > /sys/bus/pci/drivers/vfio-pci/bind || true; sleep 1; DRV=$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); echo "DRIVER: $DRV"; if [ "$DRV" = "vfio-pci" ]; then echo BIND_OK; exit 0; else echo BIND_FAILED; exit 1; fi'
    local bind_out
    bind_out=$(ssm_run_command "$DUT_INSTANCE_ID" 60 "$bind_cmd" 2>&1)
    local bind_exit=$?
    log_info "dut_bind_dpdk result (exit=$bind_exit): $bind_out"
    if [[ $bind_exit -ne 0 ]]; then
        log_error "Failed to bind DUT ENI to vfio-pci: $bind_out"
        return 1
    fi
}

dut_bind_kernel() {
    log_info "Binding DUT secondary ENI to kernel driver..."
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        'set +e; pkill -TERM -f "target/release/tcp-echo" 2>/dev/null; pkill -TERM -f "target/release/plain-tcp-echo" 2>/dev/null; pkill -TERM -f "target/release/tokio-tcp-echo" 2>/dev/null; for i in $(seq 1 10); do pgrep -f "target/release/.*tcp-echo" >/dev/null 2>&1 || break; sleep 1; done; pkill -9 -f "target/release/.*tcp-echo" 2>/dev/null; sleep 2; rm -rf /var/run/dpdk/ 2>/dev/null; echo CLEANUP_DONE' || true

    local dut_ip="$DUT_DATA_ENI_IP"
    local bind_cmd="set +e; CUR_DRV=\$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); echo PRE_STATE: driver=\$CUR_DRV; if [ \"\$CUR_DRV\" = \"ena\" ]; then IFACE=\$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); if [ -n \"\$IFACE\" ]; then ip link set \$IFACE up 2>/dev/null; ip addr add ${dut_ip}/24 dev \$IFACE 2>/dev/null; fi; echo BIND_OK; exit 0; fi; echo 0000:00:06.0 > /sys/bus/pci/devices/0000:00:06.0/driver/unbind 2>/dev/null || true; sleep 2; echo '' > /sys/bus/pci/devices/0000:00:06.0/driver_override; echo 0000:00:06.0 > /sys/bus/pci/drivers/ena/bind || true; sleep 3; IFACE=\$(ls /sys/bus/pci/devices/0000:00:06.0/net/ 2>/dev/null | head -1); if [ -n \"\$IFACE\" ]; then ip link set \$IFACE up 2>/dev/null; sleep 2; ip addr add ${dut_ip}/24 dev \$IFACE 2>/dev/null; fi; DRV=\$(readlink /sys/bus/pci/devices/0000:00:06.0/driver 2>/dev/null | xargs basename 2>/dev/null); if [ \"\$DRV\" = \"ena\" ]; then echo BIND_OK; exit 0; else echo BIND_FAILED; exit 1; fi"
    local bind_out
    bind_out=$(ssm_run_command "$DUT_INSTANCE_ID" 60 "$bind_cmd" 2>&1)
    local bind_exit=$?
    log_info "dut_bind_kernel result (exit=$bind_exit): $bind_out"
    if [[ $bind_exit -ne 0 ]]; then
        log_error "Failed to bind DUT ENI to kernel: $bind_out"
        return 1
    fi
}

dut_stop_all_apps() {
    log_info "Stopping all DUT TCP applications..."
    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        'set +e; pkill -TERM -f "target/release/tcp-echo" 2>/dev/null; pkill -TERM -f "target/release/plain-tcp-echo" 2>/dev/null; pkill -TERM -f "target/release/tokio-tcp-echo" 2>/dev/null; for i in $(seq 1 10); do pgrep -f "target/release/.*tcp-echo" >/dev/null 2>&1 || break; sleep 1; done; pkill -9 -f "target/release/.*tcp-echo" 2>/dev/null; sleep 2; echo "All TCP apps stopped"' || true
    ssm_run_command "$DUT_INSTANCE_ID" 15 \
        'rm -rf /var/run/dpdk/ 2>/dev/null; echo DPDK_STATE_CLEANED' || true
    sleep 5
}

wait_and_bind_eni() {
    local instance_id="$1"
    local label="$2"
    local driver="$3"

    log_info "Ensuring secondary ENI is attached and bound to $driver on $label..."
    local eni_cmd="for i in \$(seq 1 60); do TOKEN=\$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); MACS=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/); for mac in \$MACS; do DN=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/\${mac}device-number); if [ \"\$DN\" = \"1\" ]; then echo ENI_FOUND; if [ \"$driver\" = \"vfio-pci\" ]; then ip link set ens6 down 2>/dev/null; /usr/local/bin/dpdk-devbind.py --bind=vfio-pci 0000:00:06.0 2>/dev/null || true; else ip link set ens6 up 2>/dev/null || true; fi; echo DONE; exit 0; fi; done; sleep 2; done; echo ENI_TIMEOUT"
    local output
    output=$(ssm_run_command "$instance_id" 120 "$eni_cmd" 2>/dev/null || echo "SSM_FAILED")

    if [[ "$output" == *"ENI_TIMEOUT"* || "$output" == *"SSM_FAILED"* ]]; then
        log_error "$label secondary ENI not found"
        return 1
    fi
    log_info "$label ENI bound to $driver"
}

wait_for_trex_rx_eni() {
    local instance_id="$1"
    log_info "Waiting for TRex RX ENI (device-number 2)..."
    local rx_cmd="for i in \$(seq 1 60); do TOKEN=\$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); MACS=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/); for mac in \$MACS; do DN=\$(curl -s -H \"X-aws-ec2-metadata-token: \$TOKEN\" http://169.254.169.254/latest/meta-data/network/interfaces/macs/\${mac}device-number); if [ \"\$DN\" = \"2\" ]; then echo RX_ENI_FOUND; exit 0; fi; done; sleep 2; done; echo RX_ENI_TIMEOUT"
    local output
    output=$(ssm_run_command "$instance_id" 120 "$rx_cmd" || echo "SSM_FAILED")

    if [[ "$output" == *"RX_ENI_TIMEOUT"* || "$output" == *"SSM_FAILED"* ]]; then
        log_error "TRex RX ENI not found after 120s"
        return 1
    fi
    log_info "TRex RX ENI found"
}

# -- TRex Management --

generate_trex_config() {
    log_info "Generating TRex configuration..."
    local TX_PCI="0000:00:06.0"
    local RX_PCI="0000:00:07.0"
    local TX_BDF="00:06.0"
    local RX_BDF="00:07.0"

    # Step 1: Discover ENI MACs via IMDS
    log_info "Discovering TRex data ENI MACs..."
    local imds_cmd='TOKEN=$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); MACS=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/network/interfaces/macs/); for mac in $MACS; do mac=${mac%/}; dn=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}/device-number); if [ "$dn" = "1" ]; then echo "TX_MAC: ${mac}"; fi; if [ "$dn" = "2" ]; then echo "RX_MAC: ${mac}"; fi; done'
    local imds_result
    imds_result=$(ssm_run_command "$TREX_INSTANCE_ID" 30 "$imds_cmd" || echo "SSM_FAILED")

    TREX_DATA_MAC=$(echo "$imds_result" | grep "^TX_MAC:" | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' | head -1 || echo "")
    TREX_DATA_RX_MAC=$(echo "$imds_result" | grep "^RX_MAC:" | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' | head -1 || echo "")

    if [[ -z "$TREX_DATA_MAC" ]]; then
        log_error "Could not discover TRex TX ENI MAC"
        return 1
    fi
    if [[ -z "$TREX_DATA_RX_MAC" ]]; then
        log_error "Could not discover TRex RX ENI MAC"
        return 1
    fi
    log_info "TRex TX MAC: $TREX_DATA_MAC, RX MAC: $TREX_DATA_RX_MAC"

    # Step 2: Discover gateway MAC
    local subnet_gw
    subnet_gw=$(echo "$TREX_DATA_ENI_IP" | sed 's/\.[0-9]*$/.1/')
    log_info "Discovering gateway MAC (gw=$subnet_gw)..."

    local tx_iface
    tx_iface=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "ls /sys/bus/pci/devices/$TX_PCI/net/ 2>/dev/null | head -1 || echo ens6" || echo "ens6")
    tx_iface=$(echo "$tx_iface" | tr -d '[:space:]')
    [[ -z "$tx_iface" ]] && tx_iface="ens6"

    TREX_GATEWAY_MAC=""
    for gw_attempt in 1 2 3 4 5; do
        local gw_raw
        gw_raw=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
            "ip link set $tx_iface up 2>/dev/null; ping -c 2 -W 2 $subnet_gw 2>/dev/null; ip neigh show ${subnet_gw} dev $tx_iface 2>/dev/null" || echo "")
        TREX_GATEWAY_MAC=$(echo "$gw_raw" | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' | head -1)
        if [[ -n "$TREX_GATEWAY_MAC" ]]; then break; fi
        log_warn "Gateway MAC not found (attempt $gw_attempt), retrying..."
        ssm_run_command "$TREX_INSTANCE_ID" 30 \
            "dhclient $tx_iface 2>/dev/null; sleep 3; ping -c 3 -W 2 $subnet_gw 2>/dev/null" || true
        sleep 5
    done

    if [[ -z "$TREX_GATEWAY_MAC" ]]; then
        log_error "Could not discover gateway MAC"
        return 1
    fi
    log_info "Gateway MAC: $TREX_GATEWAY_MAC"

    # Step 3: Bind both data ENIs to vfio-pci
    log_info "Binding TRex data ENIs to vfio-pci..."
    local bind_result
    bind_result=$(ssm_run_command "$TREX_INSTANCE_ID" 60 \
        "modprobe vfio-pci 2>/dev/null; echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null; for PCI in $TX_PCI $RX_PCI; do IFACE=\$(ls /sys/bus/pci/devices/\$PCI/net/ 2>/dev/null | head -1); [ -n \"\$IFACE\" ] && ip link set \$IFACE down 2>/dev/null; echo \$PCI > /sys/bus/pci/devices/\$PCI/driver/unbind 2>/dev/null || true; sleep 1; echo vfio-pci > /sys/bus/pci/devices/\$PCI/driver_override; echo \$PCI > /sys/bus/pci/drivers/vfio-pci/bind && echo BIND_OK_\$PCI || echo BIND_FAIL_\$PCI; done" || echo "SSM_FAILED")

    if [[ "$bind_result" != *"BIND_OK_${TX_PCI}"* || "$bind_result" != *"BIND_OK_${RX_PCI}"* ]]; then
        log_error "Failed to bind TRex ENIs: $bind_result"
        return 1
    fi

    # Step 4: Write TRex config
    log_info "Writing TRex config..."
    local yaml_content="- port_limit: 2
  version: 2
  interfaces: ['${TX_BDF}', '${RX_BDF}']
  port_info:
    - dest_mac: '${TREX_GATEWAY_MAC}'
      src_mac:  '${TREX_DATA_MAC}'
    - dest_mac: '${TREX_GATEWAY_MAC}'
      src_mac:  '${TREX_DATA_RX_MAC}'
  memory:
    mbuf_9k: 4096"
    local yaml_b64
    yaml_b64=$(echo "$yaml_content" | base64 -w0)

    local write_result
    write_result=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "echo $yaml_b64 | base64 -d > /etc/trex_cfg.yaml && echo WROTE || echo WRITE_ERR" || echo "FAILED")
    if [[ "$write_result" != *"WROTE"* ]]; then
        log_error "TRex config write failed"
        return 1
    fi
}

start_trex_server() {
    log_info "Starting TRex server..."
    local TX_PCI="0000:00:06.0"
    local RX_PCI="0000:00:07.0"

    # Verify NICs
    local nic_state
    nic_state=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "for p in $TX_PCI $RX_PCI; do readlink /sys/bus/pci/devices/\$p/driver 2>/dev/null; done; ls /opt/trex/t-rex-64 2>/dev/null && echo TREX_BINARY_OK" || echo "SSM_FAILED")
    if [[ "$nic_state" != *"vfio-pci"* ]]; then
        log_error "TRex NICs not bound to vfio-pci"
        return 1
    fi

    # Hugepages
    ssm_run_command "$TREX_INSTANCE_ID" 30 \
        'echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null || true' || true

    # Start TRex
    ssm_run_command_fire_and_forget "$TREX_INSTANCE_ID" 120 \
        'pkill -f t-rex-64 2>/dev/null || true; sleep 1; cd /opt/trex && nohup /opt/trex/t-rex-64 -i --cfg /etc/trex_cfg.yaml -c 2 </dev/null >/var/log/trex-server.log 2>&1 & disown'

    log_info "Waiting 45s for TRex to initialize..."
    sleep 45

    local check
    check=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        'pgrep -f t-rex >/dev/null 2>&1 && echo PROCESS_FOUND; ss -tlnp 2>/dev/null | grep 4501 && echo API_PORT' || echo "FAILED")

    if [[ "$check" == *"PROCESS_FOUND"* || "$check" == *"API_PORT"* ]]; then
        log_info "TRex server running"
        return 0
    fi

    log_info "TRex not detected, waiting 30s more..."
    sleep 30
    check=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        'pgrep -f t-rex >/dev/null 2>&1 && echo PROCESS_FOUND; ss -tlnp 2>/dev/null | grep 4501 && echo API_PORT' || echo "FAILED")
    if [[ "$check" == *"PROCESS_FOUND"* || "$check" == *"API_PORT"* ]]; then
        log_info "TRex server running (after retry)"
        return 0
    fi

    log_error "TRex server failed to start"
    return 1
}

stop_trex_server() {
    log_info "Stopping TRex server..."
    ssm_run_command "$TREX_INSTANCE_ID" 30 \
        'pkill -9 -f t-rex-64 2>/dev/null || true; sleep 2; echo TRex stopped' 2>/dev/null || true
}

# -- DUT Config Runners --

start_dut_plain_rust_tcp() {
    log_info "Starting DUT: plain-rust-tcp (std::net TCP echo)"
    dut_bind_kernel || return 1

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/plain-tcp-echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/plain-tcp-echo.log 2>&1 &"
    sleep 10

    local status=""
    for attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            'pgrep -f "target/release/plain-tcp-echo" >/dev/null && echo running || echo stopped') || true
        [[ "$status" == *"running"* ]] && break
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "plain-rust-tcp failed to start"
        ssm_run_command "$DUT_INSTANCE_ID" 30 'tail -30 /var/log/plain-tcp-echo.log 2>/dev/null' || true
        return 1
    fi
    log_info "plain-rust-tcp running"
}

start_dut_rust_dpdk_tcp() {
    log_info "Starting DUT: rust-dpdk-tcp (DPDK TCP echo)"
    dut_bind_dpdk || return 1

    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        'echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null || true' || true

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/tcp-echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/tcp-echo.log 2>&1 &"
    sleep 15

    local status=""
    for attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            'pgrep -f "target/release/tcp-echo" >/dev/null && echo running || echo stopped') || true
        [[ "$status" == *"running"* ]] && break
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "rust-dpdk-tcp failed to start"
        ssm_run_command "$DUT_INSTANCE_ID" 30 'tail -30 /var/log/tcp-echo.log 2>/dev/null' || true
        return 1
    fi
    log_info "rust-dpdk-tcp running"
}

start_dut_tokio_dpdk_tcp() {
    log_info "Starting DUT: tokio-dpdk-tcp (async DPDK TCP echo)"
    dut_bind_dpdk || return 1

    ssm_run_command "$DUT_INSTANCE_ID" 30 \
        'echo 1024 > /proc/sys/vm/nr_hugepages 2>/dev/null; mkdir -p /mnt/huge; mount -t hugetlbfs nodev /mnt/huge 2>/dev/null || true' || true

    ssm_run_command_fire_and_forget "$DUT_INSTANCE_ID" 300 \
        "cd /opt/dpdk-stdlib && nohup ./target/release/tokio-tcp-echo --ip ${DUT_DATA_ENI_IP} --port 9000 > /var/log/tokio-tcp-echo.log 2>&1 &"
    sleep 15

    local status=""
    for attempt in 1 2 3; do
        status=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            'pgrep -f "target/release/tokio-tcp-echo" >/dev/null && echo running || echo stopped') || true
        [[ "$status" == *"running"* ]] && break
        sleep 5
    done
    if [[ "$status" != *"running"* ]]; then
        log_error "tokio-dpdk-tcp failed to start"
        ssm_run_command "$DUT_INSTANCE_ID" 30 'tail -30 /var/log/tokio-tcp-echo.log 2>/dev/null' || true
        return 1
    fi
    log_info "tokio-dpdk-tcp running"
}

# -- Benchmark Runner --

run_benchmark_for_config() {
    local config_name="$1"
    log_info "Running TRex TCP benchmark for config: $config_name"

    # Deploy benchmark script
    local benchmark_b64
    benchmark_b64=$(base64 -w0 "$SCRIPT_DIR/perf-tests/trex/run_tcp_benchmark.py")

    local deploy_out
    deploy_out=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "mkdir -p /opt/perf-tests; echo '$benchmark_b64' | base64 -d > /opt/perf-tests/run_tcp_benchmark.py; chmod +x /opt/perf-tests/run_tcp_benchmark.py; echo DEPLOY_OK") || {
        log_error "Failed to deploy TCP benchmark script"
        return 1
    }

    # Pre-flight ASTF check
    local preflight
    preflight=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        'cd /opt/trex && python3 -c "
import sys
sys.path.insert(0, \"/opt/trex/automation/trex_control_plane/interactive\")
from trex.astf.api import ASTFClient
c = ASTFClient(server=\"localhost\")
c.connect()
print(\"TRex ASTF API OK\")
c.disconnect()
print(\"PREFLIGHT_OK\")
" 2>&1' 2>&1) || true

    if [[ "$preflight" != *"PREFLIGHT_OK"* ]]; then
        log_error "TRex ASTF API preflight failed"
        return 1
    fi

    # Run benchmark
    log_info "Benchmark: src=${TREX_DATA_ENI_IP} dst=${DUT_DATA_ENI_IP} sizes=${PAYLOAD_SIZES} cps=${CPS_RATES} dur=${DURATION}s"
    local bench_cmd="cd /opt/trex && python3 /opt/perf-tests/run_tcp_benchmark.py --server localhost --config-name '${config_name}' --src-ip '${TREX_DATA_ENI_IP}' --dst-ip '${DUT_DATA_ENI_IP}' --dst-mac '${TREX_GATEWAY_MAC}' --dst-port 9000 --payload-sizes '${PAYLOAD_SIZES}' --cps-rates '${CPS_RATES}' --duration ${DURATION} --output '/tmp/perf-results/${config_name}.json' 2>&1; echo EXIT_CODE=\$?"

    local output
    output=$(ssm_run_command "$TREX_INSTANCE_ID" "$BENCHMARK_TIMEOUT" "$bench_cmd")
    local ssm_exit=$?

    mkdir -p "$LOGS_DIR"
    echo "$output" > "$LOGS_DIR/trex-tcp-benchmark-${config_name}.log"

    local output_tail
    output_tail=$(echo "$output" | tail -20)
    post_pr_comment "## [TCP Perf] Benchmark: \`$config_name\`
<details><summary>Output (last 20 lines)</summary>

\`\`\`
${output_tail}
\`\`\`
</details>"

    if [[ $ssm_exit -ne 0 ]]; then
        log_error "Benchmark SSM command failed for $config_name"
        return 1
    fi

    local py_exit
    py_exit=$(echo "$output" | sed -n 's/.*EXIT_CODE=\([0-9]*\).*/\1/p' | tail -1)
    if [[ -n "$py_exit" && "$py_exit" != "0" ]]; then
        log_error "TCP benchmark script failed for $config_name (exit=$py_exit)"
        return 1
    fi

    # Download results
    local results_json
    results_json=$(ssm_run_command "$TREX_INSTANCE_ID" 30 \
        "echo '---JSON_START---'; cat /tmp/perf-results/${config_name}.json 2>/dev/null || echo FILE_NOT_FOUND")

    if [[ "$results_json" == *"FILE_NOT_FOUND"* ]]; then
        log_error "Results file not found for $config_name"
        return 1
    fi

    local json_content
    json_content=$(echo "$results_json" | sed -n '/^---JSON_START---$/,$ p' | tail -n +2)
    mkdir -p "$RESULTS_DIR"
    echo "$json_content" > "$RESULTS_DIR/${config_name}.json"

    # Validate
    if ! python3 -c "import json; d=json.load(open('$RESULTS_DIR/${config_name}.json')); assert len(d.get('results',[])) > 0" 2>/dev/null; then
        log_error "Invalid/empty JSON for $config_name"
        return 1
    fi
    log_info "Results saved: $RESULTS_DIR/${config_name}.json"
}

# -- Results Aggregation --

aggregate_results() {
    log_info "Aggregating TCP performance results..."
    mkdir -p "$RESULTS_DIR"

    python3 - <<'PYEOF'
import json, glob, os, sys
from datetime import datetime, timezone

results_dir = os.environ.get("RESULTS_DIR", "perf-results-tcp")
output_file = os.path.join(results_dir, "tcp-perf-report.json")

configs = {}
for f in sorted(glob.glob(os.path.join(results_dir, "*.json"))):
    if os.path.basename(f) == "tcp-perf-report.json":
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
    "instance_type": os.environ.get("DUT_INSTANCE_TYPE", "unknown"),
    "protocol": "tcp",
    "configs": configs,
}

with open(output_file, "w") as f:
    json.dump(report, f, indent=2)
print(f"Aggregated TCP report: {output_file}")
PYEOF
}

generate_markdown_summary() {
    log_info "Generating TCP markdown summary..."

    python3 - <<'PYEOF'
import json, os, sys

results_dir = os.environ.get("RESULTS_DIR", "perf-results-tcp")
report_file = os.path.join(results_dir, "tcp-perf-report.json")
md_file = os.path.join(results_dir, "perf-summary-tcp.md")

if not os.path.exists(report_file):
    print("No TCP report file found", file=sys.stderr)
    sys.exit(0)

with open(report_file) as f:
    report = json.load(f)

lines = []
lines.append(f"## TCP Performance Test Results \u2014 {report.get('instance_type', 'unknown')}")
lines.append("")
lines.append(f"Commit: `{report.get('commit', 'unknown')[:8]}`")
lines.append(f"Timestamp: {report.get('timestamp', 'unknown')}")
lines.append("")

configs = report.get("configs", {})
if not configs:
    lines.append("*No results collected*")
else:
    config_order = ["plain-rust-tcp", "rust-dpdk-tcp", "tokio-dpdk-tcp"]

    for cfg_name in config_order:
        cfg_data = configs.get(cfg_name)
        if not cfg_data:
            continue

        lines.append(f"### {cfg_name}")
        lines.append("")
        lines.append("| Payload | Target CPS | Actual CPS | Throughput (Mbps) | P50 (\u00b5s) | P90 (\u00b5s) | P99 (\u00b5s) | Retransmits | Drops |")
        lines.append("|---------|-----------|-----------|------------------|---------|---------|---------|-------------|-------|")

        results = cfg_data.get("results", [])
        for r in results:
            payload = r.get("payload_size", "?")
            target_cps = f"{r.get('target_cps', 0):,}"
            actual_cps = f"{r.get('cps', 0):,.0f}"
            throughput = f"{r.get('throughput_mbps', 0):.1f}"
            p50 = r.get("lat_p50_us", -1)
            p90 = r.get("lat_p90_us", -1)
            p99 = r.get("lat_p99_us", -1)
            p50_s = f"{p50}" if p50 >= 0 else "N/A"
            p90_s = f"{p90}" if p90 >= 0 else "N/A"
            p99_s = f"{p99}" if p99 >= 0 else "N/A"
            retransmits = f"{r.get('tcp_retransmits', 0):,}"
            drops = f"{r.get('tcp_conndrops', 0) + r.get('tcp_drops', 0):,}"

            lines.append(f"| {payload}B | {target_cps} | {actual_cps} | {throughput} | {p50_s} | {p90_s} | {p99_s} | {retransmits} | {drops} |")

        lines.append("")

md_content = "\n".join(lines)
with open(md_file, "w") as f:
    f.write(md_content)

print(md_content)
PYEOF
}

# -- Cleanup --

cleanup() {
    local exit_code=$?
    if [[ $exit_code -ne 0 ]]; then
        log_warn "Script exiting with code $exit_code, collecting diagnostics..."
        if [[ -n "$DUT_INSTANCE_ID" ]]; then
            collect_instance_logs "$DUT_INSTANCE_ID" "dut" || true
            collect_networking_diagnostics "$DUT_INSTANCE_ID" "dut" "failure" || true
        fi
        if [[ -n "$TREX_INSTANCE_ID" ]]; then
            collect_instance_logs "$TREX_INSTANCE_ID" "trex" || true
            collect_networking_diagnostics "$TREX_INSTANCE_ID" "trex" "failure" || true
        fi
        write_failure_json "tcp-perf-test" "Script exited with code $exit_code"
    fi

    if [[ "$TEARDOWN" == "true" && "$SKIP_DEPLOY" == "false" ]]; then
        log_info "Tearing down PerfTestStack..."
        aws cloudformation delete-stack --stack-name "$CDK_STACK_NAME" 2>/dev/null || log_warn "Teardown failed"
    fi
}

trap cleanup EXIT

# -- Main --

main() {
    log_info "=== TCP Performance Test Suite ==="
    log_info "Configs: $CONFIGS"
    log_info "Payload sizes: $PAYLOAD_SIZES"
    log_info "CPS rates: $CPS_RATES"
    log_info "Duration per step: ${DURATION}s"

    mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

    # Phase 1: Deploy
    if [[ "$SKIP_DEPLOY" == "false" ]]; then
        log_info "Phase 1: Deploying $CDK_STACK_NAME..."
        post_pr_comment "## [TCP Perf] Stage: Deploy
Deploying \`$CDK_STACK_NAME\`...
Configs: \`$CONFIGS\` | Payload sizes: \`$PAYLOAD_SIZES\` | CPS: \`$CPS_RATES\`"

        cd "$CDK_DIR"

        local dpdk_ami trex_ami context_args=""
        dpdk_ami=$(aws ssm get-parameter --name /dpdk-stdlib-rust/ami/latest \
            --query "Parameter.Value" --output text 2>/dev/null || echo "")
        trex_ami=$(aws ssm get-parameter --name /dpdk-stdlib-rust/ami/trex-latest \
            --query "Parameter.Value" --output text 2>/dev/null || echo "")

        [[ -n "${DPDK_AMI_ID:-}" ]] && dpdk_ami="$DPDK_AMI_ID"
        [[ -n "${TREX_AMI_ID:-}" ]] && trex_ami="$TREX_AMI_ID"
        [[ -n "$dpdk_ami" ]] && context_args="$context_args -c ${DPDK_AMI_CDK_CONTEXT_KEY:-dpdkAmiId}=$dpdk_ami"
        [[ -n "$trex_ami" ]] && context_args="$context_args -c trexAmiId=$trex_ami"

        # Clean up leftover stack
        local stack_status
        stack_status=$(aws cloudformation describe-stacks \
            --stack-name "$CDK_STACK_NAME" \
            --query "Stacks[0].StackStatus" \
            --output text 2>/dev/null || echo "GONE")

        if [[ "$stack_status" != "GONE" && "$stack_status" != "DELETE_COMPLETE" && "$stack_status" != "DELETE_IN_PROGRESS" ]]; then
            log_info "Cleaning up leftover stack (status: $stack_status)..."
            aws cloudformation delete-stack --stack-name "$CDK_STACK_NAME" 2>&1 || true
        fi

        local stack_wait=0
        while [[ $stack_wait -lt 600 ]]; do
            stack_status=$(aws cloudformation describe-stacks \
                --stack-name "$CDK_STACK_NAME" \
                --query "Stacks[0].StackStatus" \
                --output text 2>/dev/null || echo "GONE")
            [[ "$stack_status" == "GONE" || "$stack_status" == "DELETE_COMPLETE" ]] && break
            log_info "Waiting for stack deletion ($stack_status, ${stack_wait}s)..."
            sleep 15
            stack_wait=$((stack_wait + 15))
        done

        if [[ "$stack_status" != "GONE" && "$stack_status" != "DELETE_COMPLETE" ]]; then
            log_error "Stack still not deleted after 600s"
            exit 2
        fi

        npx cdk deploy "$CDK_STACK_NAME" --require-approval never $context_args || {
            log_error "CDK deploy failed"
            post_pr_comment "## [TCP Perf] Deploy FAILED"
            exit 2
        }

        cd "$REPO_ROOT"
    fi

    # Phase 2: Stack outputs & SSM
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
    log_info "DUT:  $DUT_INSTANCE_ID (data: $DUT_DATA_ENI_IP)"

    wait_ssm_ready "$TREX_INSTANCE_ID" "TRex" &
    local trex_pid=$!
    wait_ssm_ready "$DUT_INSTANCE_ID" "DUT" &
    local dut_pid=$!
    wait "$trex_pid" || { log_error "TRex SSM not ready"; exit 2; }
    wait "$dut_pid"  || { log_error "DUT SSM not ready"; exit 2; }

    export DUT_INSTANCE_TYPE
    DUT_INSTANCE_TYPE=$(ssm_run_command "$DUT_INSTANCE_ID" 15 \
        'TOKEN=$(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600); curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/instance-type' 2>/dev/null || echo "unknown")

    post_pr_comment "## [TCP Perf] Instances Ready
- TRex: \`$TREX_INSTANCE_ID\` ($TREX_DATA_ENI_IP)
- DUT: \`$DUT_INSTANCE_ID\` ($DUT_DATA_ENI_IP)
- Type: \`$DUT_INSTANCE_TYPE\`"

    # Phase 2b: ENI attachment
    log_info "Phase 2b: Ensuring ENIs are attached..."
    wait_and_bind_eni "$TREX_INSTANCE_ID" "TRex" "ena" || { log_error "TRex ENI failed"; exit 2; }
    wait_for_trex_rx_eni "$TREX_INSTANCE_ID" || { log_error "TRex RX ENI failed"; exit 2; }
    wait_and_bind_eni "$DUT_INSTANCE_ID" "DUT" "ena" || { log_error "DUT ENI failed"; exit 2; }

    # Phase 3: Environment info
    log_info "Phase 3: Collecting baseline info..."
    collect_environment_info "$TREX_INSTANCE_ID" "trex"
    collect_environment_info "$DUT_INSTANCE_ID" "dut"

    # Phase 4: TRex
    log_info "Phase 4: Configuring TRex..."
    if ! generate_trex_config; then
        post_pr_comment "## [TCP Perf] TRex Config FAILED"
        exit 2
    fi

    if ! start_trex_server; then
        post_pr_comment "## [TCP Perf] TRex Start FAILED"
        exit 2
    fi
    post_pr_comment "## [TCP Perf] TRex Started — beginning benchmarks"

    # Phase 5: Benchmarks
    log_info "Phase 5: Running TCP benchmarks..."

    # Wait for DUT build
    local dut_ready=false
    for attempt in $(seq 1 12); do
        local dut_check
        dut_check=$(ssm_run_command "$DUT_INSTANCE_ID" 30 \
            'ls /opt/dpdk-stdlib/target/release/tcp-echo 2>/dev/null && echo BUILD_DONE || echo BUILD_PENDING' 2>&1) || true
        if [[ "$dut_check" == *"BUILD_DONE"* ]]; then
            dut_ready=true
            break
        fi
        sleep 10
    done
    if [[ "$dut_ready" != "true" ]]; then
        post_pr_comment "## [TCP Perf] DUT build not ready after 120s"
        exit 2
    fi

    IFS=',' read -ra CONFIG_LIST <<< "$CONFIGS"
    local total=${#CONFIG_LIST[@]}
    local idx=0
    local failed_configs=()

    for config in "${CONFIG_LIST[@]}"; do
        idx=$((idx + 1))
        log_info "=== Config $idx/$total: $config ==="

        post_pr_comment "## [TCP Perf] Benchmark $idx/$total: \`$config\`
Sizes: \`$PAYLOAD_SIZES\` | Duration: ${DURATION}s | CPS: \`$CPS_RATES\`"

        [[ $idx -eq 1 ]] && dut_stop_all_apps
        if [[ $idx -gt 1 ]]; then
            sleep 30
            local ssm_ok=false
            for retry in 1 2 3 4 5; do
                local check
                check=$(ssm_run_command "$DUT_INSTANCE_ID" 30 'echo SSM_OK' 2>/dev/null) || true
                [[ "$check" == *"SSM_OK"* ]] && { ssm_ok=true; break; }
                sleep 15
            done
            if [[ "$ssm_ok" == "false" ]]; then
                log_error "DUT SSM not responding"
                failed_configs+=("$config")
                break
            fi
        fi

        local start_ok=true
        case "$config" in
            plain-rust-tcp)   start_dut_plain_rust_tcp   || start_ok=false ;;
            rust-dpdk-tcp)    start_dut_rust_dpdk_tcp    || start_ok=false ;;
            tokio-dpdk-tcp)   start_dut_tokio_dpdk_tcp   || start_ok=false ;;
            *) log_error "Unknown config: $config"; failed_configs+=("$config"); continue ;;
        esac

        if [[ "$start_ok" == "false" ]]; then
            log_error "Failed to start DUT: $config"
            failed_configs+=("$config")
            collect_networking_diagnostics "$DUT_INSTANCE_ID" "dut" "failure-${config}"
            continue
        fi

        if ! run_benchmark_for_config "$config"; then
            log_error "Benchmark failed: $config"
            failed_configs+=("$config")
            collect_networking_diagnostics "$DUT_INSTANCE_ID" "dut" "failure-${config}"
            collect_networking_diagnostics "$TREX_INSTANCE_ID" "trex" "failure-${config}"
        fi

        dut_stop_all_apps
    done

    stop_trex_server || true

    # Phase 6: Results
    set +e
    log_info "Phase 6: Aggregating results..."
    aggregate_results
    local summary
    summary=$(generate_markdown_summary)

    if [[ ${#failed_configs[@]} -gt 0 ]]; then
        summary="$summary

### Failed Configs
$(printf -- '- \`%s\`\n' "${failed_configs[@]}")"
    fi

    post_pr_comment "## [TCP Perf] Results

$summary"

    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "$summary" >> "$GITHUB_STEP_SUMMARY"
    fi

    collect_instance_logs "$DUT_INSTANCE_ID" "dut"
    collect_instance_logs "$TREX_INSTANCE_ID" "trex"

    set -e

    if [[ ${#failed_configs[@]} -gt 0 ]]; then
        log_error "${#failed_configs[@]} config(s) failed: ${failed_configs[*]}"
        exit 1
    fi

    log_info "=== All TCP performance tests completed successfully ==="
}

main "$@"
