# dpdk CI Cost Reduction — Requirements

## Problem

Two scheduled workflows run on a fixed time schedule regardless of code changes,
burning Actions minutes with zero signal value when nothing has changed:

1. `build-dpdk-ami.yml` — runs every Sunday and Wednesday at 4AM UTC (~30 min/run).
   Packer AMI builds only produce new value when `packer/**` or the DPDK install script
   changes. Running on a fixed schedule twice weekly means ~8 runs/month × 30 min = 240 min
   of scheduled overhead regardless of code activity.

2. `build-dpdk-ami-graviton.yml` — same problem, same schedule (~30 min/run). Another
   240 min/month.

Additionally:

3. `integration-tests.yml` and `integration-tests-graviton.yml` trigger on PRs to both
   `main` and `development`. The `development` branch is explicitly intended as a
   staging/integration area that is expected to break. Running 169+65=234 min integration
   tests on a branch meant to accept incomplete work is misaligned.

## Goals

1. Reduce scheduled AMI build runs from 8/month to ≤4/month without losing coverage.
2. Ensure AMI builds still run when Packer or DPDK install scripts actually change.
3. Stop running integration tests on PRs to `development` — they belong on `main` only.
4. Preserve all current integration test coverage on PRs targeting `main`.

## Non-Goals

- Changing what the AMI build does or what DPDK version it targets.
- Modifying the integration test logic or test cases.
- Touching `perf-tests-*.yml`, `build-trex-ami.yml`, or `quic-*` workflows.

## Requirements

### REQ-1: AMI build schedule — weekly not twice-weekly

`build-dpdk-ami.yml` and `build-dpdk-ami-graviton.yml` MUST be changed from firing
every Sunday AND Wednesday to firing only once per week (Sunday at 4AM UTC).

This halves scheduled overhead immediately: 8 runs/month → 4 runs/month, saving ~240 min/month.

### REQ-2: AMI builds trigger on Packer/script changes

Both AMI build workflows MUST add a `push` trigger on `development` and `main` branches
filtered to paths that affect the AMI content:

```yaml
push:
  branches: [main, development]
  paths:
    - 'packer/**'
    - 'scripts/install_dpdk_amazon_linux.sh'
    - '.github/workflows/build-dpdk-ami.yml'  # (or build-dpdk-ami-graviton.yml)
```

The Graviton workflow additionally includes `packer/dpdk-ami-graviton.pkr.hcl`.

This ensures an AMI rebuild fires within hours of a Packer or install-script change
on any active branch, not just on the weekly schedule.

### REQ-3: Integration tests — remove `development` branch trigger

> **Revised 2026-10-22 (Gerard, implemented in the CI cost reduction PR):** main-only gating would leave the
> agent-router PRs against `development` with no hardware signal. Instead, the three EC2 integration workflows keep
> PRs to `main` and `development` but are **path-filtered** to code, harness, deploy and workflow paths (docs,
> specs and ROADMAP-only PRs skip EC2), and gain a **scheduled full run on `development`** (x86 and Graviton
> daily, QUIC weekly) behind a `nightly-gate` job that skips the run when the commit already passed a scheduled
> run. The TRex AMI schedule also drops to weekly. The original text below is kept for history.

`integration-tests.yml` and `integration-tests-graviton.yml` currently trigger on
PRs to both `main` and `development`.

Both workflows MUST be changed to trigger only on PRs to `main`:

```yaml
# BEFORE:
on:
  pull_request:
    branches: [main, development]

# AFTER:
on:
  pull_request:
    branches: [main]
  workflow_dispatch: {}
```

The `workflow_dispatch` trigger MUST be preserved so integration tests can be manually
triggered against `development` when needed.

### REQ-4: Preserve integration test coverage on `main`

All existing integration test jobs (synthetic-perf, integration-tests, teardown, etc.)
MUST continue to run unchanged on PRs targeting `main`. No test cases, no job steps,
and no teardown logic may be removed.

## Acceptance Criteria

- AMI build workflows fire at most once per week on schedule (Sunday only)
- AMI build workflows fire within 1 hour of a push to `packer/**` or `install_dpdk_amazon_linux.sh`
- A PR to `development` does NOT trigger `integration-tests.yml` or `integration-tests-graviton.yml`
- A PR to `main` triggers the full integration test suite unchanged
- Manual `workflow_dispatch` works on both integration test workflows
