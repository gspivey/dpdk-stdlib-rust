#!/usr/bin/env python3
"""Unit tests for scripts/perf-tests/evaluate_gates.py (tokio-udp-perf task 2.4).

Run: python3 -m unittest scripts/perf-tests/test/test_evaluate_gates.py
No AWS, TRex or network needed.
"""
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import evaluate_gates as eg  # noqa: E402

TARGETS_PATH = os.path.join(HERE, "..", "..", "perf-targets.json")
with open(TARGETS_PATH) as fh:
    TARGETS = json.load(fh)

ALL_CONFIGS = ["native-dpdk", "rust-dpdk", "tokio-dpdk", "tokio-dpdk-concurrent"]
SIZES = [64, 512, 1400]
STEPS = [70000, 140000, 350000, 700000]


def data(config, per_size):
    """per_size: {size: {step: rx_pps}} -> a result file body."""
    return {"config_name": config,
            "results": {f"{s}B": [{"target_pps": st, "rx_pps": rx}
                                  for st, rx in steps.items()]
                        for s, steps in per_size.items()}}


def healthy(config, ref=700000, step_ratio=1.0):
    """700k at `ref` for each size, lower steps = offered rate * step_ratio."""
    out = {}
    for s in SIZES:
        out[s] = {st: int(st * step_ratio) for st in (70000, 140000, 350000)}
        out[s][700000] = ref
    return data(config, out)


def reps_of(config, *bodies):
    return {config: {i + 1: b for i, b in enumerate(bodies)}}


def merge(*dicts):
    out = {}
    for d in dicts:
        out.update(d)
    return out


def run(reps, configs=ALL_CONFIGS, sizes=SIZES, steps=STEPS):
    return eg.evaluate(reps, TARGETS, configs, sizes, steps)


def fleet(native=700000, rust=700000, tokio=700000, concurrent=300000):
    return merge(
        reps_of("native-dpdk", *[healthy("native-dpdk", native)] * 3),
        reps_of("rust-dpdk", *[healthy("rust-dpdk", rust)] * 3),
        reps_of("tokio-dpdk", *[healthy("tokio-dpdk", tokio)] * 3),
        reps_of("tokio-dpdk-concurrent", *[healthy("tokio-dpdk-concurrent", concurrent)] * 3),
    )


class MedianTests(unittest.TestCase):
    def test_median_of_three_reps_ignores_outlier(self):
        reps = reps_of("rust-dpdk",
                       data("rust-dpdk", {64: {700000: 690000}}),
                       data("rust-dpdk", {64: {700000: 100000}}),
                       data("rust-dpdk", {64: {700000: 700000}}))
        m, n = eg.median_rx(reps["rust-dpdk"], 64, 700000)
        self.assertEqual((m, n), (690000, 3))

    def test_missing_data_returns_none(self):
        self.assertEqual(eg.median_rx({}, 64, 700000), (None, 0))

    def test_rep_files_keyed_by_config_and_rep(self):
        with tempfile.TemporaryDirectory() as d:
            for rep, rx in ((1, 10), (2, 20), (3, 30)):
                with open(os.path.join(d, f"rust-dpdk.rep{rep}.json"), "w") as fh:
                    json.dump(data("rust-dpdk", {64: {700000: rx}}), fh)
            with open(os.path.join(d, "perf-report.json"), "w") as fh:
                json.dump({"configs": {}}, fh)
            reps = eg.load_reps(d)
            self.assertEqual(sorted(reps["rust-dpdk"]), [1, 2, 3])
            self.assertNotIn("perf-report", reps)
            self.assertEqual(eg.median_rx(reps["rust-dpdk"], 64, 700000)[0], 20)


class GateTests(unittest.TestCase):
    def test_all_healthy_passes(self):
        out = run(fleet())
        self.assertIsNone(out["invalid"])
        self.assertEqual(out["verdict"], "PASS", out["lines"])

    def test_tokio_below_95pct_of_rust_fails_naming_config_size_target(self):
        out = run(fleet(tokio=600000))
        self.assertEqual(out["verdict"], "FAIL")
        joined = "\n".join(out["lines"])
        self.assertIn("GATE FAIL: tokio-dpdk @ 64B median=600,000 target=665,000", joined)

    def test_tokio_at_exactly_95pct_passes(self):
        self.assertEqual(run(fleet(tokio=665000))["verdict"], "PASS")

    def test_rust_below_95pct_of_native_fails_but_1400_is_report_only(self):
        # rust 80% of native: 64 and 512 fail; 1400 only reported.
        out = run(fleet(rust=560000, tokio=560000))
        text = "\n".join(out["lines"])
        self.assertEqual(out["verdict"], "FAIL")
        self.assertIn("GATE FAIL: rust-dpdk @ 64B", text)
        self.assertIn("GATE FAIL: rust-dpdk @ 512B", text)
        self.assertNotIn("GATE FAIL: rust-dpdk @ 1400B", text)
        self.assertIn("report-only: rust-dpdk @ 1400B", text)

    def test_concurrent_floor_is_report_only_while_disabled(self):
        out = run(fleet(concurrent=100000))
        self.assertEqual(out["verdict"], "PASS")
        self.assertIn("report-only: tokio-dpdk-concurrent @ 64B", "\n".join(out["lines"]))

    def test_concurrent_floor_gates_when_enabled(self):
        targets = json.loads(json.dumps(TARGETS))
        targets["gates"]["tokio-dpdk-concurrent"]["enabled"] = True
        out = eg.evaluate(fleet(concurrent=100000), targets, ALL_CONFIGS, SIZES, STEPS)
        self.assertEqual(out["verdict"], "FAIL")
        self.assertIn("GATE FAIL: tokio-dpdk-concurrent @ 64B median=100,000 target=290,000",
                      "\n".join(out["lines"]))
        # 512/1400 remain report-only even when enabled.
        self.assertNotIn("GATE FAIL: tokio-dpdk-concurrent @ 512B", "\n".join(out["lines"]))

    def test_missing_data_for_applicable_gate_is_fail_not_skip(self):
        reps = fleet()
        del reps["tokio-dpdk"]
        out = run(reps)
        self.assertEqual(out["verdict"], "FAIL")
        self.assertIn("GATE FAIL: tokio-dpdk @ 64B no data", "\n".join(out["lines"]))

    def test_vs_0_3_0_warns_but_never_gates(self):
        # rust 96% of 0.3.0 at 64B, same-run native equally low.
        out = run(fleet(native=670000, rust=670000, tokio=670000))
        self.assertEqual(out["verdict"], "PASS")
        self.assertIn("WARN: rust-dpdk @ 64B", "\n".join(out["lines"]))


class ApplicabilityTests(unittest.TestCase):
    def test_unrequested_config_is_skipped_not_failed(self):
        reps = merge(reps_of("rust-dpdk", *[healthy("rust-dpdk")] * 3),
                     reps_of("native-dpdk", *[healthy("native-dpdk")] * 3))
        out = run(reps, configs=["rust-dpdk", "native-dpdk"])
        self.assertEqual(out["verdict"], "PASS", out["lines"])
        text = "\n".join(out["lines"])
        self.assertIn("gate skipped: tokio-dpdk @ 64B (config/reference/size not requested)", text)

    def test_reference_not_requested_skips_gate(self):
        reps = reps_of("tokio-dpdk", *[healthy("tokio-dpdk")] * 3)
        out = run(reps, configs=["tokio-dpdk"])
        self.assertEqual(out["verdict"], "PASS", out["lines"])

    def test_700k_gates_need_700k_requested(self):
        reps = fleet()
        out = run(reps, steps=[70000, 140000])
        self.assertEqual(out["verdict"], "PASS", out["lines"])
        self.assertNotIn("GATE FAIL", "\n".join(out["lines"]))

    def test_size_not_requested_is_skipped(self):
        out = run(fleet(), sizes=[64])
        self.assertEqual(out["verdict"], "PASS")
        self.assertIn("gate skipped: rust-dpdk @ 512B", "\n".join(out["lines"]))


class PerStepTests(unittest.TestCase):
    def test_tokio_below_98pct_of_rust_at_350k_fails(self):
        reps = fleet()
        reps["tokio-dpdk"][1] = data("tokio-dpdk", {64: {70000: 70000, 140000: 140000,
                                                          350000: 300000, 700000: 700000}})
        reps["rust-dpdk"][1] = data("rust-dpdk", {64: {70000: 70000, 140000: 140000,
                                                        350000: 350000, 700000: 700000}})
        out = run(reps, sizes=[64])
        self.assertEqual(out["verdict"], "FAIL")
        self.assertIn("GATE FAIL: tokio-dpdk per-step @ 64B @ 350000 rx_pps=300,000 "
                      "< 98% of rust-dpdk 350,000", "\n".join(out["lines"]))

    def test_700k_is_not_per_step_judged(self):
        reps = fleet()
        # tokio 96% of rust at 700k: passes headline 95% and must not trip 98% per-step.
        reps = merge(reps, reps_of("tokio-dpdk", *[healthy("tokio-dpdk", 672000)] * 3))
        self.assertEqual(run(reps)["verdict"], "PASS")

    def test_per_step_uses_rep1_only(self):
        reps = fleet()
        bad = data("tokio-dpdk", {64: {350000: 1}})
        reps["tokio-dpdk"][2] = bad
        reps["tokio-dpdk"][3] = bad
        self.assertEqual(run(reps)["verdict"], "PASS")

    def test_per_step_missing_requested_step_data_fails(self):
        reps = fleet()
        reps["tokio-dpdk"][1] = data("tokio-dpdk", {64: {700000: 700000}})
        out = run(reps, sizes=[64])
        self.assertEqual(out["verdict"], "FAIL")
        self.assertIn("per-step @ 64B @ 70000 missing data", "\n".join(out["lines"]))


class InvalidRunTests(unittest.TestCase):
    def test_slow_native_is_invalid_not_fail(self):
        out = run(fleet(native=500000, rust=500000, tokio=500000))
        self.assertIsNotNone(out["invalid"])
        self.assertIsNone(out["verdict"])

    def test_native_requested_but_no_data_is_invalid(self):
        reps = fleet()
        del reps["native-dpdk"]
        self.assertIsNotNone(run(reps)["invalid"])

    def test_rule_not_applied_without_700k_or_64b_or_native(self):
        reps = fleet(native=1000)
        self.assertIsNone(run(reps, sizes=[512, 1400])["invalid"])
        self.assertIsNone(run(reps, steps=[70000])["invalid"])
        self.assertIsNone(run(reps, configs=["rust-dpdk", "tokio-dpdk"])["invalid"])

    def test_invalid_run_writes_run_invalid_and_no_verdict(self):
        with tempfile.TemporaryDirectory() as d:
            eg.write_outputs(d, run(fleet(native=100)))
            self.assertTrue(os.path.exists(os.path.join(d, "run-invalid.txt")))
            self.assertFalse(os.path.exists(os.path.join(d, "gate-verdict.txt")))


class RepPlanTests(unittest.TestCase):
    def test_default_inputs_never_include_8500_and_include_native(self):
        cfgs, sizes = eg.rep_plan(TARGETS,
                                  ["plain-rust", "rust-dpdk", "tokio-dpdk",
                                   "tokio-dpdk-concurrent", "native-dpdk"],
                                  [64, 512, 1400, 8500], STEPS)
        self.assertEqual(sizes, [64, 512, 1400])
        self.assertEqual(cfgs, ["rust-dpdk", "tokio-dpdk", "tokio-dpdk-concurrent",
                                "native-dpdk"])

    def test_no_reps_without_700k(self):
        self.assertEqual(eg.rep_plan(TARGETS, ALL_CONFIGS, SIZES, [70000]), ([], []))

    def test_requested_subset_only(self):
        cfgs, sizes = eg.rep_plan(TARGETS, ["rust-dpdk", "native-dpdk"], [64], STEPS)
        self.assertEqual((cfgs, sizes), (["rust-dpdk", "native-dpdk"], [64]))


class OutputTests(unittest.TestCase):
    def test_verdict_file_pass_and_stale_files_removed(self):
        with tempfile.TemporaryDirectory() as d:
            open(os.path.join(d, "run-invalid.txt"), "w").close()
            eg.write_outputs(d, run(fleet()))
            self.assertFalse(os.path.exists(os.path.join(d, "run-invalid.txt")))
            self.assertEqual(open(os.path.join(d, "gate-verdict.txt")).read().strip(), "PASS")
            report = json.load(open(os.path.join(d, "gate-report.json")))
            self.assertTrue(any(e["config"] == "tokio-dpdk" and e["status"] == "PASS"
                                for e in report["entries"]))

    def test_cli_end_to_end(self):
        with tempfile.TemporaryDirectory() as d:
            for cfg, body in (("native-dpdk", healthy("native-dpdk")),
                              ("rust-dpdk", healthy("rust-dpdk")),
                              ("tokio-dpdk", healthy("tokio-dpdk", 100000))):
                for rep in (1, 2, 3):
                    with open(os.path.join(d, f"{cfg}.rep{rep}.json"), "w") as fh:
                        json.dump(body, fh)
            rc = eg.main(["evaluate", "--results-dir", d, "--targets", TARGETS_PATH,
                          "--configs", "native-dpdk,rust-dpdk,tokio-dpdk",
                          "--packet-sizes", "64,512,1400",
                          "--rate-steps", "70000,140000,350000,700000"])
            self.assertEqual(rc, 0)
            self.assertEqual(open(os.path.join(d, "gate-verdict.txt")).read().strip(), "FAIL")


if __name__ == "__main__":
    unittest.main()
