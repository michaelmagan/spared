#!/usr/bin/env python3
"""Fit total(N) = fixed + N x slope to bench/results/memory-curve-*.json (docs/memory-methodology.md).
Least squares over every run with N >= 1, per target, state and accounting; N = 0 medians as a check; crossover N of each spared
curve against workerd; apps that fit in 1 and 4 GiB. Prints a table, writes bench/results/mcurve-passA-summary.json."""
import glob, json, os, re, statistics

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
R = os.path.join(ROOT, "bench", "results")
ACC = ("Pss", "Pss_Anon", "Pss_File", "RssAnon", "RssFile")
STATES = ("startup", "first", "steady", "idle15", "idle65")
pts = {}
for f in sorted(glob.glob(os.path.join(R, "mcurve-passA-*-n*-run[0-9].json"))):
    d = json.load(open(f))
    if d["run"] == 0:
        continue
    pts.setdefault(d["target"], []).append(d)


def fit(xy):
    n = len(xy); mx = sum(x for x, _ in xy) / n; my = sum(y for _, y in xy) / n
    sxx = sum((x - mx) ** 2 for x, _ in xy)
    b = sum((x - mx) * (y - my) for x, y in xy) / sxx
    a = my - b * mx
    res = [y - (a + b * x) for x, y in xy]
    return {"fixed_kb": round(a), "slope_kb": round(b, 1), "max_abs_residual_kb": round(max(abs(r) for r in res)), "points": n}


zero = {}
for t in ("spared-distinct", "workerd-empty"):
    for st in ("first",):
        z = [d["snapshots_kb"][st] for d in pts.get(t, []) if d["n"] == 0]
        if z:
            zero[t.split("-")[0]] = {a: statistics.median(s[a] for s in z) for a in ACC}
out = {"zero_app_server_kb": zero, "fits": {}, "medians_kb": {}, "crossover_n": {}, "apps_per_budget": {}}
for t, ds in pts.items():
    if t in ("workerd-empty",):
        continue
    for st in STATES:
        for a in ACC:
            xy = [(d["n"], d["snapshots_kb"][st][a]) for d in ds if d["n"] >= 1 and st in d["snapshots_kb"]]
            if len({x for x, _ in xy}) >= 3:
                out["fits"].setdefault(t, {}).setdefault(st, {})[a] = fit(xy)
        by_n = {}
        for d in ds:
            if st in d["snapshots_kb"]:
                by_n.setdefault(d["n"], []).append(d["snapshots_kb"][st])
        out["medians_kb"].setdefault(t, {})[st] = {str(n): {a: statistics.median(s[a] for s in v) for a in ACC} | {"runs": len(v)} for n, v in sorted(by_n.items())}

W = out["fits"].get("workerd-distinct", {})
for t in [t for t in out["fits"] if t.startswith("spared")]:
    for st in ("first", "steady", "idle65"):
        for a in ("Pss", "RssAnon", "Pss_Anon"):
            s, w = out["fits"][t].get(st, {}).get(a), W.get(st, {}).get(a)
            if not (s and w):
                continue
            ds_, dw = s["slope_kb"] - w["slope_kb"], w["fixed_kb"] - s["fixed_kb"]
            x = dw / ds_ if ds_ else None
            out["crossover_n"].setdefault(t, {}).setdefault(st, {})[a] = (round(x) if x and x > 0 else None, "spared lower below this N" if ds_ > 0 and dw > 0 else
                                                                          "spared lower at every N" if ds_ <= 0 and dw >= 0 else "workerd lower below this N" if ds_ < 0 else "workerd lower at every N")
for t, fs in out["fits"].items():
    for st in ("first", "steady", "idle65"):
        for a in ("Pss", "RssAnon"):
            f = fs.get(st, {}).get(a)
            if f and f["slope_kb"] > 0:
                out["apps_per_budget"].setdefault(t, {}).setdefault(st, {})[a] = {g: int((g * 1048576 - f["fixed_kb"]) / f["slope_kb"]) for g in (1, 4)}
json.dump(out, open(os.path.join(R, "mcurve-passA-summary.json"), "w"), indent=1)

for t, fs in out["fits"].items():
    for st, acc in fs.items():
        print(f"{t:18} {st:8} " + "  ".join(f"{a} {f['fixed_kb']/1024:6.1f} MB + N x {f['slope_kb']:7.1f} KB (res {f['max_abs_residual_kb']})" for a, f in acc.items() if a in ("Pss", "RssAnon", "RssFile")))
print("zero-app", json.dumps(zero))
print("crossover", json.dumps(out["crossover_n"], indent=0))
print("apps per GiB", json.dumps(out["apps_per_budget"]))
