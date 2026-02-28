# Bugfix Requirements Document

## Introduction

The integration test suite for dpdk-stdlib-rust has three interrelated bugs that undermine test coverage and CI reliability. Tier 3 (iperf3 interop) is failing outright with ENI bind errors, Tier 1 is mislabeled as "DPDK→DPDK" but actually tests Kernel→DPDK because the test-client uses `tokio::net::UdpSocket` instead of `dpdk_tokio::compat::tokio::UdpSocket` and hardcodes `bind("0.0.0.0:0")`, and no true DPDK→DPDK or explicit Kernel→DPDK (Tier 2) test exists. Together, these bugs mean the project has zero validation of its core DPDK-to-DPDK userspace networking path and a broken CI pipeline.

## Bug Analysis

### Current Behavior (Defect)

1.1 WHEN Tier 3 (iperf3 interop) tests execute THEN the system fails with "ENI bind failed on sender instance" and the tier is skipped entirely

1.2 WHEN Tier 1 tests execute with the test-client THEN the system uses `tokio::net::UdpSocket` (kernel networking) instead of `dpdk_tokio::compat::tokio::UdpSocket` (DPDK-accelerated networking), because test-client imports `tokio::net::UdpSocket` directly

1.3 WHEN Tier 1 tests execute THEN the test-client binds to `0.0.0.0:0` (hardcoded) instead of the DPDK ENI IP address, causing traffic to route through the management interface (eth0, 10.0.1.139) rather than the DPDK interface (eth1, 10.0.1.193)

1.4 WHEN Tier 1 test results are reported THEN the system labels them as "DPDK↔DPDK" despite the sender using kernel networking, producing misleading CI output

1.5 WHEN the full test suite runs THEN there is no Tier 2 (Kernel→DPDK) test, so the kernel-to-DPDK interop path that Tier 1 accidentally exercises has no dedicated, correctly-labeled test

1.6 WHEN the orchestrator transitions ENI bindings between Tier 1 and Tier 3 THEN the ENI unbind/rebind sequence fails, likely due to missing state polling or race conditions in ENI detach operations

### Expected Behavior (Correct)

2.1 WHEN Tier 3 (iperf3 interop) tests execute THEN the system SHALL successfully bind ENIs on the sender instance and run iperf3 tests in both directions without ENI bind errors

2.2 WHEN Tier 1 tests execute with the test-client THEN the system SHALL use `dpdk_tokio::compat::tokio::UdpSocket` so that traffic is sent via the DPDK userspace networking stack when bound to a DPDK-managed interface

2.3 WHEN Tier 1 tests execute THEN the test-client SHALL accept a `--bind-ip` CLI argument and bind to the specified DPDK ENI IP address (e.g., 10.0.1.193), with a default of `0.0.0.0:0` for backward compatibility

2.4 WHEN Tier 1 test results are reported THEN the system SHALL accurately reflect DPDK→DPDK communication, verified by the receiver observing packets from the sender's DPDK ENI IP (10.0.1.193), not the management IP (10.0.1.139)

2.5 WHEN the full test suite runs THEN a dedicated Tier 2 (Kernel→DPDK) test SHALL exist that explicitly tests kernel socket sender to DPDK receiver, capturing the interop path that the old Tier 1 accidentally covered

2.6 WHEN the orchestrator transitions ENI bindings between tiers THEN the system SHALL properly wait for ENI state transitions to complete before attempting the next bind operation, preventing race conditions

### Unchanged Behavior (Regression Prevention)

3.1 WHEN test-client is invoked without the `--bind-ip` argument THEN the system SHALL CONTINUE TO bind to `0.0.0.0:0` and function identically to the current behavior (backward compatible)

3.2 WHEN the echo server application receives UDP packets from any source THEN the system SHALL CONTINUE TO echo them back correctly regardless of whether the sender used DPDK or kernel networking

3.3 WHEN Tier 1 tests run the ARP resolution, send/receive, echo roundtrip, and payload integrity test cases THEN the system SHALL CONTINUE TO produce JUnit XML results with the same test case names and structure

3.4 WHEN the orchestrator is invoked with `--tier 1` or `--tier 3` THEN the system SHALL CONTINUE TO support single-tier execution via the existing `--tier` flag

3.5 WHEN `cargo build` and `cargo test` are run locally without DPDK installed THEN the system SHALL CONTINUE TO build and pass all 133+ tests using the stub system

3.6 WHEN the CI workflow triggers on pull requests THEN the system SHALL CONTINUE TO deploy infrastructure, run tests, collect JUnit XML results, and teardown using the same orchestrator entry point
