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
4. **Integration tests** — triggered automatically on PR. Agents: poll status and read logs via the GitHub MCP tools (`mcp__github__pull_request_read` with `get_check_runs`, `mcp__github__pull_request_read` with `get_comments` to read staged `[CI] Stage: …` comments). Humans: use `./scripts/ci-validate.sh` locally. On failure, read the actual PR comments and logs — don't guess.
5. **Performance tests** — trigger the `perf-tests.yml` workflow (via MCP `mcp__github__*` tooling in agent sessions, or `gh workflow run perf-tests.yml` locally). Results are posted back as PR comments; read them with `mcp__github__pull_request_read` (`get_comments`). If they regress, go back to step 1.
6. **Success** — all tests pass. Ask the user to review the PR.

**Key rules:**
- Never skip straight to PR without passing local tests
- Never assume CI will catch what local tests missed
- On failure, read the actual logs — do not guess
- Loop back to step 1 on any failure, do not try to patch forward

## Claude Code (Hooks & Skills)

### GitHub Access: MCP, Not `gh` CLI

**Agents running in Claude Code do NOT use the `gh` CLI.** The system prompt makes this
explicit: "You do NOT have access to the `gh` CLI, `hub` CLI, or direct GitHub API access.
Instead, use the GitHub MCP server tools (prefixed with `mcp__github__`) for ALL GitHub
interactions."

Why: in scheduled/remote Claude Code runs the harness doesn't vend a usable `GH_TOKEN` for
direct `api.github.com` calls (`gh` returns HTTP 401). The MCP GitHub server is the
authenticated channel the harness provides. Git operations (`git push`, `git fetch`) still
work transparently because they go through a local git proxy (`http://127.0.0.1:*/git/...`),
which handles auth server-side.

Human developers running the `./scripts/*.sh` helpers locally can keep using `gh` — those
scripts are for interactive development, not for agent use. This section is the agent path.

### Querying CI / GitHub Actions Results (agent path, MCP)

The repo is `gspivey/dpdk-stdlib-rust` (the only repo MCP is scoped to in this project).

**Find the PR for the current branch and read its status:**
```
mcp__github__list_pull_requests(owner="gspivey", repo="dpdk-stdlib-rust",
                                state="open", head="gspivey:<branch>")
mcp__github__pull_request_read(method="get_status", owner=…, repo=…, pullNumber=<n>)
mcp__github__pull_request_read(method="get_check_runs", owner=…, repo=…, pullNumber=<n>)
```

`get_check_runs` returns per-check status/conclusion for the head commit — this is the
MCP equivalent of `gh run view --json status,conclusion` and is how agents poll CI.

**Read staged CI comments on the PR (fastest path to diagnosis):**
```
mcp__github__pull_request_read(method="get_comments", owner=…, repo=…, pullNumber=<n>)
```
CI posts progress at each stage as separate comments. Look for `[CI] Stage: …` headers:
- `[CI] Stage: Deploy` — instance IDs, SSM readiness
- `[CI] Stage: Baseline Diagnostics` — networking state before tests
- `[CI] Stage: Tier N Results` — test pass/fail summary
- `[CI] Stage: Failure Diagnostics` — networking state + log excerpts on failure
- `[CI] Stage: Summary` — final pass/fail for all tiers

These comments carry the richest summary info (failed step, error, truncated logs), so
read them *before* reaching for raw workflow logs.

**Subscribe to PR activity events instead of polling:**
```
mcp__github__subscribe_pr_activity(pullRequestNumber=<n>)
```
Events arrive wrapped in `<github-webhook-activity>` tags (comments, CI status changes,
reviews). This is usually preferable to a poll loop.

### Diagnosing CI Failures (agent path)

1. **Find the PR and its current status:**
   ```
   mcp__github__list_pull_requests(...)  # get pullNumber
   mcp__github__pull_request_read(method="get_check_runs", ...)
   ```

2. **Read staged CI comments** (most efficient — CI self-documents):
   ```
   mcp__github__pull_request_read(method="get_comments", ...)
   ```
   The `[CI] Stage: Failure Diagnostics` comment usually contains the actual error and
   log excerpts already triaged by the CI workflow. Start there.

3. **Cross-reference with domain knowledge:**
   - `docs/aws-vpc-networking.md` for networking failures (ARP, MAC, packets not arriving)
   - `docs/debugging-log.md` for previously encountered issues

4. **If it's a networking issue** (most common):
   The answer is almost always: **use the gateway MAC, not the peer MAC**. Check that
   `--gateway-mac` is being discovered and passed correctly in the test harness.
   See `docs/aws-vpc-networking.md` Known Failure Patterns table.

5. **Fix and push a new commit.** CI re-runs automatically on push. Do NOT re-run
   `ci-validate.sh` from an agent session — it's a local-developer script that uses `gh`.

### Instance-log artifacts (rich log data)

When CI fails with exit code 2 (infrastructure failure), an `instance-logs` artifact is
uploaded containing `failure-summary.json`, `sender-user-data.log`, `sender-console-output.log`,
`sender-journal.txt`, `sender-build-listing.txt`, and `receiver-*` equivalents.

The `[CI] Stage: Failure Diagnostics` PR comment already inlines the most useful slices of
these. Agents should read that comment first. If deeper inspection is needed and the MCP
tools don't expose workflow artifact downloads directly, ask the user to download and
share the relevant tail — artifact fetching requires raw Actions API access.

**Exit codes:**

| Exit code | Meaning | Where to look |
|---|---|---|
| `2` | Infrastructure/setup failure | `[CI] Stage: Failure Diagnostics` comment |
| `1` | Test assertion failure | `[CI] Stage: Tier N Results` comment, or JUnit XML artifacts |
| `0` | All tests passed | — |

### Local-developer equivalents (`gh` CLI)

These commands are for humans running in a shell with a valid `GH_TOKEN`. Agents should
use the MCP equivalents above.

```bash
gh run list --repo gspivey/dpdk-stdlib-rust --workflow=integration-tests.yml --limit 5
gh run view <run-id> --repo gspivey/dpdk-stdlib-rust --json status,conclusion
gh run view <run-id> --log-failed --repo gspivey/dpdk-stdlib-rust
gh run download <run-id> --name instance-logs --repo gspivey/dpdk-stdlib-rust --dir /tmp/ci-logs
./scripts/ci-validate.sh              # local: build, test, push, trigger CI, poll, report
./scripts/diagnose-ci-failure.sh      # local: auto-fetch + summarize the most recent failed run
```

`gh run watch` does not work in this repo — the fine-grained PAT lacks `checks:read`.
Poll with `--json status,conclusion` instead.

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
   - Agents (Claude Code): use `mcp__github__create_pull_request` with `owner=gspivey`,
     `repo=dpdk-stdlib-rust`, `base=development`, `head=<feature-branch>`.
   - Humans (local shell): `gh pr create --base development --title "..." --body "..."`.

For changes that don't touch networking code (docs, CI config, scripts), you can skip integration
tests with `./scripts/ci-validate.sh --skip-integration`.

### Validation Script Reference

```
./scripts/ci-validate.sh [OPTIONS]

  --skip-local          Skip cargo build/test (only trigger CI)
  --skip-integration    Skip integration tests (only run local checks)
  -h, --help            Show help
```
