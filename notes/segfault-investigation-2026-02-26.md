# DPDK Echo Server Segfault Investigation

**Date:** 2026-02-26
**Investigator:** Kiro AI assistant
**Status:** In progress — root cause narrowed but not yet fixed
**Stack deployed:** DpdkTestStack (still running — DO NOT destroy without confirmation)

## Instance Info

| Role | Instance ID | DPDK ENI IP |
|------|-------------|-------------|
| Sender | i-0da2d7b389ea434e1 | 10.0.1.157 |
| Receiver | i-0efca5fbf07bc69e2 | 10.0.1.138 |

AMI: `ami-0abb530017ca1216f` (dpdk-stdlib-rust-dpdk-22.11.6, created 2026-02-22)
Instance type: c5n.large (2 vCPU, 5.25 GB RAM)
Region: us-east-1

SSM access:
```bash
aws ssm start-session --target i-0efca5fbf07bc69e2 --profile dpdk-test --region us-east-1
```

## Symptom

Both sender and receiver EC2 instances segfault when running the `echo` binary with real DPDK:

```
echo[3572]: segfault at 8 ip 00007f9003438270 ... error 4 in librte_eal.so.23.0
```

Integration test results: **4/6 tests failed** (3/4 tier1, 1/1 tier3-iperf-sends).

## CI Log Source

GitHub Actions run 22270814967, PR #13. The old AMI `ami-0fc9dd8d5e1420af4` was deregistered;
the SSM parameter `/dpdk-stdlib-rust/ami/latest` now points to `ami-0abb530017ca1216f`.

---

## Root Cause Analysis

### 1. Crash Location (confirmed via GDB)

Full backtrace:
```
#0  rte_memory_get_nchannel+16      (librte_eal.so.23)
#1  rte_mempool_calc_obj_size        (librte_mempool.so.23)
#2  rte_mempool_create_empty         (librte_mempool.so.23)
#3  rte_pktmbuf_pool_create_by_ops   (librte_mbuf.so.23)
#4  rte_pktmbuf_pool_create          (librte_mbuf.so.23)
#5  dpdk::mbuf::Mempool::create
#6  dpdk::mbuf::Mempool::create_with_config
#7  dpdk_udp::get_or_init_dpdk
#8  dpdk_udp::UdpSocket::bind
#9  echo::main
```

Crash instruction (disassembled from librte_eal.so):
```asm
rte_memory_get_nchannel:
  sub    $0x8,%rsp
  call   rte_eal_get_configuration    ; returns rte_config* in rax
  mov    0x298(%rax),%rax             ; load mem_config pointer → gets NULL
  mov    0x8(%rax),%eax               ; dereference NULL+8 → SIGSEGV
  add    $0x8,%rsp
  ret
```

**`rte_config->mem_config` is NULL.** The memory configuration pointer was never set or was corrupted.

### 2. Struct Layout (verified — NOT a mismatch)

The `rte_config` struct in DPDK 22.11.6 (`eal_private.h`):
```c
struct rte_config {
    uint32_t main_lcore;                          // offset 0
    uint32_t lcore_count;                         // offset 4
    uint32_t numa_node_count;                     // offset 8
    uint32_t numa_nodes[RTE_MAX_NUMA_NODES];      // offset 12,  128 bytes (32×4)
    uint32_t service_lcore_count;                 // offset 140
    enum rte_lcore_role_t lcore_role[RTE_MAX_LCORE]; // offset 144, 512 bytes (128×4)
    enum rte_proc_type_t process_type;            // offset 656
    enum rte_iova_mode iova_mode;                 // offset 660
    struct rte_mem_config *mem_config;             // offset 664 = 0x298 ✓
};
// Total size: 672 = 0x2a0
```

Confirmed from `nm`:
```
000000000005cc20 00000000000002a0 d rte_config       (size 0x2a0 = 672 ✓)
0000000000061780 0000000000006300 b early_mem_config
```

Build config: `RTE_MAX_LCORE=128`, `RTE_MAX_NUMA_NODES=32` — consistent between headers and library.

### 3. EAL Init Succeeds

EAL output from the echo binary (verbose logging):
```
EAL: Detected CPU lcores: 2
EAL: Detected NUMA nodes: 1
EAL: Detected shared linkage of DPDK
EAL: Multi-process socket /var/run/dpdk/rte/mp_socket
EAL: Selected IOVA mode 'PA'
EAL: VFIO support initialized
EAL: Using IOMMU type 8 (No-IOMMU)
EAL: TSC frequency is ~3000000 KHz
EAL: Main lcore 0 is ready
EAL: Heap on socket 0 was expanded by 2MB
EAL: PCI device 0000:00:06.0 on NUMA socket -1
EAL:   probe driver: 1d0f:ec20 net_ena
EAL: VFIO reports MSI-X BAR as mappable
EAL:   PCI memory mapped at 0x1100800000
EAL:   PCI memory mapped at 0x1100804000
EAL: Probe PCI driver: net_ena (1d0f:ec20) device: 0000:00:06.0 (socket -1)
EAL: Heap on socket 0 was expanded by 2MB
TELEMETRY: No legacy callbacks, legacy socket not created
```

`rte_eal_init()` returns 4 (success). Memory is allocated. PCI device is probed and mapped.
The crash happens AFTER EAL init, when `rte_pktmbuf_pool_create()` is called.

### 4. System State (verified — all healthy)

| Check | Result |
|-------|--------|
| Hugepages | 1024 × 2MB allocated, all free |
| Hugetlbfs mount | `/dev/hugepages` and `/mnt/huge` both mounted |
| vfio modules | vfio_pci, vfio_pci_core, vfio_virqfd, vfio_iommu_type1 loaded |
| ENI binding | 0000:00:06.0 bound to vfio-pci successfully |
| DPDK runtime files | `/var/run/dpdk/rte/config`, fbarray files, hugepage_info all present |
| Binary linkage | Real DPDK shared libs from `/usr/local/lib/` |

### 5. Differential Testing

| Test | Linkage | Result |
|------|---------|--------|
| `dpdk-testpmd -l 0 -n 4` | Static | ✅ Works (exits with "no cores for forwarding" but no crash) |
| C test: `rte_eal_init` + `rte_pktmbuf_pool_create` | Shared (all DPDK libs via pkg-config) | ✅ Works |
| C test: same params as Rust (pool "udp_pool", 8192 entries, data_room=0, socket=0) | Shared | ✅ Works |
| C test: argv freed after `rte_eal_init` (replicating Rust drop behavior) | Shared | ✅ Works |
| C test: no NULL terminator on argv | Shared | ✅ Works |
| Rust `echo` binary | Shared (subset of DPDK libs) | ❌ SIGSEGV |

**All C tests pass. Only the Rust binary crashes.** The issue is Rust-specific.

### 6. Ruled Out

- ❌ Hugepage allocation failure — 1024 hugepages available, DPDK maps them
- ❌ Stale DPDK runtime state — crash persists after `rm -rf /var/run/dpdk/rte/*`
- ❌ Missing NULL terminator on argv — C test without NULL terminator works
- ❌ argv lifetime (use-after-free) — C test with freed argv works
- ❌ Struct layout mismatch — offset 0x298 is correct for RTE_MAX_LCORE=128 + numa_nodes[32] + iova_mode
- ❌ Symbol conflict — `nm` shows no `rte_memory` or `rte_eal_get_config` symbols in echo binary
- ❌ Missing DPDK libraries — echo links against MORE libs than the working C test
- ❌ DPDK shared library bug — C test with shared linkage works perfectly
- ❌ Pre-built AMI issue — fresh deploy with current AMI reproduces the crash

---

## Remaining Hypotheses (in order of likelihood)

### H1: Rust binary corrupts `rte_config.mem_config` between EAL init and mempool creation

Something in the Rust runtime or our code overwrites the `mem_config` pointer (at a fixed address in librte_eal.so's data segment) between `rte_eal_init()` returning and `rte_pktmbuf_pool_create()` being called.

**Next step:** Use GDB to set a watchpoint on `rte_config.mem_config` (address = `rte_config + 0x298`) and see what writes NULL to it.

```bash
# In GDB:
# 1. Find rte_config address
call (long)rte_eal_get_configuration()
# 2. Set watchpoint on mem_config field
watch *(long*)($result + 0x298)
# 3. Continue and see what triggers the watchpoint
continue
```

### H2: Rust linker (lld) handles DPDK shared library initialization differently

The Rust toolchain may use `lld` instead of `ld`, which could handle library constructor ordering or symbol resolution differently. DPDK relies on `__attribute__((constructor))` functions in shared libraries for driver registration and initialization.

**Next step:** Check which linker the Rust build uses and try forcing `ld`:
```bash
RUSTFLAGS="-C linker=gcc" cargo build --release --features dpdk-sys/bindgen,echo/dpdk
```

### H3: Rust's global allocator interferes with DPDK's memory management

Rust's default allocator (system malloc) and DPDK's internal allocator both use the same underlying glibc malloc. If Rust makes allocations that happen to land in memory DPDK expects to control, it could corrupt state.

**Next step:** Try running with `MALLOC_CHECK_=3` to enable glibc malloc debugging.

### H4: The `dpdk_shim.c` compiled object introduces a conflicting symbol or initialization

The shim is compiled by `cc` and linked into the Rust binary. If it introduces any global state or constructor that conflicts with DPDK's own initialization, it could cause issues.

**Next step:** Check if the shim object has any constructor functions:
```bash
objdump -d target/release/build/dpdk-sys-*/out/libdpdk_shim.a | grep -i 'init\|constructor'
```

---

## Recommended Next Steps

1. **GDB watchpoint on `mem_config`** — This is the most direct way to find what zeroes the pointer:
   ```bash
   sudo -i
   cd /opt/dpdk-stdlib
   rm -rf /var/run/dpdk/rte/*
   gdb --args target/release/echo --ip 10.0.1.138 --port 9000
   # In GDB:
   break rte_eal_init
   run
   finish
   # rte_eal_init has returned, get rte_config address:
   set $cfg = (long)rte_eal_get_configuration()
   print/x $cfg
   # Set hardware watchpoint on mem_config field:
   watch *(long*)($cfg + 0x298)
   continue
   # GDB will stop when mem_config is written to NULL
   bt
   ```

2. **Try building with GCC linker** — to rule out lld-specific behavior:
   ```bash
   cd /opt/dpdk-stdlib
   RUSTFLAGS="-C linker=gcc" PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
     cargo build --release --features dpdk-sys/bindgen,echo/dpdk
   timeout 5 target/release/echo --ip 10.0.1.138 --port 9000
   ```

3. **Minimal Rust reproducer** — Write a tiny Rust program that just calls `rte_eal_init` + `rte_pktmbuf_pool_create` via FFI, without the full dpdk/dpdk-udp crate stack. If it crashes, the issue is in the FFI layer. If it works, the issue is in our crate code.

4. **Check for DPDK global variable shadowing** — Verify no Rust crate accidentally defines a symbol that shadows a DPDK global:
   ```bash
   nm target/release/echo | grep -i 'rte_config\|early_mem\|mem_config'
   ```

---

## Files & Tools on the Instance

| Path | Description |
|------|-------------|
| `/opt/dpdk-stdlib/` | Project source + release build |
| `/opt/dpdk-stable-22.11.6/` | DPDK source tree |
| `/usr/local/lib/librte_*.so` | DPDK shared libraries |
| `/usr/local/include/rte_*.h` | DPDK headers (note: NOT in a `dpdk/` subdirectory) |
| `/usr/local/bin/dpdk-testpmd` | DPDK test app (statically linked, works) |
| `/tmp/test_dpdk` | C test program (shared linkage, works) |
| `/tmp/test_dpdk2` | C test with no NULL argv terminator (works) |
| `/tmp/test_dpdk3` | C test with freed argv (works) |
| `/tmp/gdb_full.txt` | GDB command script |
| `gdb` | Installed via `dnf install -y gdb` |

## Teardown

When done debugging, destroy the stack:
```bash
cd ~/Development/dpdk-stdlib/deploy/cdk
npx cdk destroy DpdkTestStack --profile dpdk-test --force
```
