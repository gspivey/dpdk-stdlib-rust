# CLAUDE.md

@AGENTS.md

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
   - Polls with `gh run watch --exit-status` until CI finishes
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
