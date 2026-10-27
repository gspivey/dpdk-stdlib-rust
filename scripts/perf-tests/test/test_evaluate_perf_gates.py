#!/usr/bin/env python3
"""Unit tests for scripts/perf-tests/evaluate_perf_gates.py.

Run:  python3 -m unittest scripts/perf-tests/test/test_evaluate_perf_gates.py
"""
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import evaluate_perf_gates as epg  # noqa: E402

with open(os.path.join(HERE, "..", "..", "perf-targets.json")) as _fh:
    TARGETS = json.load(_fh)

ALL_CONFIGS = ["plain-rust", "rust-dpdk", "tokio-dpdk", "native-dpdk"]
SIZES = [64, 512, 1400]
STEPS = [70000, 140000, 350000, 700000]


def make_report(pps_700k, per_step=None):
    """pps_700k: {config: {size: [rep values]}}; per_step: {config: {size: {step: rx}}}."""
    report = {"rep_values": {}, "configs": {}}
    for cfg, by_size in pps_700k.items():
        report["rep_values"][cfg] = {f"{s}B": {"700000": v} for s, v in by_size.items()}
    for cfg, by_size in (per_step or {}).items():
        results = {}
        for s, by_step in by_size.items():
            results[f"{s}B"] = [{"target_pps": st, "rx_pps": rx} for st, rx in by_step.items()]
        report["configs"][cfg] = {"results": results}
    return report


def healthy(rust=698000, tokio=690000, native=699000):
    return {
        "native-dpdk": {s: [native] * 3 for s in SIZES},
        "rust-dpdk": {s: [rust] * 3 for s in SIZES},
        "tokio-dpdk": {s: [tokio] * 3 for s in SIZES},
    }


def per_step_ok():
    steps = {70000: 69000, 140000: 138000, 350000: 340000}
    return {"rust-dpdk": {s: steps for s in SIZES}, "tokio-dpdk": {s: steps for s in SIZES}}


class EvaluateTests(unittest.TestCase):
    def run_eval(self, report, configs=ALL_CONFIGS, sizes=SIZES, steps=STEPS):
        return epg.evaluate(report, TARGETS, configs, sizes, steps)

    def test_healthy_run_passes(self):
        r = self.run_eval(make_report(healthy(), per_step_ok()))
        self.assertEqual(r["verdict"], "PASS", r["lines"])
        self.assertEqual(r["lines"], [])

    def test_median_not_mean(self):
        # One bad rep out of three must not fail: median is 690k.
        h = healthy()
        h["tokio-dpdk"][64] = [690000, 100000, 695000]
        r = self.run_eval(make_report(h, per_step_ok()))
        self.assertEqual(r["verdict"], "PASS", r["lines"])

    def test_tokio_below_95pct_of_rust_fails(self):
        h = healthy(rust=700000, tokio=300000)
        r = self.run_eval(make_report(h, per_step_ok()))
        self.assertEqual(r["verdict"], "FAIL")
        self.assertTrue(any(l.startswith("GATE FAIL: tokio-dpdk @ 64B median=300,000") for l in r["lines"]))

    def test_rust_dpdk_vs_native_gate_only_64_and_512(self):
        h = healthy(rust=400000, native=700000)
        r = self.run_eval(make_report(h, per_step_ok()))
        fails = [l for l in r["lines"] if l.startswith("GATE FAIL: rust-dpdk")]
        self.assertTrue(any("@ 64B" in l for l in fails))
        self.assertTrue(any("@ 512B" in l for l in fails))
        self.assertFalse(any("@ 1400B" in l for l in fails))  # report-only size
        row = [x for x in r["rows"] if x["config"] == "rust-dpdk" and x["size"] == 1400][0]
        self.assertEqual(row["status"], "REPORT")

    def test_native_below_600k_is_invalid_run(self):
        h = healthy(native=500000)
        r = self.run_eval(make_report(h, per_step_ok()))
        self.assertEqual(r["verdict"], "INVALID")
        self.assertTrue(r["invalid"])

    def test_native_requested_but_no_data_is_invalid(self):
        h = healthy()
        del h["native-dpdk"]
        r = self.run_eval(make_report(h, per_step_ok()))
        self.assertTrue(r["invalid"])

    def test_missing_data_for_applicable_gate_fails(self):
        h = healthy()
        del h["tokio-dpdk"][512]
        r = self.run_eval(make_report(h, per_step_ok()))
        self.assertEqual(r["verdict"], "FAIL")
        self.assertTrue(any("tokio-dpdk @ 512B missing data" in l for l in r["lines"]))

    def test_gate_skipped_when_reference_not_requested(self):
        configs = ["tokio-dpdk", "plain-rust"]  # rust-dpdk (rel_to) not requested
        r = self.run_eval(make_report({"tokio-dpdk": {64: [1]}}), configs=configs)
        self.assertEqual(r["verdict"], "PASS")
        self.assertIn("gate skipped: tokio-dpdk (config/reference/size not requested)", r["skipped"])

    def test_no_700k_means_no_700k_gates(self):
        steps = [70000, 140000, 350000]
        r = self.run_eval(make_report({}, per_step_ok()), steps=steps)
        self.assertEqual(r["verdict"], "PASS", r["lines"])
        self.assertFalse(any(x["kind"] == "gate" for x in r["rows"]))

    def test_per_step_gate_fails_below_98pct(self):
        ps = per_step_ok()
        ps["tokio-dpdk"][64] = {70000: 69000, 140000: 138000, 350000: 300000}
        r = self.run_eval(make_report(healthy(), ps))
        self.assertEqual(r["verdict"], "FAIL")
        self.assertTrue(any("tokio-dpdk per-step @ 64B @ 350000" in l for l in r["lines"]))

    def test_per_step_excludes_700k(self):
        r = self.run_eval(make_report(healthy(), per_step_ok()))
        self.assertFalse(any(x["kind"] == "per_step" and x["step"] == 700000 for x in r["rows"]))

    def test_concurrent_gate_report_only_while_disabled(self):
        h = healthy()
        h["tokio-dpdk-concurrent"] = {64: [1000]}
        configs = ALL_CONFIGS + ["tokio-dpdk-concurrent"]
        r = self.run_eval(make_report(h, per_step_ok()), configs=configs)
        self.assertEqual(r["verdict"], "PASS", r["lines"])

    def test_concurrent_gate_enforced_when_enabled(self):
        targets = json.loads(json.dumps(TARGETS))
        targets["gates"]["tokio-dpdk-concurrent"]["enabled"] = True
        h = healthy()
        h["tokio-dpdk-concurrent"] = {64: [1000]}
        r = epg.evaluate(make_report(h, per_step_ok()), targets, ALL_CONFIGS + ["tokio-dpdk-concurrent"], SIZES, STEPS)
        self.assertEqual(r["verdict"], "FAIL")

    def test_vs_0_3_0_is_warn_only(self):
        h = healthy(rust=600000, native=600000, tokio=590000)
        r = self.run_eval(make_report(h, per_step_ok()))
        self.assertEqual(r["verdict"], "PASS", r["lines"])
        self.assertTrue(any(w.startswith("WARN: rust-dpdk @ 64B") for w in r["warnings"]))


class RepsPlanTests(unittest.TestCase):
    def test_default_inputs_exclude_8500_and_plain_rust(self):
        configs, sizes = epg.reps_plan(TARGETS, ALL_CONFIGS, [64, 512, 1400, 8500], STEPS)
        self.assertEqual(sizes, [64, 512, 1400])
        self.assertEqual(configs, ["rust-dpdk", "tokio-dpdk", "native-dpdk"])

    def test_no_700k_no_reps(self):
        self.assertEqual(epg.reps_plan(TARGETS, ALL_CONFIGS, SIZES, [70000]), ([], []))


class OutputTests(unittest.TestCase):
    def test_verdict_files_and_stale_cleanup(self):
        with tempfile.TemporaryDirectory() as d:
            open(os.path.join(d, "run-invalid.txt"), "w").close()  # stale
            epg.write_outputs({"verdict": "PASS", "invalid": False, "lines": []}, d)
            self.assertFalse(os.path.exists(os.path.join(d, "run-invalid.txt")))
            self.assertEqual(open(os.path.join(d, "gate-verdict.txt")).read(), "PASS")
            epg.write_outputs({"verdict": "INVALID", "invalid": True, "lines": ["x"]}, d)
            self.assertFalse(os.path.exists(os.path.join(d, "gate-verdict.txt")))
            self.assertTrue(os.path.exists(os.path.join(d, "run-invalid.txt")))


if __name__ == "__main__":
    unittest.main()
