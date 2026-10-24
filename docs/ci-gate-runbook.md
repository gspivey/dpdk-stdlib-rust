# CI `continue-on-error` Gate Runbook (TCP + QUIC integration)

The TCP and QUIC EC2 integration jobs run with `continue-on-error: true` while each
protocol stabilizes. This runbook defines the **evidence-based** procedure for graduating
a job from non-blocking to blocking, per the `quic-tcp-ec2-integration` spec
(Requirements 6 and 7).

## Gate definition

A protocol's integration CI job graduates to blocking once its **10 most recent
(non-skipped) runs are all Clean_Runs**. This applies independently to:

- `tcp-integration-tests` (in `.github/workflows/integration-tests.yml`)
- `quic-integration-tests` (in `.github/workflows/quic-integration-tests.yml`)

A **Clean_Run** is a completed run whose **job-level** conclusion is `success` AND whose
collected JUnit results contain the **expected number of test cases** with **zero
failures**:

- QUIC Tier 1 expects exactly **2** test cases: `quic_handshake`, `quic_bidir_echo`.
- A run that found **0** test cases does NOT count as clean (guards against a zero-test
  false positive while `continue-on-error` masks step failures).

Two nuances govern the count:

- **`cancelled` runs are SKIPPED** — neither counted as clean nor treated as a break.
  Because all these jobs share the `integration-tests` concurrency group, a queued run is
  frequently `cancelled` by a newer run.
- Only **`failure`** or **`startup_failure`** conclusions **break** the chain and reset
  the count.

Scanning proceeds newest-to-oldest: skip `cancelled`, count `success`-with-expected-tests,
stop at the first `failure`/`startup_failure`.

> Note: the job's `continue-on-error: true` means a job step can fail without failing the
> whole run. The agent MUST therefore read the **job-level** conclusion AND the JUnit
> artifacts (expected test count + zero failures) — not just the run-level conclusion — to
> distinguish a truly clean run from one that was green only because of the gate.

## Counting procedure (agent runbook)

```bash
# 1. List the most recent runs of the workflow, newest first.
#    TCP job lives in integration-tests.yml; QUIC job in quic-integration-tests.yml.
gh run list --repo gspivey/dpdk-stdlib-rust \
  --workflow integration-tests.yml --limit 40 \
  --json databaseId,createdAt \
  --jq 'sort_by(.createdAt) | reverse | .[].databaseId'

# 2. For each run (newest first), read the JOB-level conclusion for the target job.
gh run view <run_id> --repo gspivey/dpdk-stdlib-rust --json jobs \
  --jq '.jobs[] | select(.name == "tcp-integration-tests") | .conclusion'
#    (use "quic-integration-tests" for the QUIC gate, --workflow quic-integration-tests.yml)

# 3. Walk newest-to-oldest:
#      cancelled                       -> SKIP (does not count, does not break)
#      success + expected test count   -> increment counter (verify via JUnit artifact)
#      failure | startup_failure       -> STOP
#    Gate condition met iff counter reaches 10.
```

To assert the expected test count, download the run's `*-integration-test-results`
artifact and confirm `test-results/*.xml` reports the expected `tests="N"` with
`failures="0"` (QUIC Tier 1: N = 2).

## Removal action (only when the condition is met)

Change **only** the gate lines, leaving all surrounding job logic unchanged:

1. Delete `continue-on-error: true` from the target job.
2. Flip the associated `dorny/test-reporter` step from `fail-on-error: false` to
   `fail-on-error: true`.

When the condition is **not** met, leave the gate in place and report the current
consecutive Clean_Run count.

### `tcp-synthetic-perf` is not gated

The `tcp-synthetic-perf` job runs locally with a mock `PacketBackend` and no AWS
dependency; it already passes reliably and carries no `continue-on-error` gate. Verify
with:

```bash
grep -n -A15 'tcp-synthetic-perf:' .github/workflows/integration-tests.yml \
  | grep -n 'continue-on-error'
```

If this finds nothing (expected), there is nothing to do. If a gate IS found, remove it —
it does not require the 10-run wait.

## Current evaluation (as of PR for ROADMAP item 38)

| Job | Consecutive Clean_Runs | Threshold | Gate action |
|-----|------------------------|-----------|-------------|
| `tcp-integration-tests` | **0** | 10 | **Gate retained** — first non-skipped run (newest-to-oldest) concludes `failure`; no `success` run exists in the scanned history. All newer runs are `cancelled` (skipped). |
| `quic-integration-tests` | **0** | 10 | **Gate retained** — the QUIC workflow is introduced by this PR and has no run history yet. Its first live run IS the end-to-end validation. |
| `tcp-synthetic-perf` | n/a | n/a | No gate present (verified by grep). Nothing to remove. |

Both integration gates therefore remain in place. Re-run this procedure after each new
batch of CI runs; remove a gate only once its count reaches 10.
