# Design Document

## Overview

This design wires the existing QUIC Tier 1 harness into the shared EC2 integration
orchestrator, adds a dedicated QUIC CI job that reuses `DpdkTestStack`, defines the
evidence-based procedure for graduating TCP and QUIC integration tests from
non-blocking to blocking, and adds QUIC performance CI mirroring the TCP perf workflow.

The guiding principle is **additive integration**. Every change slots into an existing,
proven pattern:

- The new `run_quic_tier1` orchestrator function follows the exact shape of the TCP
  DPDK↔DPDK pair driver (`_run_tcp_dpdk_pair`), with one QUIC-specific step inserted:
  certificate transfer between the two instances.
- The new `quic-integration-tests.yml` workflow is a near-clone of the
  `tcp-integration-tests` job, differing only in the tier filter, artifact names, and
  PR-comment wording.
- The QUIC perf workflow is a near-clone of `perf-tests-tcp.yml`.
- The `continue-on-error` removal is a mechanical, gated edit driven by a run-history
  count.

No harness script, CDK construct, or existing tier function is modified.

## Architecture

### Component Map

```
scripts/run-integration-tests.sh   (Orchestrator — MODIFIED, additively)
  ├── --tier arg parser            add "quic" to the valid-values case
  ├── run_quic_tier1()             NEW function
  │     ├── configure_eni(bind) x2      (reuse)
  │     ├── warm_arp_cache              (reuse)
  │     ├── ssm_run_command_async ──▶ tier1-quic-handshake.sh --role server
  │     ├── quic_transfer_cert()        NEW helper (poll cert = readiness; SSM read + SSM write)
  │     ├── ssm_run_command ────────▶ tier1-quic-handshake.sh --role client
  │     └── ssm_wait_command / ssm_cancel_command   (reuse)
  └── main() dispatch              add gated `run_quic_tier1` block (quic-only)

scripts/integration-tests/tier1-quic-handshake.sh   (UNCHANGED harness)
dpdk-stdlib-quic/src/bin/quic-echo-server.rs         (UNCHANGED binary)
dpdk-stdlib-quic/src/bin/quic-test-client.rs         (UNCHANGED binary)

.github/workflows/quic-integration-tests.yml   (NEW — clone of tcp-integration-tests)
.github/workflows/quic-perf-tests.yml          (NEW — clone of perf-tests-tcp.yml)
.github/workflows/integration-tests.yml        (MODIFIED — gate removals only)
```

### Where QUIC Runs in the Tier Model

QUIC Tier 1 is DPDK↔DPDK, identical in topology to UDP Tier 1 and TCP Tier 1: both
instances bind their DPDK ENI to vfio-pci, the receiver runs the echo server, and the
sender runs the client. The only structural difference from TCP is the mandatory
certificate handoff, because QUIC's TLS layer requires the client to trust the server's
self-signed certificate.

The orchestrator role mapping is unchanged from the rest of the suite:

| Orchestrator role | Instance variable       | QUIC harness role |
|-------------------|-------------------------|-------------------|
| receiver          | `RECEIVER_INSTANCE_ID`  | `--role server`   |
| sender            | `SENDER_INSTANCE_ID`    | `--role client`   |

### Concurrency and Stack Sharing

All integration jobs that deploy `DpdkTestStack` must never run concurrently, because
they race to create and destroy the same CloudFormation stack. The existing
`integration-tests.yml` already serializes on `concurrency.group: integration-tests`.
The new QUIC integration workflow declares the **same** group so that a QUIC run and a
UDP/TCP run queue behind one another instead of colliding.

```yaml
concurrency:
  group: integration-tests
  cancel-in-progress: false
```

The QUIC perf workflow follows `perf-tests-tcp.yml`: if it reuses `DpdkTestStack` it
shares the `integration-tests` group; if it uses a dedicated perf stack it uses a
perf-specific group. See the Performance CI section.

## Orchestrator Changes

### `run_quic_tier1` Function

Modeled on `_run_tcp_dpdk_pair`, with the QUIC readiness gate and cert transfer added.
Pseudocode (bash), reusing existing helpers verbatim:

```bash
run_quic_tier1() {
    log_section "QUIC Tier 1: Handshake + bidir echo (DPDK <-> DPDK)"

    local suite="tier1-quic-handshake"
    local port=4433

    # SSM command timeouts (measurable constants, per Requirement 1.9).
    local QUIC_SERVER_TIMEOUT=300   # async server SSM command
    local QUIC_CLIENT_TIMEOUT=240   # synchronous client SSM command

    # 0. Verify QUIC binaries exist before doing anything else (Requirement 2.7).
    #    Does NOT touch the existing UDP verify_build path.
    if ! quic_verify_build "$suite"; then
        return 1
    fi

    # 1. Bind ENIs on both instances (same as TCP pair).
    if ! configure_eni "$SENDER_INSTANCE_ID" "bind"; then
        generate_failure_xml "$suite" "ENI bind failed on sender instance"
        return 1
    fi
    if ! configure_eni "$RECEIVER_INSTANCE_ID" "bind"; then
        generate_failure_xml "$suite" "ENI bind failed on receiver instance"
        return 1
    fi

    # 2. Warm ARP so the QUIC binaries can seed the gateway MAC.
    warm_arp_cache

    # 3. Start the QUIC server (receiver) as an async SSM command.
    local server_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier1-quic-handshake.sh \
        --role server --bind-ip $RECEIVER_DPDK_ENI_IP --port $port"
    local server_cmd_id
    server_cmd_id=$(ssm_run_command_async "$RECEIVER_INSTANCE_ID" "$QUIC_SERVER_TIMEOUT" "$server_cmd")
    if [[ -z "$server_cmd_id" ]]; then
        generate_failure_xml "$suite" "Failed to start QUIC server on receiver"
        return 1
    fi

    # 4. Readiness IS the cert poll: the server writes /tmp/quic-server-cert.pem only
    #    after it logs QUIC_SERVER_READY, so a non-empty cert observed via a separate
    #    SSM cat/base64 command is the readiness signal. Transfer the cert to the client.
    if ! quic_transfer_cert "$suite"; then
        ssm_cancel_command "$RECEIVER_INSTANCE_ID" "$server_cmd_id"
        return 1
    fi

    # 5. Run the QUIC client (sender). This writes the JUnit XML.
    local client_cmd="cd /opt/dpdk-stdlib && bash scripts/integration-tests/tier1-quic-handshake.sh \
        --role client --bind-ip $SENDER_DPDK_ENI_IP --peer-ip $RECEIVER_DPDK_ENI_IP \
        --port $port --output $RESULTS_REMOTE_DIR/${suite}.xml"
    if ! ssm_run_command "$SENDER_INSTANCE_ID" "$QUIC_CLIENT_TIMEOUT" "$client_cmd"; then
        generate_failure_xml "$suite" "QUIC client execution failed or timed out"
    fi

    # 6. Wait for / cancel the server (same as TCP pair).
    if ! ssm_wait_command "$RECEIVER_INSTANCE_ID" "$server_cmd_id" 30; then
        ssm_cancel_command "$RECEIVER_INSTANCE_ID" "$server_cmd_id"
    fi

    log_info "QUIC Tier 1 execution complete"
}
```

This maps directly onto Requirement 1 (function, ENI bind, ARP warm, server/client
invocation, wait/cancel, failure-XML-on-bind-fail, additive) and Requirement 10
(binary names, port 4433).

### `quic_transfer_cert` Helper

The certificate handoff is the only QUIC-specific mechanic. The server harness writes
the PEM to `/tmp/quic-server-cert.pem` **after** it logs `QUIC_SERVER_READY`. There is
therefore no separate readiness signal to wait on: a non-empty cert file observed on the
server IS the readiness check. The orchestrator polls the server for the cert (via a
separate SSM command), reads it base64-encoded, and writes it to the client at the same
path. The base64 approach improves on the reference implementation in
`run-quic-integration-tests.sh` / `run-quic-perf.sh`, which use sed-escaping + printf
format-string injection (a `%` in the cert corrupts the transfer). Base64 is byte-exact
and shell-safe; do NOT revert to the reference's escaping approach.

```bash
# Poll the server for the cert file (this poll IS the readiness check), read it
# base64-encoded, and write it to the client. Returns 0 on success; on failure,
# emits a synthetic failure XML and returns 1.
quic_transfer_cert() {
    local suite="$1"
    local cert_path="/tmp/quic-server-cert.pem"

    # Bounded wait for the server to produce a non-empty cert. The cert only
    # appears after the server logs QUIC_SERVER_READY, so this loop is both the
    # readiness gate and the cert read. Read it as a single-line base64 string.
    local waited=0 b64=""
    while [[ $waited -lt 60 ]]; do
        sleep 5
        waited=$((waited + 5))
        # Separate SSM command: base64 -w 0 collapses the PEM to one line.
        # "|| true" ensures the remote command always exits 0, suppressing false SSM failure logs
        # while the cert doesn't exist yet. ssm_run_get_output captures remote stdout.
        b64=$(ssm_run_get_output "$RECEIVER_INSTANCE_ID" 60             "base64 -w 0 $cert_path 2>/dev/null || true") || b64=""
        b64=$(echo "$b64" | tr -d '[:space:]')
        [[ -n "$b64" ]] && break
    done

    if [[ -z "$b64" ]]; then
        log_error "QUIC server cert not available after ${waited}s"
        generate_failure_xml "$suite" "QUIC certificate transfer failed: server cert unavailable"
        return 1
    fi

    # Write the cert to the client instance, decoding the base64 back to bytes.
    # printf '%s' avoids trailing newlines; base64 -d restores exact PEM bytes.
    if ! ssm_run_command "$SENDER_INSTANCE_ID" 60 \
        "printf '%s' '$b64' | base64 -d > $cert_path"; then
        generate_failure_xml "$suite" "QUIC certificate transfer failed: could not write to client"
        return 1
    fi

    log_info "QUIC server cert transferred to client at $cert_path"
}
```

Cert transfer mechanism (Requirement 2, HIGH-5):

1. **Read (server):** run `base64 -w 0 /tmp/quic-server-cert.pem` via a separate SSM
   command to obtain a single-line base64 string of the PEM.
2. **Transfer:** carry the base64 string in a shell variable (`$b64`).
3. **Write (client):** run `printf '%s' '$b64' | base64 -d > /tmp/quic-server-cert.pem`
   via SSM, which restores the PEM byte-for-byte. Multi-line PEM content and shell
   metacharacters cannot corrupt the transfer this way (Requirement 2.5).

This satisfies Requirement 2 (cert-poll readiness, read cert via SSM, write to client,
bounded-wait failure handling, byte-preservation, ordering after a non-empty cert is
observed / before client).

### `quic_verify_build` Helper

QUIC has its own build-verification step that runs only for the QUIC tier and does not
touch the existing UDP `verify_build` (Requirement 2.7). It confirms the QUIC binaries
are present before the orchestrator attempts to run them:

```bash
# Verify the QUIC binaries exist on both instances before running the tier.
# Emits a synthetic failure XML and returns 1 if either binary is missing.
# Uses ssm_run_get_output (to be added to run-integration-tests.sh alongside
# the existing SSM helpers; pattern borrowed from run-quic-perf.sh:175).
quic_verify_build() {
    local suite="$1"
    local bindir="/opt/dpdk-stdlib/target/release"
    # Check server binary on receiver; commands always exit 0 to suppress false SSM failure logs.
    local recv_out
    recv_out=$(ssm_run_get_output "$RECEIVER_INSTANCE_ID" 60         "test -x $bindir/quic-echo-server && echo OK || echo MISSING") || recv_out=""
    # Check client binary on sender.
    local send_out
    send_out=$(ssm_run_get_output "$SENDER_INSTANCE_ID" 60         "test -x $bindir/quic-test-client && echo OK || echo MISSING") || send_out=""
    if [[ "$recv_out" != *OK* || "$send_out" != *OK* ]]; then
        generate_failure_xml "$suite" "QUIC build verification failed: quic-echo-server / quic-test-client missing at $bindir"
        return 1
    fi
}
```

### `--tier quic` Filter and Dispatch

Extend the `--tier` validation case and add a gated dispatch block in `main()`.

Argument validation (add `quic`):

```bash
case "$TIER_FILTER" in
    1|2|3|4|tcp1|tcp1a|tcp2|tcp-full|quic) ;;
    *)
        echo "ERROR: --tier must be one of: 1 2 3 4 tcp1 tcp1a tcp2 tcp-full quic, got: $TIER_FILTER" >&2
        exit 2
        ;;
esac
```

Dispatch in `main()` — QUIC is **quic-only** and does not run in run-all mode, so the
default `integration-tests` job behavior is unchanged (Requirement 3.3):

```bash
# QUIC tier — runs only when explicitly selected, in its own CI job.
if [[ "$TIER_FILTER" == "quic" ]]; then
    run_quic_tier1 || true
fi
```

This mirrors the existing `tcp-full` handling, which is likewise gated on an explicit
filter value rather than the run-all path. Result collection, `print_summary`, and
`generate_json_summary` already iterate over whatever XML files land in `RESULTS_DIR`,
so the QUIC result is picked up with no further change (Requirement 3.5).

### Sequence Diagram — QUIC Tier 1

```
Orchestrator            Receiver (server)          Sender (client)
     |                        |                          |
     |-- configure_eni bind ->|                          |
     |-- configure_eni bind ------------------------------>|
     |-- warm_arp_cache ----->|                          |
     |-- warm_arp_cache ---------------------------------->|
     |-- SSM async: harness --role server -->|            |
     |                        |-- quic-echo-server start   |
     |                        |-- log QUIC_SERVER_READY    |
     |                        |-- write /tmp cert.pem      |
     |-- SSM: base64 -w 0 cert.pem (poll = readiness) -->| |
     |<-- single-line base64 stdout --|                    |
     |-- SSM: printf '%s' b64 | base64 -d > cert.pem ----->|
     |-- SSM: harness --role client --------------------->|
     |                        |<==== QUIC handshake =====>|
     |                        |<==== bidir echo =========>|
     |                        |            writes tier1-quic-handshake.xml
     |-- ssm_wait_command (server) -->|                   |
     |-- ssm_cancel_command (if running) -->|             |
     |-- collect_results (pulls XML from sender + receiver) ->|
```

## QUIC Integration CI Workflow

`.github/workflows/quic-integration-tests.yml` is a structural clone of the
`tcp-integration-tests` job. Key elements:

```yaml
name: QUIC Integration Tests

on:
  pull_request:
    branches: [main, development]
  workflow_dispatch: {}

concurrency:
  group: integration-tests          # SAME group as UDP/TCP — serializes DpdkTestStack
  cancel-in-progress: false

permissions:
  id-token: write
  contents: read
  checks: write
  pull-requests: write
  issues: write

env:
  AWS_REGION: us-east-1

jobs:
  validate-cdk:
    # identical to the existing validate-cdk pre-flight (synth + invariants)
    ...

  quic-integration-tests:
    runs-on: ubuntu-latest
    timeout-minutes: 55
    needs: validate-cdk
    if: needs.validate-cdk.result == 'success'
    continue-on-error: true          # REMOVED once the QUIC gate condition is met
    steps:
      - Checkout
      - Install Node.js (20) + aws-cdk + CDK deps
      - Install Session Manager plugin
      - Configure AWS credentials (secrets.AWS_ROLE_ARN)
      - Fetch DPDK AMI from SSM (/dpdk-stdlib-rust/ami/latest)
      - Resolve PR number
      - Run: ./scripts/run-integration-tests.sh --teardown --json-summary --tier quic
      - Post QUIC results to PR (pass/fail/skip counts + logs, marked non-blocking)
      - Upload quic-integration-test-results (test-results/)
      - Upload quic-instance-logs (instance-logs/)
      - Publish results via dorny/test-reporter (fail-on-error: false while gated)
      - Safety-net teardown: cdk destroy DpdkTestStack --force
```

Design notes:

- **Serialization vs. `tcp-integration-tests`:** the TCP job declares
  `needs: [validate-cdk, integration-tests]` inside the same workflow file to run
  after the UDP job. The QUIC job lives in a separate workflow file, so it cannot
  `needs:` a job in another file. Cross-workflow serialization is achieved entirely by
  the shared `concurrency.group: integration-tests` (Requirement 5.1–5.3). This means a
  QUIC run and an integration run never hold `DpdkTestStack` simultaneously; whichever
  starts first runs to completion (including teardown) before the other begins.
- **Non-blocking while gated:** `continue-on-error: true` on the job and
  `fail-on-error: false` on the test-reporter step keep QUIC failures from failing the
  check run (Requirement 4.4). The PR comment states explicitly that QUIC is
  non-blocking (Requirement 4.6).
- **Teardown safety:** the orchestrator is invoked with `--teardown`, and a
  `failure()`-conditioned safety-net `cdk destroy` covers the case where the run dies
  before the orchestrator's own teardown (Requirement 4.7).

## Continue-on-Error Removal Gates

### Gate Definition

A protocol's integration CI job graduates to blocking once its 10 most recent
(non-skipped) runs all concluded `success` with the expected test count. This applies
independently to:

- `tcp-integration-tests` (Requirement 6)
- `quic-integration-tests` (Requirement 7)

A **Clean_Run** is a completed run whose conclusion is `success` AND whose collected
JUnit results contain the **expected number of test cases** with zero failures. Two
nuances govern the count:

- **Cancelled runs are skipped, not breaks.** Because all these jobs share the
  `integration-tests` concurrency group, a queued run can be `cancelled` by a newer run.
  A `cancelled` run is SKIPPED when counting consecutive clean runs — it is neither
  counted as clean nor treated as a break. Only `failure` or `startup_failure`
  conclusions break the chain and reset the count.
- **Zero-test false positive guard.** A run that found 0 test cases does NOT count as
  clean, even if its job conclusion is `success` (a real risk while `continue-on-error`
  masks step failures). The counting procedure MUST assert the expected test count:
  QUIC Tier 1 expects exactly 2 test cases (`quic_handshake`, `quic_bidir_echo`).

Scanning proceeds newest-to-oldest, skipping `cancelled` runs, counting `success` runs
that meet the expected-test-count assertion, and stopping at the first `failure` /
`startup_failure`.

### Counting Procedure (Agent Runbook)

The agent counts consecutive Clean_Runs by scanning the newest N conclusions of the
target job, for example with the GitHub CLI:

```bash
# List the most recent runs of the integration workflow, newest first.
gh run list \
  --workflow integration-tests.yml \
  --branch development \
  --limit 20 \
  --json databaseId,conclusion,createdAt \
  --jq 'sort_by(.createdAt) | reverse'
```

Because `tcp-integration-tests` and `integration-tests` share one workflow file, the
job-level conclusion is derived from the run's jobs:

```bash
# For a given run, read the conclusion of the specific job.
gh run view <run_id> --json jobs \
  --jq '.jobs[] | select(.name == "tcp-integration-tests") | .conclusion'
```

The agent walks runs newest-to-oldest: `cancelled` runs are skipped (they do not count
and do not break), `success` runs that meet the expected-test-count assertion increment
the counter, and the first `failure` / `startup_failure` stops the walk. If the counter
reaches 10, the gate condition is met. Important nuance: while `continue-on-error: true`
is set, the job's own step failures do **not** fail the run, so the agent must read the
**job-level** conclusion **and** the JUnit artifacts / `summary.json` to distinguish a
truly clean run from one that was green only because of the gate. The required signal is
the job's `conclusion == success` combined with the expected number of test cases (QUIC
Tier 1: exactly 2) and zero `failures` across the uploaded `test-results/*.xml`. A run
reporting 0 tests does NOT count as clean.

For the standalone `quic-integration-tests.yml` workflow, the same procedure applies
with `--workflow quic-integration-tests.yml` and the `quic-integration-tests` job name.

### Removal Action

When the condition is met, the agent removes exactly the gate lines:

- Delete `continue-on-error: true` from the target job.
- Flip the associated `dorny/test-reporter` step from `fail-on-error: false` to
  `fail-on-error: true`, so published results now fail the check.

No surrounding job logic changes (Requirement 6.6). When the condition is not met, the
agent leaves the gate in place and reports the current consecutive Clean_Run count
(Requirements 6.4 / 7.4).

### `tcp-synthetic-perf` Gate Verification

The `tcp-synthetic-perf` job runs locally with a mock `PacketBackend` and no AWS
dependency; it already passes reliably and should not carry a `continue-on-error` gate.
The agent verifies this with a grep check rather than assuming a gate exists:

```bash
# Confirm there is NO continue-on-error gate on tcp-synthetic-perf.
grep -n -A15 'tcp-synthetic-perf:' .github/workflows/integration-tests.yml \
  | grep -n 'continue-on-error'
```

If the grep finds no `continue-on-error` on that job, there is nothing to do. If one is
found, the agent removes it (the same mechanical edit as the other gates), because this
job does not require the 10-run wait (Requirement 6.5).

## QUIC Performance CI

`.github/workflows/quic-perf-tests.yml` is a `workflow_dispatch` workflow that invokes
the **existing** QUIC perf dispatch through `scripts/run-perf-tests.sh`. It does not
re-implement any perf logic: `run-perf-tests.sh` already short-circuits the
`quic-native-dpdk-nic` config token to `scripts/run-quic-perf.sh` (real-NIC, 2-instance,
`DpdkTestStack`). The `quic-stock` and `quic-native-dpdk` tokens are in-process loopback
benchmarks (delegated to `run-quic-benchmarks.sh`) and do NOT require EC2 instances; only
`quic-native-dpdk-nic` does, so CI uses that token.

`run-quic-perf.sh` reads its workload from environment variables — `QUIC_PERF_DURATION`,
`QUIC_PERF_STREAMS`, `QUIC_PERF_PAYLOAD`, `QUIC_PERF_PORT` — not CLI flags, so the
workflow maps its `workflow_dispatch` inputs onto those env vars.

```yaml
name: QUIC Performance Tests

on:
  workflow_dispatch:
    inputs:
      duration:
        description: 'Seconds per run (QUIC_PERF_DURATION)'
        default: '30'
      streams:
        description: 'Concurrent stream count (QUIC_PERF_STREAMS)'
        default: '8'
      payload:
        description: 'Payload size in bytes (QUIC_PERF_PAYLOAD)'
        default: '65536'
      port:
        description: 'QUIC port (QUIC_PERF_PORT)'
        default: '4433'
      teardown:
        default: 'true'
        type: choice
        options: ['true', 'false']

concurrency:
  group: integration-tests          # DpdkTestStack — same group as UDP/TCP/QUIC integration
  cancel-in-progress: false

permissions: { id-token: write, contents: read, checks: write, pull-requests: write, issues: write }
env: { AWS_REGION: us-east-1 }

jobs:
  validate-cdk:
    steps:
      - Checkout / Node + aws-cdk + CDK deps
      - Synthesize DpdkTestStack:  cd deploy/cdk && npx cdk synth DpdkTestStack --quiet
  perf-tests-quic:
    needs: validate-cdk
    timeout-minutes: 90
    env:
      QUIC_PERF_DURATION: ${{ inputs.duration }}
      QUIC_PERF_STREAMS:  ${{ inputs.streams }}
      QUIC_PERF_PAYLOAD:  ${{ inputs.payload }}
      QUIC_PERF_PORT:     ${{ inputs.port }}
    steps:
      - Checkout / Node+CDK / SM plugin / AWS creds (secrets.AWS_ROLE_ARN)
      - Fetch DPDK AMI from SSM (/dpdk-stdlib-rust/ami/latest)
      - Resolve PR number
      - Run: |  # --teardown or --no-teardown based on teardown input, matching perf-tests-tcp.yml pattern
          TEARDOWN_FLAG=$([[ "${{ inputs.teardown }}" == 'true' ]] && echo '--teardown' || echo '--no-teardown')
          ./scripts/run-perf-tests.sh "$TEARDOWN_FLAG" --configs quic-native-dpdk-nic --json-summary
      - Print failure diagnostics (clone of TCP perf diagnostics)
      - Upload quic-perf-results (perf-results/, retention 90 days)
      - Upload quic-perf-instance-logs (instance-logs/, retention 30 days)
      - Safety-net teardown on failure:  cd deploy/cdk && npx cdk destroy DpdkTestStack --force
```

Design notes:

- **Invocation (Requirement 8.1–8.2):** the run step uses a `TEARDOWN_FLAG` conditional
  (matching `perf-tests-tcp.yml:141-149`) and calls
  `./scripts/run-perf-tests.sh "$TEARDOWN_FLAG" --configs quic-native-dpdk-nic --json-summary`.
  Workload parameters are passed via the `QUIC_PERF_*` env vars that `run-quic-perf.sh` reads;
  no `--payload-sizes` / `--streams` / extra flags are passed to a QUIC-specific script.
  Note: `--json-summary` is passed for consistency but is a no-op for the `quic-native-dpdk-nic`
  path (aggregation in `run-perf-tests.sh` exits before the summary step); results arrive
  in the `perf-results/` artifact.
- **Stack and concurrency are pinned (Requirement 8.7, HIGH-2):** because
  `quic-native-dpdk-nic` deploys `DpdkTestStack` (app-to-app QUIC; TRex cannot speak
  QUIC), the workflow MUST use `concurrency.group: integration-tests`, synthesize
  `DpdkTestStack` in `validate-cdk`, and target `DpdkTestStack` in the safety-net
  destroy. It does NOT mirror `perf-tests-tcp.yml`, which uses `PerfTestStack` and the
  `perf-tests-tcp` group.
- **Retention/AMIs/creds:** 90-day perf artifacts, 30-day instance logs,
  `secrets.AWS_ROLE_ARN`, SSM DPDK-AMI lookup (Requirements 8.4–8.6).

## Performance Baseline Verification

Baseline comparison is deferred until a protocol's integration suite is stable (its
gate removed or eligible for removal), per Requirement 9.1. The verification loop:

1. Trigger the perf workflow (TCP via `perf-tests-tcp.yml`, QUIC via
   `quic-perf-tests.yml`).
2. Collect the perf-results artifact (structured JSON + markdown).
3. Compare key metrics — throughput and P50/P90/P99 latency (and connection/handshake
   rate where applicable) — against the relevant run entries in
   `docs/perf-test-log.md`. The log already tracks per-config columns
   (`plain-rust`, `rust-dpdk`, `tokio-dpdk`, `native-dpdk`) across payload sizes, which
   is the comparison surface for TCP; QUIC results compare against the QUIC/`native-dpdk`
   equivalent once recorded.
4. If results are within normal run-to-run variance, append a new dated entry to
   `docs/perf-test-log.md` in the existing format (git context, configuration, results
   table, analysis) (Requirement 9.3).
5. If results regress beyond variance, report the regression instead of recording it as
   the new baseline (Requirement 9.4).

"Normal variance" is judged against the historical spread visible in adjacent runs in
`docs/perf-test-log.md`; this is a human/agent judgment step, not an automated
threshold, because the log records real hardware runs with inherent jitter.

## Error Handling

| Failure point                         | Handling                                                                 | Requirement |
|---------------------------------------|--------------------------------------------------------------------------|-------------|
| ENI bind fails (either instance)      | `generate_failure_xml` for the QUIC suite; skip test; return 1           | 1.7         |
| QUIC server fails to start via SSM    | `generate_failure_xml`; return 1                                         | 1.4         |
| Cert unavailable within bounded wait  | `generate_failure_xml` (cert-transfer failure); cancel server; return 1  | 2.3         |
| Cert write to client fails            | `generate_failure_xml` (cert-transfer failure); return 1                 | 2.3         |
| Client run fails / times out          | `generate_failure_xml`; continue to server wait/cancel                   | 1.6         |
| Invalid `--tier` value                | Print valid values (incl. `quic`); exit 2                                | 3.4         |
| CI run dies before orchestrator teardown | `failure()` safety-net `cdk destroy DpdkTestStack --force`             | 4.7         |
| Concurrent stack access               | Serialized by shared `integration-tests` concurrency group               | 5.1–5.3     |

All QUIC-suite failures surface as JUnit XML consumed by `collect_results`,
`print_summary`, and `generate_json_summary`, so an agent can diagnose a run from
`summary.json` and the uploaded artifacts without reading raw logs.

## Testing Strategy

Because this feature is CI/orchestration plumbing rather than library code, testing is
validation-oriented:

1. **Local shell validation:** `bash -n scripts/run-integration-tests.sh` (syntax) and,
   where practical, a dry-run of `run_quic_tier1` with SSM helpers stubbed to confirm
   the sequence (bind → warm → server async → cert transfer → client → wait/cancel) and
   the `--tier quic` dispatch path.
2. **CI workflow lint:** validate `quic-integration-tests.yml` and
   `quic-perf-tests.yml` YAML, confirm the concurrency group value, the tier filter in
   the run command, `continue-on-error: true`, and the safety-net teardown step.
3. **First live QUIC CI run:** because the QUIC stack has never run end-to-end on EC2,
   the first `quic-integration-tests` run is itself the validation step; it runs
   non-blocking so a failure surfaces diagnostics without blocking merges.
4. **Gate-procedure dry run:** exercise the Clean_Run counting commands against the
   actual run history to confirm the agent can compute the consecutive-success count
   for both `tcp-integration-tests` and `quic-integration-tests`.
5. **No regression:** confirm the `integration-tests` (UDP) and `tcp-integration-tests`
   jobs are byte-unchanged except for any performed gate removal, and that run-all mode
   still does not execute the QUIC tier.

## Design Decisions and Rationale

- **Fold QUIC into the main orchestrator rather than keep the standalone script.** The
  brief mandates a single orchestrator entry point (`run_quic_tier1` + `--tier quic`)
  so QUIC shares the UDP/TCP lifecycle, result collection, and JSON summary. The
  existing standalone `run-quic-integration-tests.sh` is used as the reference
  implementation for the cert-transfer mechanic, not as the CI entry point.
- **Base64 the cert during transfer.** PEM is multi-line and passes through SSM's
  shell-command parameter; base64 encode/decode guarantees byte-exact transfer and
  avoids quoting/newline corruption (Requirement 2.4).
- **QUIC is quic-only, not part of run-all.** Keeping QUIC out of the default run-all
  path guarantees the primary `integration-tests` job is unchanged and QUIC's
  (initially unproven) run only executes in its own gated job (Requirements 3.3, 10.3).
- **Cross-workflow serialization via concurrency group, not `needs:`.** A job in a
  separate workflow file cannot `needs:` the UDP job, so the shared
  `integration-tests` concurrency group is the mechanism that prevents concurrent
  `DpdkTestStack` deploys (Requirement 5).
- **Evidence-based gate removal.** Ten consecutive clean runs is a concrete, checkable
  bar that avoids removing the safety net prematurely, and the counting runbook makes
  the decision reproducible by any agent (Requirements 6, 7).
