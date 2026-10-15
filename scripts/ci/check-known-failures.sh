#!/usr/bin/env bash
# check-known-failures.sh — Known-failures gate (tcp-ci-reliability Requirement 1)
#
# Enforces the strict xfail rules for TCP integration tests so that a PR fixing
# exactly one of the known TCP bugs can reach green while the other known bugs
# still fail. See ci/known-failures.txt for the entry format and the full rule
# table.
#
# Two independent checks:
#
#   1. Ratchet (Req 1.4): a PR MUST NOT add lines to ci/known-failures.txt
#      relative to the base branch. Run with `--ratchet` in CI before the test
#      run. Needs `git` and a fetched base ref (BASE_REF env or --base <ref>).
#
#   2. xfail evaluation (Req 1.1, 1.2, 1.3): after the test run, every listed
#      test must have FAILED (expected). A listed test that PASSED is an
#      "unexpected pass"; a listed test that did not run at all is a "known
#      failure did not run". Either makes the gate exit non-zero.
#
# Usage:
#   check-known-failures.sh [--known-file FILE] [--results-dir DIR]
#                           [--ratchet] [--base REF] [-h|--help]
#
#   --known-file FILE   Path to known-failures list (default: ci/known-failures.txt)
#   --results-dir DIR   Directory of JUnit *.xml results (default: test-results)
#   --ratchet           Run ONLY the ratchet check (no xfail evaluation)
#   --base REF          Base git ref for the ratchet diff (default: $BASE_REF
#                       or origin/development)
#
# Exit codes:
#   0  gate satisfied
#   1  gate violated (unexpected pass / did-not-run / ratchet added lines)
#   2  usage / environment error (missing file, missing git, etc.)
#
# The script is intentionally self-contained and side-effect free so it can be
# invoked from CI (the tcp-integration-tests job / run-integration-tests.sh) and
# unit-tested locally with synthetic fixtures (see test-known-failures.sh).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

KNOWN_FILE="$REPO_ROOT/ci/known-failures.txt"
RESULTS_DIR="$REPO_ROOT/test-results"
MODE="xfail"            # xfail | ratchet
BASE_REF="${BASE_REF:-origin/development}"

usage() { sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --known-file)  KNOWN_FILE="$2"; shift 2 ;;
        --results-dir) RESULTS_DIR="$2"; shift 2 ;;
        --ratchet)     MODE="ratchet"; shift ;;
        --base)        BASE_REF="$2"; shift 2 ;;
        -h|--help)     usage; exit 0 ;;
        *) echo "ERROR: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

# ── Parse the known-failures list ────────────────────────────────────────────
# Strips inline `# ...` comments and surrounding whitespace. Emits one test name
# per line on stdout. Blank/comment-only lines are skipped.
parse_known_list() {
    local file="$1"
    [[ -f "$file" ]] || { echo "ERROR: known-failures file not found: $file" >&2; exit 2; }
    # Remove inline comment, trim whitespace, drop empties.
    sed -e 's/#.*$//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$file" \
        | grep -v '^$' || true
}

# ── Ratchet check (Req 1.4) ──────────────────────────────────────────────────
run_ratchet() {
    command -v git >/dev/null 2>&1 || { echo "ERROR: git not available for ratchet check" >&2; exit 2; }

    # Operate on the git repo that actually contains the known-failures file, so
    # the check works both in CI (file inside this repo) and under test (file in
    # a throwaway repo). Fall back to the script's REPO_ROOT.
    local kf_dir git_root
    kf_dir="$(cd "$(dirname "$KNOWN_FILE")" && pwd)"
    git_root="$(git -C "$kf_dir" rev-parse --show-toplevel 2>/dev/null || echo "$REPO_ROOT")"

    local rel
    rel="$(python3 -c 'import os,sys; print(os.path.relpath(os.path.abspath(sys.argv[1]), sys.argv[2]))' \
        "$KNOWN_FILE" "$git_root")"

    if ! git -C "$git_root" rev-parse --verify --quiet "$BASE_REF" >/dev/null; then
        echo "ERROR: base ref not found for ratchet: $BASE_REF (fetch it first)" >&2
        exit 2
    fi

    # Count ADDED content lines (diff '+' not counting the '+++' header). An added
    # line that is a comment or blank does not introduce a new xfail entry, so we
    # only count lines that carry a real test name after comment/whitespace strip.
    local added
    added=$(git -C "$git_root" diff "$BASE_REF" -- "$rel" \
        | grep '^+[^+]' \
        | sed -e 's/^+//' -e 's/#.*$//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
        | grep -c -v '^$' || true)

    if [[ "${added:-0}" -gt 0 ]]; then
        echo "::error::PRs may not add lines to $rel (ratchet: $added new xfail entr$([[ $added -eq 1 ]] && echo y || echo ies))" >&2
        git -C "$git_root" diff "$BASE_REF" -- "$rel" | grep '^+[^+]' >&2 || true
        return 1
    fi
    echo "known-failures ratchet: ok (no new xfail entries vs $BASE_REF)"
    return 0
}

# ── xfail evaluation (Req 1.1/1.2/1.3) ───────────────────────────────────────
# For each listed test, determine its status from the JUnit XMLs:
#   PASS       -> unexpected pass (Req 1.2) -> fail
#   FAIL       -> expected (Req 1.1)        -> ok
#   NOT-RUN    -> did not run (Req 1.3)     -> fail
#
# A listed name may be bare (`bidir_multi`, matches any suite's testcase of that
# name) or suite-qualified (`suite/testcase`). Status resolution: a name is PASS
# only if it ran AND no matching occurrence failed; FAIL if any matching
# occurrence failed; NOT-RUN if no occurrence exists.
run_xfail() {
    local known_list
    known_list="$(parse_known_list "$KNOWN_FILE")"
    if [[ -z "$known_list" ]]; then
        echo "known-failures gate: list is empty — nothing to enforce"
        return 0
    fi

    KNOWN_NAMES="$known_list" RESULTS_DIR="$RESULTS_DIR" python3 - <<'PYEOF'
import os, re, sys, html

results_dir = os.environ["RESULTS_DIR"]
known = [l.strip() for l in os.environ["KNOWN_NAMES"].splitlines() if l.strip()]

# status[(suite, name)] = 'pass' | 'fail'
observed = {}
if os.path.isdir(results_dir):
    for fn in sorted(os.listdir(results_dir)):
        if not fn.endswith(".xml"):
            continue
        with open(os.path.join(results_dir, fn), "r", encoding="utf-8", errors="replace") as f:
            content = f.read()
        sm = re.search(r'<testsuite\s+([^>]*)>', content)
        suite = "unknown"
        if sm:
            m = re.search(r'name="([^"]*)"', sm.group(1))
            if m:
                suite = html.unescape(m.group(1))
        for tc in re.finditer(r'<testcase\s+([^>]*?)(/>|>(.*?)</testcase>)', content, re.DOTALL):
            attrs = tc.group(1)
            body = tc.group(3) or ""
            m = re.search(r'name="([^"]*)"', attrs)
            name = html.unescape(m.group(1)) if m else "unknown"
            failed = ("<failure" in body) or ("<error" in body)
            key = (suite, name)
            # If any occurrence fails, the test is considered FAIL.
            prev = observed.get(key)
            observed[key] = "fail" if (failed or prev == "fail") else "pass"

def resolve(entry):
    """Return 'pass' | 'fail' | 'notrun' for a known-failures entry."""
    if "/" in entry:
        suite, name = entry.split("/", 1)
        matches = [v for (s, n), v in observed.items() if s == suite and n == name]
    else:
        matches = [v for (s, n), v in observed.items() if n == entry]
    if not matches:
        return "notrun"
    return "fail" if "fail" in matches else "pass"

fail = 0
for entry in known:
    status = resolve(entry)
    if status == "fail":
        print(f"known-failures gate: '{entry}' failed as expected (xfail ok)")
    elif status == "pass":
        print(f"::error::unexpected pass: remove '{entry}' from ci/known-failures.txt")
        fail = 1
    else:  # notrun
        print(f"::error::known failure did not run: {entry}")
        fail = 1

sys.exit(1 if fail else 0)
PYEOF
}

case "$MODE" in
    ratchet) run_ratchet ;;
    xfail)   run_xfail ;;
esac
