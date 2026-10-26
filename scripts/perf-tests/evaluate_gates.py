#!/usr/bin/env python3
"""Perf gate evaluation for scripts/run-perf-tests.sh.

Pure functions over the aggregated perf-report.json plus scripts/perf-targets.json,
so the gate logic is unit-testable without AWS (see test/test_evaluate_gates.py).

Report shape (written by aggregate_results): report["configs"][name] is the rep-1
result file, with ["results"]["<size>B"] = list of step rows (target_pps, rx_pps, ...)
and, for configs that ran the 700k-only reps, ["reps"]["2"|"3"] = {"<size>B": [rows]}.

Verdicts: PASS | FAIL | INVALID. INVALID means a bad deployment (native-dpdk too slow
or missing), which the harness maps to exit 2 (infra) rather than exit 1 (gate miss).
"""
import argparse
import json
import os
import statistics
import sys

STEP_700K = 700000


def rows_for(report, cfg, size, step, rep1_only=False):
    """rx_pps samples for (cfg, size, step): rep 1 plus, at 700k, reps 2-3."""
    data = report.get("configs", {}).get(cfg)
    if not isinstance(data, dict):
        return []
    key = f"{size}B"
    sources = []
    res = data.get("results", {})
    if isinstance(res, dict):
        sources.append(res)
    if not rep1_only and step == STEP_700K:
        for rep in sorted((data.get("reps") or {}).keys()):
            sources.append(data["reps"][rep])
    out = []
    for src in sources:
        for r in src.get(key, []) or []:
            if r.get("target_pps") == step and r.get("rx_pps") is not None:
                out.append(r["rx_pps"])
    return out


def median_at(report, cfg, size, step):
    vals = rows_for(report, cfg, size, step)
    return statistics.median(vals) if vals else None


def rep1_at(report, cfg, size, step):
    vals = rows_for(report, cfg, size, step, rep1_only=True)
    return vals[0] if vals else None


def _applicable(requested, cfgs, size=None, step=None):
    if any(c not in requested["configs"] for c in cfgs):
        return False
    if size is not None and size not in requested["sizes"]:
        return False
    if step is not None and step not in requested["rate_steps"]:
        return False
    return True


def vs_target(report, targets, cfg, size, step):
    """Return (pct, status) for the summary 'vs target' column, or None.

    status is PASS/FAIL for gated cells and REPORT for report-only ones."""
    gates = targets["gates"]
    g = gates.get(cfg)
    if g is None:
        return None
    if "rel_to" in g:
        if size in g.get("sizes", []):
            status_gated = True
        elif size in g.get("report_only_sizes", []):
            status_gated = False
        else:
            return None
        ref_cfg = g["rel_to"]
        if step == STEP_700K:
            mine, ref = median_at(report, cfg, size, step), median_at(report, ref_cfg, size, step)
            ratio_min = g["min_ratio"]
        else:
            ps = gates.get("per_step_tokio_vs_rust", {})
            if cfg != "tokio-dpdk" or step not in ps.get("rate_steps", []) or size not in ps.get("sizes", []):
                return None
            mine, ref = rep1_at(report, cfg, size, step), rep1_at(report, ref_cfg, size, step)
            ratio_min = ps["min_ratio"]
        if mine is None or ref is None or ref == 0:
            return (None, "FAIL" if status_gated else "REPORT")
        pct = mine / ref * 100.0
        if not status_gated:
            return (pct, "REPORT")
        return (pct, "PASS" if mine >= ratio_min * ref else "FAIL")
    if "abs_min" in g and step == STEP_700K:
        if size in g.get("sizes", []):
            gated = g.get("enabled", True)
        elif size in g.get("report_only_sizes", []):
            gated = False
        else:
            return None
        mine = median_at(report, cfg, size, step)
        if mine is None:
            return (None, "FAIL" if gated else "REPORT")
        pct = mine / g["abs_min"] * 100.0
        if not gated:
            return (pct, "REPORT")
        return (pct, "PASS" if mine >= g["abs_min"] else "FAIL")
    return None


def evaluate(report, targets, requested):
    """Return (verdict, lines). `requested` has sets: configs, sizes, rate_steps."""
    lines = []
    failed = False
    step = targets.get("step_pps", STEP_700K)
    gates = targets["gates"]

    # 1. Native sanity (invalid-run) rule first.
    inv = targets.get("invalid_run_rule")
    if inv and _applicable(requested, [inv["config"]], inv["size"], step):
        m = median_at(report, inv["config"], inv["size"], step)
        if m is None:
            lines.append(f"RUN INVALID: {inv['config']} @ {inv['size']}B produced no data at {step} pps (infrastructure)")
            return "INVALID", lines
        if m < inv["min_abs"]:
            lines.append(
                f"RUN INVALID: {inv['config']} @ {inv['size']}B median={m:,.0f} < {inv['min_abs']:,} "
                "(bad deployment, redeploy)")
            return "INVALID", lines

    # 2. 700k gates judged on the median of reps 1-3.
    for cfg in ("tokio-dpdk", "rust-dpdk"):
        g = gates.get(cfg)
        if not g:
            continue
        ref_cfg = g["rel_to"]
        for size in g.get("sizes", []) + g.get("report_only_sizes", []):
            gated = size in g.get("sizes", [])
            name = f"{cfg}>={g['min_ratio']:.0%}-of-{ref_cfg}@{size}B"
            if not _applicable(requested, [cfg, ref_cfg], size, step):
                lines.append(f"gate skipped: {name} (config/reference/size not requested)")
                continue
            mine, ref = median_at(report, cfg, size, step), median_at(report, ref_cfg, size, step)
            if mine is None or ref is None:
                msg = f"{cfg} @ {size}B median={mine} target={g['min_ratio']:.0%} of {ref_cfg} median={ref} (missing data)"
                if gated:
                    lines.append(f"GATE FAIL: {msg}")
                    failed = True
                else:
                    lines.append(f"report-only: {msg}")
                continue
            target = g["min_ratio"] * ref
            pct = mine / ref * 100.0
            if mine < target:
                if gated:
                    lines.append(f"GATE FAIL: {cfg} @ {size}B median={mine:,.0f} target={target:,.0f} ({pct:.1f}% of {ref_cfg})")
                    failed = True
                else:
                    lines.append(f"report-only: {cfg} @ {size}B median={mine:,.0f} is {pct:.1f}% of {ref_cfg} (below {g['min_ratio']:.0%}, not gated)")

    # 3. Concurrent floor (report-only unless enabled).
    g = gates.get("tokio-dpdk-concurrent")
    if g:
        cfg = "tokio-dpdk-concurrent"
        enabled = g.get("enabled", True)
        for size in g.get("sizes", []) + g.get("report_only_sizes", []):
            gated = enabled and size in g.get("sizes", [])
            if not _applicable(requested, [cfg], size, step):
                lines.append(f"gate skipped: {cfg}>={g['abs_min']:,}pps@{size}B (config/reference/size not requested)")
                continue
            mine = median_at(report, cfg, size, step)
            if mine is None:
                msg = f"{cfg} @ {size}B median=None target={g['abs_min']:,} (missing data)"
                if gated:
                    lines.append(f"GATE FAIL: {msg}")
                    failed = True
                else:
                    lines.append(f"report-only: {msg}")
            elif mine < g["abs_min"]:
                pct = mine / g["abs_min"] * 100.0
                if gated:
                    lines.append(f"GATE FAIL: {cfg} @ {size}B median={mine:,.0f} target={g['abs_min']:,} ({pct:.1f}%)")
                    failed = True
                else:
                    lines.append(f"report-only: {cfg} @ {size}B median={mine:,.0f} is {pct:.1f}% of {g['abs_min']:,} (not gated)")

    # 4. Per-step rule at 70k/140k/350k (rep 1, single sample).
    ps = gates.get("per_step_tokio_vs_rust")
    if ps:
        ref_cfg = ps["rel_to"]
        for s in ps["rate_steps"]:
            for size in ps["sizes"]:
                if not _applicable(requested, ["tokio-dpdk", ref_cfg], size, s):
                    lines.append(f"gate skipped: per-step tokio-dpdk @ {size}B @ {s} (config/reference/size not requested)")
                    continue
                mine, ref = rep1_at(report, "tokio-dpdk", size, s), rep1_at(report, ref_cfg, size, s)
                if mine is None or ref is None:
                    lines.append(f"GATE FAIL: tokio-dpdk per-step @ {size}B @ {s} rx_pps={mine} vs {ref_cfg} {ref} (missing data)")
                    failed = True
                elif mine < ps["min_ratio"] * ref:
                    lines.append(
                        f"GATE FAIL: tokio-dpdk per-step @ {size}B @ {s} rx_pps={mine:,} < "
                        f"{ps['min_ratio']:.0%} of {ref_cfg} {ref:,}")
                    failed = True

    # 5. Report-only: rust-dpdk vs 0.3.0 absolute (WARN, never fails).
    v = targets.get("report_only_metrics", {}).get("vs_0_3_0")
    if v and v["config"] in requested["configs"] and step in requested["rate_steps"]:
        base = targets.get("baseline_0_3_0", {}).get(v["config"], {})
        for size in v["sizes"]:
            if size not in requested["sizes"] or str(size) not in base:
                continue
            m = median_at(report, v["config"], size, step)
            if m is None:
                continue
            ratio = m / base[str(size)]
            if ratio < v["warn_below_ratio"]:
                lines.append(f"WARN: {v['config']} @ {size}B median={m:,.0f} is {ratio*100:.1f}% of 0.3.0 ({base[str(size)]:,}) — report-only")

    return ("FAIL" if failed else "PASS"), lines


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--report", required=True)
    ap.add_argument("--targets", required=True)
    ap.add_argument("--results-dir", required=True)
    ap.add_argument("--configs", required=True)
    ap.add_argument("--sizes", required=True)
    ap.add_argument("--rate-steps", required=True)
    a = ap.parse_args(argv)
    with open(a.report) as fh:
        report = json.load(fh)
    with open(a.targets) as fh:
        targets = json.load(fh)
    requested = {
        "configs": {c for c in a.configs.split(",") if c},
        "sizes": {int(x) for x in a.sizes.split(",") if x},
        "rate_steps": {int(x) for x in a.rate_steps.split(",") if x},
    }
    verdict, lines = evaluate(report, targets, requested)
    for name in ("gate-verdict.txt", "run-invalid.txt", "gate-lines.md"):
        try:
            os.remove(os.path.join(a.results_dir, name))
        except FileNotFoundError:
            pass
    body = "\n".join(f"- {l}" for l in lines)
    with open(os.path.join(a.results_dir, "gate-lines.md"), "w") as fh:
        fh.write(f"### Perf Gates: {verdict}\n\n{body}\n" if lines else f"### Perf Gates: {verdict}\n")
    if verdict == "INVALID":
        with open(os.path.join(a.results_dir, "run-invalid.txt"), "w") as fh:
            fh.write("\n".join(lines) + "\n")
    else:
        with open(os.path.join(a.results_dir, "gate-verdict.txt"), "w") as fh:
            fh.write(verdict + "\n")
    for l in lines:
        print(l)
    print(f"Perf gate verdict: {verdict}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
