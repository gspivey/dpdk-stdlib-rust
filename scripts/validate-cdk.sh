#!/usr/bin/env bash
# validate-cdk.sh - Synthesize CDK template and validate invariants without deploying.
#
# PURPOSE
# -------
# Catches infrastructure bugs in seconds rather than waiting 10-35 minutes for a
# CloudFormation deployment to time out. Currently checks:
#
#   1. cfn-signal resource names match CloudFormation logical IDs
#      (the original cause of integration test failures - see DEBUGGING.md)
#
# KNOWN BRITTLENESS
# -----------------
# This script depends on CDK's `cdk synth` producing a predictable template
# structure. If CDK changes how it serializes UserData or generates logical IDs,
# the Python checker (check-cfn-signals.py) may need updating. The checker
# documents its own assumptions and limitations.
#
# If this script produces false positives (blocks CI when the stack is actually
# correct), add `--skip-cfn-signal-check` to skip that specific check while
# investigating.
#
# No AWS credentials are required. CDK synth is a local TypeScript compilation
# step that resolves CDK constructs to CloudFormation JSON without calling AWS.
#
# Usage:
#   ./scripts/validate-cdk.sh [--skip-cfn-signal-check]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CDK_DIR="$SCRIPT_DIR/../deploy/cdk"
SKIP_CFN_SIGNAL_CHECK=false

for arg in "$@"; do
    case "$arg" in
        --skip-cfn-signal-check) SKIP_CFN_SIGNAL_CHECK=true ;;
        *) echo "Unknown argument: $arg" >&2; exit 1 ;;
    esac
done

echo "=== CDK pre-flight validation ==="
echo "CDK directory: $CDK_DIR"
echo ""

# ── Step 1: Synthesize the template ─────────────────────────────────────────
# CDK_DEFAULT_ACCOUNT/REGION don't need to be real - synthesis only uses them
# to resolve Stack.account / Stack.region (used in cfn-signal command strings).
# We pass amiId context so CDK synthesizes the pre-built-AMI path, which is the
# production code path used in integration tests.

echo "Step 1: Running cdk synth (no AWS credentials needed)..."
cd "$CDK_DIR"

CDK_DEFAULT_ACCOUNT="${CDK_DEFAULT_ACCOUNT:-123456789012}" \
CDK_DEFAULT_REGION="${CDK_DEFAULT_REGION:-us-east-1}" \
    npx cdk synth \
    --quiet \
    --context "amiId=ami-dummy-for-validation" \
    2>&1

TEMPLATE="$CDK_DIR/cdk.out/DpdkTestStack.template.json"
if [[ ! -f "$TEMPLATE" ]]; then
    echo "ERROR: Synthesized template not found at $TEMPLATE" >&2
    echo "  'cdk synth' did not produce expected output." >&2
    exit 1
fi

echo "  Template synthesized: $TEMPLATE"
echo ""

# ── Step 2: Validate cfn-signal resource names ───────────────────────────────
if [[ "$SKIP_CFN_SIGNAL_CHECK" == "true" ]]; then
    echo "Step 2: Skipping cfn-signal check (--skip-cfn-signal-check passed)"
else
    echo "Step 2: Checking cfn-signal resource name invariants..."
    python3 "$SCRIPT_DIR/check-cfn-signals.py" "$TEMPLATE"
fi

echo ""
echo "=== CDK validation passed ==="
