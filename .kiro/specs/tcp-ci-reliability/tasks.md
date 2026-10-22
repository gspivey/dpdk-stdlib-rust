# Tasks — tcp-ci-reliability

These tasks map to ROADMAP items that the agent-router dpdk cron implements one at a time.

**Important — "iterate until green" SOP exception:**
Because all three TCP bugs fail in the same CI suite, a single-bug PR cannot
reach green unless the known-failures gate (Task 0) is already in place.
The agent MUST check `ci/known-failures.txt`: if the test it is fixing is listed
there, CI is allowed to fail for the OTHER listed tests. A PR is green when:
- Its own target test is no longer listed (was removed in this PR) AND passes
- All OTHER listed tests fail as expected (the xfail rules pass)
- No unlisted test fails

---

## Task 0 — Known-failures gate

*Lands as ROADMAP item 39a or as a prerequisite commit in the item 39 PR.*

- [x] Add `ci/known-failures.txt` with the three initial entries (see design §Group 0)
- [x] Add the known-failures check step to `.github/workflows/tcp-integration-tests.yml`
  (or the summary script it calls): listed+FAIL → ok; listed+PASS → CI fail;
  listed+NOT-RUN → CI fail
- [x] Add the ratchet step: PR adds lines → CI fail
- [x] Verify CI goes green on a branch where all three tests are failing
  (the xfail list covers all three)
- *Requirements: 1.1, 1.2, 1.3, 1.4, 1.5*

---

## Task 1 — RFC 6191 TIME_WAIT (engine.rs)

*Lands as a standalone ROADMAP item. Requires Task 0 to already be merged.*

- [ ] Add RST handling in `handle_time_wait`: remove TCB on RST (Req 2.3)
- [ ] Add SYN handling in `handle_time_wait`:
  - ISN > rcv_nxt → `handle_syn_with_isn_floor(seg, four_tuple, old_snd_nxt)`
  - ISN ≤ rcv_nxt → re-ACK (triggers RST→SYN retransmit from peer)
  (Req 2.1, 2.2)
- [ ] Add `handle_syn_with_isn_floor` (or extend `handle_syn`) to accept an
  optional `isn_floor: Option<SeqNum>` and enforce `seq_gt(iss, floor)` on the
  generated ISN (Req 2.1)
- [ ] Verify or add `handle_syn_sent` unacceptable-ACK path: RST(seq=SEG.ACK)
  + stay in SYN_SENT (Req 2.4)
- [ ] Add unit tests (no EAL, no EC2):
  - `time_wait_syn_higher_seq_opens_new_connection`
  - `time_wait_syn_isn_floor_applied`
  - `time_wait_syn_lower_seq_reacks_and_stays`
  - `time_wait_rst_removes_tcb`
  - `time_wait_syn_seq_compare_wraps` (rcv_nxt near u32::MAX)
  - `syn_sent_unacceptable_ack_sends_rst_and_retries`
  - `reconnect_same_tuple_20_iterations` (two engines with different secrets,
    20 sequential reconnects on same 4-tuple — proves the coin-flip is covered)
- [ ] Remove `bidir_multi` from `ci/known-failures.txt` in this PR
- [ ] Verify CI: `bidir_echo` and `bidir_multi` both pass; harness logs show
  `tw_syn_reopened > 0` or `tw_rst_closed > 0` (proving the TIME_WAIT path ran)
- *Requirements: 2.1, 2.2, 2.3, 2.4, 2.5*

---

## Task 2 — Kernel client bind-ip

*Lands as a standalone ROADMAP item. Requires Task 0 to already be merged.*

- [ ] Add `--bind-ip <IPv4>` to `tcp-test-client` using `tokio::net::TcpSocket`
  (bind before connect); print `local_addr=<ip>:<port>` after connecting (Req 3.1)
- [ ] Verify default (no `--bind-ip`) is unchanged (Req 3.2)
- [ ] Update Phase C `kernel_to_dpdk_echo` invocation in
  `scripts/run-integration-tests.sh` to pass `--bind-ip "$ETH1_IP"` (Req 3.3)
- [ ] Add unit tests (no root, loopback only):
  - `bind_ip_arg_parsed`
  - `bind_ip_sets_source_addr` (listener on 127.0.0.1; client --bind-ip 127.0.0.2;
    accepted peer addr == 127.0.0.2)
  - `bind_ip_unassigned_fails_cleanly`
- [ ] Remove `kernel_to_dpdk_echo` from `ci/known-failures.txt` in this PR
- [ ] Verify CI: `kernel_to_dpdk_echo` passes; logs contain
  `local_addr=$ETH1_IP:<port>`
- *Requirements: 3.1, 3.2, 3.3, 3.4*

---

## Task 3 — vfio-pci bind wait

*Lands as a standalone ROADMAP item. Ideally merged before Tasks 1 and 2
since a port-init failure masks the other tests' results.*

- [ ] Add `wait_for_dpdk_bind()` to the harness bind helper (see design §Group 3):
  polls driver symlink + `/dev/vfio/noiommu-*` node, 60s timeout, clear error (Req 4.1, 4.2)
- [ ] Call `wait_for_dpdk_bind "$DPDK_BDF"` after every `dpdk-devbind.py -b vfio-pci`
  invocation in `scripts/run-integration-tests.sh` (Req 4.4)
- [ ] Harden the DPDK binary port-init: check `rte_eth_dev_count_avail() == 0`
  and print `eal_ports=0` with a driver-binding hint before panicking (Req 4.3)
- [ ] Remove the Task 3 entry from `ci/known-failures.txt` in this PR
  (update the entry name once the exact failing test/phase name is confirmed in CI)
- [ ] Verify CI: no "Invalid port ID" in any log; harness prints
  `bind_wait: BDF=<bdf> driver=vfio-pci group=<n> ok` before each binary launch
- *Requirements: 4.1, 4.2, 4.3, 4.4*
