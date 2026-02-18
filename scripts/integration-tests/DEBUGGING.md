# Integration Test Debugging Guide

This document encodes root causes and fixes for integration test failures so an
agent or engineer can diagnose problems without spelunking across multiple files.

> **For agents:** Start here when `run-integration-tests.sh` exits with code 2.
> Check `instance-logs/failure-summary.json` for the exact `failed_step`, then
> look up that step in the table below.

---

## Quick diagnosis

After a failed run, two structured artifacts help identify the cause:

| Artifact | Path | What it tells you |
|---|---|---|
| Failure JSON | `instance-logs/failure-summary.json` | Which step failed, error message, instance IDs |
| Step summary | GitHub Actions UI → "Summary" tab | Last 80 lines of user-data.log per instance, inline |
| Console output | `instance-logs/sender-console-output.log` | Raw EC2 boot log (always available, even after teardown) |
| User data log | `instance-logs/sender-user-data.log` | User-data script execution (richer, needs SSM) |

The GitHub Actions step summary is the fastest path — it shows log excerpts
without downloading any artifact.

---

## Failure modes by step

### `deploy_infrastructure` → exit code 2

**Symptom:** CDK deploy returns non-zero. CloudFormation events show one of:
- `Failed to receive 1 resource signal(s) within the specified duration`
- `CREATE_FAILED` on `DpdkSender` or `DpdkReceiver`

**Most likely cause A: cfn-signal resource name mismatch**

CDK generates CloudFormation logical IDs with hash suffixes unless
`overrideLogicalId()` is called. If the user-data script sends
`cfn-signal --resource DpdkSender` but CDK generated logical ID
`DpdkSender0BAA6CA3`, CloudFormation waits the full timeout before failing.

*Symptom:* exactly `PT20M` (or `PT35M`) elapses, then `CREATE_FAILED`.

*Fix:* `cfnInstance.overrideLogicalId('DpdkSender')` in `dpdk-test-stack.ts`.
This was the root cause of the failure on 2026-02-18.

*Prevent recurrence:* `scripts/validate-cdk.sh` catches this at synth time
(no AWS credentials needed, runs in seconds). The CI `validate-cdk` job
runs this before the expensive integration test deploy.

**Most likely cause B: creation timeout too short**

Even with the correct resource name, `cargo build --release` on a c5n.large
takes 8-12 minutes. If `creationTimeout` < build time, CloudFormation times
out before cfn-signal fires.

*Current timeout:* `PT20M` for pre-built AMI (DPDK installed, Rust build needed),
`PT35M` for full bootstrap (DPDK built from source).

*Symptom:* exactly `PT20M` elapses, then `CREATE_FAILED`, AND the user-data
log shows the cargo build was still running.

*Fix:* Increase `creationTimeout` in `dpdk-test-stack.ts`.

**Most likely cause C: user-data script failure**

`set -euo pipefail` means any failing command exits the script immediately,
and cfn-signal never runs (CloudFormation waits the full timeout).

*Symptom:* timeout fires, console output shows an error partway through setup.

*Diagnose:* Look at `instance-logs/sender-console-output.log` — this captures
the full user-data output even after the instance is terminated. Search for
the last successful `===` section header.

*Common triggers:*
- `dnf install` failing due to repo unavailability
- `aws s3 cp` failing due to IAM permissions
- `cargo build` compilation error (check for Rust/DPDK version mismatches)

---

### `fetch_stack_outputs` → exit code 2

**Symptom:** Cannot read CloudFormation outputs after deploy succeeds.

*Most likely cause:* CDK output names changed. The orchestrator reads specific
output keys (`SenderInstanceId`, `ReceiverInstanceId`, etc.). If a CDK change
renamed an output, the jq query returns empty.

*Fix:* Run `aws cloudformation describe-stacks --stack-name DpdkTestStack`
and compare output keys against `fetch_stack_outputs()` in `run-integration-tests.sh`.

---

### `wait_for_ssm_readiness` → exit code 2

**Symptom:** Instances deployed but SSM agent isn't responding within 600 seconds.

*Most likely cause A:* Instance bootstrapping is still running. SSM agent starts
after user-data completes. If cargo build takes 15+ minutes, the SSM readiness
window (600s) may expire first.

*Fix:* Increase `SSM_POLL_TIMEOUT` in `run-integration-tests.sh`, or speed up
the user-data (pre-compile the Rust project in the AMI — see future work below).

*Most likely cause B:* SSM agent not installed or not starting. The AMI should
have the SSM agent pre-installed. Check `/var/log/amazon/ssm/` in console output.

*Most likely cause C:* IAM role doesn't have `AmazonSSMManagedInstanceCore`.
Check the instance profile attached to the instances.

---

### `verify_build` → exit code 2

**Symptom:** SSM is ready but `echo` binary not found in `/opt/dpdk-stdlib/target/release/`.

*Most likely cause A:* `cargo build` failed. Look at `sender-user-data.log` for
compiler errors. Common: missing DPDK libraries, wrong `PKG_CONFIG_PATH`.

*Most likely cause B:* (Fixed 2026-02-18) Race condition — SSM command sent
but result retrieved before command completed. Fixed by using `ssm_wait_command()`
instead of `sleep 5`.

*Most likely cause C:* Project files not correctly downloaded/extracted from S3.
The user-data does `unzip dpdk-stdlib.zip && cp -r * /opt/dpdk-stdlib/`. If the
zip structure changes or the S3 copy fails, the project won't be set up.

---

## Validator: scripts/validate-cdk.sh

Runs CDK synth and checks invariants locally (no AWS, no deploy):

```bash
cd /path/to/dpdk-stdlib-rust
./scripts/validate-cdk.sh
```

Currently checks:
- cfn-signal resource names match CloudFormation logical IDs

If the validator itself is broken (false positive), investigate with:

```bash
# Inspect the synthesized template manually
cd deploy/cdk
CDK_DEFAULT_REGION=us-east-1 npx cdk synth --context amiId=ami-dummy
# Check cdk.out/DpdkTestStack.template.json
```

To skip the cfn-signal check while investigating a suspected false positive:
```bash
./scripts/validate-cdk.sh --skip-cfn-signal-check
```

---

## Future work: pre-compile Rust in the AMI

The current setup compiles the Rust project from scratch on every integration
test deploy (~8-12 min on c5n.large). This creates a large window where
infrastructure failures can silently swallow errors.

Pre-compiling in the AMI would:
- Reduce deployment to ~2 min (hugepages config + S3 download + signal)
- Eliminate the cargo build timeout risk entirely
- Allow `creationTimeout` to drop back to `PT5M`

This requires modifying `build-dpdk-ami.yml` to clone and build the project
before snapshotting, and updating the CDK stack's pre-built path to skip the
build step entirely.
