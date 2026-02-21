# Integration Test Debug Notes

**Session date:** 2026-02-21
**Status:** Three new fixes applied. Ready for CI run.

---

## Root Cause Analysis (2026-02-21)

Run 22213572134 (2026-02-20) failed with exit code 2 (infrastructure failure) after
~25 minutes — consistent with a 20-minute CloudFormation creation timeout. The previous
fixes (noiommu, bindgen) were in place but the deployment still failed.

### Root Cause 3: cfn-signal never fires on user-data failure

The user-data script uses `set -euo pipefail` but `cfn-signal` was the LAST command
in the script. When any earlier command fails (e.g., build error, missing package),
`set -e` exits the script immediately — cfn-signal never runs. CloudFormation then
waits the full creation timeout (PT20M or PT35M) before detecting the failure.

**Fix:** Replaced the explicit cfn-signal at the end with an EXIT trap at the top
of the script. The trap ensures cfn-signal fires on EVERY exit (success or failure),
turning 20-minute timeout failures into instant failures with actual error codes in
CloudFormation events.

```bash
# Install cfn-bootstrap BEFORE set -e
dnf install -y aws-cfn-bootstrap 2>/dev/null || true
# Trap EXIT so cfn-signal always fires
trap '/opt/aws/bin/cfn-signal -e $? --stack ... --resource ... --region ... 2>/dev/null || true' EXIT
set -euo pipefail
```

Also moved `dnf install -y aws-cfn-bootstrap` before `set -e` so a missing package
doesn't abort the script before the trap is established.

### Root Cause 4: Full bootstrap meson config mismatched install script

The CDK full-bootstrap path had a different meson command than
`scripts/install_dpdk_amazon_linux.sh`:
- Missing `--libdir=lib` → DPDK installs to `/usr/local/lib64/` on AL2023
  but `PKG_CONFIG_PATH` pointed to `/usr/local/lib/pkgconfig`
- Used `-Denable_kmods=true` → igb_uio compilation fails on newer AL2023 kernels

**Fix:** Aligned the CDK meson command with the install script:
`--libdir=lib --buildtype=release -Denable_kmods=false`

### Root Cause 5: Safety net teardown still set AWS_PROFILE=default

The workflow's safety net teardown step still had `env: AWS_PROFILE: default`,
which shadows env-var credentials in GitHub Actions (same bug as 2026-02-19 SSM fix
but in a different step).

**Fix:** Removed `AWS_PROFILE: default` from the safety net teardown step.

### Other improvements

- Added `unzip` to package installs (both pre-built AMI and full bootstrap paths)
- Added pkg-config verification step before cargo build (diagnostic)
- Added pip `--break-system-packages` fallback for pyelftools (AL2023 PEP 668)

---

## Changes Made (2026-02-21)

1. `deploy/cdk/lib/dpdk-test-stack.ts`:
   - cfn-signal: replaced explicit call with EXIT trap (fires on success AND failure)
   - cfn-bootstrap: install before `set -e` with `|| true` fallback
   - Full bootstrap meson: added `--libdir=lib`, changed `-Denable_kmods=true` to `false`
   - Added `unzip` to package installs
   - Added pkg-config verification step before cargo build
   - Added `--break-system-packages` fallback for pip pyelftools
2. `.github/workflows/integration-tests.yml`:
   - Removed `AWS_PROFILE: default` from safety net teardown step

---

## Root Cause Analysis (2026-02-20)

Both Tier 1 and Tier 3 failed at ENI bind with:
```
scripts/integration-tests/configure-eni.sh: line 128: echo: write error: No such device
```

### Root Cause 1: vfio-pci requires noiommu mode on EC2 Nitro

EC2 `c5n.large` (Nitro) instances don't expose hardware IOMMU to the guest.
The `vfio-pci` driver refuses to bind a device without IOMMU unless you enable:
```bash
echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode
```

This was never set — not in user-data, not in the AMI, not in `configure-eni.sh`.

**Fix:** Added noiommu enable to both `configure-eni.sh` (before bind) and
the CDK user-data runtime config (at boot).

### Root Cause 2: Build uses stubs instead of real DPDK

The build log showed:
```
warning: dpdk-sys@0.1.0: bindgen feature not enabled, using stub implementations
```

`dpdk-sys/build.rs` finds DPDK via pkg-config but then checks
`cfg!(feature = "bindgen")` — which was never enabled. The cargo build
command was `cargo build --release` without `--features dpdk-sys/bindgen`.

**Fix:** Changed the CDK user-data build command to:
```bash
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build --release --features dpdk-sys/bindgen
```

Also added `clang-devel` installation (required by bindgen) to:
- Packer AMI build
- CDK user-data (both full bootstrap and pre-built AMI paths)

---

## Changes Made (2026-02-20)

1. `scripts/integration-tests/configure-eni.sh` — enable noiommu mode before vfio-pci bind
2. `deploy/cdk/lib/dpdk-test-stack.ts` — three changes:
   - Runtime config: enable noiommu mode at boot
   - Build command: add `--features dpdk-sys/bindgen`
   - Install `clang-devel` for bindgen (both AMI paths)
3. `packer/dpdk-ami.pkr.hcl` — add `clang-devel` to system packages

---

## Previous Issues (Resolved)

### Stub code overwriting real project (2026-02-19)
CDK user-data was downloading the real project then overwriting with inline stubs.
Fixed by removing all `inlineProjectFiles`.

### SSM timeout in CI (2026-02-19)
`AWS_PROFILE=default` was being exported, shadowing env-var credentials.
Fixed by only exporting AWS_PROFILE when it's a real named profile.
