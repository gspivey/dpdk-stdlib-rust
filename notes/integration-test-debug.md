# Integration Test Debug Notes

**Session date:** 2026-02-19  
**Status:** Fix pushed, waiting for next CI run to fail with real code so we can introspect.

---

## What Was Broken

The CDK user-data (`deploy/cdk/lib/dpdk-test-stack.ts`) was downloading the real project from S3, then **overwriting it with inline stub files**:

- `Cargo.toml` replaced with a stripped workspace (no `dpdk-tokio`, added fake `apps/peer-app`)
- `dpdk-udp/Cargo.toml` replaced with a version missing real deps
- `apps/echo/src/main.rs` replaced with a stub that just printed strings and exited
- `apps/peer-app` created inline (not in the real project, not used by any test script)

The "build" that succeeded and triggered cfn-signal was building **stub code**, not dpdk-stdlib. The integration tests were testing nothing real.

## What Was Fixed (2026-02-19)

1. **`deploy/cdk/lib/dpdk-test-stack.ts`** — removed all `inlineProjectFiles`. Now:
   - Downloads real project zip from S3: `unzip -q /tmp/dpdk-stdlib.zip -d /opt/dpdk-stdlib`
   - Builds with `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build --release`
   - Verifies `target/release/echo` and `target/release/test-client` exist

2. **`.github/workflows/integration-tests.yml`** — removed `--teardown` flag and disabled safety-net teardown so instances stay alive after failure for introspection.

---

## Current Live Instances (manual deploy, still running as of 2026-02-19)

These were deployed manually to debug the SSM issue. SSM is confirmed working.

| Role     | Instance ID           | Private IP   |
|----------|-----------------------|--------------|
| Sender   | i-0fbb09e0b63f3f995   | 10.0.1.97    |
| Receiver | i-04e55970abe388b44   | 10.0.1.192   |

Connect via SSM:
```bash
aws ssm start-session --target i-0fbb09e0b63f3f995 --profile dpdk-test --region us-east-1
aws ssm start-session --target i-04e55970abe388b44 --profile dpdk-test --region us-east-1
```

These instances have the **old stub build** (pre-fix). They are useful for checking the environment (DPDK libs, hugepages, ENI state) but not for testing the real code.

**Teardown when done:**
```bash
cd deploy/cdk && npx cdk destroy DpdkTestStack --force --profile dpdk-test
```

---

## SSM Timeout in CI — Still Unresolved

The previous CI run (2026-02-18, instances i-072db54160927f06f / i-0ffe205367be54cb5) showed SSM never registering in 10 minutes. Our manual deploy with the same AMI and CDK stack had SSM register in ~2 minutes.

**Hypothesis:** The `wait_for_ssm_readiness` check in `run-integration-tests.sh` may be silently returning empty results due to `AWS_PROFILE=default` being exported while the `default` profile doesn't exist in `~/.aws/credentials` on the runner. The `2>/dev/null || true` suppresses any error. The instances may actually have been SSM-registered but the check was blind to it.

**To verify after next CI run fails:**
1. Check the `instance-logs` artifact — `failure-summary.json` will show which step failed
2. If it's `wait_for_ssm_readiness` again, add explicit debug logging to the SSM check:
   ```bash
   aws ssm describe-instance-information \
     --filters "Key=InstanceIds,Values=$SENDER_ID,$RECEIVER_ID" \
     --output json  # (no 2>/dev/null so errors surface)
   ```
3. Check if `AWS_PROFILE=default` with env-var credentials causes silent failures on the runner

---

## Next Steps for the Next Agent Session

After the next CI run (which will use the fixed user-data):

1. **Check the `instance-logs` artifact** from the failed run
2. **Connect to the live instances** via SSM (they won't be torn down now):
   ```bash
   # Get instance IDs from the failed run's CloudFormation stack
   aws cloudformation describe-stacks --stack-name DpdkTestStack \
     --query "Stacks[0].Outputs" --profile dpdk-test --region us-east-1
   ```
3. **Verify the real build is there:**
   ```bash
   ls -la /opt/dpdk-stdlib/target/release/echo
   /opt/dpdk-stdlib/target/release/echo --help
   ```
4. **Check what the tier1 test actually does** — the echo server uses `std::net::UdpSocket` by default (no `--features dpdk`). For real DPDK testing we need to build with `--features dpdk` and bind the DPDK ENI.
5. **Investigate the SSM timeout** if it recurs (see hypothesis above)

---

## Architecture Reminder

The tier1 test flow:
- **Receiver** runs: `echo --ip <DPDK_ENI_IP> --port 9000` (listener)
- **Sender** runs: `test-client --target <DPDK_ENI_IP> --port 9000 --message ... --count N`
- Results written to `/tmp/test-results/tier1-dpdk-echo.xml` on sender, collected via SSM

The DPDK ENI IPs are in the CDK stack outputs (`SenderDpdkEniPrivateIp`, `ReceiverDpdkEniPrivateIp`).

For DPDK to actually work, the secondary ENI must be bound to `vfio-pci` (done by `configure-eni.sh --action bind`) and the binary must be built with `--features dpdk`.

---

## Vision Context

The goal is a self-sufficient dev loop where Kiro/Claude Code can implement tasks, run integration tests, and verify their work without human intervention. See the OpenAI Harness Engineering blog post (pasted in session) for the north star. Key principles:
- Agents need fast feedback loops (SSM + live instances = introspection capability)
- Tests must test real code, not stubs
- Infrastructure knowledge should live in the repo (this notes file is a start)
