#!/usr/bin/env bash
# store-ami-id.sh - Store or retrieve DPDK AMI ID from SSM Parameter Store
#
# Usage:
#   ./scripts/store-ami-id.sh put <ami-id>
#   ./scripts/store-ami-id.sh get
#   ./scripts/store-ami-id.sh check
#
# Works on the x86_64 AMI of one DPDK LTS line: DPDK_LINE (default 25.11,
# the primary line). Parameter: /dpdk-stdlib-rust/ami/dpdk-<line>/latest.
# The build workflow (build-dpdk-ami.yml) normally writes it; `put` is for
# manual recovery only.

set -euo pipefail

DPDK_LINE="${DPDK_LINE:-25.11}"
if [[ ! "$DPDK_LINE" =~ ^[0-9]{2}\.[0-9]{2}$ ]]; then
    echo "DPDK_LINE must look like 25.11, got '$DPDK_LINE'" >&2
    exit 1
fi
SSM_PARAMETER="/dpdk-stdlib-rust/ami/dpdk-${DPDK_LINE}/latest"
AWS_REGION="${AWS_REGION:-us-east-1}"

case "${1:-}" in
    put)
        AMI_ID="${2:?Usage: $0 put <ami-id>}"
        aws ssm put-parameter \
            --name "$SSM_PARAMETER" \
            --type String \
            --value "$AMI_ID" \
            --overwrite \
            --region "$AWS_REGION"
        echo "Stored AMI ID: $AMI_ID"
        ;;
    get)
        AMI_ID=$(aws ssm get-parameter \
            --name "$SSM_PARAMETER" \
            --query "Parameter.Value" \
            --output text \
            --region "$AWS_REGION" 2>/dev/null || echo "")
        if [[ -z "$AMI_ID" ]]; then
            echo "No AMI ID found in SSM at $SSM_PARAMETER" >&2
            exit 1
        fi
        echo "$AMI_ID"
        ;;
    check)
        if aws ssm get-parameter --name "$SSM_PARAMETER" --region "$AWS_REGION" &>/dev/null; then
            AMI_ID=$(aws ssm get-parameter \
                --name "$SSM_PARAMETER" \
                --query "Parameter.Value" \
                --output text \
                --region "$AWS_REGION")
            echo "AMI_AVAILABLE=true"
            echo "AMI_ID=$AMI_ID"
        else
            echo "AMI_AVAILABLE=false"
            echo "AMI_ID="
        fi
        ;;
    *)
        echo "Usage: $0 <put|get|check> [ami-id]" >&2
        exit 1
        ;;
esac
