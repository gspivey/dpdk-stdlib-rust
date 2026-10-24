# Requirements — tcp-ci-reliability

## Introduction

Three TCP integration-test failures were root-caused after PR #122 merged:

- **Bug 1 — RFC 6191 TIME_WAIT SYN drop**: `bidir_multi` times out because
  `handle_time_wait` silently drops any packet that isn't a FIN, including a new
  SYN on the same 4-tuple after the previous connection enters TIME_WAIT.

- **Bug 2 — asymmetric routing in kernel_to_dpdk_echo**: the kernel client sends
  from eth0 (management IP) but the DPDK receiver's SYN-ACK returns to eth1 (the
  data-plane IP); `rp_filter=1` drops it.

- **Bug 3 — Port init race on rebind**: after Phase B→C ENI rebind the DPDK port
  init fires before vfio-pci attachment completes, producing
  "Port init failed: Invalid port ID: 0".

Each fix is a separate ROADMAP item. Because CI runs all three tests in the same
suite, a fix PR for one bug cannot go green while the other two bugs still fail.
The `ci/known-failures.txt` mechanism (Requirement 1) makes the "iterate until
green" SOP loop work with independent items.

## Requirement 1 — Known-failures gate (prerequisite)

**User story:** As the agent-router agent implementing one TCP fix at a time, I
want CI to pass when only the bugs I have NOT yet fixed are failing, so each
single-bug PR can reach green independently.

### Acceptance criteria

1. WHEN `ci/known-failures.txt` exists in the repo THEN the
   `tcp-integration-tests` CI job SHALL treat any test whose name is listed
   there as an expected failure and SHALL NOT count it against the pass/fail
   result.
2. WHEN a listed test passes unexpectedly THEN CI SHALL fail with the message
   "unexpected pass: remove <test> from ci/known-failures.txt", so a fixed bug
   forces its own line to be deleted.
3. WHEN a listed test does not execute at all THEN CI SHALL fail with "known
   failure did not run: <test>", so the xfail list cannot mask a test that was
   accidentally skipped.
4. WHEN a PR adds lines to `ci/known-failures.txt` compared with the base branch
   THEN CI SHALL fail, so no fix PR may introduce new xfail entries.
5. WHERE `ci/known-failures.txt` contains a line it SHALL contain the exact test
   name followed by an inline comment naming the ROADMAP task that removes it,
   e.g. `bidir_multi  # Task 1 — RFC 6191`.

## Requirement 2 — RFC 6191 SYN handling in TIME_WAIT

**User story:** As a caller running sequential TCP connections over the same
4-tuple, I want a new SYN that arrives while the prior connection is in
TIME_WAIT to open a new connection rather than being silently dropped.

### Acceptance criteria

1. WHEN a SYN (not ACK) arrives for a 4-tuple in TIME_WAIT AND the SYN's
   sequence number is greater than `tcb.rcv_nxt` (wrapping comparison) THEN the
   engine SHALL remove the TIME_WAIT TCB and begin a new connection, with the
   new ISN constrained to be greater than the old `snd_nxt`.
2. WHEN a SYN arrives for a 4-tuple in TIME_WAIT AND the SYN's sequence number
   is NOT greater than `rcv_nxt` THEN the engine SHALL emit a plain ACK (to
   trigger a RST from the peer) and SHALL NOT remove the TCB or open a
   connection.
3. WHEN a RST arrives for a 4-tuple in TIME_WAIT THEN the engine SHALL remove
   the TCB immediately (RFC 793; `tcp_rfc1337=0` behaviour).
4. WHEN a SYN in SYN_SENT receives an unacceptable ACK (ack field != snd_nxt)
   THEN the engine SHALL reply with RST(seq = SEG.ACK) and SHALL remain in
   SYN_SENT so the retransmit timer can resend the SYN.
5. WHEN `bidir_multi` runs sequentially on the same 4-tuple as `bidir_echo`
   THEN both tests SHALL complete within their timeout with no silent drop.

## Requirement 3 — Kernel client source IP for kernel_to_dpdk_echo

**User story:** As the CI harness running `kernel_to_dpdk_echo`, I want the
kernel-side TCP client to bind its socket to the data-plane IP (eth1) so that
DPDK's reply can reach it via the source-based routing rule already in place.

### Acceptance criteria

1. WHEN `tcp-test-client` is invoked with `--bind-ip <IPv4>` THEN it SHALL bind
   its socket to that address before calling connect, so the OS chooses it as
   the packet source.
2. WHEN `tcp-test-client` is invoked without `--bind-ip` THEN it SHALL behave
   identically to the current default (bind to 0.0.0.0:0), preserving backward
   compatibility.
3. WHEN the Phase C harness runs `kernel_to_dpdk_echo` THEN it SHALL pass
   `--bind-ip "$ETH1_IP"` to `tcp-test-client`, where `ETH1_IP` is the
   kernel-owned IP on the same subnet as the DPDK ENI.
4. WHEN `kernel_to_dpdk_echo` completes THEN the test log SHALL contain
   `local_addr=<ETH1_IP>:<port>` so CI can verify the correct interface was used.

## Requirement 4 — DPDK port init waits for vfio-pci attachment

**User story:** As the CI harness transitioning between phases, I want the ENI
bind step to confirm the device is fully attached to vfio-pci before launching
any DPDK binary, so that port 0 is always valid when EAL initialises.

### Acceptance criteria

1. WHEN a `dpdk-devbind.py -b vfio-pci` call completes THEN the bind helper
   SHALL poll until the BDF's driver symlink resolves to `vfio-pci` AND the
   corresponding `/dev/vfio/noiommu-*` node exists, before returning.
2. WHEN the poll timeout is exceeded (60 s) THEN the bind helper SHALL exit
   non-zero and print the current driver binding, so CI fails loudly instead of
   launching a binary that will error.
3. WHEN EAL initialises with zero available ports THEN the DPDK binary SHALL
   print `eal_ports=0` and exit with a clear error referencing driver binding,
   rather than panicking on "Invalid port ID: 0".
4. WHEN a Phase B or Phase C test begins THEN CI SHALL contain the line
   `bind_wait: BDF=<bdf> driver=vfio-pci group=<n> ok` in the harness output.
