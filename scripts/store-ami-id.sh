#!/usr/bin/env bash
# store-ami-id.sh - Store or retrieve DPDK AMI ID from SSM Parameter Store
#
# Usage:
#   ./scripts/store-ami-id.sh put <ami-id>
#   ./scripts/store-ami-id.sh get
#   ./scripts/store-ami-id.sh check

set -euo pipefail

SSM_PARAMETER="/dpdk-stdlib-rust/ami/latest"
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
