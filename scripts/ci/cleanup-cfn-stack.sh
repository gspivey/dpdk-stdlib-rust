#!/usr/bin/env bash
# cleanup-cfn-stack.sh <stack-name>
# Removes a stuck CloudFormation stack before a fresh deploy.
# Safe to call when no stack exists. Exits non-zero if delete fails.
#
# After any FORCE_DELETE_STACK, CFN abandons resources it cannot delete
# (e.g. VPCs with attached DPDK ENIs). This script detects and purges those
# orphaned VPCs (tagged aws:cloudformation:stack-name=<stack>) so they do not
# accumulate and hit the 5-VPC-per-region account limit.
set -euo pipefail

STACK="${1:?Usage: cleanup-cfn-stack.sh <stack-name>}"

# ── Orphaned VPC purge ────────────────────────────────────────────────────────
# FORCE_DELETE_STACK removes the CFN stack record but abandons any resources
# that couldn't be deleted (VPCs with attached DPDK ENIs, EIPs, subnets, IGWs).
# Those orphans accumulate and hit the 5-VPC-per-region limit, blocking future
# deploys. This function finds VPCs tagged with the stack name and tears them
# down completely. Safe to call when no orphans exist (no-ops).
purge_orphaned_vpcs() {
  local vpc_ids
  vpc_ids=$(aws ec2 describe-vpcs \
    --filters "Name=tag:aws:cloudformation:stack-name,Values=$STACK" \
    --query 'Vpcs[*].VpcId' \
    --output text 2>/dev/null || true)

  if [[ -z "$vpc_ids" || "$vpc_ids" == "None" ]]; then
    echo "No orphaned VPCs found for stack $STACK"
    return 0
  fi

  echo "::warning::Found orphaned VPCs from stack $STACK: $vpc_ids"

  for VPC_ID in $vpc_ids; do
    echo "Purging orphaned VPC $VPC_ID ..."

    # 1. Terminate EC2 instances
    local instance_ids
    instance_ids=$(aws ec2 describe-instances \
      --filters "Name=vpc-id,Values=$VPC_ID" \
                "Name=instance-state-name,Values=pending,running,stopping,stopped" \
      --query 'Reservations[*].Instances[*].InstanceId' \
      --output text 2>/dev/null || true)
    if [[ -n "$instance_ids" && "$instance_ids" != "None" ]]; then
      echo "  Terminating instances: $instance_ids"
      aws ec2 terminate-instances --instance-ids $instance_ids > /dev/null
      echo "  Waiting for instances to terminate..."
      timeout 10m aws ec2 wait instance-terminated --instance-ids $instance_ids
    fi

    # 2. Delete network interfaces (e.g. orphaned DPDK ENIs)
    # Loop until none remain -- terminating instances releases primary ENIs
    # asynchronously, so retry up to ~30s.
    local attempts=0
    while [[ $attempts -lt 6 ]]; do
      local eni_ids
      eni_ids=$(aws ec2 describe-network-interfaces \
        --filters "Name=vpc-id,Values=$VPC_ID" \
        --query 'NetworkInterfaces[*].NetworkInterfaceId' \
        --output text 2>/dev/null || true)
      [[ -z "$eni_ids" || "$eni_ids" == "None" ]] && break
      for ENI_ID in $eni_ids; do
        echo "  Deleting ENI $ENI_ID"
        local attachment_id
        attachment_id=$(aws ec2 describe-network-interfaces \
          --network-interface-ids "$ENI_ID" \
          --query 'NetworkInterfaces[0].Attachment.AttachmentId' \
          --output text 2>/dev/null || true)
        if [[ -n "$attachment_id" && "$attachment_id" != "None" ]]; then
          aws ec2 detach-network-interface --attachment-id "$attachment_id" --force 2>/dev/null || true
          sleep 3
        fi
        aws ec2 delete-network-interface --network-interface-id "$ENI_ID" 2>/dev/null || true
      done
      attempts=$((attempts + 1))
      sleep 5
    done

    # 3. Release EIPs tagged with this stack
    local tagged_eip_alloc_ids
    tagged_eip_alloc_ids=$(aws ec2 describe-addresses \
      --filters "Name=tag:aws:cloudformation:stack-name,Values=$STACK" \
      --query 'Addresses[*].AllocationId' \
      --output text 2>/dev/null || true)
    for ALLOC_ID in $tagged_eip_alloc_ids; do
      echo "  Releasing EIP $ALLOC_ID"
      aws ec2 release-address --allocation-id "$ALLOC_ID" 2>/dev/null || true
    done

    # 4. Delete subnets
    local subnet_ids
    subnet_ids=$(aws ec2 describe-subnets \
      --filters "Name=vpc-id,Values=$VPC_ID" \
      --query 'Subnets[*].SubnetId' \
      --output text 2>/dev/null || true)
    for SUBNET_ID in $subnet_ids; do
      echo "  Deleting subnet $SUBNET_ID"
      aws ec2 delete-subnet --subnet-id "$SUBNET_ID" 2>/dev/null || true
    done

    # 5. Detach and delete internet gateways
    local igw_ids
    igw_ids=$(aws ec2 describe-internet-gateways \
      --filters "Name=attachment.vpc-id,Values=$VPC_ID" \
      --query 'InternetGateways[*].InternetGatewayId' \
      --output text 2>/dev/null || true)
    for IGW_ID in $igw_ids; do
      echo "  Detaching and deleting IGW $IGW_ID"
      aws ec2 detach-internet-gateway --internet-gateway-id "$IGW_ID" --vpc-id "$VPC_ID" 2>/dev/null || true
      aws ec2 delete-internet-gateway --internet-gateway-id "$IGW_ID" 2>/dev/null || true
    done

    # 6. Delete non-main route tables
    local rt_ids
    rt_ids=$(aws ec2 describe-route-tables \
      --filters "Name=vpc-id,Values=$VPC_ID" \
      --query 'RouteTables[?Associations[?Main==`false`] || length(Associations)==`0`].RouteTableId' \
      --output text 2>/dev/null || true)
    for RT_ID in $rt_ids; do
      echo "  Deleting route table $RT_ID"
      aws ec2 delete-route-table --route-table-id "$RT_ID" 2>/dev/null || true
    done

    # 7. Delete non-default security groups
    local sg_ids
    sg_ids=$(aws ec2 describe-security-groups \
      --filters "Name=vpc-id,Values=$VPC_ID" \
      --query 'SecurityGroups[?GroupName!=`default`].GroupId' \
      --output text 2>/dev/null || true)
    for SG_ID in $sg_ids; do
      echo "  Deleting security group $SG_ID"
      aws ec2 delete-security-group --group-id "$SG_ID" 2>/dev/null || true
    done

    # 8. Finally delete the VPC itself
    echo "  Deleting VPC $VPC_ID"
    if ! aws ec2 delete-vpc --vpc-id "$VPC_ID" 2>/dev/null; then
      echo "::warning::Could not delete VPC $VPC_ID -- may still have attached resources. Manual cleanup needed."
    else
      echo "  VPC $VPC_ID deleted"
    fi
  done
}

# ── Stack status check ────────────────────────────────────────────────────────

# Treat only "does not exist" as absent -- all other errors (expired creds,
# IAM deny, throttle) are real failures and must not silently become NONE.
if ! OUT=$(aws cloudformation describe-stacks \
             --stack-name "$STACK" \
             --query 'Stacks[0].StackStatus' \
             --output text 2>&1); then
  if grep -q 'does not exist' <<< "$OUT"; then
    echo "No existing stack $STACK -- checking for orphaned VPCs"
    purge_orphaned_vpcs
    exit 0
  fi
  echo "::error::describe-stacks failed: $OUT"
  exit 1
fi
STATUS="$OUT"
echo "Existing $STACK status: $STATUS"

# Refuse to touch a stack mid-operation (concurrent run guard).
case "$STATUS" in
  DELETE_IN_PROGRESS)
    # Already being deleted -- fall through to wait.
    ;;
  REVIEW_IN_PROGRESS)
    # CDK created a change set but the deploy crashed before any resources were
    # created. The stack has no resources; a plain delete succeeds immediately.
    echo "$STACK is REVIEW_IN_PROGRESS (empty change-set stack) -- deleting directly"
    aws cloudformation delete-stack --stack-name "$STACK"
    sleep 5
    if ! timeout 5m aws cloudformation wait stack-delete-complete --stack-name "$STACK"; then
      echo "::error::Could not delete REVIEW_IN_PROGRESS stack $STACK"
      exit 1
    fi
    echo "$STACK successfully removed"
    purge_orphaned_vpcs
    exit 0
    ;;
  *_IN_PROGRESS)
    echo "::error::$STACK is $STATUS -- concurrent run? Refusing to delete."
    exit 1
    ;;
esac

print_failed_events() {
  aws cloudformation describe-stack-events \
    --stack-name "$STACK" --max-items 50 \
    --query "StackEvents[?ResourceStatus=='DELETE_FAILED'].[Timestamp,LogicalResourceId,ResourceStatusReason]" \
    --output table || true
}

# DELETE_FAILED stacks usually fail again for the same reason (attached ENI, etc).
# Use force-delete; print orphaned resources first so there is a record.
DEL_ARGS=()
if [ "$STATUS" = "DELETE_FAILED" ]; then
  echo "::warning::$STACK is DELETE_FAILED -- force-deleting (orphaned resources listed below)"
  print_failed_events
  DEL_ARGS=(--deletion-mode FORCE_DELETE_STACK)
fi

if [ "$STATUS" != "DELETE_IN_PROGRESS" ]; then
  aws cloudformation delete-stack --stack-name "$STACK" "${DEL_ARGS[@]}"
  sleep 5   # allow IN_PROGRESS to register before the waiter polls
fi

# aws cloudformation wait default is 60 min, exceeding the 20-min step budget.
# Wrap in timeout so the diagnostic branch runs before GitHub kills the step.
if ! timeout 17m aws cloudformation wait stack-delete-complete --stack-name "$STACK"; then
  # Check if we landed in DELETE_FAILED after a plain delete (stuck ENI, etc).
  # Retry once with force-delete so teardown clears debris in the same run
  # rather than leaving the stack stuck for the next run's pre-deploy step.
  NOW=$(aws cloudformation describe-stacks --stack-name "$STACK" \
          --query 'Stacks[0].StackStatus' --output text 2>/dev/null || echo GONE)
  if [ "$NOW" = DELETE_FAILED ] && [ ${#DEL_ARGS[@]} -eq 0 ]; then
    echo "::warning::$STACK entered DELETE_FAILED -- retrying with force-delete"
    print_failed_events
    aws cloudformation delete-stack --stack-name "$STACK" --deletion-mode FORCE_DELETE_STACK
    if ! timeout 10m aws cloudformation wait stack-delete-complete --stack-name "$STACK"; then
      print_failed_events
      echo "::error::Force-delete of $STACK failed -- orphaned resources need manual cleanup"
      exit 1
    fi
  else
    print_failed_events
    echo "::error::Could not delete stuck stack $STACK (status: $NOW)"
    exit 1
  fi
fi
echo "$STACK successfully removed"

# Always run orphaned VPC purge after any successful stack deletion.
# FORCE_DELETE_STACK abandons resources it cannot delete; purge_orphaned_vpcs
# finds them by the aws:cloudformation:stack-name tag and tears them down so
# they do not accumulate and hit the 5-VPC-per-region account limit.
purge_orphaned_vpcs
