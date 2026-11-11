#!/usr/bin/env python3
"""Summarize bench/workloads/run.sh output: per workload and target, the median across runs of p50, p99 and server CPU per
request; ratio = workerd p50 / spared p50 (above 1 means spared answered faster). Writes <prefix>-summary.json and prints a
markdown table sorted by ratio."""
import argparse
import glob
import json
import os
import statistics

ap = argparse.ArgumentParser()
ap.add_argument("--results", default=os.path.join(os.path.dirname(__file__), "../results"))
ap.add_argument("--prefix", default="workloads")
ap.add_argument("--rounds", type=int, default=3)
a = ap.parse_args()

runs = {}
for f in sorted(glob.glob(os.path.join(a.results, f"{a.prefix}-*-run*.json"))):
    d = json.load(open(f))
    if "workload" not in d:
        continue
    runs.setdefault((d["workload"], d["target"]), []).append(d)

med = lambda xs: statistics.median(xs) if xs else None
rows = []
for wl in sorted({k[0] for k in runs}):
    row = {"workload": wl}
    for t in ("spared", "workerd"):
        rs = sorted(runs.get((wl, t), []), key=lambda d: d["run"])
        row[t] = {
            "runs": len(rs),
            "requests_per_run": rs[0]["requests"] if rs else None,
            "p50_us_runs": [d["full_body_us"]["p50"] for d in rs],
            "p99_us_runs": [d["full_body_us"]["p99"] for d in rs],
            "cpu_us_runs": [d["server_cpu_us_per_request"] for d in rs],
            "p50_us": med([d["full_body_us"]["p50"] for d in rs]),
            "p99_us": med([d["full_body_us"]["p99"] for d in rs]),
            "cpu_us_per_request": med([d["server_cpu_us_per_request"] for d in rs]),
            "errors": sum(d["errors"] for d in rs),
            "all_quiet": all(d["quiet"] for d in rs),
            "loadavg1_before": [d["loadavg1_before"] for d in rs],
        }
    s, w = row["spared"]["p50_us"], row["workerd"]["p50_us"]
    row["ratio_workerd_over_spared_p50"] = round(w / s, 2) if s and w else None
    sc, wc = row["spared"]["cpu_us_per_request"], row["workerd"]["cpu_us_per_request"]
    row["ratio_workerd_over_spared_cpu"] = round(wc / sc, 2) if sc and wc else None
    rows.append(row)
rows.sort(key=lambda r: -(r["ratio_workerd_over_spared_p50"] or 0))
json.dump({"method": "median across runs of each run's p50/p99 (full body, microseconds) and server CPU per request "
           "(schedstat, all threads); ratio = workerd/spared", "workloads": rows},
          open(os.path.join(a.results, f"{a.prefix}-summary.json"), "w"), indent=1)

ms = lambda us: f"{us / 1000:.2f}" if us is not None else "-"
print("| workload | spared p50 ms | workerd p50 ms | ratio (workerd/spared) | spared p99 ms | workerd p99 ms | spared CPU ms | workerd CPU ms | quiet |")
print("|---|---|---|---|---|---|---|---|---|")
for r in rows:
    s, w = r["spared"], r["workerd"]
    q = "yes" if s["all_quiet"] and w["all_quiet"] else "no"
    print(f"| {r['workload']} | {ms(s['p50_us'])} | {ms(w['p50_us'])} | {r['ratio_workerd_over_spared_p50']} | {ms(s['p99_us'])} | "
          f"{ms(w['p99_us'])} | {ms(s['cpu_us_per_request'])} | {ms(w['cpu_us_per_request'])} | {q} |")
