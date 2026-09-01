# Requirements Document

## Introduction

This feature wires the existing QUIC integration harness into the shared EC2 test
infrastructure, adds the CI plumbing that lets QUIC and TCP integration tests run
alongside the established UDP suite, and defines the gates that graduate maturing
protocols from "non-blocking" to "blocking" CI.

The dpdk-stdlib-rust project already ships:

- A two-instance EC2 test stack (`DpdkTestStack`) with ENI bind/unbind, SSM
  orchestration, and JUnit XML output.
- `scripts/run-integration-tests.sh` — the orchestrator that drives UDP tiers 1–4
  and TCP tiers (`tcp1`, `tcp1a`, `tcp2`, `tcp-full`).
- `scripts/integration-tests/tier1-quic-handshake.sh` — a complete QUIC harness
  (server + client roles, cert exchange via `/tmp`, handshake + bidirectional echo
  test cases).
- `dpdk-stdlib-quic/src/bin/quic-echo-server.rs` and `quic-test-client.rs` — the
  binaries the harness invokes.
- `.github/workflows/integration-tests.yml` — the CI workflow with a
  `tcp-integration-tests` job running `continue-on-error: true`.
- `.github/workflows/perf-tests-tcp.yml` — the TCP performance workflow.
- Three existing QUIC helper scripts that already implement the QUIC lifecycle:
  - `scripts/run-quic-integration-tests.sh` — standalone QUIC integration runner
    (server + client roles on two EC2 instances, cert transfer via SSM).
  - `scripts/run-quic-benchmarks.sh` — in-process loopback benchmark (stock vs
    native-dpdk providers), invoked by the `quic-stock` / `quic-native-dpdk` config
    tokens of `run-perf-tests.sh`.
  - `scripts/run-quic-perf.sh` — real-NIC 2-instance QUIC throughput perf over DPDK,
    invoked by the `quic-native-dpdk-nic` config token of `run-perf-tests.sh`. It reads
    its workload from the env vars `QUIC_PERF_DURATION`, `QUIC_PERF_STREAMS`,
    `QUIC_PERF_PAYLOAD`, and `QUIC_PERF_PORT`.

What is missing is the connective tissue: the orchestrator never calls the QUIC
harness, there is no QUIC CI job, the TCP `continue-on-error` gate has no defined
removal procedure, and QUIC has no performance CI workflow that dispatches the existing
`run-perf-tests.sh` QUIC path. This spec closes those gaps **additively** — it does not
modify the QUIC/TCP harness scripts or the CDK stack.

## Glossary

- **Orchestrator**: The shell script `scripts/run-integration-tests.sh` that drives
  the full EC2 test lifecycle (deploy, ENI config, tier execution, result
  collection, teardown).
- **QUIC_Harness**: The existing script `scripts/integration-tests/tier1-quic-handshake.sh`,
  which runs the QUIC server and client roles. It is treated as correct and MUST NOT
  be modified.
- **Server_Instance** / **Receiver_Instance**: The EC2 instance that runs the QUIC
  echo server (the `receiver` in orchestrator terms).
- **Client_Instance** / **Sender_Instance**: The EC2 instance that runs the QUIC test
  client (the `sender` in orchestrator terms).
- **DpdkTestStack**: The existing CDK stack that provisions the two integration-test
  instances and their DPDK ENIs. Shared by UDP, TCP, and QUIC integration tests.
- **Cert_File**: The QUIC server's self-signed certificate PEM, written by the server
  role to `/tmp/quic-server-cert.pem` and required by the client role before it can
  connect.
- **Cert_Transfer**: The orchestrator step that reads the Cert_File from the
  Server_Instance via SSM and writes it to the Client_Instance at the same path
  before the client role starts.
- **QUIC_CI_Job**: The `quic-integration-tests` job defined in the new
  `.github/workflows/quic-integration-tests.yml` workflow.
- **TCP_CI_Job**: The existing `tcp-integration-tests` job in
  `.github/workflows/integration-tests.yml`.
- **Continue_On_Error_Gate**: The `continue-on-error: true` setting on a CI job that
  lets its failures pass the overall workflow while the protocol matures.
- **Clean_Run**: A completed CI run of a given job whose conclusion is `success` AND
  whose collected JUnit results contain the expected number of test cases with zero
  failures. A run with `failure` or `startup_failure` conclusion breaks the consecutive
  chain. A run that was `cancelled` (for example by concurrency) is SKIPPED when counting
  consecutive clean runs — it neither counts as clean nor breaks the chain. A run that
  found 0 test cases does NOT count as clean (guards against a zero-test false positive).
- **Concurrency_Group**: The GitHub Actions `concurrency.group` value that serializes
  workflow runs. All jobs touching `DpdkTestStack` share the `integration-tests`
  group so they never deploy or destroy the stack concurrently.
- **Perf_Baseline**: The recorded performance figures in `docs/perf-test-log.md`
  against which new perf-test results are compared.

## Requirements

### Requirement 1: Wire QUIC Tier 1 into the Orchestrator

**User Story:** As a developer, I want the orchestrator to run the QUIC Tier 1 test
using the same two-instance infrastructure as UDP and TCP, so that QUIC is exercised
end-to-end without a separate harness invocation path.

#### Acceptance Criteria

1. THE Orchestrator SHALL define a `run_quic_tier1` function that executes the QUIC
   Tier 1 test (DPDK ↔ DPDK) using the existing QUIC_Harness.
2. WHEN `run_quic_tier1` runs, THE Orchestrator SHALL bind the DPDK ENI to vfio-pci on
   both the Server_Instance and the Client_Instance before starting the test.
3. WHEN the ENIs are bound, THE Orchestrator SHALL warm the kernel ARP cache on both
   instances (reusing the existing `warm_arp_cache` step) so the QUIC binaries can
   seed the gateway MAC from `/proc/net/arp`.
4. WHEN starting the server role, THE Orchestrator SHALL invoke the QUIC_Harness with
   `--role server`, the Server_Instance DPDK ENI IP as `--bind-ip`, and port 4433 as
   a non-blocking (async) SSM command.
5. WHEN starting the client role, THE Orchestrator SHALL invoke the QUIC_Harness with
   `--role client`, the Client_Instance DPDK ENI IP as `--bind-ip`, the Server_Instance
   DPDK ENI IP as `--peer-ip`, port 4433, and `--output /tmp/test-results/tier1-quic-handshake.xml`.
6. WHEN the client role finishes, THE Orchestrator SHALL wait for the server SSM
   command to complete and cancel it if it is still running (reusing the existing
   `ssm_wait_command` / `ssm_cancel_command` steps).
7. IF ENI binding fails on either instance, THEN THE Orchestrator SHALL generate a
   synthetic failure JUnit XML for the QUIC suite and skip the test rather than run it
   with an incorrect configuration.
8. THE `run_quic_tier1` function SHALL be additive: it MUST NOT alter the behavior of
   any existing tier function (`run_tier1`..`run_tier4`, `run_tier1_tcp`,
   `run_tier1_tcp_async`, `run_tier2_tcp`, `run_tcp_full`).
9. THE Orchestrator SHALL define measurable SSM timeout constants for QUIC Tier 1:
   `QUIC_SERVER_TIMEOUT=300` (seconds) for the async server SSM command and
   `QUIC_CLIENT_TIMEOUT=240` (seconds) for the synchronous client SSM command.

### Requirement 2: QUIC Certificate Transfer via SSM

**User Story:** As a developer, I want the orchestrator to move the QUIC server's
certificate to the client instance before the client connects, so that the client can
validate the TLS handshake without a manual copy step.

#### Acceptance Criteria

1. THE Orchestrator SHALL define server readiness as a non-empty
   `/tmp/quic-server-cert.pem` observed on the Server_Instance via a separate SSM `cat`
   command. The cert file is written by the server role only after it has logged
   `QUIC_SERVER_READY`, so the cert poll IS the readiness check; there is no separate
   readiness signal.
2. THE Orchestrator SHALL read the Cert_File from the Server_Instance by running
   `base64 -w 0 /tmp/quic-server-cert.pem 2>/dev/null || true` via a separate SSM output-capturing
   command (`ssm_run_get_output`, to be added to `run-integration-tests.sh` alongside existing SSM
   helpers) and capturing its single-line base64 stdout. The command MUST always exit 0 to suppress
   false SSM failure logs while the cert does not yet exist.
3. WHEN the Cert_File content is retrieved, THE Orchestrator SHALL write it to the
   Client_Instance at `/tmp/quic-server-cert.pem` via SSM before starting the client
   role.
4. IF the Cert_File is empty or cannot be retrieved from the Server_Instance within a
   bounded wait, THEN THE Orchestrator SHALL generate a synthetic failure JUnit XML for
   the QUIC suite describing the certificate-transfer failure and skip the client run.
5. WHEN writing the Cert_File to the Client_Instance, THE Orchestrator SHALL preserve
   the PEM content byte-for-byte so the certificate remains valid.
6. THE Cert_Transfer SHALL occur strictly after a non-empty cert is observed on the
   Server_Instance and strictly before the client role is invoked.
7. WHEN `TIER_FILTER=quic`, THE Orchestrator SHALL verify that the `quic-echo-server`
   and `quic-test-client` binaries exist at `/opt/dpdk-stdlib/target/release/` on the
   relevant instances before proceeding; the existing UDP `verify_build` step MUST NOT
   be modified.

### Requirement 3: `--tier quic` Filter

**User Story:** As a developer or CI job, I want to run only the QUIC Tier 1 test via a
tier filter, so that QUIC can execute in its own dedicated CI job without running the
UDP or TCP suites.

#### Acceptance Criteria

1. THE Orchestrator SHALL accept `quic` as a valid value for the `--tier` argument, in
   addition to the existing values (`1`, `2`, `3`, `4`, `tcp1`, `tcp1a`, `tcp2`,
   `tcp-full`).
2. WHEN `--tier quic` is provided, THE Orchestrator SHALL run only `run_quic_tier1` and
   SHALL NOT run any UDP or TCP tier.
3. WHEN no `--tier` filter is provided (run-all mode), THE Orchestrator SHALL NOT run
   the QUIC tier by default, so that the primary `integration-tests` job's behavior is
   unchanged.
4. IF `--tier` is given an unrecognized value, THEN THE Orchestrator SHALL print an
   error listing all valid tier values (including `quic`) and exit with a non-zero
   code.
5. WHEN `--tier quic` runs, THE Orchestrator SHALL collect the QUIC JUnit XML result
   and include it in the run summary and JSON summary, consistent with other tiers.

### Requirement 4: QUIC Integration CI Workflow

**User Story:** As a developer, I want QUIC integration tests to run in CI in their own
job, so that QUIC regressions on real DPDK hardware are visible without blocking merges
while the QUIC stack stabilizes.

#### Acceptance Criteria

1. THE feature SHALL add a new workflow `.github/workflows/quic-integration-tests.yml`
   containing a `quic-integration-tests` job.
2. THE QUIC_CI_Job SHALL mirror the structure of the existing TCP_CI_Job: checkout,
   Node.js + CDK setup, Session Manager plugin install, AWS credential configuration
   via `secrets.AWS_ROLE_ARN`, DPDK AMI lookup from SSM, PR-number resolution, test
   execution, PR-comment posting, artifact upload, and result publishing via
   `dorny/test-reporter`.
3. THE QUIC_CI_Job SHALL invoke the Orchestrator with `--teardown --json-summary
   --tier quic`.
4. THE QUIC_CI_Job SHALL run with `continue-on-error: true` initially, because the QUIC
   stack has never been validated end-to-end on EC2.
5. THE QUIC_CI_Job SHALL upload the QUIC JUnit XML results and instance logs as GitHub
   Actions artifacts.
6. WHEN the QUIC_CI_Job completes, THE QUIC_CI_Job SHALL post a PR comment summarizing
   pass/fail/skip counts and including relevant application logs, clearly marked as
   non-blocking while the gate is in place.
7. IF the workflow fails partway through, THEN THE QUIC_CI_Job SHALL run a safety-net
   `cdk destroy DpdkTestStack` teardown to avoid orphaned AWS resources.
8. THE QUIC_CI_Job SHALL trigger on pull requests to `main` and `development` and on
   `workflow_dispatch`, matching the trigger surface of the integration workflow.
9. THE QUIC_CI_Job SHALL declare `timeout-minutes: 55`.

### Requirement 5: Shared Concurrency Group for Stack Safety

**User Story:** As a developer, I want every job that touches `DpdkTestStack` to be
serialized, so that concurrent runs never race to deploy or destroy the same
CloudFormation stack.

#### Acceptance Criteria

1. THE `quic-integration-tests` workflow SHALL declare `concurrency.group:
   integration-tests` with `cancel-in-progress: false`, matching the existing
   integration workflow.
2. WHEN a QUIC integration run and a UDP/TCP integration run are queued together, THE
   Concurrency_Group SHALL serialize them so only one holds `DpdkTestStack` at a time.
3. WHEN an in-flight integration run is executing, THE Concurrency_Group SHALL allow it
   to finish (including teardown) rather than cancelling it mid-deploy.
4. THE new QUIC workflow SHALL NOT introduce a new CDK stack; it SHALL reuse
   `DpdkTestStack`.

### Requirement 6: TCP `continue-on-error` Removal Gate

**User Story:** As a maintainer, I want a defined, checkable condition for graduating
the TCP integration tests from non-blocking to blocking, so that the gate is removed
based on evidence rather than guesswork.

#### Acceptance Criteria

1. THE spec SHALL define the removal condition for the TCP_CI_Job gate as: 10 consecutive Clean_Runs
   of the `tcp-integration-tests` job (as defined in the Glossary: conclusion `success`, expected
   test count met, zero failures).
2. THE spec SHALL define an agent procedure to count consecutive Clean_Runs by scanning
   the conclusions of the recent CI runs of the `tcp-integration-tests` job (for example
   via the GitHub CLI / API). WHEN counting, a run whose conclusion is `cancelled` SHALL
   be SKIPPED (neither counted as clean nor treated as a break); only `failure` or
   `startup_failure` conclusions break the chain. The counting procedure SHALL assert
   the expected test count for the job's JUnit results, and a run that found 0 test cases
   does NOT count as clean.
3. WHEN the agent determines that the removal condition is met, THE agent SHALL remove
   `continue-on-error: true` from the `tcp-integration-tests` job in
   `integration-tests.yml`.
4. WHEN the removal condition is NOT met, THE agent SHALL leave the
   `continue-on-error: true` setting in place and report the current consecutive
   Clean_Run count.
5. THE agent SHALL confirm that no `continue-on-error` gate exists on the
   `tcp-synthetic-perf` job via a grep check; IF one is found, THE agent SHALL remove it,
   because that job runs locally with a mock backend and already passes reliably (its
   removal does not require the 10-run gate).
6. WHEN removing a gate, THE agent SHALL change only the `continue-on-error` line(s) and
   any directly-dependent result-publishing setting (for example `fail-on-error` in the
   test-reporter step) and SHALL NOT alter the surrounding job logic.

### Requirement 7: QUIC `continue-on-error` Removal Gate

**User Story:** As a maintainer, I want the same evidence-based graduation for QUIC
integration tests, so that QUIC becomes blocking only once it is proven stable on EC2.

#### Acceptance Criteria

1. THE spec SHALL define the removal condition for the QUIC_CI_Job gate as: 10
   consecutive Clean_Runs of the `quic-integration-tests` job. This threshold matches the
   TCP gate and intentionally overrides ROADMAP item 12's "5+" figure, which SHALL be
   updated to 10 in the same PR.
2. THE spec SHALL reuse the same Clean_Run counting procedure defined for the TCP gate,
   applied to the `quic-integration-tests` job. QUIC Tier 1 expects exactly 2 test cases
   (`quic_handshake` and `quic_bidir_echo`); the counting procedure SHALL assert this
   expected test count, and a run that found 0 test cases does NOT count as clean.
3. WHEN the QUIC removal condition is met, THE agent SHALL remove `continue-on-error:
   true` from the `quic-integration-tests` job.
4. WHEN the QUIC removal condition is NOT met, THE agent SHALL leave the gate in place
   and report the current consecutive Clean_Run count.

### Requirement 8: QUIC Performance CI

**User Story:** As a developer, I want a `workflow_dispatch` GitHub Actions workflow that
invokes the existing QUIC perf dispatch, so that QUIC throughput and latency can be
measured on EC2 without re-implementing the perf harness.

#### Acceptance Criteria

1. THE feature SHALL add a new `workflow_dispatch` workflow
   `.github/workflows/quic-perf-tests.yml` that invokes the existing QUIC perf path via
   `scripts/run-perf-tests.sh`. The workflow SHALL NOT re-implement QUIC perf logic;
   `run-perf-tests.sh` already delegates the `quic-native-dpdk-nic` config token to
   `scripts/run-quic-perf.sh`.
2. THE QUIC perf workflow SHALL invoke
   `scripts/run-perf-tests.sh --teardown --configs quic-native-dpdk-nic --json-summary`
   and SHALL pass the workload parameters via the environment variables that
   `run-quic-perf.sh` actually reads: `QUIC_PERF_DURATION`, `QUIC_PERF_STREAMS`,
   `QUIC_PERF_PAYLOAD`, and `QUIC_PERF_PORT`. The only config token that requires EC2
   instances is `quic-native-dpdk-nic`; the `quic-stock` and `quic-native-dpdk` tokens
   run in-process loopback and do not.
3. THE QUIC perf workflow SHALL be triggered by `workflow_dispatch` and SHALL expose
   `workflow_dispatch` inputs that map onto those env vars (duration, streams, payload,
   port) plus a teardown toggle.
4. THE QUIC perf workflow SHALL configure AWS credentials via `secrets.AWS_ROLE_ARN`
   and resolve the required AMI IDs from SSM (DPDK AMI), matching the TCP perf workflow.
5. THE QUIC perf workflow SHALL upload its performance results as GitHub Actions
   artifacts with a retention period consistent with the TCP perf workflow.
6. THE QUIC perf workflow SHALL run a safety-net stack teardown on failure to avoid
   orphaned AWS resources.
7. BECAUSE the `quic-native-dpdk-nic` path deploys `DpdkTestStack` (not `PerfTestStack`),
   THE QUIC perf workflow SHALL declare `concurrency.group: integration-tests`, its CDK
   synth target SHALL be `DpdkTestStack`, and its safety-net destroy target SHALL be
   `DpdkTestStack`. It SHALL NOT mirror `perf-tests-tcp.yml`'s `PerfTestStack` /
   `perf-tests-tcp` group.

### Requirement 9: Performance Baseline Verification

**User Story:** As a maintainer, I want stabilized TCP and QUIC performance results
compared against recorded baselines, so that performance regressions are caught once
the integration suites are reliable.

#### Acceptance Criteria

1. THE spec SHALL define that TCP and QUIC performance verification against
   `docs/perf-test-log.md` baselines occurs only after the corresponding integration
   suite is stable (its `continue-on-error` gate has been removed or is eligible for
   removal).
2. WHEN a TCP or QUIC performance run completes, THE agent SHALL compare its key
   metrics (for example throughput and latency percentiles) against the relevant
   Perf_Baseline entries in `docs/perf-test-log.md`.
3. WHEN a performance run establishes a new reference point, THE agent SHALL record it
   as a new entry in `docs/perf-test-log.md` following the existing log format (git
   context, configuration, results, analysis).
4. IF a performance run shows a regression beyond normal run-to-run variance relative to
   the Perf_Baseline, THEN THE agent SHALL report the regression rather than silently
   recording it as the new baseline.

### Requirement 10: Additive, Non-Destructive Changes

**User Story:** As a maintainer, I want these changes to be strictly additive to the
existing harness and infrastructure, so that the established UDP and TCP paths keep
working exactly as before.

#### Acceptance Criteria

1. THE feature SHALL NOT modify the existing harness scripts
   (`tier1-quic-handshake.sh`, `tier1-tcp-echo.sh`, `tier2-tcp-echo.sh`,
   `tier1-dpdk-echo.sh`, and peer harness scripts).
2. THE feature SHALL NOT modify the CDK stack definition; all integration and perf
   tests SHALL reuse `DpdkTestStack` (and any existing perf stack) as-is.
3. THE feature SHALL NOT modify the existing `integration-tests` or
   `tcp-integration-tests` jobs in `integration-tests.yml` beyond the defined
   `continue-on-error` gate removals.
4. THE Orchestrator changes SHALL be additive only: a new `run_quic_tier1` function,
   the new `quic` tier-filter value, the Cert_Transfer step, and any new helper needed
   for those, without changing existing tier functions.
5. THE QUIC integration-tier binary names invoked SHALL be `quic-echo-server` and
   `quic-test-client`, matching the existing `dpdk-stdlib-quic` binary targets, and the
   QUIC test port SHALL be 4433.
6. THE QUIC perf-tier binary names invoked (via `run-quic-perf.sh`) SHALL be
   `quic-echo-server` (receiver server) and `quic-perf-client` (sender client), matching
   the binaries that `run-quic-perf.sh` actually launches on the EC2 instances.
7. THE feature SHALL NOT delete `scripts/run-quic-integration-tests.sh`. THE feature
   SHALL add a header comment to that script marking it "superseded by
   `./run-integration-tests.sh --tier quic`; retained for local developer convenience".
   Its cert-transfer logic remains valid as a reference implementation.

### Out of Scope

- QUIC Tier 2 (DPDK ↔ Linux) and Tier 3 (DPDK ↔ external tools) tests. Only Tier 1
  (DPDK ↔ DPDK) is in scope.
- New EC2 instance types or any CDK stack changes.
- IPv6 integration or performance tests (tracked in a separate spec).
- Graviton QUIC integration or performance tests (a separate workflow if needed).
- Redesign of the QUIC or TCP harness scripts, which are treated as correct.
