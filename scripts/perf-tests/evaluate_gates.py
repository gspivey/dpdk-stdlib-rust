#!/usr/bin/env python3
"""Evaluate the perf gates in scripts/perf-targets.json against a results dir.

Called by `evaluate_perf_gates` in scripts/run-perf-tests.sh (tokio-udp-perf
tasks 2.4 / 2.4a). It reads the per-(config, rep) TRex result files
(`<config>.rep<N>.json`), takes the per-size median RX pps at the 700k step
across reps, and judges each gate that is *applicable* to the run's requested
configs / packet sizes / rate steps.

It never exits non-zero for a gate miss. Instead it writes, into the results
dir:

  run-invalid.txt   native-dpdk sanity rule missed (bad deployment -> exit 2)
  gate-verdict.txt  PASS | FAIL            (absent when run-invalid.txt is)
  gate-lines.md     GATE FAIL / WARN / gate skipped / report-only lines
  gate-report.json  per-(config,size) median, target, pct, status for the
                    summary's "vs target" column

The shell caller treats a missing / unreadable / non-PASS verdict as FAIL
(fail-closed), so a crash of this script cannot pass a run silently.
"""
import argparse
import glob
import json
import os
import re
import statistics
import sys

REP_FILE_RE = re.compile(r"^(?P<config>.+)\.rep(?P<rep>\d+)\.json$")
NON_RESULT_FILES = {"perf-report.json", "gate-report.json"}


def parse_int_list(text):
    return [int(x) for x in str(text).split(",") if x.strip()]


def load_reps(results_dir):
    """Return {config_name: {rep_number: data}} from `<config>.rep<N>.json`.

    A legacy un-suffixed `<config>.json` (e.g. TCP configs) counts as rep 1.
    The key is the file name's config, not data["config_name"], so rep files of
    one config can never overwrite each other (D9 keying fix)."""
    reps = {}
    for path in sorted(glob.glob(os.path.join(results_dir, "*.json"))):
        base = os.path.basename(path)
        if base in NON_RESULT_FILES:
            continue
        m = REP_FILE_RE.match(base)
        if m:
            config, rep = m.group("config"), int(m.group("rep"))
        else:
            config, rep = base[: -len(".json")], 1
        try:
            with open(path) as fh:
                data = json.load(fh)
        except (OSError, ValueError):
            continue
        if not isinstance(data, dict):
            continue
        reps.setdefault(config, {})[rep] = data
    return reps


def step_rows(data, size, step):
    """Rows of `data` for packet `size` at target rate `step`."""
    results = data.get("results", {})
    if not isinstance(results, dict):
        return []
    return [r for r in results.get(f"{size}B", []) if r.get("target_pps") == step]


def median_rx(reps_for_config, size, step):
    """(median rx_pps, n_samples) over all reps at (size, step); (None, 0) if none."""
    vals = []
    for rep in sorted(reps_for_config):
        for row in step_rows(reps_for_config[rep], size, step):
            if isinstance(row.get("rx_pps"), (int, float)):
                vals.append(row["rx_pps"])
    if not vals:
        return None, 0
    return statistics.median(vals), len(vals)


def rep1_rx(reps_for_config, size, step):
    """Single-sample rx_pps from rep 1 (the full sweep), or None."""
    rep1 = reps_for_config.get(1)
    if rep1 is None:
        return None
    vals = [r["rx_pps"] for r in step_rows(rep1, size, step)
            if isinstance(r.get("rx_pps"), (int, float))]
    return vals[0] if vals else None


def fmt(n):
    return f"{n:,.0f}"


def evaluate(reps, targets, configs, sizes, steps, results_dir=None):
    """Pure evaluation. Returns a dict:
      invalid: str|None   reason when the run is INVALID (exit 2)
      verdict: "PASS"|"FAIL"|None
      lines:   list[str]  markdown lines for the summary
      entries: list[dict] per-(config,size) rows for "vs target"
      medians: {config: {size: {"median": x, "n": k}}}
    """
    step = targets["step_pps"]
    step_requested = step in steps
    lines, entries, fails = [], [], []

    medians = {}
    for config in configs:
        for size in sizes:
            m, n = median_rx(reps.get(config, {}), size, step)
            if m is not None:
                medians.setdefault(config, {})[str(size)] = {"median": m, "n": n}

    def med(config, size):
        return medians.get(config, {}).get(str(size), {}).get("median")

    # ── native-dpdk sanity (invalid_run_rule) — applied FIRST ────────────────
    rule = targets.get("invalid_run_rule")
    if rule and rule["config"] in configs and rule["size"] in sizes and step_requested:
        m = med(rule["config"], rule["size"])
        if m is None:
            return {"invalid": f"{rule['config']} @ {rule['size']}B produced no data "
                               f"(testpmd failed to run) — bad deployment",
                    "verdict": None, "lines": lines, "entries": entries, "medians": medians}
        if m < rule["min_abs"]:
            return {"invalid": f"{rule['config']} @ {rule['size']}B median={fmt(m)} < "
                               f"{fmt(rule['min_abs'])} pps — bad deployment, run INVALID",
                    "verdict": None, "lines": lines, "entries": entries, "medians": medians}

    # ── ratio / absolute gates at the 700k step ──────────────────────────────
    for gate_name, gate in targets["gates"].items():
        if gate_name == "per_step_tokio_vs_rust":
            continue
        gated_sizes = gate.get("sizes", [])
        report_sizes = gate.get("report_only_sizes", [])
        enabled = gate.get("enabled", True)
        rel_to = gate.get("rel_to")
        for size in gated_sizes + report_sizes:
            report_only = (size in report_sizes) or not enabled
            applicable = (gate_name in configs and size in sizes and step_requested
                          and (rel_to is None or rel_to in configs))
            if not applicable:
                lines.append(f"gate skipped: {gate_name} @ {size}B "
                             f"(config/reference/size not requested)")
                continue
            m = med(gate_name, size)
            ref = med(rel_to, size) if rel_to else None
            if rel_to:
                target = gate["min_ratio"] * ref if ref is not None else None
            else:
                target = gate["abs_min"]
            if m is None or target is None:
                status = "REPORT-ONLY" if report_only else "FAIL"
                what = "no data" if m is None else f"no {rel_to} reference data"
                if not report_only:
                    fails.append(f"GATE FAIL: {gate_name} @ {size}B {what} "
                                 f"(median={'n/a' if m is None else fmt(m)} "
                                 f"target={'n/a' if target is None else fmt(target)})")
                else:
                    lines.append(f"report-only: {gate_name} @ {size}B {what}")
                entries.append({"gate": gate_name, "config": gate_name, "size": size,
                                "median": m, "target": target, "pct": None,
                                "status": status})
                continue
            pct = m / target * 100.0
            ok = m >= target
            if report_only:
                status = "REPORT-ONLY"
                ref_txt = f", {m / ref * 100:.1f}% of {rel_to}" if rel_to else ""
                lines.append(f"report-only: {gate_name} @ {size}B median={fmt(m)} "
                             f"target={fmt(target)} ({pct:.1f}% of target{ref_txt})")
            elif ok:
                status = "PASS"
            else:
                status = "FAIL"
                ref_txt = f", {m / ref * 100:.1f}% of {rel_to}" if rel_to else ""
                fails.append(f"GATE FAIL: {gate_name} @ {size}B median={fmt(m)} "
                             f"target={fmt(target)} ({pct:.1f}% of target{ref_txt})")
            entries.append({"gate": gate_name, "config": gate_name, "size": size,
                            "median": m, "target": target, "pct": pct,
                            "status": status})

    # ── per-step rule (70k/140k/350k only; 700k excluded — MEDIUM-7) ─────────
    ps = targets["gates"].get("per_step_tokio_vs_rust")
    if ps:
        cfg, ref_cfg, ratio = ps.get("config", "tokio-dpdk"), ps["rel_to"], ps["min_ratio"]
        for rate in ps["rate_steps"]:
            for size in ps["sizes"]:
                if not (cfg in configs and ref_cfg in configs
                        and size in sizes and rate in steps):
                    lines.append(f"gate skipped: per_step_tokio_vs_rust @ {size}B @ {rate} "
                                 f"(config/reference/size not requested)")
                    continue
                x = rep1_rx(reps.get(cfg, {}), size, rate)
                y = rep1_rx(reps.get(ref_cfg, {}), size, rate)
                if x is None or y is None:
                    fails.append(f"GATE FAIL: {cfg} per-step @ {size}B @ {rate} "
                                 f"missing data (rx_pps={'n/a' if x is None else fmt(x)} "
                                 f"{ref_cfg}={'n/a' if y is None else fmt(y)})")
                elif x < ratio * y:
                    fails.append(f"GATE FAIL: {cfg} per-step @ {size}B @ {rate} "
                                 f"rx_pps={fmt(x)} < {ratio * 100:.0f}% of {ref_cfg} {fmt(y)}")

    # ── vs 0.3.0 absolute — REPORT-ONLY, WARN below warn_below_ratio ─────────
    v = targets.get("report_only_metrics", {}).get("vs_0_3_0")
    if v and v["config"] in configs and step_requested:
        base = targets.get("baseline_0_3_0", {}).get(v["config"], {})
        for size in v["sizes"]:
            m = med(v["config"], size)
            if size not in sizes or m is None or str(size) not in base:
                continue
            r = m / base[str(size)]
            if r < v["warn_below_ratio"]:
                lines.append(f"WARN: {v['config']} @ {size}B median={fmt(m)} is "
                             f"{r * 100:.1f}% of 0.3.0 ({fmt(base[str(size)])}) — "
                             f"report-only, never gates")

    # ── idle CPU (PR 5) — judged only when enabled AND the 0-pps step ran ────
    idle = targets.get("idle_cpu")
    if idle and idle.get("enabled") and idle["config"] in configs:
        data = reps.get(idle["config"], {}).get(1, {})
        pct = data.get("idle_cpu_pct")
        if pct is None:
            lines.append("gate skipped: idle_cpu (0-pps step did not run)")
        elif pct > idle["max_pct_of_one_vcpu"]:
            fails.append(f"GATE FAIL: {idle['config']} idle cpu={pct:.1f}% of one vCPU "
                         f"target<={idle['max_pct_of_one_vcpu']}%")

    for f in fails:
        lines.insert(0, f)
    return {"invalid": None, "verdict": "FAIL" if fails else "PASS",
            "lines": lines, "entries": entries, "medians": medians}


def write_outputs(results_dir, outcome):
    for stale in ("run-invalid.txt", "gate-verdict.txt"):
        try:
            os.remove(os.path.join(results_dir, stale))
        except FileNotFoundError:
            pass
    with open(os.path.join(results_dir, "gate-lines.md"), "w") as fh:
        body = list(outcome["lines"])
        if outcome["invalid"]:
            body.insert(0, f"RUN INVALID: {outcome['invalid']}")
        fh.write("\n".join(f"- {l}" for l in body) + ("\n" if body else ""))
    with open(os.path.join(results_dir, "gate-report.json"), "w") as fh:
        json.dump({"entries": outcome["entries"], "medians": outcome["medians"],
                   "verdict": outcome["verdict"], "invalid": outcome["invalid"]}, fh, indent=2)
    if outcome["invalid"]:
        with open(os.path.join(results_dir, "run-invalid.txt"), "w") as fh:
            fh.write(outcome["invalid"] + "\n")
    else:
        with open(os.path.join(results_dir, "gate-verdict.txt"), "w") as fh:
            fh.write(outcome["verdict"] + "\n")


def judged_configs(targets):
    """Configs the gates judge or use as a reference (reps 2-3 run for these)."""
    out = set()
    for name, gate in targets["gates"].items():
        out.add(gate.get("config", name))
        if gate.get("rel_to"):
            out.add(gate["rel_to"])
    rule = targets.get("invalid_run_rule")
    if rule:
        out.add(rule["config"])
    return out


def rep_plan(targets, configs, sizes, steps):
    """What reps 2-3 re-run at the 700k step: (configs, sizes), both empty when
    700k was not requested. Configs = requested ∩ judged; sizes = requested ∩
    the union of every gate's sizes (so never 8500 with default inputs)."""
    if targets["step_pps"] not in steps:
        return [], []
    gate_sizes = set()
    for gate in targets["gates"].values():
        gate_sizes.update(gate.get("sizes", []))
        gate_sizes.update(gate.get("report_only_sizes", []))
    judged = judged_configs(targets)
    return ([c for c in configs if c in judged],
            [s for s in sizes if s in gate_sizes])


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("command", choices=["evaluate", "rep-plan"],
                    help="evaluate: write verdict files; rep-plan: print the reps 2-3 plan")
    ap.add_argument("--results-dir")
    ap.add_argument("--targets", required=True)
    ap.add_argument("--configs", required=True, help="comma list of requested configs")
    ap.add_argument("--packet-sizes", required=True, help="comma list of requested sizes")
    ap.add_argument("--rate-steps", required=True, help="comma list of requested rate steps")
    args = ap.parse_args(argv)

    with open(args.targets) as fh:
        targets = json.load(fh)
    configs = [c for c in args.configs.split(",") if c]
    if args.command == "rep-plan":
        rc, rs = rep_plan(targets, configs, parse_int_list(args.packet_sizes),
                          parse_int_list(args.rate_steps))
        print(f"REP_CONFIGS={','.join(rc)}")
        print(f"REP_SIZES={','.join(str(x) for x in rs)}")
        return 0
    if not args.results_dir:
        ap.error("evaluate requires --results-dir")
    outcome = evaluate(load_reps(args.results_dir), targets, configs,
                       parse_int_list(args.packet_sizes), parse_int_list(args.rate_steps))
    write_outputs(args.results_dir, outcome)
    if outcome["invalid"]:
        print(f"RUN INVALID: {outcome['invalid']}")
    else:
        print(f"Perf gate verdict: {outcome['verdict']}")
    for l in outcome["lines"]:
        print(l)
    return 0


if __name__ == "__main__":
    sys.exit(main())
