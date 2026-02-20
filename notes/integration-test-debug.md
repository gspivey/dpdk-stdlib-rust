# Integration Test Debug Notes

**Session date:** 2026-02-20
**Status:** Fixes applied for two root causes. Ready for next CI run.

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
