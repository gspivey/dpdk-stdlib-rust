# CLAUDE.md

@AGENTS.md

## Development Loop

**Every code change MUST follow this loop. Do not skip steps.**

```
  +------------------+
  |  1. Write Code   |<-----------------------------------------+
  +--------+---------+                                           |
           |                                                     |
           v                                                     |
  +------------------+                                           |
  |  2. Unit Tests   |   cargo build && cargo test               |
  +--------+---------+                                           |
           |                                                     |
       pass|   fail --> fix code --------------------------------+
           v                                                     |
  +------------------+                                           |
  |  3. Push / PR    |   git push or gh pr create                |
  +--------+---------+                                           |
           |                                                     |
           v                                                     |
  +------------------+                                           |
  | 4. Integration   |   (auto-triggered on PR, or               |
  |    Tests         |    ./scripts/ci-validate.sh)              |
  +--------+---------+                                           |
           |                                                     |
       pass|   fail --> read logs, fix code ---------------------+
           v                                                     |
  +------------------+                                           |
  | 5. Performance   |   gh workflow run perf-tests.yml          |
  |    Tests         |   poll with gh run view --json            |
  +--------+---------+                                           |
           |                                                     |
       pass|   fail --> read PR comments, fix code -------------+
           v
  +------------------+
  | 6. Success!      |   Ask user to review PR
  +------------------+
```

**Step details:**

1. **Write code** — read files before modifying, follow patterns in AGENTS.md
2. **Unit tests** — `cargo build && cargo test` locally. If they fail, fix and re-run. Do NOT proceed with failures.
3. **Push / PR** — push to the feature branch. Create a PR if one doesn't exist yet, otherwise push a new commit.
4. **Integration tests** — triggered automatically on PR, or manually via `./scripts/ci-validate.sh`. Poll with `gh run view --json status,conclusion`. If they fail, read the PR comments and instance logs to diagnose. Fix the code and go back to step 1.
5. **Performance tests** — trigger with `gh workflow run perf-tests.yml`. Poll until complete. Read the PR comments for benchmark results and app logs. If they fail or regress, fix and go back to step 1.
6. **Success** — all tests pass. Ask the user to review the PR.

**Key rules:**
- Never skip straight to PR without passing local tests
- Never assume CI will catch what local tests missed
- On failure, read the actual logs — do not guess
- Loop back to step 1 on any failure, do not try to patch forward

## Claude Code (Hooks & Skills)

### Querying CI / GitHub Actions Results

The repo is **private** — WebFetch cannot access it unauthenticated. Use the `gh` CLI.
The session-start hook installs `gh` and authenticates it automatically in remote sessions.
Verify it's ready:

```bash
gh auth status        # should show "Logged in to github.com"
gh --version          # confirm installed
```

If not ready, check `~/.local/bin/gh` and `$GH_TOKEN` / `$GITHUB_TOKEN`:
```bash
export PATH="$HOME/.local/bin:$PATH"
echo "${GH_TOKEN:-$GITHUB_TOKEN}" | gh auth login --with-token
```

**List recent integration test runs:**
```bash
gh run list --repo gspivey/dpdk-stdlib-rust --workflow=integration-tests.yml --limit 5
```

**Check a specific run (status + step breakdown):**
```bash
gh run view <run-id> --repo gspivey/dpdk-stdlib-rust
```

**⚠ `gh run watch` does NOT work in this repo.** The `GH_TOKEN` is a fine-grained PAT, which
does not support the `checks:read` permission required by `gh run watch`. It will silently fail
or error. **Always poll with `gh run view --json status,conclusion` instead:**
```bash
# Poll until a run completes
while true; do
  json=$(gh run view <run-id> --repo gspivey/dpdk-stdlib-rust --json status,conclusion)
  status=$(echo "$json" | jq -r '.status')
  if [ "$status" = "completed" ]; then
    echo "Conclusion: $(echo "$json" | jq -r '.conclusion')"
    break
  fi
  sleep 30
done
```

**Get only failed step logs (fastest path to root cause):**
```bash
gh run view <run-id> --log-failed --repo gspivey/dpdk-stdlib-rust
```

**Download failure data (exit code 2 — infrastructure failure):**

`ci-validate.sh` does this automatically on failure. To do it manually:

```bash
# Download the instance-logs artifact — this is the authoritative source of truth
gh run download <run-id> --name instance-logs --repo gspivey/dpdk-stdlib-rust --dir /tmp/ci-logs

# Structured summary: which step failed and the error message
cat /tmp/ci-logs/failure-summary.json

# Richest log: full EC2 user-data script output (what ran on the instance)
# Search for the last === section header to find where it stopped
tail -100 /tmp/ci-logs/sender-user-data.log

# Fallback if SSM wasn't available (always present, even after instance teardown)
tail -100 /tmp/ci-logs/sender-console-output.log
```

**What's in the `instance-logs` artifact:**

| File | When present | What it contains |
|---|---|---|
| `failure-summary.json` | Always on exit 2 | `failed_step`, `error`, instance IDs, run URL |
| `sender-user-data.log` | SSM available | Full `/var/log/user-data.log` from sender EC2 |
| `sender-console-output.log` | Always | EC2 console output (survives termination) |
| `sender-journal.txt` | SSM available | `journalctl` last 500 lines |
| `sender-build-listing.txt` | SSM available | `ls /opt/dpdk-stdlib/target/release/` |
| Same files for `receiver-*` | — | Receiver instance logs |

**Interpreting exit codes:**

| Exit code | Meaning | What to read |
|---|---|---|
| `2` | Infrastructure/setup failure | `failure-summary.json` → then read the log files above |
| `1` | Test assertion failure | `test-results/*.xml` JUnit files |
| `0` | All tests passed | — |

**Diagnosis pattern:**
```bash
# Run ci-validate.sh — it auto-downloads failure data and prints it on failure
./scripts/ci-validate.sh

# Or manually inspect a past run:
gh run list --repo gspivey/dpdk-stdlib-rust --workflow=integration-tests.yml --limit 5
gh run download <run-id> --name instance-logs --repo gspivey/dpdk-stdlib-rust --dir /tmp/ci-logs
cat /tmp/ci-logs/failure-summary.json
tail -100 /tmp/ci-logs/sender-user-data.log
```

**Job-level step breakdown:**
```bash
gh api repos/gspivey/dpdk-stdlib-rust/actions/runs/<run-id>/jobs \
  --jq '.jobs[] | {name, conclusion, failed_steps: [.steps[] | select(.conclusion != "success") | .name]}'
```

### Diagnosing CI Failures (Step-by-Step)

When integration tests fail, follow this sequence. This works in **Claude Code web** (no AWS
access needed — only `gh` CLI).

**Quick path — use the diagnostic script:**
```bash
./scripts/diagnose-ci-failure.sh                    # Most recent failed run
./scripts/diagnose-ci-failure.sh <run-id>           # Specific run
./scripts/diagnose-ci-failure.sh --pr <number>      # Read staged CI comments from PR
```

**Manual path:**

1. **Get the run ID:**
   ```bash
   gh run list --repo gspivey/dpdk-stdlib-rust --workflow=integration-tests.yml --limit 5
   ```

2. **Read staged CI comments on the PR** (fastest — CI posts progress at each stage):
   ```bash
   gh pr view <pr-number> --comments --repo gspivey/dpdk-stdlib-rust
   ```
   Look for comments with `[CI] Stage:` headers. Each stage posts its own comment:
   - `[CI] Stage: Deploy` — instance IDs, SSM readiness
   - `[CI] Stage: Baseline Diagnostics` — networking state before tests
   - `[CI] Stage: Tier N Results` — test pass/fail summary
   - `[CI] Stage: Failure Diagnostics` — networking state + log excerpts on failure
   - `[CI] Stage: Summary` — final pass/fail for all tiers

3. **Cross-reference with domain knowledge:**
   - Read `docs/aws-vpc-networking.md` for networking failures (ARP, MAC, packets not arriving)
   - Read `docs/debugging-log.md` for previously encountered issues

4. **If it's a networking issue** (most common):
   The answer is almost always: **use the gateway MAC, not the peer MAC**.
   Check that `--gateway-mac` is being discovered and passed correctly in the test harness.
   See `docs/aws-vpc-networking.md` Known Failure Patterns table.

5. **Fix and re-run:**
   ```bash
   ./scripts/ci-validate.sh --skip-local   # Skip local cargo checks, just trigger CI
   ```

### Session Start Hook

A session-start hook (`.claude/hooks/session-start.sh`) runs automatically when a Claude Code
remote session begins. It ensures the Rust toolchain is installed and the workspace is pre-built
so that `cargo test` and `cargo build` are fast (incremental) from the first interaction.

The hook is registered in `.claude/settings.json` and only runs in remote environments
(`$CLAUDE_CODE_REMOTE=true`). Local Claude Code sessions skip it.

### Before Creating a PR

Every session is responsible for validating its own work before opening a PR.
Do NOT push a PR and hope CI catches problems — close the feedback loop in-session.

1. **Run local checks first** (fast, catches most issues):
   ```bash
   cargo build && cargo test
   ```

2. **Push the branch and trigger integration tests** (when changes touch networking, backends, or deployment):
   ```bash
   ./scripts/ci-validate.sh
   ```
   This script:
   - Runs `cargo build` + `cargo test` locally
   - Pushes the current branch
   - Triggers the `integration-tests.yml` workflow via `gh workflow run`
   - Polls with `gh run view --json status,conclusion` until CI finishes
   - Exits 0 only if everything passes

3. **If integration tests fail**, `ci-validate.sh` will automatically download and print
   the `failure-summary.json` and relevant log tail. Read the actual log output to diagnose —
   do not guess. Fix the code, then re-run with `--skip-local` to skip the local cargo checks:
   ```bash
   ./scripts/ci-validate.sh --skip-local
   ```

4. **Only after all checks pass**, create the PR:
   ```bash
   gh pr create --title "..." --body "..."
   ```

For changes that don't touch networking code (docs, CI config, scripts), you can skip integration
tests with `./scripts/ci-validate.sh --skip-integration`.

### Validation Script Reference

```
./scripts/ci-validate.sh [OPTIONS]

  --skip-local          Skip cargo build/test (only trigger CI)
  --skip-integration    Skip integration tests (only run local checks)
  -h, --help            Show help
```
