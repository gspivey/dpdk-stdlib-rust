# Integration Test Debugging Log

Structured record of debugging sessions for integration test issues. Each entry records
what was hypothesized, what was tried, what was observed, and what was concluded.

**Read `docs/aws-vpc-networking.md` first** — it contains the root cause and fix for the
primary Tier 1 failure.

---

## 2026-03-01: Tier 1 DPDK→DPDK packets never arrive

### Context
- Sender binds to 10.0.1.166 (secondary ENI, vfio-pci/DPDK), MAC: 02:1c:a0:fb:69:57
- Receiver binds to 10.0.1.251 (secondary ENI, vfio-pci/DPDK), MAC: 02:02:94:8e:a9:6d
- Tier 2 (Kernel→DPDK) passes: 4/4 tests
- Tier 1 (DPDK→DPDK) fails: 4/4 tests

### Hypothesis 1: ARP resolution failing — no reply to broadcast ARP
- **Action**: Added `resolve_arp()` method to `UdpSocket`. Sends ARP request, polls for 3 seconds.
- **Observation**: ARP request sent, no reply received. Falls back to broadcast MAC `ff:ff:ff:ff:ff:ff`.
- **Conclusion**: ARP fails because DPDK port (ENA PMD in no-IOMMU mode) either can't send broadcast
  ARP or can't receive the proxy ARP reply. Broadcast fallback is then dropped by VPC.

### Hypothesis 2: Interior mutability prevents connect() through Arc
- **Action**: Changed `connect()` from `&mut self` to `&self` using `Mutex`/`RwLock` for
  `connected_addr` and `connection_state`.
- **Observation**: Compiles, tests pass. But Tier 1 still fails (this was a separate bug).
- **Conclusion**: Fix correct but orthogonal to the packet delivery issue.

### Hypothesis 3: EAL state leaks between test processes
- **Action**: Added `rm -rf /var/run/dpdk/` cleanup between tests in tier1-dpdk-echo.sh.
- **Observation**: Second test process no longer fails on EAL init.
- **Conclusion**: Fix correct — DPDK shared memory must be cleaned between separate processes.

### Root Cause (identified 2026-03-02)
AWS VPC is **L3-routed, not L2-switched**. There is no real broadcast domain. The correct
approach is to use the **VPC gateway MAC** as the Ethernet destination for all outbound DPDK
frames. The VPC virtual router does L3 forwarding based on dst_ip.

See `docs/aws-vpc-networking.md` for the full explanation and the fix (gateway MAC pre-population).

### Planned Fix
1. Discover gateway MAC from kernel interface (ens5) via arping
2. Pass `--gateway-mac` to test-client and echo server
3. Pre-populate ARP cache: `target_ip → gateway_mac`
4. VPC router forwards based on dst_ip — packets arrive at correct host

---

## 2026-02-28: CDK deployment timing issues

### Context
CloudFormation waited full 20-35 minute timeout before detecting user-data failures.

### Hypothesis: cfn-signal not firing on failure
- **Action**: Added EXIT trap in user-data that always calls cfn-signal with exit code and last 3 lines of log.
- **Observation**: Failures now signal within seconds instead of waiting for timeout.
- **Conclusion**: Fix correct. Also added `validate-cdk` pre-flight job to catch synth-time errors.

---

## 2026-03-05: Tier 3 our-app-sends — only 1 packet sent, then deadlock

### Context
- Tier 3 our-app-sends: DPDK test-client sends to Python kernel echo server
- Test output: "Sent 30 bytes: 'dpdk-to-kernel-test-payload #1'" then "Timeout waiting for response"
- Only 1 of 10 packets sent, process killed by 60s `run_with_timeout`
- Tiers 1, 2, and 3-iperf-sends all passing

### Root Cause 1: Port 0 literal
- DPDK `bind("ip:0")` used port 0 literally in UDP headers (no ephemeral port allocation)
- Python echo server received from port 0, tried `sendto(data, (ip, 0))` — port 0 is invalid
- **Fix**: Added `allocate_ephemeral_port()` using Linux range 32768-60999 with `AtomicU16` counter

### Root Cause 2: Async recv_from deadlock
- `tokio::time::timeout(5s, socket.recv_from())` fires, dropping the future
- But `spawn_blocking` thread keeps running holding the socket's `Mutex` lock forever
- Next `send_to` call deadlocks waiting for the lock — explains why only 1 packet sent
- **Fix**: Set 1-second `set_read_timeout` in spawn_blocking, loop in async wrapper with `yield_now`
  between iterations so the lock is always released within 1 second

### Conclusion
Both fixes needed: ephemeral port ensures responses reach the correct port, and the
deadlock fix ensures the send/recv cycle can repeat. See `docs/ephemeral-ports.md`.

---

## 2026-02-27: test-client using kernel networking instead of DPDK

### Context
test-client was importing `tokio::net::UdpSocket` instead of `dpdk_tokio::compat::tokio::UdpSocket`.

### Hypothesis: Wrong socket type means kernel path
- **Action**: Changed import to `dpdk_tokio::compat::tokio::UdpSocket`, added `dpdk` feature to
  test-client Cargo.toml, added `--bind-ip` CLI arg.
- **Observation**: test-client now uses DPDK when feature is enabled. Confirmed by log output
  "Using DPDK acceleration".
- **Conclusion**: Fix correct, but exposed the Tier 1 ARP/MAC issue (which was masked when
  using kernel networking).
