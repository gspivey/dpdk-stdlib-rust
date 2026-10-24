#!/usr/bin/env bash
# run-quic-perf.sh — Real-NIC QUIC throughput perf over DPDK (2-instance).
#
# Deploys DpdkTestStack (sender + receiver), binds both DPDK ENIs to vfio-pci,
# starts `quic-echo-server --throughput` on the receiver, transfers its self-
# signed cert to the sender, runs `quic-perf-client` on the sender, scrapes the
# PERF_RESULT line, writes perf-results/quic-native-dpdk-nic.json, and posts a
# markdown table to the PR.
#
# Unlike the UDP/TCP TRex path this is app-to-app (TRex cannot speak QUIC), so
# it uses DpdkTestStack (sender/receiver) rather than PerfTestStack (TRex/DUT).
# It is normally invoked via the `quic-native-dpdk-nic` token of
# run-perf-tests.sh, but can also be run directly.
#
# Usage:  ./scripts/run-quic-perf.sh [--teardown] [--skip-deploy]
# Exit:   0 = perf completed, 1 = perf failure, 2 = infrastructure failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CDK_DIR="$REPO_ROOT/deploy/cdk"

# ── Configuration ────────────────────────────────────────────────────────────

SSM_READINESS_TIMEOUT=600
ENI_BIND_TIMEOUT=90
SSM_POLL_INTERVAL=15
CDK_STACK_NAME="${CDK_STACK_NAME:-DpdkTestStack}"
RESULTS_DIR="$REPO_ROOT/perf-results"
LOGS_DIR="$REPO_ROOT/instance-logs"

# Workload (overridable via env)
PERF_DURATION="${QUIC_PERF_DURATION:-30}"
PERF_STREAMS="${QUIC_PERF_STREAMS:-8}"
PERF_PAYLOAD="${QUIC_PERF_PAYLOAD:-65536}"
PERF_PORT="${QUIC_PERF_PORT:-4433}"

# ── CLI parsing ──────────────────────────────────────────────────────────────

FLAG_TEARDOWN=false
FLAG_SKIP_DEPLOY=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --teardown)    FLAG_TEARDOWN=true;    shift ;;
        --skip-deploy) FLAG_SKIP_DEPLOY=true; shift ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done

# ── Logging ──────────────────────────────────────────────────────────────────

log_info()  { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] INFO: $*"; }
log_error() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ERROR: $*" >&2; }

# ── State ────────────────────────────────────────────────────────────────────

SENDER_INSTANCE_ID=""
RECEIVER_INSTANCE_ID=""
SENDER_DPDK_ENI_IP=""
RECEIVER_DPDK_ENI_IP=""
GATEWAY_MAC=""

# ── PR comment helper ────────────────────────────────────────────────────────

post_pr_comment() {
    local body="$1"
    [[ -n "${PR_NUMBER:-}" && -n "${GITHUB_REPOSITORY:-}" ]] || return 0
    echo "$body" | gh pr comment "$PR_NUMBER" --body-file - --repo "$GITHUB_REPOSITORY" 2>/dev/null || true
}

# ── CDK deploy (2-instance) ──────────────────────────────────────────────────

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

    SENDER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='SenderInstanceId']" 2>/dev/null || echo "")
    RECEIVER_INSTANCE_ID=$(echo "$cdk_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='ReceiverInstanceId']" 2>/dev/null || echo "")
    SENDER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='SenderDpdkEniPrivateIp']" 2>/dev/null || echo "")
    RECEIVER_DPDK_ENI_IP=$(echo "$cdk_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='ReceiverDpdkEniPrivateIp']" 2>/dev/null || echo "")

    if [[ -z "$SENDER_INSTANCE_ID" || -z "$RECEIVER_INSTANCE_ID" ]]; then
        log_error "Could not extract instance IDs from CDK outputs"
        exit 2
    fi
    if [[ -z "$SENDER_DPDK_ENI_IP" || -z "$RECEIVER_DPDK_ENI_IP" ]]; then
        log_error "Could not extract DPDK ENI IPs from CDK outputs"
        exit 2
    fi

    log_info "Sender:   $SENDER_INSTANCE_ID ($SENDER_DPDK_ENI_IP)"
    log_info "Receiver: $RECEIVER_INSTANCE_ID ($RECEIVER_DPDK_ENI_IP)"
    cd "$REPO_ROOT"
}

read_existing_outputs() {
    local cf_outputs
    cf_outputs=$(aws cloudformation describe-stacks \
        --stack-name "$CDK_STACK_NAME" \
        --query "Stacks[0].Outputs" --output json 2>/dev/null || echo "[]")
    SENDER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='SenderInstanceId']" 2>/dev/null || echo "")
    RECEIVER_INSTANCE_ID=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='ReceiverInstanceId']" 2>/dev/null || echo "")
    SENDER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='SenderDpdkEniPrivateIp']" 2>/dev/null || echo "")
    RECEIVER_DPDK_ENI_IP=$(echo "$cf_outputs" | python3 -c "import json,sys; [print(o['OutputValue']) for o in json.load(sys.stdin) if o['OutputKey']=='ReceiverDpdkEniPrivateIp']" 2>/dev/null || echo "")
}

# ── SSM helpers ──────────────────────────────────────────────────────────────

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
            log_error "$label ($instance_id) did not become SSM-ready within ${SSM_READINESS_TIMEOUT}s"
            exit 2
        fi
    done
}

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
    [[ -n "$cmd_id" ]] || return 1
    local waited=0
    while true; do
        local status
        status=$(aws ssm get-command-invocation \
            --command-id "$cmd_id" --instance-id "$instance_id" \
            --query "Status" --output text 2>/dev/null || echo "Pending")
        case "$status" in
            Success) return 0 ;;
            Failed|TimedOut|Cancelled) return 1 ;;
        esac
        sleep 5
        waited=$((waited + 5))
        [[ $waited -ge $((timeout + 30)) ]] && return 1
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
            --command-id "$cmd_id" --instance-id "$instance_id" \
            --query "Status" --output text 2>/dev/null || echo "Pending")
        case "$status" in
            Success)
                aws ssm get-command-invocation \
                    --command-id "$cmd_id" --instance-id "$instance_id" \
                    --query "StandardOutputContent" --output text 2>/dev/null
                return 0
                ;;
            Failed|TimedOut|Cancelled) return 1 ;;
        esac
        sleep 5
        waited=$((waited + 5))
        [[ $waited -ge $((timeout + 30)) ]] && return 1
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

# ── Gateway MAC discovery ────────────────────────────────────────────────────
# In an AWS VPC the router presents the same MAC across the VPC's subnets, so
# the default-route gateway MAC (always present in the primary ENI's kernel ARP
# cache) is the correct L2 next-hop for the DPDK data subnet too. Reading it
# from the primary ENI works regardless of whether the data ENI is bound to
# vfio-pci.

discover_gateway_mac() {
    log_info "Discovering VPC gateway MAC from sender..."
    local attempt raw
    for attempt in 1 2 3 4 5; do
        raw=$(ssm_run_get_output "$SENDER_INSTANCE_ID" \
            "set +e; GW=\$(ip route show default 2>/dev/null | awk '{print \$3; exit}'); ping -c 2 -W 2 \$GW >/dev/null 2>&1; ip neigh show \$GW 2>/dev/null" 30) || true
        GATEWAY_MAC=$(echo "$raw" | grep -oE '([0-9a-f]{2}:){5}[0-9a-f]{2}' | head -1)
        [[ -n "$GATEWAY_MAC" ]] && break
        log_info "Gateway MAC not found (attempt $attempt) — retrying..."
        sleep 5
    done
    if [[ -z "$GATEWAY_MAC" ]]; then
        log_error "Could not discover gateway MAC — QUIC frames would be dropped by VPC"
        return 1
    fi
    log_info "Gateway MAC: $GATEWAY_MAC"
}

# ── Run real-NIC QUIC perf ───────────────────────────────────────────────────

run_quic_perf() {
    mkdir -p "$RESULTS_DIR" "$LOGS_DIR"
    local config_name="quic-native-dpdk-nic"
    local out_json="$RESULTS_DIR/${config_name}.json"

    # Step 1: discover gateway MAC (kernel mode), then bind both data ENIs.
    discover_gateway_mac || return 1

    log_info "Binding DPDK ENIs to vfio-pci on both instances..."
    if ! configure_eni "$RECEIVER_INSTANCE_ID" "bind" "$RECEIVER_DPDK_ENI_IP"; then
        log_error "ENI bind failed on receiver"; return 1
    fi
    if ! configure_eni "$SENDER_INSTANCE_ID" "bind" "$SENDER_DPDK_ENI_IP"; then
        log_error "ENI bind failed on sender"; return 1
    fi

    # Step 2: start quic-echo-server --throughput on the receiver (background).
    log_info "Starting quic-echo-server --throughput on receiver ($RECEIVER_DPDK_ENI_IP)..."
    local bin="/opt/dpdk-stdlib/target/release"
    local server_cmd="cd /opt/dpdk-stdlib && nohup $bin/quic-echo-server --ip $RECEIVER_DPDK_ENI_IP --port $PERF_PORT --gateway-mac $GATEWAY_MAC --throughput > /tmp/quic-perf-server-stdout.log 2> /tmp/quic-perf-server.log & echo STARTED"
    aws ssm send-command \
        --instance-ids "$RECEIVER_INSTANCE_ID" \
        --document-name "AWS-RunShellScript" \
        --parameters "{\"commands\":[\"$server_cmd\"],\"executionTimeout\":[\"60\"]}" \
        --timeout-seconds 60 \
        --query "Command.CommandId" --output text >/dev/null 2>&1 || {
        log_error "Failed to launch quic-echo-server"; return 1
    }

    # Step 3: wait for the server to be ready (QUIC_SERVER_READY on stderr).
    log_info "Waiting for QUIC_SERVER_READY..."
    local ready=false attempt server_err
    for attempt in 1 2 3 4 5 6; do
        sleep 5
        server_err=$(ssm_run_get_output "$RECEIVER_INSTANCE_ID" \
            "grep -q QUIC_SERVER_READY /tmp/quic-perf-server.log 2>/dev/null && echo READY || echo WAIT" 20) || true
        if [[ "$server_err" == *"READY"* ]]; then ready=true; break; fi
    done
    if [[ "$ready" != "true" ]]; then
        log_error "quic-echo-server did not report QUIC_SERVER_READY"
        ssm_run_get_output "$RECEIVER_INSTANCE_ID" "tail -30 /tmp/quic-perf-server.log 2>/dev/null" 20 > "$LOGS_DIR/quic-perf-server.log" 2>/dev/null || true
        return 1
    fi

    # Step 4: extract the server cert PEM (stdout) and copy it to the sender.
    log_info "Transferring server cert to sender..."
    local cert_content
    cert_content=$(ssm_run_get_output "$RECEIVER_INSTANCE_ID" \
        "sed -n '/-----BEGIN CERTIFICATE-----/,/-----END CERTIFICATE-----/p' /tmp/quic-perf-server-stdout.log 2>/dev/null" 30) || true
    if [[ -z "$cert_content" ]]; then
        sleep 5
        cert_content=$(ssm_run_get_output "$RECEIVER_INSTANCE_ID" \
            "sed -n '/-----BEGIN CERTIFICATE-----/,/-----END CERTIFICATE-----/p' /tmp/quic-perf-server-stdout.log 2>/dev/null" 30) || true
    fi
    if [[ -z "$cert_content" ]]; then
        log_error "Could not retrieve server certificate"
        return 1
    fi
    local escaped_cert
    escaped_cert=$(echo "$cert_content" | sed 's/"/\\"/g' | sed ':a;N;$!ba;s/\n/\\n/g')
    ssm_run "$SENDER_INSTANCE_ID" "printf '$escaped_cert' > /tmp/quic-server-cert.pem" 30 || {
        log_error "Failed to write cert on sender"; return 1
    }

    # Step 5: run the perf client on the sender; capture the PERF_RESULT line.
    log_info "Running quic-perf-client on sender ($SENDER_DPDK_ENI_IP)..."
    local client_cmd="cd /opt/dpdk-stdlib && $bin/quic-perf-client --server-ip $RECEIVER_DPDK_ENI_IP --port $PERF_PORT --bind-ip $SENDER_DPDK_ENI_IP --gateway-mac $GATEWAY_MAC --cert-pem /tmp/quic-server-cert.pem --duration $PERF_DURATION --streams $PERF_STREAMS --payload-size $PERF_PAYLOAD 2>&1"
    local client_out
    client_out=$(ssm_run_get_output "$SENDER_INSTANCE_ID" "$client_cmd" $((PERF_DURATION + 120))) || {
        log_error "quic-perf-client execution failed"
        echo "$client_out" > "$LOGS_DIR/quic-perf-client.log" 2>/dev/null || true
        return 1
    }
    echo "$client_out" > "$LOGS_DIR/quic-perf-client.log"
    log_info "Client output tail: $(echo "$client_out" | tail -5 | tr '\n' ' ')"

    # Stop the server.
    ssm_run "$RECEIVER_INSTANCE_ID" "pkill -f quic-echo-server || true" 15 || true

    # Step 6: parse the PERF_RESULT line into JSON.
    local perf_line
    perf_line=$(echo "$client_out" | grep -E '^PERF_RESULT ' | tail -1)
    if [[ -z "$perf_line" ]]; then
        log_error "No PERF_RESULT line in quic-perf-client output"
        return 1
    fi
    log_info "PERF_RESULT: $perf_line"

    PERF_LINE="$perf_line" CONFIG_NAME="$config_name" DURATION="$PERF_DURATION" \
    STREAMS="$PERF_STREAMS" PAYLOAD="$PERF_PAYLOAD" python3 - "$out_json" <<'PYEOF'
import json, os, sys
out_path = sys.argv[1]
line = os.environ["PERF_LINE"]
fields = {}
for tok in line.split()[1:]:  # skip the "PERF_RESULT" prefix
    if "=" in tok:
        k, v = tok.split("=", 1)
        try:
            fields[k] = float(v) if ("." in v or "e" in v.lower()) else int(v)
        except ValueError:
            fields[k] = v
doc = {
    "config_name": os.environ["CONFIG_NAME"],
    "protocol": "quic",
    "mode": "real-nic",
    "duration_sec": int(os.environ["DURATION"]),
    "streams": int(os.environ["STREAMS"]),
    "payload_size": int(os.environ["PAYLOAD"]),
    "metrics": fields,
}
os.makedirs(os.path.dirname(out_path), exist_ok=True)
with open(out_path, "w") as f:
    json.dump(doc, f, indent=2)
print("Wrote", out_path)
PYEOF

    # Step 7: post a markdown table to the PR.
    local gbps rx_drops tx_drops hs_us
    gbps=$(echo "$perf_line"  | grep -oE 'gbps=[0-9.]+'     | cut -d= -f2 || echo "?")
    hs_us=$(echo "$perf_line" | grep -oE 'hs_us=[0-9]+'     | cut -d= -f2 || echo "?")
    rx_drops=$(echo "$perf_line" | grep -oE 'rx_drops=[0-9]+' | cut -d= -f2 || echo "?")
    tx_drops=$(echo "$perf_line" | grep -oE 'tx_drops=[0-9]+' | cut -d= -f2 || echo "?")
    post_pr_comment "## [Perf] QUIC real-NIC (DPDK, 2-instance)

Workload: duration=${PERF_DURATION}s, streams=${PERF_STREAMS}, payload=${PERF_PAYLOAD}B

| Config | Throughput (Gbps) | Handshake (us) | RX drops | TX drops |
|--------|-------------------|----------------|----------|----------|
| ${config_name} | ${gbps} | ${hs_us} | ${rx_drops} | ${tx_drops} |

<details><summary>Raw PERF_RESULT</summary>

\`\`\`
${perf_line}
\`\`\`
</details>"

    log_info "QUIC real-NIC perf results saved to $out_json"
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

    if [[ "$FLAG_SKIP_DEPLOY" != "true" ]]; then
        deploy_stack
    else
        log_info "Skipping deploy, reading existing stack outputs..."
        read_existing_outputs
    fi

    wait_for_ssm "$SENDER_INSTANCE_ID" "sender"
    wait_for_ssm "$RECEIVER_INSTANCE_ID" "receiver"

    local rc=0
    run_quic_perf || rc=$?

    teardown

    if [[ $rc -ne 0 ]]; then
        log_error "QUIC real-NIC perf failed (rc=$rc)"
        exit 1
    fi
    log_info "QUIC real-NIC perf complete"
}

trap teardown EXIT
main
