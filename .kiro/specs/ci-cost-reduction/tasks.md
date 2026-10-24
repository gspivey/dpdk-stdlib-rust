# dpdk CI Cost Reduction — Tasks

Read requirements.md, design.md, and this file in full before starting.

All changes are in `.github/workflows/` only. No source code, no tests, no CDK changes.

## Task 1: Update `build-dpdk-ami.yml` schedule and push trigger

File: `.github/workflows/build-dpdk-ami.yml`

Apply the following change to the `on:` block only (do not touch any other part of the file):

1. Change `cron: '0 4 * * 0,3'` to `cron: '0 4 * * 0'` (remove Wednesday — Sunday only)
2. In the `push:` trigger, change `branches: [main]` to `branches: [main, development]`
3. Add `.github/workflows/build-dpdk-ami.yml` to the `paths:` list under `push:`

The rest of the file (jobs, steps, AWS credentials, Packer steps, SSM writes) is unchanged.

## Task 2: Update `build-dpdk-ami-graviton.yml` schedule and push trigger

File: `.github/workflows/build-dpdk-ami-graviton.yml`

Apply the same changes as Task 1:

1. Change `cron: '0 4 * * 0,3'` to `cron: '0 4 * * 0'`
2. Change `branches: [main]` to `branches: [main, development]`
3. Add `packer/**` glob to the `paths:` list (catches any new Graviton Packer files)
4. Add `.github/workflows/build-dpdk-ami-graviton.yml` to the `paths:` list

The rest of the file is unchanged.

## Task 3: Remove `development` from `integration-tests.yml`

File: `.github/workflows/integration-tests.yml`

Change the `pull_request` trigger branches:

```yaml
# BEFORE:
  pull_request:
    branches: [main, development]

# AFTER:
  pull_request:
    branches: [main]
```

The `workflow_dispatch: {}` trigger MUST be preserved unchanged.
No other changes to this file.

## Task 4: Remove `development` from `integration-tests-graviton.yml`

File: `.github/workflows/integration-tests-graviton.yml`

Apply the identical change as Task 3:

```yaml
# BEFORE:
  pull_request:
    branches: [main, development]

# AFTER:
  pull_request:
    branches: [main]
```

Preserve `workflow_dispatch: {}`.
No other changes to this file.

## Task 5: Update ROADMAP

Add items 43 and 44 to ROADMAP.md and tick them as complete after the PR merges:
- Item 43 = Tasks 1, 2 (AMI build schedule reduction)
- Item 44 = Tasks 3, 4 (integration test branch filter)
