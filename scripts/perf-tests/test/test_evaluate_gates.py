"""Unit tests for scripts/perf-tests/evaluate_gates.py (no AWS needed).

Run: python3 -I -m unittest discover -s scripts/perf-tests/test -p 'test_evaluate_gates.py'
"""
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import evaluate_gates as eg  # noqa: E402

with open(os.path.join(HERE, "..", "..", "perf-targets.json")) as fh:
    TARGETS = json.load(fh)

ALL = {"plain-rust", "rust-dpdk", "tokio-dpdk", "tokio-dpdk-concurrent", "native-dpdk"}
REQ = {"configs": ALL, "sizes": {64, 512, 1400, 8500}, "rate_steps": {70000, 140000, 350000, 700000}}


def cfg(per_size):
    """per_size: {size: {step: rx_pps}} -> config data; 700k may be a list (reps 1-3)."""
    results, reps = {}, {"2": {}, "3": {}}
    for size, steps in per_size.items():
        rows = []
        for step, v in steps.items():
            vals = v if isinstance(v, list) else [v]
            rows.append({"target_pps": step, "rx_pps": vals[0]})
            for i, extra in enumerate(vals[1:], start=2):
                reps[str(i)].setdefault(f"{size}B", []).append({"target_pps": step, "rx_pps": extra})
        results[f"{size}B"] = rows
    return {"results": results, "reps": reps}


def healthy():
    steps = lambda v700: {70000: 69000, 140000: 139000, 350000: 349000, 700000: v700}
    return {"configs": {
        "native-dpdk": cfg({s: steps(699000) for s in (64, 512, 1400)}),
        "rust-dpdk": cfg({s: steps([698000, 697000, 699000]) for s in (64, 512, 1400)}),
        "tokio-dpdk": cfg({s: steps([698000, 690000, 696000]) for s in (64, 512, 1400)}),
        "tokio-dpdk-concurrent": cfg({64: steps([300000, 305000, 310000]), 512: steps(230000), 1400: steps(176000)}),
    }}


class Gates(unittest.TestCase):
    def test_healthy_run_passes(self):
        v, lines = eg.evaluate(healthy(), TARGETS, REQ)
        self.assertEqual(v, "PASS", lines)

    def test_median_is_used_not_best_rep(self):
        r = healthy()
        # reps: 600k, 600k, 699k -> median 600k, below 95% of rust ~698k
        r["configs"]["tokio-dpdk"]["results"]["64B"][3]["rx_pps"] = 699000
        r["configs"]["tokio-dpdk"]["reps"]["2"]["64B"][0]["rx_pps"] = 600000
        r["configs"]["tokio-dpdk"]["reps"]["3"]["64B"][0]["rx_pps"] = 600000
        v, lines = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "FAIL")
        self.assertTrue(any(l.startswith("GATE FAIL: tokio-dpdk @ 64B median=600,000") for l in lines), lines)

    def test_missing_data_for_applicable_gate_fails(self):
        r = healthy()
        del r["configs"]["tokio-dpdk"]
        v, lines = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "FAIL")
        self.assertTrue(any("missing data" in l for l in lines))

    def test_unrequested_config_is_skipped_not_failed(self):
        r = healthy()
        del r["configs"]["tokio-dpdk"]
        req = dict(REQ, configs=ALL - {"tokio-dpdk"})
        v, lines = eg.evaluate(r, TARGETS, req)
        self.assertEqual(v, "PASS", lines)
        self.assertTrue(any(l.startswith("gate skipped: tokio-dpdk") for l in lines))

    def test_700k_gates_skipped_when_700k_not_requested(self):
        req = dict(REQ, rate_steps={70000, 140000})
        v, lines = eg.evaluate(healthy(), TARGETS, req)
        self.assertEqual(v, "PASS", lines)

    def test_native_below_600k_is_invalid(self):
        r = healthy()
        r["configs"]["native-dpdk"]["results"]["64B"][3]["rx_pps"] = 500000
        v, _ = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "INVALID")

    def test_native_requested_without_data_is_invalid(self):
        r = healthy()
        del r["configs"]["native-dpdk"]
        v, _ = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "INVALID")

    def test_rust_vs_native_gates_64_but_1400_is_report_only(self):
        r = healthy()
        r["configs"]["rust-dpdk"]["results"]["1400B"][3]["rx_pps"] = 400000
        r["configs"]["rust-dpdk"]["reps"]["2"]["1400B"][0]["rx_pps"] = 400000
        v, lines = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "PASS", lines)
        self.assertTrue(any(l.startswith("report-only: rust-dpdk @ 1400B") for l in lines))
        for rep in ("2", "3"):
            r["configs"]["rust-dpdk"]["reps"][rep]["64B"][0]["rx_pps"] = 500000
        r["configs"]["rust-dpdk"]["results"]["64B"][3]["rx_pps"] = 500000
        v, _ = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "FAIL")

    def test_concurrent_floor_is_report_only_until_enabled(self):
        r = healthy()
        r["configs"]["tokio-dpdk-concurrent"] = cfg({64: {700000: 200000}, 512: {700000: 1}, 1400: {700000: 1}})
        v, lines = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "PASS", lines)
        enabled = json.loads(json.dumps(TARGETS))
        enabled["gates"]["tokio-dpdk-concurrent"]["enabled"] = True
        v, lines = eg.evaluate(r, enabled, REQ)
        self.assertEqual(v, "FAIL")
        self.assertTrue(any(l.startswith("GATE FAIL: tokio-dpdk-concurrent @ 64B") for l in lines))

    def test_per_step_rule_uses_rep1_and_excludes_700k(self):
        r = healthy()
        r["configs"]["tokio-dpdk"]["results"]["512B"][2]["rx_pps"] = 300000  # 350k step
        v, lines = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "FAIL")
        self.assertTrue(any(l.startswith("GATE FAIL: tokio-dpdk per-step @ 512B @ 350000") for l in lines), lines)

    def test_vs_0_3_0_warns_but_never_fails(self):
        r = healthy()
        for rep_cfg in ("rust-dpdk", "native-dpdk", "tokio-dpdk"):
            r["configs"][rep_cfg] = cfg({s: {70000: 69000, 140000: 139000, 350000: 349000, 700000: 640000}
                                         for s in (64, 512, 1400)})
        v, lines = eg.evaluate(r, TARGETS, REQ)
        self.assertEqual(v, "PASS", lines)
        self.assertTrue(any(l.startswith("WARN: rust-dpdk @ 64B") for l in lines))

    def test_vs_target_cell(self):
        pct, status = eg.vs_target(healthy(), TARGETS, "tokio-dpdk", 64, 700000)
        self.assertEqual(status, "PASS")
        self.assertGreater(pct, 99)
        self.assertIsNone(eg.vs_target(healthy(), TARGETS, "native-dpdk", 64, 700000))


class Cli(unittest.TestCase):
    def _run(self, report):
        with tempfile.TemporaryDirectory() as d:
            rp = os.path.join(d, "perf-report.json")
            with open(rp, "w") as fh:
                json.dump(report, fh)
            eg.main(["--report", rp, "--targets", os.path.join(HERE, "..", "..", "perf-targets.json"),
                     "--results-dir", d, "--configs", ",".join(sorted(ALL)),
                     "--sizes", "64,512,1400,8500", "--rate-steps", "70000,140000,350000,700000"])
            out = {}
            for n in os.listdir(d):
                if n != "perf-report.json":
                    with open(os.path.join(d, n)) as fh:
                        out[n] = fh.read()
            return out

    def test_pass_writes_verdict_only(self):
        files = self._run(healthy())
        self.assertEqual(files["gate-verdict.txt"].strip(), "PASS")
        self.assertNotIn("run-invalid.txt", files)

    def test_invalid_writes_invalid_marker_and_no_verdict(self):
        r = healthy()
        del r["configs"]["native-dpdk"]
        files = self._run(r)
        self.assertIn("run-invalid.txt", files)
        self.assertNotIn("gate-verdict.txt", files)


if __name__ == "__main__":
    unittest.main()
