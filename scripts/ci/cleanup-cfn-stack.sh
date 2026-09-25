#!/usr/bin/env bash
# cleanup-cfn-stack.sh <stack-name>
# Removes a stuck CloudFormation stack before a fresh deploy.
# Safe to call when no stack exists. Exits non-zero if delete fails.
set -euo pipefail

STACK="${1:?Usage: cleanup-cfn-stack.sh <stack-name>}"

# Treat only "does not exist" as absent -- all other errors (expired creds,
# IAM deny, throttle) are real failures and must not silently become NONE.
if ! OUT=$(aws cloudformation describe-stacks \
             --stack-name "$STACK" \
             --query 'Stacks[0].StackStatus' \
             --output text 2>&1); then
  if grep -q 'does not exist' <<< "$OUT"; then
    echo "No existing stack $STACK -- nothing to clean up"
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
  print_failed_events
  echo "::error::Could not delete stuck stack $STACK"
  exit 1
fi
echo "$STACK successfully removed"
