#!/usr/bin/env bash
# ci-validate.sh - Push, trigger CI, and poll for results
#
# Designed for use by Claude Code sessions to validate changes before
# creating a PR. Runs local checks first, then triggers GitHub Actions
# and waits for the result.
#
# Usage:
#   ./scripts/ci-validate.sh [--skip-local] [--skip-integration]
#
# Exit codes:
#   0 = all checks passed
#   1 = local checks failed (cargo build/test)
#   2 = integration tests failed
#   3 = CI trigger/poll failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FLAG_SKIP_LOCAL=false
FLAG_SKIP_INTEGRATION=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-local)        FLAG_SKIP_LOCAL=true;        shift ;;
        --skip-integration)  FLAG_SKIP_INTEGRATION=true;  shift ;;
        -h|--help)
            echo "Usage: $0 [--skip-local] [--skip-integration]"
            echo ""
            echo "  --skip-local         Skip cargo build/test (only run CI)"
            echo "  --skip-integration   Skip integration tests (only run local checks)"
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 3
            ;;
    esac
done

log() {
    echo "[$(date -u '+%H:%M:%S')] $*"
}

# ── Step 1: Local checks ───────────────────────────────────────────────────

if [[ "$FLAG_SKIP_LOCAL" != "true" ]]; then
    log "=== Local validation ==="

    log "Running cargo build..."
    if ! cargo build --manifest-path "$REPO_ROOT/Cargo.toml" 2>&1; then
        log "FAIL: cargo build failed"
        exit 1
    fi

    log "Running cargo test..."
    if ! cargo test --manifest-path "$REPO_ROOT/Cargo.toml" 2>&1; then
        log "FAIL: cargo test failed"
        exit 1
    fi

    log "Local checks passed"
fi

# ── Step 2: Push current branch ────────────────────────────────────────────

BRANCH=$(git -C "$REPO_ROOT" branch --show-current)

if [[ -z "$BRANCH" ]]; then
    log "FAIL: not on a branch (detached HEAD?)"
    exit 3
fi

log "Pushing branch: $BRANCH"
git -C "$REPO_ROOT" push -u origin "$BRANCH" 2>&1

# ── Step 3: Trigger integration tests ──────────────────────────────────────

if [[ "$FLAG_SKIP_INTEGRATION" != "true" ]]; then
    log "=== Integration tests ==="

    if ! command -v gh &>/dev/null; then
        log "WARN: gh CLI not available, skipping integration tests"
        log "Install gh: https://cli.github.com/"
        exit 0
    fi

    log "Triggering integration-tests workflow on $BRANCH..."
    if ! gh workflow run integration-tests.yml --ref "$BRANCH" 2>&1; then
        log "FAIL: could not trigger workflow (check gh auth status and repo permissions)"
        exit 3
    fi

    # Wait for the run to register
    log "Waiting for run to appear..."
    sleep 15

    RUN_ID=""
    for attempt in 1 2 3 4 5; do
        RUN_ID=$(gh run list \
            --workflow=integration-tests.yml \
            --branch="$BRANCH" \
            --limit=1 \
            --json databaseId,status \
            -q '.[0].databaseId' 2>/dev/null || true)

        if [[ -n "$RUN_ID" ]]; then
            break
        fi
        log "Run not yet visible (attempt $attempt/5), waiting..."
        sleep 10
    done

    if [[ -z "$RUN_ID" ]]; then
        log "FAIL: could not find triggered workflow run"
        exit 3
    fi

    log "Watching run $RUN_ID (this may take 20-30 minutes)..."
    if gh run watch "$RUN_ID" --exit-status 2>&1; then
        log "Integration tests PASSED"
    else
        log "Integration tests FAILED — downloading failure data..."

        # Download the instance-logs artifact which contains:
        #   failure-summary.json      - structured: failed_step, error, instance IDs
        #   sender-user-data.log      - full EC2 user-data script output (richest source)
        #   sender-console-output.log - EC2 console (always available, even post-teardown)
        #   sender-journal.txt        - systemd journal (500 lines)
        ARTIFACT_DIR="/tmp/ci-logs-${RUN_ID}"
        if gh run download "$RUN_ID" --name instance-logs --dir "$ARTIFACT_DIR" 2>/dev/null; then
            if [[ -f "$ARTIFACT_DIR/failure-summary.json" ]]; then
                echo ""
                echo "=== failure-summary.json ==="
                cat "$ARTIFACT_DIR/failure-summary.json"
                echo ""
            fi

            # Show the most informative log available
            for logfile in \
                "$ARTIFACT_DIR/sender-user-data.log" \
                "$ARTIFACT_DIR/sender-console-output.log"; do
                if [[ -f "$logfile" && -s "$logfile" ]]; then
                    echo "=== $(basename "$logfile") — last 80 lines ==="
                    tail -80 "$logfile"
                    echo ""
                    break
                fi
            done

            log "Full logs: $ARTIFACT_DIR/"
        else
            # Artifact not yet available (upload may still be in progress)
            log "instance-logs artifact not available; fetch manually once upload completes:"
            log "  gh run download $RUN_ID --name instance-logs --dir /tmp/ci-logs-${RUN_ID}"
        fi

        log "Full workflow logs: gh run view $RUN_ID --log-failed"
        exit 2
    fi
else
    log "Skipping integration tests (--skip-integration)"
fi

log "=== All checks passed ==="
