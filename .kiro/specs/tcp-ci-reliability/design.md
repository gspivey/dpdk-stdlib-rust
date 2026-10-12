# Design — tcp-ci-reliability

## Overview

Four independent changes, each delivered as a separate ROADMAP item:

| Group | File(s) | Req |
|---|---|---|
| 0 — known-failures gate | `ci/known-failures.txt`, `tcp-integration-tests` CI job | 1 |
| 1 — RFC 6191 TIME_WAIT | `dpdk-stdlib-tcp/src/engine.rs` | 2 |
| 2 — kernel client bind-ip | `dpdk-stdlib-tcp-apps/tcp-test-client/src/main.rs`, `scripts/run-integration-tests.sh` | 3 |
| 3 — vfio-pci bind wait | `scripts/run-integration-tests.sh` (bind helper) | 4 |

## Group 0 — Known-failures gate (Req 1)

Add `ci/known-failures.txt` with the initial three entries:

```
bidir_multi           # Task 1 — RFC 6191
kernel_to_dpdk_echo   # Task 2 — bind-ip
port_init_race        # Task 3 — vfio-pci wait   [verify: actual test/phase name in CI]
```

In `.github/workflows/tcp-integration-tests.yml` (or the summary script it calls),
after the test run, add a check step:

```bash
KNOWN="ci/known-failures.txt"
while IFS='#' read -r test _; do
  test="${test// /}"
  [ -z "$test" ] && continue
  if grep -q "^PASS: $test$" test-results.txt; then
    echo "::error::unexpected pass: remove '$test' from $KNOWN"; FAIL=1
  elif ! grep -qE "^(PASS|FAIL): $test$" test-results.txt; then
    echo "::error::known failure did not run: $test"; FAIL=1
  fi
done < "$KNOWN"
[ "${FAIL:-0}" -eq 1 ] && exit 1
```

Ratchet: add a CI step before the run:

```bash
NEW=$(git diff "origin/$GITHUB_BASE_REF" -- ci/known-failures.txt | grep '^+[^+]' | wc -l)
if [ "$NEW" -gt 0 ]; then
  echo "::error::PRs may not add lines to known-failures.txt"; exit 1
fi
```

## Group 1 — RFC 6191 TIME_WAIT (Req 2)

### Root cause

`engine.rs handle_time_wait` (lines ~1557–1578) handles only FIN retransmits.
Any other packet — including a SYN for a new incarnation — is silently dropped:

```rust
fn handle_time_wait(...) -> Vec<Vec<u8>> {
    ...
    if seg.flags.contains(TcpFlags::FIN) { ... }
    Vec::new()  // SYN reaches here and is dropped
}
```

### Why ISN comparison alone is insufficient

The ISN generator (`isn.rs`) uses a **per-process** random 128-bit secret + clock.
Two separate `tcp-test-client` processes (one per test run) each draw a fresh
secret, so the hash component `F(4-tuple, secret)` changes by a random 32-bit
offset. The second client's ISN vs the server's TIME_WAIT `rcv_nxt` is
approximately a coin flip. The fix therefore implements the full recovery sequence:

1. ISN > rcv_nxt → open new connection (RFC 6191 / RFC 1122 §4.2.2.13)
2. ISN ≤ rcv_nxt → re-ACK (triggers RST from the client's SYN_SENT state)
3. RST in TIME_WAIT → remove TCB (RFC 793, `tcp_rfc1337=0`)
4. After step 3, client retransmits SYN, which now hits the LISTEN path cleanly

### Changes to `engine.rs handle_time_wait`

Insert before the existing FIN branch:

```rust
let rcv_nxt = tcb.rcv_nxt;
let old_snd_nxt = tcb.snd_nxt;

// RST terminates TIME_WAIT (RFC 793; tcp_rfc1337=0)
if seg.flags.contains(TcpFlags::RST) {
    self.tcbs.remove(four_tuple);
    return Vec::new();
}

if seg.flags.contains(TcpFlags::SYN) && !seg.flags.contains(TcpFlags::ACK) {
    if seq_gt(seg.seq, rcv_nxt) {  // seq_gt = wrapping greater-than in seq.rs
        // RFC 6191: new incarnation — reap TW TCB, hand SYN to listener
        // with ISN floor so new ISN > old snd_nxt
        self.tcbs.remove(four_tuple);
        return self.handle_syn_with_isn_floor(seg, four_tuple, old_snd_nxt);
    }
    // Old or colliding SYN: re-ACK to trigger RST from SYN_SENT
    return match build_ack_for_tcb(tcb) {
        Ok(f) => vec![f],
        Err(_) => Vec::new(),
    };
}
```

### ISN floor in handle_syn / IsnGenerator

When `handle_syn_with_isn_floor` is called, it passes an `Option<SeqNum>` floor
to `IsnGenerator::generate_with_floor`. If the generated ISN is not
`seq_gt(isn, floor)`, use `floor.wrapping_add(65536)` instead.

### SYN_SENT unacceptable-ACK path (Req 2.4)

Verify that `handle_syn_sent` already handles an ACK whose `ack != snd_nxt` by
sending RST(seq = SEG.ACK) and staying in SYN_SENT. If that path is missing, add it.

## Group 2 — Kernel client bind-ip (Req 3)

### Root cause

`kernel_to_dpdk_echo` runs in Phase C: receiver is on kernel (eth1 IP), sender is
on DPDK. The kernel test client opens a `TcpStream::connect` without binding,
so the OS picks eth0 (management IP 10.0.1.x) as source. The DPDK receiver's
SYN-ACK returns to the eth1 subnet, `rp_filter=1` drops it.

The `ip rule add from $ETH1_IP table 200` in the harness only helps if the
socket is already bound to ETH1_IP before connecting.

### Changes

**`tcp-test-client/src/main.rs`**: add `--bind-ip <IPv4>` argument using
`socket2` or `tokio::net::TcpSocket`:

```rust
// --bind-ip provided:
let socket = TcpSocket::new_v4()?;
socket.bind(SocketAddr::new(bind_ip, 0))?;
let stream = socket.connect(server_addr).await?;
println!("local_addr={}", stream.local_addr()?);
```

**`scripts/run-integration-tests.sh`** Phase C `kernel_to_dpdk_echo` invocation:
pass `--bind-ip "$ETH1_IP"` where `ETH1_IP` is the kernel-visible IP on the
data-plane interface (already known from the ENI setup).

## Group 3 — vfio-pci bind wait (Req 4)

### Root cause

After `dpdk-devbind.py -b vfio-pci`, the PCI device appears as vfio-pci in
`/sys/bus/pci/devices/<BDF>/driver` a short time before the
`/dev/vfio/noiommu-<N>` device node appears. EAL queries the device node during
port init; if it isn't there yet, EAL finds 0 ports and `rte_eth_dev_get_port_by_name`
returns -ENODEV, manifesting as "Invalid port ID: 0". This is intermittent and
Graviton-specific because arm64's IOMMU/SMMU setup takes slightly longer.

### Changes

Add `wait_for_dpdk_bind()` to the harness bind helper:

```bash
wait_for_dpdk_bind() {
  local bdf=$1 timeout=${2:-60}
  local drv grp_node
  for ((i=0; i<timeout; i++)); do
    drv=$(basename "$(readlink -f /sys/bus/pci/devices/$bdf/driver 2>/dev/null)" 2>/dev/null)
    grp_node=$(ls /dev/vfio/noiommu-* 2>/dev/null | head -1)
    if [[ $drv == vfio-pci ]] && [[ -e ${grp_node:-/nonexistent} ]]; then
      echo "bind_wait: BDF=$bdf driver=vfio-pci group=$(basename $grp_node) ok"
      return 0
    fi
    sleep 1
  done
  echo "FATAL: $bdf not bound after ${timeout}s (driver=${drv:-none})" >&2
  return 1
}
```

Call `wait_for_dpdk_bind "$DPDK_BDF"` immediately after every `dpdk-devbind.py -b vfio-pci` call.

Also harden the DPDK binary: add a `select_port` function that returns a clear error
if `rte_eth_dev_count_avail() == 0`, printing `eal_ports=0` and suggesting a driver-binding check.
