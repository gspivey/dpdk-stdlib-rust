# Integration Test Debug Notes

**Session date:** 2026-02-21
**Status:** Four root causes fixed across two sessions. Latest fix: C shim for real-DPDK build.

---

## Root Cause Analysis (2026-02-21, session 2)

Run failed at `cargo build --release --features dpdk-sys/bindgen` with 26 compilation
errors.  The build succeeded locally (stubs) but failed on EC2 with real DPDK 22.11.6
installed.  Three categories of failures:

### Root Cause 6: bindgen cannot wrap static inline DPDK functions

DPDK defines performance-critical functions as `static inline` in headers:
- `rte_eth_rx_burst`, `rte_eth_tx_burst` (packet I/O)
- `rte_pktmbuf_alloc`, `rte_pktmbuf_alloc_bulk`, `rte_pktmbuf_free` (mbuf lifecycle)
- `rte_mempool_full`, `rte_mempool_empty` (pool queries)
- `rte_errno` (per-lcore error variable accessed via macro)

bindgen skips `static inline` functions entirely — they don't appear in the generated
`bindings.rs`.  The stubs define them as regular Rust functions, so the mismatch is
invisible locally.

**Fix:** Added a C shim (`dpdk-sys/csrc/dpdk_shim.c`) that wraps each inline function
with a non-inline `dpdk_shim_*` variant.  A Rust shim module (`dpdk-sys/src/shim.rs`)
declares the externs and re-exports them under the original names.  `build.rs` compiles
the C shim via `cc` crate when in bindgen mode.

### Root Cause 7: bindgen cannot capture C preprocessor macro constants

DPDK offload flags and NUMA constants are `#define` macros:
```c
#define RTE_ETH_RX_OFFLOAD_VLAN_STRIP  RTE_BIT64(0)   // 0x1
#define RTE_ETH_RX_OFFLOAD_IPV4_CKSUM  RTE_BIT64(1)   // 0x2
#define RTE_ETH_TX_OFFLOAD_IPV4_CKSUM  RTE_BIT64(1)   // 0x2
// ... 8 total offload constants
#define SOCKET_ID_ANY  -1
```

bindgen does not evaluate arbitrary `#define` expressions, especially those involving
other macros like `RTE_BIT64()`.

**Fix:** Defined all 9 missing constants directly in the Rust shim module with values
matching DPDK 22.11.  These are only compiled under `#[cfg(dpdk_bindgen)]`; the stubs
already have their own copies.

### Root Cause 8: bindgen generates methods for bitfield accessors

`rte_eth_link` uses C bitfields for `link_duplex`, `link_autoneg`, `link_status`.
bindgen generates accessor methods (e.g., `link.link_duplex()`) rather than fields.
The stubs had plain struct fields, so `link.link_duplex` compiled locally but not
against real bindings.

**Fix:** Added accessor methods to the stub `rte_eth_link` (`fn link_duplex(&self) -> u16`)
matching bindgen's generated API.  Changed `dpdk/src/port.rs` to use method-call syntax
(`link.link_duplex()`) which works with both stubs and real bindings.

---

## Changes Made (2026-02-21, session 2)

1. `dpdk-sys/csrc/dpdk_shim.c` — **NEW**: C wrappers for 8 static inline DPDK functions
2. `dpdk-sys/src/shim.rs` — **NEW**: Rust extern declarations + pub wrappers + 9 constants
3. `dpdk-sys/src/lib.rs` — Include shim module under `#[cfg(dpdk_bindgen)]`
4. `dpdk-sys/Cargo.toml` — Add `cc = "1"` build dependency
5. `dpdk-sys/build.rs` — Call `compile_shim()` when in bindgen mode
6. `dpdk-sys/src/stubs.rs` — Add `link_duplex()`, `link_autoneg()`, `link_status()` methods
7. `dpdk/src/port.rs` — Use method syntax for bitfield access

All 133 tests pass locally (stubs mode).

---

## Stub Improvement Recommendations

The stubs serve development well today, but have gaps that limit local test coverage.
These are ranked by impact on catching real-DPDK integration issues earlier:

### 1. Loopback packet I/O (HIGH IMPACT)

**Current:** `rte_eth_rx_burst` and `rte_eth_tx_burst` always return 0 (no packets).
This means no end-to-end socket path exercises the actual packet build → send → recv
→ parse loop without real hardware.

**Proposal:** Add a per-port `VecDeque<Vec<u8>>` ring behind a `Mutex`.  `tx_burst`
copies frame bytes into the ring; `rx_burst` drains from it.  A loopback flag (default
on for stubs) would connect a port's TX to its own RX.

```
Thread A: UdpSocket::send_to → build_udp_frame → tx_burst → ring.push_back(frame)
Thread B: UdpSocket::recv_from → rx_burst → ring.pop_front() → parse_udp_packet
```

This would allow `test_synthetic_socket_echo` to exercise the real packet path instead
of faking recv data.  It also unlocks multi-threaded send/recv stress tests.

### 2. Device capability reporting (MEDIUM IMPACT)

**Current:** `rte_eth_dev_info_get` reports `rx_offload_capa: 0`, `tx_offload_capa: 0`.

**Proposal:** Report realistic capabilities (IPv4/UDP/TCP checksum offload, VLAN
strip/insert) so the offload negotiation path in `Port::new()` is exercised:

```rust
rx_offload_capa: RTE_ETH_RX_OFFLOAD_IPV4_CKSUM | RTE_ETH_RX_OFFLOAD_UDP_CKSUM
                 | RTE_ETH_RX_OFFLOAD_TCP_CKSUM | RTE_ETH_RX_OFFLOAD_VLAN_STRIP,
tx_offload_capa: RTE_ETH_TX_OFFLOAD_IPV4_CKSUM | RTE_ETH_TX_OFFLOAD_UDP_CKSUM
                 | RTE_ETH_TX_OFFLOAD_TCP_CKSUM | RTE_ETH_TX_OFFLOAD_VLAN_INSERT,
```

### 3. Statistics tracking (MEDIUM IMPACT)

**Current:** `rte_eth_stats_get` always returns zeros.

**Proposal:** Increment `opackets`/`obytes` on `tx_burst` and `ipackets`/`ibytes` on
`rx_burst`.  Reset to zero on `rte_eth_stats_reset`.  This validates that the stats
reporting path in `Port::stats()` actually reflects I/O activity.

### 4. Error injection (LOW-MEDIUM IMPACT)

**Current:** All functions return success unconditionally.

**Proposal:** Add a `thread_local!` or global configuration that lets tests inject:
- Mempool exhaustion (`rte_pktmbuf_alloc` returns null)
- Port start failure (`rte_eth_dev_start` returns -1)
- EAL init failure

This tests error handling paths that are currently dead code under stubs.

### 5. Multi-port support (LOW IMPACT)

**Current:** `rte_eth_dev_count_avail()` returns 1; all port ops use port_id 0.

**Proposal:** Support configurable port count (2–4 simulated ports) with independent
MAC addresses, stats, and loopback rings.  Enables testing multi-NIC scenarios.

---

## Architectural Note: Stub vs Bindgen Parity

The root cause of issues 6–8 is a **parity gap** between the stub API surface and the
real bindgen output.  The stubs were written by hand against the DPDK documentation, but:
- They define regular functions where DPDK uses `static inline`
- They define struct fields where DPDK uses bitfields (bindgen generates methods)
- They define constants where DPDK uses macro chains

**Going forward**, any new DPDK function or constant added to the stubs should be
cross-checked against a real bindgen output.  The C shim + Rust shim pattern established
here is the canonical way to bridge the gap.

---

## Root Cause Analysis (2026-02-21, session 1)

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

## Changes Made (2026-02-21, session 1)

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
