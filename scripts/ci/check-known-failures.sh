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
#   --rewrite-xfails    Rewrite listed+failed testcases to <skipped> in the JUnit
#                       XMLs (decrementing suite failures) so reporters that read
#                       the XMLs (dorny/test-reporter, fail-on-error) stay green
#                       on known bugs. Unlisted failures are left intact. The
#                       orchestrator calls this only on a GREEN gate verdict.
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
        --rewrite-xfails) MODE="rewrite"; shift ;;
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
suites_seen = set()
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
        suites_seen.add(suite)
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
    """Return 'pass' | 'fail' | 'notrun' | 'inapplicable' for an entry.

    A suite-qualified entry (suite/name) whose SUITE did not run at all in this
    results set is 'inapplicable' — the suite isn't part of this CI mode (e.g.
    the tcp-full-only `tier1-tcp-handshake` suite under an all-tiers run). If the
    suite DID run but the testcase is absent, that is 'notrun' (Req 1.3 — a
    listed xfail must not be silently skipped within a suite that executed).
    Bare entries must appear in some suite or they are 'notrun'.
    """
    if "/" in entry:
        suite, name = entry.split("/", 1)
        if suite not in suites_seen:
            return "inapplicable"
        matches = [v for (s, n), v in observed.items() if s == suite and n == name]
    else:
        matches = [v for (s, n), v in observed.items() if n == entry]
    if not matches:
        return "notrun"
    return "fail" if "fail" in matches else "pass"

fail = 0
for entry in known:
    status = resolve(entry)
    if status == "inapplicable":
        print(f"known-failures gate: '{entry}' suite not in this run — not applicable (skipped)")
    elif status == "fail":
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

# ── Rewrite expected failures as skipped (so JUnit reporters stay green) ─────
# dorny/test-reporter (fail-on-error: true) reads test-results/*.xml directly
# and fails the CI job on any <failure> element — it does not know about the
# known-failures gate. To keep the job green when only expected bugs fail, this
# mode rewrites each LISTED testcase that FAILED into a <skipped> testcase and
# decrements its suite's `failures` count. UNLISTED failures are left intact so
# the reporter still fails the job for a genuine regression. Idempotent.
run_rewrite() {
    local known_list
    known_list="$(parse_known_list "$KNOWN_FILE")"
    [[ -z "$known_list" ]] && { echo "known-failures rewrite: list empty — nothing to rewrite"; return 0; }

    KNOWN_NAMES="$known_list" RESULTS_DIR="$RESULTS_DIR" python3 - <<'PYEOF'
import os, re, html, sys

results_dir = os.environ["RESULTS_DIR"]
known = [l.strip() for l in os.environ["KNOWN_NAMES"].splitlines() if l.strip()]

def listed(suite, name):
    for e in known:
        if "/" in e:
            s, n = e.split("/", 1)
            if s == suite and n == name:
                return True
        elif e == name:
            return True
    return False

if not os.path.isdir(results_dir):
    print(f"known-failures rewrite: no results dir {results_dir}")
    sys.exit(0)

rewritten_total = 0
for fn in sorted(os.listdir(results_dir)):
    if not fn.endswith(".xml"):
        continue
    path = os.path.join(results_dir, fn)
    with open(path, encoding="utf-8", errors="replace") as f:
        content = f.read()

    sm = re.search(r'<testsuite\s+([^>]*)>', content)
    if not sm:
        continue
    m = re.search(r'name="([^"]*)"', sm.group(1))
    suite = html.unescape(m.group(1)) if m else fn
    file_rewrites = 0

    def repl(tc):
        nonlocal_body = tc.group(0)
        attrs = tc.group(1)
        nm = re.search(r'name="([^"]*)"', attrs)
        name = html.unescape(nm.group(1)) if nm else "unknown"
        has_fail = ("<failure" in nonlocal_body) or ("<error" in nonlocal_body)
        if not has_fail or not listed(suite, name):
            return nonlocal_body
        # Replace the <failure.../> or <failure>...</failure> (and <error>) with
        # a <skipped> element marking it an expected (known) failure.
        new = re.sub(
            r'<(failure|error)\b[^>]*?(/>|>.*?</\1>)',
            '<skipped message="known-failure (tcp-ci-reliability xfail); '
            'tracked in ci/known-failures.txt"/>',
            nonlocal_body, flags=re.DOTALL)
        return new

    # Count how many we will rewrite (listed + failed) for the failures= fixup.
    to_rewrite = 0
    for tc in re.finditer(r'<testcase\s+([^>]*?)(?:/>|>.*?</testcase>)', content, re.DOTALL):
        body = tc.group(0)
        a = tc.group(1)
        nm = re.search(r'name="([^"]*)"', a)
        name = html.unescape(nm.group(1)) if nm else "unknown"
        if (("<failure" in body) or ("<error" in body)) and listed(suite, name):
            to_rewrite += 1

    if to_rewrite == 0:
        continue

    new_content = re.sub(r'<testcase\s+([^>]*?)(?:/>|>.*?</testcase>)', repl, content, flags=re.DOTALL)

    # Decrement the suite failures="N" by the number rewritten (floor 0).
    def fix_failures(mm):
        attrs = mm.group(1)
        def dec(a):
            n = int(a.group(1))
            return 'failures="%d"' % max(0, n - to_rewrite)
        attrs2 = re.sub(r'failures="(\d+)"', dec, attrs)
        return "<testsuite " + attrs2 + ">"
    new_content = re.sub(r'<testsuite\s+([^>]*)>', fix_failures, new_content, count=1)

    if new_content != content:
        with open(path, "w", encoding="utf-8") as f:
            f.write(new_content)
        rewritten_total += to_rewrite
        print(f"known-failures rewrite: {suite} — {to_rewrite} expected failure(s) marked skipped")

print(f"known-failures rewrite: {rewritten_total} testcase(s) rewritten as skipped")
PYEOF
}

case "$MODE" in
    ratchet) run_ratchet ;;
    xfail)   run_xfail ;;
    rewrite) run_rewrite ;;
esac
