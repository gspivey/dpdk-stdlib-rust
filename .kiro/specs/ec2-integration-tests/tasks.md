# Implementation Plan: EC2 Integration Tests

## Overview

Build an automated EC2 integration testing pipeline for dpdk-stdlib-rust. The implementation proceeds bottom-up: shared JUnit XML helpers first, then test harness scripts, then the orchestrator that ties everything together, CDK stack modifications, the GitHub Actions CI workflow, and finally the agent-legible JSON summary.

## Tasks

- [x] 1. Create JUnit XML helper library (`scripts/integration-tests/harness-common.sh`)
  - [x] 1.1 Implement JUnit XML generation functions
    - Create `scripts/integration-tests/harness-common.sh` with functions: `junit_start_suite`, `junit_add_pass`, `junit_add_failure`, `junit_end_suite`, `junit_write`
    - Implement `run_with_timeout` function that runs a command with a timeout and captures exit code, stdout, stderr
    - Implement `log_info` and `log_error` helper functions with timestamps
    - Implement `result_path` function that generates deterministic output paths from tier and scenario names
    - _Requirements: 7.1, 7.2, 7.3, 8.3_

  - [x] 1.2 Write property tests for JUnit XML generation
    - **Property 1: JUnit XML structural validity**
    - **Property 2: JUnit XML pass element completeness**
    - **Property 3: JUnit XML failure element completeness**
    - **Property 7: Deterministic output path generation**
    - **Validates: Requirements 7.1, 7.2, 7.3, 8.3**

- [x] 2. Create ENI configuration helper (`scripts/integration-tests/configure-eni.sh`)
  - [x] 2.1 Implement ENI bind/unbind/status wrapper
    - Create `scripts/integration-tests/configure-eni.sh` with `--action bind|unbind|status`
    - Wrap existing `scripts/bind_eni.sh` and `scripts/unbind_eni.sh` with idempotency checks
    - Add status reporting (check if secondary ENI is bound to vfio-pci or ena)
    - Return appropriate exit codes for success/failure
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_

- [x] 3. Create Tier 1 test harness (`scripts/integration-tests/tier1-dpdk-echo.sh`)
  - [x] 3.1 Implement Tier 1 DPDK↔DPDK echo test script
    - Create `scripts/integration-tests/tier1-dpdk-echo.sh` with `--role listener|sender` argument parsing
    - Listener role: start dpdk-stdlib echo app, wait for traffic, verify receipt
    - Sender role: send UDP packets to listener, wait for echo response, verify payload match
    - Include ARP resolution verification (check that MAC addresses were resolved)
    - Use `harness-common.sh` functions to produce JUnit XML with test cases: arp_resolution, udp_send_receive, echo_roundtrip, payload_integrity
    - Set per-test timeouts using `run_with_timeout`
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 8.2, 8.4_

- [x] 4. Create Tier 3 test harness (`scripts/integration-tests/tier3-iperf-interop.sh`)
  - [x] 4.1 Implement Tier 3 DPDK↔iperf3 interoperability test script
    - Create `scripts/integration-tests/tier3-iperf-interop.sh` with `--role` and `--direction` argument parsing
    - "our-app-sends" direction: Instance B runs iperf3 server, Instance A runs dpdk-stdlib sending UDP
    - "iperf-sends" direction: Instance A runs dpdk-stdlib listener, Instance B runs iperf3 client
    - Verify non-zero bytes transferred for each direction
    - Use `harness-common.sh` functions to produce JUnit XML with test cases per direction
    - Set per-test timeouts using `run_with_timeout`
    - _Requirements: 6.1, 6.2, 6.3, 6.4, 8.2, 8.4_

- [x] 5. Checkpoint - Verify harness scripts
  - Ensure all harness scripts are syntactically valid (`bash -n` check)
  - Ensure harness-common.sh functions produce valid XML locally
  - Ensure all tests pass, ask the user if questions arise.

- [x] 6. Update CDK stack (`deploy/cdk/lib/dpdk-test-stack.ts`)
  - [x] 6.1 Add new CDK outputs and iperf3 installation
    - Add `iperf3` to user data install commands (`dnf install -y iperf3`)
    - Add CDK outputs for DPDK ENI private IP addresses (`SenderDpdkEniPrivateIp`, `ReceiverDpdkEniPrivateIp`)
    - Widen DPDK security group to allow all UDP traffic between instances (not just port 9000)
    - Ensure the integration test scripts directory is included in the project asset (not excluded)
    - _Requirements: 2.1, 2.2, 2.3_

- [x] 7. Create orchestrator script (`scripts/run-integration-tests.sh`)
  - [x] 7.1 Implement CLI argument parsing and validation
    - Parse positional AWS_PROFILE argument and optional flags: `--teardown`, `--skip-deploy`, `--tier 1|3`, `--json-summary`
    - Print usage and exit with code 2 if AWS_PROFILE is missing
    - Define configuration constants (timeouts, paths)
    - _Requirements: 1.4, 1.6_

  - [x] 7.2 Implement infrastructure deployment and readiness check
    - Run `cdk deploy` (unless `--skip-deploy`), capture stack outputs (instance IDs, ENI IDs, ENI private IPs)
    - Implement SSM readiness polling: loop `aws ssm describe-instance-information` until both instances appear, with configurable timeout
    - Verify project build by running a quick SSM command to check binary exists
    - _Requirements: 1.1, 1.3, 2.4, 2.5, 2.6_

  - [x] 7.3 Implement tier execution with ENI configuration
    - For each tier: configure ENIs via SSM (bind/unbind as needed), then run harness scripts on both instances via SSM send-command
    - Tier 1: bind both ENIs, run tier1-dpdk-echo.sh on both instances (listener first, then sender)
    - Tier 3: bind Instance A ENI only, run tier3-iperf-interop.sh for both directions
    - Handle `--tier` flag to run only specified tier
    - Unbind ENIs between tiers
    - _Requirements: 1.1, 1.6, 3.1, 4.1, 4.2, 4.3, 4.4, 4.5_

  - [x] 7.4 Implement result collection and summary
    - Retrieve JUnit XML files from instances via SSM (cat file content, write locally)
    - Collect into local `test-results/` directory
    - Parse JUnit XML files to extract test counts, failure counts, total time
    - Print human-readable summary table
    - Generate synthetic failure XML if retrieval fails
    - _Requirements: 3.3, 3.4, 7.4, 7.5_

  - [x] 7.5 Implement JSON summary generation
    - When `--json-summary` flag is set, parse all collected JUnit XML files
    - Generate `test-results/summary.json` with: commit hash, timestamp, infrastructure details, per-test results, aggregate counts
    - Ensure the JSON contains sufficient detail for an agent to identify which test failed and why
    - _Requirements: 11.1, 11.2, 11.4_

  - [x] 7.6 Write property tests for summary aggregation and exit code logic
    - **Property 4: Summary counts match JUnit XML content**
    - **Property 5: Exit code reflects aggregate test results**
    - **Property 6: Teardown failure preserves test exit code**
    - **Property 8: JSON summary consistency with JUnit XML**
    - **Validates: Requirements 7.5, 1.5, 9.3, 11.1**

  - [x] 7.7 Implement teardown logic
    - If `--teardown` flag is set, run `cdk destroy` after result collection
    - If teardown fails, report error but preserve test exit code
    - If teardown not requested, print manual teardown instructions
    - _Requirements: 1.2, 9.1, 9.2, 9.3_

- [x] 8. Checkpoint - Orchestrator validation
  - Ensure orchestrator script passes `bash -n` syntax check
  - Ensure CLI argument parsing handles all flag combinations correctly
  - Ensure all tests pass, ask the user if questions arise.

- [x] 9. Create GitHub Actions CI workflow (`.github/workflows/integration-tests.yml`)
  - [x] 9.1 Implement the integration test workflow
    - Create `.github/workflows/integration-tests.yml` triggered on pull_request to main and workflow_dispatch
    - Configure AWS credentials step using OIDC or repository secrets
    - Install prerequisites: Node.js, CDK CLI, AWS Session Manager plugin
    - Run orchestrator: `./scripts/run-integration-tests.sh "$AWS_PROFILE" --teardown --json-summary`
    - Upload `test-results/` directory as GitHub Actions artifact
    - Add test reporter step (e.g., `dorny/test-reporter`) to publish JUnit XML results in PR checks UI
    - Add `if: always()` teardown safety net step that runs `cdk destroy` even if orchestrator crashes
    - Set workflow timeout to 45 minutes
    - _Requirements: 10.1, 10.2, 10.3, 10.4, 10.5, 10.6_

- [x] 10. Final checkpoint - End-to-end validation
  - Ensure all scripts are executable and pass `bash -n` syntax check
  - Ensure harness-common.sh property tests pass (if implemented)
  - Ensure GitHub Actions workflow YAML is valid
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Each task references specific requirements for traceability
- The implementation is all Bash scripts except for CDK (TypeScript) and CI workflow (YAML)
- Property tests use bats-core with a custom random input driver
- The harness scripts are designed to be testable locally (XML generation) even though the actual integration tests require AWS
- The GitHub Actions workflow uses `--teardown` by default plus a safety-net teardown step to prevent orphaned infrastructure
