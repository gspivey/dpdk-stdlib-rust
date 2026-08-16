# Requirements Document

## Introduction

Automated EC2 integration testing infrastructure for the dpdk-stdlib-rust project. The system deploys two EC2 instances with dual NICs (kernel + DPDK), executes integration tests remotely via SSM, and produces structured JUnit XML test results. The goal is to replace the current manual SSH-and-eyeball testing workflow with a fully automated, agent-legible, CI-ready pipeline.

Three test tiers are supported using the same two-instance infrastructure with different ENI binding configurations:
- **Tier 1 (DPDK ↔ DPDK)**: Both instances bind ENI to DPDK — your code talks to your code with no kernel safety net. Finds internal consistency bugs in ARP, ICMP, packet build/parse symmetry.
- **Tier 2 (DPDK ↔ Linux)**: Only one instance binds ENI to DPDK, the other uses kernel networking — validates standards compliance against the kernel stack.
- **Tier 3 (DPDK ↔ External tools)**: One instance runs dpdk-stdlib, the other runs iperf3 — proves interoperability with tools you didn't write.

The system is designed for three execution contexts: local developer invocation, GitHub Actions CI at PR time, and agent-driven execution (e.g., Codex, Kiro). All three use the same orchestrator script and produce the same structured JUnit XML output.

## Glossary

- **Orchestrator**: The local shell script (`run-integration-tests.sh`) that coordinates the full test lifecycle from the developer's machine
- **Test_Harness**: The set of scripts deployed to EC2 instances that execute individual test cases and produce structured output
- **Instance_A**: The first EC2 instance (c5n.large) participating in integration tests
- **Instance_B**: The second EC2 instance (c5n.large) participating in integration tests
- **Primary_ENI**: The eth0 network interface that remains bound to the kernel for SSM management access
- **DPDK_ENI**: The eth1 secondary network interface bound to vfio-pci for DPDK userspace networking
- **SSM**: AWS Systems Manager, used for remote command execution on instances without SSH
- **JUnit_XML**: The structured XML test result format consumable by CI systems and agents
- **CDK_Stack**: The AWS CDK infrastructure-as-code definition that provisions all AWS resources
- **Tier_1**: DPDK ↔ DPDK test configuration where both instances bind their DPDK_ENI to vfio-pci
- **Tier_2**: DPDK ↔ Linux test configuration where only Instance_A binds its DPDK_ENI, Instance_B uses kernel networking
- **Tier_3**: DPDK ↔ External test configuration where Instance_A runs dpdk-stdlib and Instance_B runs standard tools (iperf3)
- **CI_Workflow**: The GitHub Actions workflow that runs integration tests on pull requests
- **GitHub_Runner**: The GitHub Actions runner (ubuntu-latest) that executes the CI_Workflow

## Requirements

### Requirement 1: Single Entry Point Orchestration

**User Story:** As a developer, I want a single script that orchestrates the entire integration test lifecycle, so that I can run tests without manual multi-step procedures.

#### Acceptance Criteria

1. WHEN a developer executes `./scripts/run-integration-tests.sh <AWS_PROFILE>`, THE Orchestrator SHALL deploy infrastructure, wait for readiness, execute all test tiers, collect results, and report a summary
2. WHEN the `--teardown` flag is provided, THE Orchestrator SHALL destroy all AWS infrastructure after test execution completes
3. WHEN the `--skip-deploy` flag is provided and infrastructure already exists, THE Orchestrator SHALL skip CDK deployment and proceed directly to test execution
4. IF the AWS_PROFILE argument is missing, THEN THE Orchestrator SHALL print usage instructions and exit with a non-zero exit code
5. WHEN all test tiers complete, THE Orchestrator SHALL exit with code 0 if all tests passed and a non-zero code if any test failed
6. WHEN the `--tier` flag is provided with a value of 1, 2, or 3, THE Orchestrator SHALL execute only the specified test tier instead of all tiers

### Requirement 2: Infrastructure Deployment and Readiness

**User Story:** As a developer, I want the infrastructure to be provisioned automatically and verified ready before tests run, so that I don't waste time on tests against unready instances.

#### Acceptance Criteria

1. WHEN the Orchestrator deploys infrastructure, THE CDK_Stack SHALL create two c5n.large EC2 instances (Instance_A and Instance_B) in a VPC with private subnets and a NAT gateway
2. WHEN instances are created, THE CDK_Stack SHALL attach a secondary ENI (DPDK_ENI) to each instance in addition to the Primary_ENI
3. WHEN instances launch, THE CDK_Stack SHALL install Rust, DPDK 22.11.6, iperf3, and build the dpdk-stdlib-rust project on each instance via user data
4. WHEN deployment completes, THE Orchestrator SHALL verify that both instances are reachable via SSM before proceeding to test execution
5. IF an instance fails to become reachable within a configurable timeout, THEN THE Orchestrator SHALL report the failure and exit with a non-zero code
6. WHEN verifying readiness, THE Orchestrator SHALL confirm that the dpdk-stdlib-rust project has been built successfully on each instance

### Requirement 3: Remote Test Execution via SSM

**User Story:** As a developer, I want tests to execute remotely on EC2 instances without manual SSH, so that the process is fully automated and scriptable.

#### Acceptance Criteria

1. WHEN the Orchestrator executes a test tier, THE Orchestrator SHALL use AWS SSM send-command to run Test_Harness scripts on the target instances
2. WHEN a Test_Harness script runs on an instance, THE Test_Harness SHALL execute the test case, capture stdout and stderr, and write a JUnit_XML result file to a known path on the instance
3. WHEN a test tier completes on an instance, THE Orchestrator SHALL retrieve the JUnit_XML result file from the instance via SSM
4. IF an SSM command times out or fails, THEN THE Orchestrator SHALL record the test as failed in the JUnit_XML output with the error details

### Requirement 4: Tier Configuration — ENI Binding Per Test Tier

**User Story:** As a developer, I want the same two instances to support all three test tiers by changing ENI binding configuration, so that I don't need separate infrastructure per tier.

#### Acceptance Criteria

1. WHEN executing Tier_1 tests, THE Orchestrator SHALL bind the DPDK_ENI to vfio-pci on both Instance_A and Instance_B before running tests
2. WHEN executing Tier_2 tests, THE Orchestrator SHALL bind the DPDK_ENI to vfio-pci on Instance_A only, and leave Instance_B using kernel networking on the DPDK_ENI
3. WHEN executing Tier_3 tests, THE Orchestrator SHALL bind the DPDK_ENI to vfio-pci on Instance_A only, and run iperf3 on Instance_B using kernel networking
4. WHEN transitioning between tiers, THE Orchestrator SHALL unbind ENIs from vfio-pci and rebind to the kernel ena driver as needed before starting the next tier
5. IF ENI binding or unbinding fails, THEN THE Orchestrator SHALL report the failure and skip the affected tier rather than running tests with incorrect configuration

### Requirement 5: Tier 1 — DPDK ↔ DPDK Peer-to-Peer

**User Story:** As a developer, I want to test bidirectional UDP communication between two dpdk-stdlib instances, so that I can verify internal consistency of ARP resolution, ICMP handling, and packet build/parse symmetry without a kernel safety net.

#### Acceptance Criteria

1. WHEN Tier_1 runs, THE Test_Harness SHALL start the dpdk-stdlib echo application in listen mode on Instance_B and send UDP packets from Instance_A using the dpdk-stdlib socket
2. WHEN Instance_B receives a UDP packet, THE Test_Harness SHALL verify that Instance_B echoes the packet back to Instance_A
3. WHEN the echo round-trip completes, THE Test_Harness SHALL verify that the received payload matches the sent payload
4. WHEN Tier_1 runs the ARP resolution test, THE Test_Harness SHALL verify that both instances can resolve each other's MAC addresses using the dpdk-stdlib ARP handler without kernel assistance
5. WHEN Tier_1 completes, THE Test_Harness SHALL produce a JUnit_XML file containing test cases for send, receive, echo, payload integrity, and ARP resolution with pass/fail status

### Requirement 6: Tier 3 — DPDK ↔ iperf3 Interoperability

**User Story:** As a developer, I want to test that dpdk-stdlib can interoperate with standard iperf3 UDP traffic, so that I can prove compatibility with standard networking tools.

#### Acceptance Criteria

1. WHEN Tier_3 runs in the "our-app-sends" direction, THE Test_Harness SHALL start iperf3 in server mode on Instance_B and run the dpdk-stdlib application sending UDP traffic to Instance_B from Instance_A
2. WHEN Tier_3 runs in the "iperf-sends" direction, THE Test_Harness SHALL start the dpdk-stdlib application in listen mode on Instance_A and run iperf3 as a UDP client on Instance_B sending traffic to Instance_A
3. WHEN a direction test completes, THE Test_Harness SHALL verify that UDP packets were successfully transmitted and received by checking for non-zero bytes transferred
4. WHEN both directions complete, THE Test_Harness SHALL produce a JUnit_XML file containing a test case for each direction with pass/fail status and transfer statistics

### Requirement 7: Structured JUnit XML Test Output

**User Story:** As a developer or CI system, I want test results in JUnit XML format, so that results can be parsed programmatically by agents, CI pipelines, and reporting tools.

#### Acceptance Criteria

1. THE Test_Harness SHALL produce JUnit_XML files that conform to the JUnit XML schema with `<testsuite>`, `<testcase>`, and optional `<failure>` elements
2. WHEN a test case passes, THE Test_Harness SHALL write a `<testcase>` element with the test name, classname, and execution time
3. WHEN a test case fails, THE Test_Harness SHALL write a `<testcase>` element containing a `<failure>` child element with the failure message and relevant output
4. WHEN all test tiers complete, THE Orchestrator SHALL collect all JUnit_XML files into a local `test-results/` directory
5. WHEN results are collected, THE Orchestrator SHALL print a human-readable summary showing total tests, passed, failed, and execution time

### Requirement 8: Test Harness Scripts on Instances

**User Story:** As a developer, I want test logic to live in dedicated harness scripts deployed to instances, so that test logic is maintainable, version-controlled, and not embedded in user-data blocks.

#### Acceptance Criteria

1. THE Test_Harness scripts SHALL be stored in the repository under a dedicated directory and deployed to instances as part of the project build
2. WHEN a Test_Harness script executes a test case, THE Test_Harness SHALL set a per-test timeout and terminate the test process if the timeout is exceeded
3. WHEN a Test_Harness script produces output, THE Test_Harness SHALL write structured JUnit_XML to a deterministic file path on the instance (e.g., `/tmp/test-results/<tier>-<scenario>.xml`)
4. IF a test process crashes or produces unexpected output, THEN THE Test_Harness SHALL capture the error and record it as a test failure in the JUnit_XML output

### Requirement 9: Infrastructure Teardown

**User Story:** As a developer, I want to cleanly destroy test infrastructure when done, so that I don't incur unnecessary AWS costs.

#### Acceptance Criteria

1. WHEN the `--teardown` flag is provided, THE Orchestrator SHALL run `cdk destroy` after test execution completes to remove all AWS resources
2. WHEN teardown is not requested, THE Orchestrator SHALL leave infrastructure running and print instructions for manual teardown
3. IF teardown fails, THEN THE Orchestrator SHALL report the teardown error but still exit with the test result exit code (not mask test results with teardown failures)

### Requirement 10: GitHub Actions CI Integration

**User Story:** As a developer, I want integration tests to run automatically on pull requests, so that regressions in real DPDK networking are caught before merge.

#### Acceptance Criteria

1. WHEN a pull request is opened or updated against the main branch, THE CI_Workflow SHALL trigger the integration test pipeline
2. WHEN the CI_Workflow runs, THE GitHub_Runner SHALL execute the Orchestrator script with `--teardown` to deploy infrastructure, run tests, and clean up
3. WHEN integration tests complete, THE CI_Workflow SHALL upload the JUnit_XML files from `test-results/` as GitHub Actions artifacts
4. WHEN integration tests complete, THE CI_Workflow SHALL publish the JUnit_XML results using a test reporter action so that pass/fail details appear in the PR checks UI
5. WHEN the CI_Workflow runs, THE GitHub_Runner SHALL use AWS credentials from GitHub Actions secrets (not hardcoded profiles) to authenticate with AWS
6. IF the CI_Workflow fails due to infrastructure issues (not test failures), THEN THE CI_Workflow SHALL still attempt teardown to avoid orphaned AWS resources

### Requirement 11: Agent-Legible Execution

**User Story:** As an AI coding agent (Codex, Kiro, etc.), I want to invoke integration tests and parse the results programmatically, so that I can validate my code changes against real DPDK hardware in a feedback loop.

#### Acceptance Criteria

1. THE Orchestrator SHALL support a `--json-summary` flag that writes a machine-readable JSON summary file to `test-results/summary.json` containing run metadata, per-test results, and aggregate pass/fail counts
2. WHEN the `--json-summary` flag is used, THE Orchestrator SHALL include in the JSON summary: commit hash, timestamp, infrastructure details (instance type, DPDK version), and per-test name, status, duration, and error details
3. THE Orchestrator SHALL be invocable from any working directory by using paths relative to the repository root
4. WHEN an agent reads the JSON summary, THE summary SHALL contain sufficient detail for the agent to determine which specific test failed and what the failure mode was without needing to read raw logs
