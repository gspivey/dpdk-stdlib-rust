#!/usr/bin/env bash
# diagnose-ci-failure.sh — Analyze a CI failure using only gh CLI
#
# Designed for Claude Code web environment where only gh CLI is available
# (no AWS CLI, no SSM access). Reads PR comments and artifacts to diagnose.
#
# Usage:
#   ./scripts/diagnose-ci-failure.sh [run-id]     # Specific run
#   ./scripts/diagnose-ci-failure.sh               # Most recent failed run
#   ./scripts/diagnose-ci-failure.sh --pr <number> # Read PR comments
#
# Requires: gh CLI authenticated (gh auth status)

set -euo pipefail

REPO="gspivey/dpdk-stdlib-rust"
WORKFLOW="integration-tests.yml"
RUN_ID=""
PR_NUMBER=""
ARTIFACT_DIR=""

# ── Parse arguments ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --pr)
            PR_NUMBER="$2"
            shift 2
            ;;
        --repo)
            REPO="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: $0 [run-id] [--pr <number>] [--repo <owner/repo>]"
            echo ""
            echo "Diagnoses CI failures using gh CLI. No AWS access needed."
            echo ""
            echo "Options:"
            echo "  run-id          GitHub Actions run ID (default: most recent failed)"
            echo "  --pr <number>   Read staged CI comments from a specific PR"
            echo "  --repo <repo>   Repository (default: $REPO)"
            exit 0
            ;;
        *)
            RUN_ID="$1"
            shift
            ;;
    esac
done

# Verify gh is authenticated
if ! gh auth status >/dev/null 2>&1; then
    echo "ERROR: gh CLI not authenticated. Run: gh auth login" >&2
    exit 1
fi

echo "# CI Failure Diagnosis"
echo ""

# ── Mode 1: Read PR comments ───────────────────────────────────────────────
if [[ -n "$PR_NUMBER" ]]; then
    echo "## PR #${PR_NUMBER} CI Comments"
    echo ""

    # Get comments that look like CI staged output
    gh api "repos/${REPO}/issues/${PR_NUMBER}/comments" \
        --jq '.[] | select(.body | test("\\[CI\\]|Integration Test|Failure|Diagnostics|Deploy")) | "### Comment by \(.user.login) at \(.created_at)\n\(.body)\n---"' \
        2>/dev/null || echo "No CI-related comments found on PR #${PR_NUMBER}"

    echo ""
fi

# ── Mode 2: Analyze a specific run (or most recent failure) ─────────────────
if [[ -z "$RUN_ID" ]]; then
    echo "## Finding Most Recent Failed Run"
    echo ""
    RUN_ID=$(gh run list --repo "$REPO" --workflow="$WORKFLOW" --status=failure \
        --json databaseId --jq '.[0].databaseId' 2>/dev/null || echo "")

    if [[ -z "$RUN_ID" ]]; then
        echo "No failed runs found. Checking recent runs..."
        echo ""
        gh run list --repo "$REPO" --workflow="$WORKFLOW" --limit 5 \
            --json databaseId,status,conclusion,headBranch,createdAt \
            --jq '.[] | "  \(.databaseId)  \(.status)/\(.conclusion)  \(.headBranch)  \(.createdAt)"' \
            2>/dev/null || echo "  Could not list runs"
        echo ""
        echo "No failed runs to diagnose. Use: $0 <run-id> to analyze a specific run."
        exit 0
    fi
fi

echo "## Run: $RUN_ID"
echo ""

# Get run metadata
echo "### Run Details"
gh run view "$RUN_ID" --repo "$REPO" 2>/dev/null || echo "Could not fetch run details"
echo ""

# Get failed steps
echo "### Failed Steps"
gh api "repos/${REPO}/actions/runs/${RUN_ID}/jobs" \
    --jq '.jobs[] | {name, conclusion, failed_steps: [.steps[] | select(.conclusion != "success" and .conclusion != "skipped") | {name, conclusion}]}' \
    2>/dev/null || echo "Could not fetch job details"
echo ""

# Try to get failed logs
echo "### Failed Step Logs (last 100 lines)"
gh run view "$RUN_ID" --log-failed --repo "$REPO" 2>/dev/null | tail -100 || echo "Could not fetch failed logs"
echo ""

# ── Download artifacts ──────────────────────────────────────────────────────
ARTIFACT_DIR=$(mktemp -d)
trap 'rm -rf "$ARTIFACT_DIR"' EXIT

echo "### Instance Logs"
if gh run download "$RUN_ID" --name instance-logs --dir "$ARTIFACT_DIR/logs" --repo "$REPO" 2>/dev/null; then
    # Parse failure-summary.json
    if [[ -f "$ARTIFACT_DIR/logs/failure-summary.json" ]]; then
        echo ""
        echo "#### failure-summary.json"
        echo '```json'
        cat "$ARTIFACT_DIR/logs/failure-summary.json"
        echo '```'
    fi

    # Show networking diagnostics if present
    for diag in "$ARTIFACT_DIR/logs/"*-networking-diag*.txt; do
        [[ -f "$diag" ]] || continue
        echo ""
        echo "#### $(basename "$diag")"
        echo '```'
        cat "$diag"
        echo '```'
    done

    # Show last N lines of key logs
    for log in "$ARTIFACT_DIR/logs/"*-user-data.log; do
        [[ -f "$log" ]] || continue
        echo ""
        echo "#### $(basename "$log") (last 50 lines)"
        echo '```'
        tail -50 "$log"
        echo '```'
    done
else
    echo "Could not download instance-logs artifact (may not exist or access denied)"
fi

echo ""
echo "### Test Results"
if gh run download "$RUN_ID" --name integration-test-results --dir "$ARTIFACT_DIR/results" --repo "$REPO" 2>/dev/null; then
    for xml in "$ARTIFACT_DIR/results/"*.xml; do
        [[ -f "$xml" ]] || continue
        echo ""
        echo "#### $(basename "$xml")"
        echo '```xml'
        cat "$xml"
        echo '```'
    done
else
    echo "Could not download test-results artifact"
fi

# ── Cross-reference with known failure patterns ─────────────────────────────
echo ""
echo "## Diagnosis"
echo ""

# Check for common patterns
DIAGNOSIS=""

if [[ -f "$ARTIFACT_DIR/logs/failure-summary.json" ]]; then
    FAILED_STEP=$(python3 -c "import json; d=json.load(open('$ARTIFACT_DIR/logs/failure-summary.json')); print(d.get('failed_step',''))" 2>/dev/null || echo "")
    ERROR_MSG=$(python3 -c "import json; d=json.load(open('$ARTIFACT_DIR/logs/failure-summary.json')); print(d.get('error',''))" 2>/dev/null || echo "")

    if [[ -n "$FAILED_STEP" ]]; then
        DIAGNOSIS="**Failed step**: $FAILED_STEP"
        [[ -n "$ERROR_MSG" ]] && DIAGNOSIS="$DIAGNOSIS\n**Error**: $ERROR_MSG"
    fi
fi

# Check for ARP/MAC issues in logs
if grep -r -q "broadcast MAC\|ARP resolution\|ff:ff:ff:ff:ff:ff\|resolve_arp" "$ARTIFACT_DIR/" 2>/dev/null; then
    DIAGNOSIS="$DIAGNOSIS\n\n**Likely cause**: ARP/MAC resolution failure in AWS VPC."
    DIAGNOSIS="$DIAGNOSIS\nSee \`docs/aws-vpc-networking.md\` — DPDK must use the gateway MAC, not peer MAC."
    DIAGNOSIS="$DIAGNOSIS\nFix: Pass \`--gateway-mac\` to test-client and echo server."
fi

# Check for DPDK init failures
if grep -r -q "EAL: Error\|DPDK initialization failed\|Cannot init EAL" "$ARTIFACT_DIR/" 2>/dev/null; then
    DIAGNOSIS="$DIAGNOSIS\n\n**Likely cause**: DPDK EAL initialization failure."
    DIAGNOSIS="$DIAGNOSIS\nCheck: hugepages, vfio-pci driver, /var/run/dpdk/ cleanup."
fi

# Check for build failures
if grep -r -q "error\[E\|cannot find\|unresolved import" "$ARTIFACT_DIR/" 2>/dev/null; then
    DIAGNOSIS="$DIAGNOSIS\n\n**Likely cause**: Rust compilation error."
    DIAGNOSIS="$DIAGNOSIS\nRun \`cargo build\` locally to reproduce."
fi

if [[ -n "$DIAGNOSIS" ]]; then
    echo -e "$DIAGNOSIS"
else
    echo "No known failure pattern matched. Read the logs above manually."
    echo ""
    echo "Useful next steps:"
    echo "- Check the failed step logs for the actual error message"
    echo "- Read docs/aws-vpc-networking.md for networking-related failures"
    echo "- Read docs/debugging-log.md for previously encountered issues"
fi

echo ""
echo "---"
echo "Run \`gh run view $RUN_ID --repo $REPO\` for full details."
