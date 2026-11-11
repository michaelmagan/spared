#!/usr/bin/env python3
"""Summary of bench/native-baseline.sh: medians across runs of warm latency, cold start and memory, and memory fits total(N) = fixed + N x slope
(least squares over every run, as bench/demo/mem-curve-fit.py does). Reads bench/results/native-{warm,cold,memory}-*.json and
container-*.json; writes bench/results/native-baseline-summary.json and prints a table."""
import glob, json, os, statistics

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
R = os.path.join(ROOT, "bench", "results")
med = statistics.median
out = {"warm_us": {}, "cold_first_byte_us": {}, "memory_kb": {}, "memory_fits_kb": {}, "container_memory_kb": {}, "container_fits_kb": {}}


def load(pat):
    return [(f, json.load(open(f))) for f in sorted(glob.glob(os.path.join(R, pat)))]


def fit(xy):
    n = len(xy); mx = sum(x for x, _ in xy) / n; my = sum(y for _, y in xy) / n
    b = sum((x - mx) * (y - my) for x, y in xy) / sum((x - mx) ** 2 for x, _ in xy)
    a = my - b * mx
    return {"fixed_kb": round(a), "slope_kb": round(b, 1), "max_abs_residual_kb": round(max(abs(y - a - b * x) for x, y in xy)), "points": n}


for t in ("spared", "native"):
    for c in (1, 16):
        runs = [d for _, d in load(f"native-warm-{t}-c{c}-run*.json")]
        if runs:
            out["warm_us"][f"{t}_c{c}"] = {k: med(d["first_byte_us"][k] for d in runs) for k in ("p50", "p99")} | {"runs": len(runs), "errors": sum(d["errors"] for d in runs)}
for t, pat in (("spared", "native-cold-spared-run*.json"), ("native", "native-cold-native-run*.json"), ("container", "container-cold-run*.json")):
    runs = [d for _, d in load(pat)]
    if runs:
        out["cold_first_byte_us"][t] = {k: med(d["first_byte_us"][k] for d in runs) for k in ("p50", "p99")} | {"runs": len(runs), "samples_per_run": runs[0].get("samples", runs[0].get("requests")), "errors": sum(d["errors"] for d in runs)}

pts = {}
for f, d in load("native-memory-*-n*-run[0-9].json"):
    pts.setdefault(d["target"], []).append(d)
for t, ds in pts.items():
    for st in ("first", "steady"):
        by_n = {}
        for d in ds:
            by_n.setdefault(d["n"], []).append(d["snapshots_kb"][st])
        out["memory_kb"].setdefault(t, {})[st] = {str(n): {a: med(s[a] for s in v) for a in ("Pss", "RssAnon", "RssFile")} | {"runs": len(v)} for n, v in sorted(by_n.items())}
        for a in ("Pss", "Pss_Anon", "Pss_File", "RssAnon", "RssFile"):
            xy = [(d["n"], d["snapshots_kb"][st][a]) for d in ds]
            if len({x for x, _ in xy}) >= 2:
                out["memory_fits_kb"].setdefault(t, {}).setdefault(st, {})[a] = fit(xy)
    out["memory_kb"][t]["errors"] = sum(d["errors"] for d in ds)

cds = [d for _, d in load("container-memory-n*-run*.json")]
if cds:
    by_n = {}
    for d in cds:
        by_n.setdefault(d["n"], []).append(d)
    for n, v in sorted(by_n.items()):
        row = {"runs": len(v), "start_to_first_byte_us_p50": med(d["start_to_first_byte_us_p50"] for d in v),
               "daemons_before_pss": med(d["daemons_before_kb"]["Pss"] for d in v)}
        for g in ("app", "shim", "proxy", "daemons"):
            row[g] = {a: med(d["snapshots_kb"]["first"][g][a] for d in v) for a in ("Pss", "RssAnon", "VmRSS")}
        row["per_container_pss"] = med((d["snapshots_kb"]["first"]["app"]["Pss"] + d["snapshots_kb"]["first"]["shim"]["Pss"] + d["snapshots_kb"]["first"]["proxy"]["Pss"]) / n for d in v)
        row["cgroup_sum"] = {k: med(d["snapshots_kb"]["first"]["cgroup_sum_kb"][k] for d in v) for k in ("usage", "rss", "cache", "mapped_file")}
        out["container_memory_kb"][str(n)] = row
    for g in ("app", "shim", "proxy", "daemons", "all"):
        for a in ("Pss", "RssAnon"):
            xy = [(d["n"], sum(d["snapshots_kb"]["first"][x][a] for x in (("app", "shim", "proxy", "daemons") if g == "all" else (g,)))) for d in cds]
            if len({x for x, _ in xy}) >= 2:
                out["container_fits_kb"].setdefault(g, {})[a] = fit(xy)

json.dump(out, open(os.path.join(R, "native-baseline-summary.json"), "w"), indent=1)
for k, v in out["warm_us"].items():
    print(f"warm {k:12} p50 {v['p50']:6} p99 {v['p99']:6} us ({v['runs']} runs, {v['errors']} errors)")
for k, v in out["cold_first_byte_us"].items():
    print(f"cold {k:12} p50 {v['p50']:8} p99 {v['p99']:8} us ({v['runs']} runs, {v['errors']} errors)")
for t, fs in out["memory_fits_kb"].items():
    for a in ("Pss", "RssAnon", "RssFile"):
        f = fs["first"][a]
        print(f"memory {t:16} first {a:8} {f['fixed_kb'] / 1024:6.1f} MiB + N x {f['slope_kb']:7.1f} KiB (res {f['max_abs_residual_kb']})")
for g, fs in out["container_fits_kb"].items():
    for a, f in fs.items():
        print(f"container {g:8} {a:8} {f['fixed_kb'] / 1024:6.1f} MiB + N x {f['slope_kb']:7.1f} KiB (res {f['max_abs_residual_kb']})")
for n, row in out["container_memory_kb"].items():
    print(f"container n={n}: per container (app+shim+proxy) {row['per_container_pss']:.0f} KiB PSS, start p50 {row['start_to_first_byte_us_p50']} us")
