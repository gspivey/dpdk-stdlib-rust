# dpdk CI Cost Reduction — Design

## Changes

Four minimal workflow edits. No new files, no new actions, no new logic.

### 1. `build-dpdk-ami.yml` — schedule + push trigger

```yaml
# BEFORE:
on:
  schedule:
    - cron: '0 4 * * 0,3' # Sunday and Wednesday 4:00 AM UTC (twice weekly)
  workflow_dispatch:
    inputs:
      dpdk_version:
        description: 'DPDK version to build'
        required: false
        default: '22.11.6'
  push:
    branches: [main]
    paths:
      - 'packer/**'
      - 'scripts/install_dpdk_amazon_linux.sh'

# AFTER:
on:
  schedule:
    - cron: '0 4 * * 0'  # Sunday 4:00 AM UTC (weekly)
  workflow_dispatch:
    inputs:
      dpdk_version:
        description: 'DPDK version to build'
        required: false
        default: '22.11.6'
  push:
    branches: [main, development]
    paths:
      - 'packer/**'
      - 'scripts/install_dpdk_amazon_linux.sh'
      - '.github/workflows/build-dpdk-ami.yml'
```

Change: remove `,3` from the cron day-of-week field; add `development` to push branches;
add workflow file itself to path filter.

### 2. `build-dpdk-ami-graviton.yml` — schedule + push trigger

```yaml
# BEFORE:
on:
  schedule:
    - cron: '0 4 * * 0,3' # Sunday and Wednesday 4:00 AM UTC (twice weekly)
  workflow_dispatch:
    inputs:
      dpdk_version:
        description: 'DPDK version to build'
        required: false
        default: '22.11.6'
  push:
    branches: [main]
    paths:
      - 'packer/dpdk-ami-graviton.pkr.hcl'
      - 'scripts/install_dpdk_amazon_linux.sh'

# AFTER:
on:
  schedule:
    - cron: '0 4 * * 0'  # Sunday 4:00 AM UTC (weekly)
  workflow_dispatch:
    inputs:
      dpdk_version:
        description: 'DPDK version to build'
        required: false
        default: '22.11.6'
  push:
    branches: [main, development]
    paths:
      - 'packer/dpdk-ami-graviton.pkr.hcl'
      - 'packer/**'
      - 'scripts/install_dpdk_amazon_linux.sh'
      - '.github/workflows/build-dpdk-ami-graviton.yml'
```

Change: remove `,3`; add `development`; add `packer/**` glob (catches any new Packer file);
add workflow file to path filter.

### 3. `integration-tests.yml` — remove `development` branch

```yaml
# BEFORE:
on:
  pull_request:
    branches: [main, development]
  workflow_dispatch: {}

# AFTER:
on:
  pull_request:
    branches: [main]
  workflow_dispatch: {}
```

Change: remove `development` from the `branches` list. Everything else in the file is
unchanged — all jobs, all steps, all teardown logic stays identical.

### 4. `integration-tests-graviton.yml` — remove `development` branch

```yaml
# BEFORE:
on:
  pull_request:
    branches: [main, development]
  workflow_dispatch: {}

# AFTER:
on:
  pull_request:
    branches: [main]
  workflow_dispatch: {}
```

Identical change to the Graviton variant.

## Cost Impact

| Change | Before | After | Saving |
|---|---|---|---|
| AMI builds: twice weekly → weekly | 8 runs/month × 30 min = 240 min | 4 runs × 30 min = 120 min | −120 min/month |
| Graviton AMI: same | 240 min/month | 120 min/month | −120 min/month |
| Integration tests on `development` PRs | ~234 min per PR to development | 0 | −234 min per PR |
| **Total** | | | **−240 min/month + savings per dev PR** |

At the historical rate of one dpdk PR per week to `development`, removing that trigger
saves another ~936 min/month. Combined scheduled saving alone is 240 min/month.

## Risk

Minimal. All changes are additive removals of triggers:
- No test logic changes
- No job changes
- No new dependencies
- The weekly Sunday AMI build continues to provide a baseline rebuild even with no code changes
  (covers base AMI updates, upstream package changes)
- `workflow_dispatch` preserved for on-demand integration test runs against `development`
