# Implementation Plan: QUIC + TCP EC2 Integration & Performance Tests

## Overview

This plan wires the existing QUIC Tier 1 harness into the shared EC2 orchestrator,
adds QUIC integration and performance CI, and defines/executes the gates that graduate
TCP and QUIC integration tests from non-blocking to blocking.

All changes are **additive** and must not modify the QUIC/TCP harness scripts, the CDK
stack, or the existing `integration-tests` / `tcp-integration-tests` jobs beyond the
defined `continue-on-error` gate removals.

Validation commands (from AGENTS.md):
- Local shell syntax: `bash -n scripts/run-integration-tests.sh`
- Full local checks: `cargo build && cargo test`
- Integration validation: `./scripts/ci-validate.sh` (or `--skip-integration` for
  non-networking changes)

## Tasks

- [ ] 1. Add QUIC cert-transfer SSM helpers to the orchestrator
  - Add `ssm_run_get_output <instance_id> <timeout> <cmd>` to `scripts/run-integration-tests.sh`
    alongside existing SSM helpers, using argument order `(instance_id, timeout, command)` to
    match the orchestrator's own `ssm_run_command` / `ssm_run_command_async` convention.
    NOTE: the source at `run-quic-perf.sh:175` uses the OPPOSITE order `(instance_id, command, timeout)` --
    this is a DELIBERATE REORDERING; do not copy the signature verbatim. Port only the
    send+wait+get-stdout logic. Remote commands passed to this helper MUST always exit 0
    (append `|| true` or `|| echo MISSING`) to suppress false SSM failure log entries while polling.
    The cert read doubles as the readiness check (cert only appears after the server logs `QUIC_SERVER_READY`)
  - Add a write step that carries the base64 string in a shell variable and runs
    `printf '%s' '<b64>' | base64 -d > <path>` on the client for byte-exact PEM transfer
  - Place both helpers alongside the existing SSM helpers in
    `scripts/run-integration-tests.sh`; do not alter existing helper behavior
  - _Requirements: 2.2, 2.3, 2.5, 10.4_

- [ ] 2. Implement `quic_transfer_cert` in the orchestrator
  - Add `quic_transfer_cert <suite>` that bounded-polls the Server_Instance (receiver)
    via a separate SSM `base64 -w 0 /tmp/quic-server-cert.pem` command until it returns a
    non-empty string (up to ~60s). Readiness IS this cert poll — do NOT add a separate
    "wait for `QUIC_SERVER_READY`" step, because the cert is only written after that log
  - Write the decoded cert to the Client_Instance (sender) at the same path via
    `printf '%s' '<b64>' | base64 -d > /tmp/quic-server-cert.pem` over SSM
  - On empty/unavailable cert or failed write, call `generate_failure_xml "$suite"`
    with a cert-transfer message and return 1
  - Ensure the transfer occurs after a non-empty cert is observed and before the client
    is invoked
  - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6_

- [ ] 2a. Implement `quic_verify_build` in the orchestrator
  - Add `quic_verify_build <suite>` that verifies the QUIC binaries via two separate
    SSM calls: check `quic-echo-server` on the RECEIVER_INSTANCE (receiver runs the server)
    and `quic-test-client` on the SENDER_INSTANCE (sender runs the client). Each call uses
    `ssm_run_get_output` with `test -x <bindir>/<binary> && echo OK || echo MISSING`
    (always exits 0 to suppress false failure logs)
  - On a missing binary, call `generate_failure_xml "$suite"` and return 1
  - Do NOT modify the existing UDP `verify_build`; this is a separate QUIC-only check
    that runs only when `TIER_FILTER=quic`
  - _Requirements: 2.7, 10.5_

- [ ] 3. Implement `run_quic_tier1` in the orchestrator
  - Define the SSM timeout constants `QUIC_SERVER_TIMEOUT=300` (async server command) and
    `QUIC_CLIENT_TIMEOUT=240` (synchronous client command)
  - Add `run_quic_tier1` modeled on `_run_tcp_dpdk_pair`: first call
    `quic_verify_build "tier1-quic-handshake"` (return on failure), then bind DPDK ENI on
    both instances, `warm_arp_cache`, start `tier1-quic-handshake.sh --role server
    --bind-ip $RECEIVER_DPDK_ENI_IP --port 4433` as an async SSM command with
    `QUIC_SERVER_TIMEOUT`
  - Call `quic_transfer_cert "tier1-quic-handshake"`; on failure, cancel the server
    command and return
  - Run `tier1-quic-handshake.sh --role client --bind-ip $SENDER_DPDK_ENI_IP --peer-ip
    $RECEIVER_DPDK_ENI_IP --port 4433 --output $RESULTS_REMOTE_DIR/tier1-quic-handshake.xml`
    as a synchronous SSM command with `QUIC_CLIENT_TIMEOUT`; on failure call
    `generate_failure_xml`
  - Wait for the server command with `ssm_wait_command`; cancel with
    `ssm_cancel_command` if still running
  - On ENI bind failure (either instance), emit `generate_failure_xml` and return 1
  - Do not modify any existing tier function
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8, 1.9, 2.7, 10.1, 10.5_

- [ ] 4. Add the `--tier quic` filter and dispatch
  - Add `quic` to the `--tier` validation `case` list in the argument parser
  - Update the invalid-tier error message to list `quic` among valid values
  - In `main()`, add a dispatch block `if [[ "$TIER_FILTER" == "quic" ]]; then
    run_quic_tier1 || true; fi` (quic-only; not part of run-all)
  - Verify QUIC does not run in run-all mode (no `-z "$TIER_FILTER"` branch)
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 10.3_

- [ ] 5. Validate the orchestrator changes locally
  - Run `bash -n scripts/run-integration-tests.sh` for syntax
  - Dry-run/trace `run_quic_tier1` with SSM/ENI helpers stubbed to confirm the sequence:
    bind → warm → server(async) → cert transfer → client(sync) → wait/cancel
  - Confirm `--tier quic` selects only `run_quic_tier1`, and run-all still excludes QUIC
  - Confirm existing tier functions are byte-unchanged
  - _Requirements: 1.8, 3.2, 3.3, 10.1, 10.3_

- [ ] 6. Checkpoint — Orchestrator wiring complete
  - Ensure orchestrator syntax passes and QUIC dispatch is correct. Ask the user if
    questions arise.

- [ ] 7. Create the QUIC integration CI workflow
  - Create `.github/workflows/quic-integration-tests.yml` as a structural clone of the
    `tcp-integration-tests` job, including a `validate-cdk` pre-flight
  - Set `concurrency.group: integration-tests` with `cancel-in-progress: false`
  - Triggers: `pull_request` to `main` and `development`, plus `workflow_dispatch`
  - Steps: checkout, Node 20 + aws-cdk + CDK deps, Session Manager plugin, AWS creds via
    `secrets.AWS_ROLE_ARN`, DPDK AMI lookup from SSM (`/dpdk-stdlib-rust/ami/latest`),
    PR-number resolution
  - Run step: `./scripts/run-integration-tests.sh --teardown --json-summary --tier quic`
  - Set `timeout-minutes: 55` on the `quic-integration-tests` job and gate it with
    `if: needs.validate-cdk.result == 'success'` (do NOT prefix with `always()`)
  - Set `continue-on-error: true` on the `quic-integration-tests` job and
    `fail-on-error: false` on the `dorny/test-reporter` step
  - Add PR-comment step (pass/fail/skip counts + app logs, marked non-blocking), artifact
    uploads (`quic-integration-test-results` → `test-results/`, `quic-instance-logs` →
    `instance-logs/`), and a `failure()` safety-net `cdk destroy DpdkTestStack --force`
  - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5, 4.6, 4.7, 4.8, 5.1, 5.2, 5.3, 5.4, 10.2_

- [ ] 8. Validate the QUIC integration workflow
  - Lint the workflow YAML
  - Confirm the concurrency group is `integration-tests`, the run command uses
    `--tier quic`, `continue-on-error: true` is present, and the safety-net teardown
    targets `DpdkTestStack`
  - Confirm the QUIC workflow introduces no new CDK stack
  - _Requirements: 4.3, 4.4, 4.7, 5.1, 5.4_

- [ ] 9. Checkpoint — QUIC integration CI in place (first live run is the validation)
  - The first `quic-integration-tests` run is expected to be the end-to-end validation;
    it runs non-blocking. Ask the user before triggering if credentials/infra approval
    is needed.

- [ ] 10. Create the QUIC performance CI workflow
  - Create `.github/workflows/quic-perf-tests.yml` as a `workflow_dispatch` workflow that
    invokes the existing QUIC perf dispatch via `scripts/run-perf-tests.sh` (do NOT
    re-implement perf logic; `run-perf-tests.sh` delegates `quic-native-dpdk-nic` to
    `run-quic-perf.sh`)
  - Trigger: `workflow_dispatch` with inputs `duration`, `streams`, `payload`, `port`
    (defaults 30 / 8 / 65536 / 4433) and a `teardown` toggle; map them onto the env vars
    `QUIC_PERF_DURATION`, `QUIC_PERF_STREAMS`, `QUIC_PERF_PAYLOAD`, `QUIC_PERF_PORT` that
    `run-quic-perf.sh` actually reads
  - Run step: use a `TEARDOWN_FLAG` conditional (matching `perf-tests-tcp.yml:141-149`):
    `$([[ "${{ inputs.teardown }}" == 'true' ]] && echo '--teardown' || echo '--no-teardown')`,
    then `./scripts/run-perf-tests.sh "$TEARDOWN_FLAG" --configs quic-native-dpdk-nic --json-summary`
    (do NOT pass `--payload-sizes` / `--streams` / extra flags to a QUIC-specific script)
  - `validate-cdk` job synthesizes `DpdkTestStack` (`npx cdk synth DpdkTestStack
    --quiet`); perf job `needs: validate-cdk`
  - AWS creds via `secrets.AWS_ROLE_ARN`; resolve DPDK AMI from SSM
    (`/dpdk-stdlib-rust/ami/latest`)
  - Upload `quic-perf-results` (`perf-results/`, 90-day retention) and
    `quic-perf-instance-logs` (`instance-logs/`, 30-day retention); add `failure()`
    safety-net teardown `npx cdk destroy DpdkTestStack --force`
  - Pin the stack/concurrency explicitly (do NOT mirror `perf-tests-tcp.yml`'s
    `PerfTestStack` / `perf-tests-tcp` group): `concurrency.group: integration-tests`,
    synth target `DpdkTestStack`, safety-net destroy target `DpdkTestStack`
  - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.5, 8.6, 8.7_

- [ ] 11. Validate the QUIC performance workflow
  - Lint the workflow YAML
  - Confirm the run step uses the `TEARDOWN_FLAG` conditional and calls
    `./scripts/run-perf-tests.sh "$TEARDOWN_FLAG" --configs quic-native-dpdk-nic --json-summary`,
    and that `QUIC_PERF_*` env vars are wired to the workflow_dispatch inputs
  - Confirm `secrets.AWS_ROLE_ARN` usage, DPDK AMI lookup, artifact retention, and
    safety-net teardown
  - Confirm `concurrency.group: integration-tests`, synth target `DpdkTestStack`, and
    destroy target `DpdkTestStack` (NOT `PerfTestStack`)
  - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.5, 8.6, 8.7_

- [ ] 12. Checkpoint — QUIC integration + perf CI complete
  - Ensure both new workflows lint and follow the TCP patterns. Ask the user if
    questions arise.

- [ ] 13. Verify the `tcp-synthetic-perf` job has no `continue-on-error` gate
  - Grep-check the job: `grep -n -A15 'tcp-synthetic-perf:'
    .github/workflows/integration-tests.yml | grep -n 'continue-on-error'`
  - If no gate is found, nothing to do (expected — this job runs locally with a mock
    backend and already passes reliably)
  - If a `continue-on-error` gate IS found, remove it; change only the gate line(s) and
    leave surrounding logic unchanged
  - _Requirements: 6.5, 6.6, 10.3_

- [ ] 14. Implement/run the Clean_Run counting procedure
  - Document and exercise the agent runbook: scan the newest N runs of the target
    workflow with `gh run list --workflow <file> --json databaseId,conclusion,createdAt`
    and read the job-level conclusion via `gh run view <id> --json jobs`
  - Walk newest-to-oldest: SKIP `cancelled` runs (neither clean nor a break); count a run
    as clean only when its job conclusion is `success` AND its `test-results/*.xml` show
    the expected number of test cases with zero failures (QUIC Tier 1 expects exactly 2:
    `quic_handshake`, `quic_bidir_echo`); a run with 0 tests does NOT count as clean; stop
    at the first `failure` / `startup_failure`
  - Condition is met when the 10 most recent non-skipped conclusions are all clean
  - Apply the procedure to both `tcp-integration-tests` and `quic-integration-tests`
  - _Requirements: 6.1, 6.2, 7.1, 7.2_

- [ ] 15. Evaluate and (if met) remove the TCP integration gate
  - Compute the consecutive Clean_Run count for `tcp-integration-tests`
  - If ≥10, remove `continue-on-error: true` from the `tcp-integration-tests` job and
    flip its `dorny/test-reporter` step to `fail-on-error: true`; change nothing else
  - If <10, leave the gate in place and report the current count
  - _Requirements: 6.1, 6.3, 6.4, 6.6, 10.3_

- [ ] 16. Evaluate and (if met) remove the QUIC integration gate
  - Compute the consecutive Clean_Run count for `quic-integration-tests` (threshold 10,
    matching TCP)
  - If ≥10, remove `continue-on-error: true` from the `quic-integration-tests` job and
    flip its test-reporter step to `fail-on-error: true`
  - If <10, leave the gate in place and report the current count
  - _Requirements: 7.1, 7.2, 7.3, 7.4_

- [ ] 16a. Update ROADMAP.md item 12 gate threshold
  - In the same PR, change ROADMAP.md item 12 from "Remove `continue-on-error: true`
    once 5+ consecutive runs pass" to "once 10 consecutive runs pass" so the roadmap
    matches the spec's 10-run QUIC gate
  - _Requirements: 7.1_

- [ ] 17. Verify TCP and QUIC performance against baselines
  - Only after the corresponding integration gate is removed/eligible, trigger the perf
    workflow and collect the perf-results artifact
  - Compare throughput and P50/P90/P99 latency (and handshake/connection rate where
    applicable) against the relevant entries in `docs/perf-test-log.md`
  - If within normal run-to-run variance, append a new dated entry to
    `docs/perf-test-log.md` in the existing format (git context, config, results,
    analysis)
  - If a regression beyond variance is observed, report it rather than recording it as
    the new baseline
  - _Requirements: 9.1, 9.2, 9.3, 9.4_

- [ ] 17a. Mark `run-quic-integration-tests.sh` as superseded (do NOT delete)
  - Add a header comment to `scripts/run-quic-integration-tests.sh` marking it
    "superseded by `./run-integration-tests.sh --tier quic`; retained for local developer
    convenience"
  - Do NOT delete the script; its cert-transfer logic remains valid as a reference
  - _Requirements: 10.7_

- [ ] 18. Final checkpoint — Full validation
  - Confirm `bash -n scripts/run-integration-tests.sh` passes and QUIC wiring works
  - Confirm both new workflows lint and share/serialize the stack correctly
  - Confirm the existing UDP `integration-tests` and `tcp-integration-tests` jobs are
    unchanged except for any performed gate removal, and run-all still excludes QUIC
  - Run `./scripts/ci-validate.sh` (integration validation) if credentials/infra are
    available; otherwise state what could not be verified
  - Ask the user if questions arise
  - _Requirements: 10.1, 10.2, 10.3, 10.4, 10.5_

- [ ] 19. Update tracking docs on completion (same PR)
  - Update ROADMAP.md item 12 (`dpdk-stdlib-quic`: EC2 integration and performance CI): the
    item is already marked `[x] Complete` with PR #76 (stale — the CI workflow it claims to
    deliver does not exist). Add the new PR number to the item after this feature lands to
    reflect that tasks 15.1/15.2 are now complete.
  - Update `.kiro/specs/s2n-quic-provider/tasks.md`, marking task 15.1 complete (created the
    CI workflow). Do NOT mark 15.2 complete — 15.2 is the in-process loopback benchmark path
    (`quic-stock`/`quic-native-dpdk`) which this spec explicitly excludes from scope.
  - _Requirements: 7.1_

## Notes

- The QUIC and TCP harness scripts, the CDK stack, and the existing UDP/TCP CI jobs are
  treated as correct; changes are additive except for the explicit `continue-on-error`
  gate removals.
- QUIC integration-tier binaries are `quic-echo-server` / `quic-test-client` (port 4433);
  the QUIC perf tier uses `quic-echo-server` (receiver) / `quic-perf-client` (sender) via
  `run-quic-perf.sh` (not `quic-bench`, which is the in-process loopback benchmark only).
- The QUIC perf CI dispatches the existing `run-perf-tests.sh --configs
  quic-native-dpdk-nic` path (which deploys `DpdkTestStack`), pinned to the
  `integration-tests` concurrency group — NOT `perf-tests-tcp.yml`'s `PerfTestStack`.
- Cross-workflow serialization relies on the shared `integration-tests` concurrency
  group, not `needs:`.
- Gate removals (tasks 15, 16) are conditional: perform the edit only when the ≥10
  consecutive Clean_Run condition is met; otherwise report the count and stop.
- Each task references specific requirements for traceability.
