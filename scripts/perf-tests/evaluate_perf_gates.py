#!/usr/bin/env python3
"""Perf gate evaluator for scripts/run-perf-tests.sh (tokio-udp-perf tasks 2.4/2.4a).

Reads ``perf-report.json`` (written by ``aggregate_results``) and
``scripts/perf-targets.json`` and judges the 700k step by the per-(config, size)
MEDIAN over reps. Outputs, into ``--out-dir``:

  gate-results.json   rows consumed by the markdown summary ("vs target" column)
  gate-verdict.txt    ``PASS`` | ``FAIL``
  run-invalid.txt     present only when the native-dpdk sanity rule trips (exit 2)

Subcommands:
  evaluate    judge the report
  reps-plan   print the judged configs / sizes for the 700k rep passes

Gate applicability: a gate is judged only for (config, size) pairs where the gated
config, its ``rel_to`` reference and the size were all *requested*; otherwise it is
reported as ``gate skipped`` and never fails. For an applicable gate, missing data
is a FAIL, not a skip.
"""
import argparse
import json
import os
import statistics
import sys


def _csv(value):
    return [v.strip() for v in value.split(",") if v.strip()]


def _int_csv(value):
    return [int(v) for v in _csv(value)]


def size_key(size):
    return f"{size}B"


def median_700k(report, config, size, step):
    """Median rx_pps at ``step`` for (config, size) over every rep, or None."""
    values = (
        report.get("rep_values", {}).get(config, {}).get(size_key(size), {}).get(str(step))
    )
    if not values:
        return None
    return statistics.median(values)


def rep1_rx_pps(report, config, size, step):
    """Rep-1 rx_pps for a single (config, size, step); None when absent."""
    rows = report.get("configs", {}).get(config, {}).get("results", {})
    if not isinstance(rows, dict):
        return None
    for r in rows.get(size_key(size), []):
        if r.get("target_pps") == step:
            return r.get("rx_pps")
    return None


def judged_configs(targets):
    """Configs that carry a 700k gate, plus every ``rel_to`` reference they need."""
    names = []
    for name, gate in targets["gates"].items():
        for n in (name, gate.get("rel_to")):
            if n and n not in names:
                names.append(n)
    return names


def reps_plan(targets, configs, sizes, steps):
    """(configs, sizes) for the extra 700k reps, derived from the requested lists.

    Empty when the judged step was not requested. Sizes are the requested sizes
    intersected with the union of every gate's sizes (so 8500 never appears).
    """
    if targets["judged_step"] not in steps:
        return [], []
    gate_sizes = set()
    for gate in targets["gates"].values():
        gate_sizes.update(gate.get("sizes", []))
        gate_sizes.update(gate.get("report_only_sizes", []))
    plan_configs = [c for c in configs if c in judged_configs(targets)]
    plan_sizes = [s for s in sizes if s in gate_sizes]
    if not plan_configs or not plan_sizes:
        return [], []
    return plan_configs, plan_sizes


def _row(kind, gate, config, size, step, value, target, status, note=""):
    pct = (value / target * 100.0) if (value is not None and target) else None
    return {
        "kind": kind,
        "gate": gate,
        "config": config,
        "size": size,
        "step": step,
        "value": value,
        "target": target,
        "pct": pct,
        "status": status,
        "note": note,
    }


def evaluate(report, targets, configs, sizes, steps):
    """Return dict(verdict, invalid, rows, lines, skipped, warnings)."""
    rows, lines, skipped, warnings = [], [], [], []
    failed = False
    step = targets["judged_step"]
    judged_700k = step in steps

    # ── native-dpdk sanity (invalid_run_rule) — applied first ────────────────
    rule = targets.get("invalid_run_rule")
    if rule and judged_700k and rule["config"] in configs and rule["size"] in sizes:
        med = median_700k(report, rule["config"], rule["size"], step)
        if med is None or med < rule["min_abs"]:
            shown = "no data" if med is None else f"{med:,.0f}"
            lines.append(
                f"RUN INVALID: {rule['config']} @ {rule['size']}B median={shown} "
                f"< {rule['min_abs']:,} (bad deployment, redeploy)"
            )
            return {
                "verdict": "INVALID",
                "invalid": True,
                "rows": rows,
                "lines": lines,
                "skipped": skipped,
                "warnings": warnings,
            }

    # ── 700k median gates ────────────────────────────────────────────────────
    for name, gate in targets["gates"].items():
        ref = gate.get("rel_to")
        all_sizes = list(gate.get("sizes", [])) + list(gate.get("report_only_sizes", []))
        if not judged_700k or name not in configs or (ref and ref not in configs) or not any(
            s in sizes for s in all_sizes
        ):
            skipped.append(f"gate skipped: {name} (config/reference/size not requested)")
            continue
        enabled = gate.get("enabled", True)
        for size in all_sizes:
            if size not in sizes:
                continue
            judged = enabled and size in gate.get("sizes", [])
            med = median_700k(report, name, size, step)
            if ref:
                ref_med = median_700k(report, ref, size, step)
                target = None if ref_med is None else gate["min_ratio"] * ref_med
            else:
                target = gate["abs_min"]
            if med is None or target is None:
                status = "FAIL" if judged else "REPORT"
                rows.append(_row("gate", name, name, size, step, med, target, status, "missing data"))
                if judged:
                    failed = True
                    lines.append(f"GATE FAIL: {name} @ {size}B missing data (median={med} target={target})")
                continue
            ok = med >= target
            status = ("PASS" if ok else "FAIL") if judged else "REPORT"
            rows.append(_row("gate", name, name, size, step, med, target, status))
            if judged and not ok:
                failed = True
                lines.append(
                    f"GATE FAIL: {name} @ {size}B median={med:,.0f} target={target:,.0f} "
                    f"({med / target * 100:.1f}%)"
                )

    # ── vs 0.3.0 absolute: WARN-only, never gates ────────────────────────────
    vs = targets.get("report_only_metrics", {}).get("vs_0_3_0")
    base = targets.get("baseline_0_3_0")
    if vs and base and judged_700k and base["config"] in configs:
        for size_s, base_pps in base["rx_pps"].items():
            size = int(size_s)
            if size not in sizes:
                continue
            med = median_700k(report, base["config"], size, step)
            if med is not None and med < vs["warn_below_ratio"] * base_pps:
                warnings.append(
                    f"WARN: {base['config']} @ {size}B median={med:,.0f} is "
                    f"{med / base_pps * 100:.1f}% of 0.3.0 baseline {base_pps:,} (report-only)"
                )

    # ── per-step tokio vs rust (70k/140k/350k only, rep 1 single sample) ─────
    ps = targets.get("per_step_tokio_vs_rust")
    if ps:
        cfg, ref = ps["config"], ps["rel_to"]
        req_steps = [s for s in ps["rate_steps"] if s in steps]
        req_sizes = [s for s in ps["sizes"] if s in sizes]
        if cfg not in configs or ref not in configs or not req_steps or not req_sizes:
            skipped.append(f"gate skipped: per_step_tokio_vs_rust (config/reference/size not requested)")
        else:
            for size in req_sizes:
                for st in req_steps:
                    val = rep1_rx_pps(report, cfg, size, st)
                    ref_val = rep1_rx_pps(report, ref, size, st)
                    target = None if ref_val is None else ps["min_ratio"] * ref_val
                    if val is None or target is None:
                        failed = True
                        rows.append(_row("per_step", "per_step_tokio_vs_rust", cfg, size, st, val, target, "FAIL", "missing data"))
                        lines.append(
                            f"GATE FAIL: {cfg} per-step @ {size}B @ {st} missing data "
                            f"(rx_pps={val} target={target})"
                        )
                        continue
                    ok = val >= target
                    rows.append(_row("per_step", "per_step_tokio_vs_rust", cfg, size, st, val, target, "PASS" if ok else "FAIL"))
                    if not ok:
                        failed = True
                        lines.append(
                            f"GATE FAIL: {cfg} per-step @ {size}B @ {st} rx_pps={val:,.0f} "
                            f"target={target:,.0f} ({val / target * 100:.1f}%)"
                        )

    return {
        "verdict": "FAIL" if failed else "PASS",
        "invalid": False,
        "rows": rows,
        "lines": lines,
        "skipped": skipped,
        "warnings": warnings,
    }


def write_outputs(result, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    # Clear stale verdicts first so a crash below can never pass on old data.
    for name in ("gate-verdict.txt", "run-invalid.txt"):
        try:
            os.remove(os.path.join(out_dir, name))
        except FileNotFoundError:
            pass
    with open(os.path.join(out_dir, "gate-results.json"), "w") as fh:
        json.dump(result, fh, indent=2)
    if result["invalid"]:
        with open(os.path.join(out_dir, "run-invalid.txt"), "w") as fh:
            fh.write("\n".join(result["lines"]) + "\n")
    else:
        with open(os.path.join(out_dir, "gate-verdict.txt"), "w") as fh:
            fh.write(result["verdict"])


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("evaluate", "reps-plan"):
        sp = sub.add_parser(name)
        sp.add_argument("--targets", required=True)
        sp.add_argument("--configs", required=True)
        sp.add_argument("--sizes", required=True)
        sp.add_argument("--steps", required=True)
        if name == "evaluate":
            sp.add_argument("--report", required=True)
            sp.add_argument("--out-dir", required=True)
    args = p.parse_args(argv)

    with open(args.targets) as fh:
        targets = json.load(fh)
    configs, sizes, steps = _csv(args.configs), _int_csv(args.sizes), _int_csv(args.steps)

    if args.cmd == "reps-plan":
        plan_configs, plan_sizes = reps_plan(targets, configs, sizes, steps)
        print(",".join(plan_configs))
        print(",".join(str(s) for s in plan_sizes))
        return 0

    with open(args.report) as fh:
        report = json.load(fh)
    result = evaluate(report, targets, configs, sizes, steps)
    write_outputs(result, args.out_dir)
    for line in result["lines"] + result["warnings"] + result["skipped"]:
        print(line)
    print(f"gate verdict: {result['verdict']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
