# Agent-Router Prompt — dpdk-stdlib-rust

> The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
> "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
> interpreted as described in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

Repo: https://github.com/gspivey/dpdk-stdlib-rust

---

## 1. Setup

The agent MUST create a persistent working directory outside of `/tmp`:

```bash
mkdir -p /home/agentrouter/agent-runs
WORKDIR=/home/agentrouter/agent-runs/$(date +%Y%m%d-%H%M%S)-dpdk
mkdir -p "$WORKDIR" && cd "$WORKDIR"
git clone https://github.com/gspivey/dpdk-stdlib-rust.git
cd dpdk-stdlib-rust
```

The agent MUST read `AGENTS.md` and `CLAUDE.md` before writing any code.

---

## 2. Roadmap Selection

The agent MUST select the first entry in `ROADMAP.md` whose completion checkbox is
unchecked (`- [ ] Complete`). The agent MUST implement exactly that one feature and
open exactly one PR in this session. The agent MUST NOT begin a second feature.

The agent MUST read all spec files listed on the selected item's `Spec:` line before
writing any code.

---

## 3. Implementation

1. The agent MUST create a branch: `git checkout -b agent/<short-feature-slug>`.
2. Unit tests are REQUIRED. Integration and synthetic tests SHOULD be added where the
   spec calls for them. The agent SHOULD commit tests before implementation code.
3. The agent MUST run `cargo build && cargo test` locally and MUST fix any failure
   before proceeding.
4. After every commit the agent MUST push: `git push -u origin agent/<short-feature-slug>`
   (subsequent pushes: `git push`).
5. The agent MUST open a PR via `gh pr create` with a title matching the roadmap item
   name and a body that identifies the roadmap item addressed, tests added, and any
   relevant tradeoffs.
6. The agent MUST immediately call the agent-router MCP `register_pr` tool with the PR
   number and MUST NOT push additional commits until registration is confirmed.

---

## 4. CI Iteration

After any `git push` the agent MUST stop and wait. The agent MUST NOT poll for CI
results by executing `gh run view`, `gh run watch`, or `gh run list` in a loop.
Agent-router delivers CI results as `check_run` events. When a result arrives, the
agent MUST act: fix failures and push, or proceed if green.

---

## 5. Performance Tests

Performance tests are a separate GitHub Actions workflow. The agent MUST NOT run
performance tests locally and MUST NOT treat local benchmark or synthetic test output
as performance data.

After all CI checks on the PR are green, the agent MUST trigger the performance
workflow:

```bash
gh workflow run perf-tests.yml --ref <your-branch-name>
```

The agent MUST then stop and wait for the result to be delivered as a `check_run`
event. When results arrive, the agent MUST check for regressions against
`docs/perf-test-log.md`. If regressions are present, the agent MUST fix them and
re-trigger both CI and performance tests.

The agent MUST append only the GitHub Actions performance test results to
`docs/perf-test-log.md` in the existing format. Synthetic, local, or otherwise
non-CI-sourced data MUST NOT be appended to the perf log.

---

## 6. Finalize

Before requesting merge the agent MUST commit all of the following to the feature branch:

1. The `docs/perf-test-log.md` entry from the GitHub Actions performance test run.
2. An update to `ROADMAP.md`: change the selected item's completion line from
   `- [ ] Complete · PR: —` to `- [x] Complete · PR: #<number>`.

Both changes MUST be present on the feature branch before merge.

---

## 7. Merge

Once CI and performance tests are green and the feature branch contains the perf-log
entry and ROADMAP update, the agent MUST squash-merge to `development`. The session
is then complete. The agent MUST NOT start a second feature.

---

## 8. Constraints

- **One PR per session.** The agent MUST NOT open additional PRs or select a second
  roadmap item.
- **Missing toolchain.** If `cargo build` fails due to a missing C compiler, `rustc`,
  or system library, the agent MUST stop and report the missing dependency in a PR
  comment or session message. The agent MUST NOT attempt to bootstrap a toolchain via
  conda, snap, or any other user-space package manager.
- **Auth failures.** If `git push` or `gh pr create` fails with an authentication
  error, the agent MUST stop and report the error. The agent MUST NOT attempt to fix
  credentials.
- **CI divergence.** If the agent cannot converge after a reasonable number of CI
  cycles, it MUST post a PR comment summarizing the blocker and stop.
- **No root.** The agent MUST NOT run `sudo` commands. If a task requires root, the
  agent MUST report it and stop.
