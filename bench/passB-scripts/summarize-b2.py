#!/usr/bin/env python3
"""Old/new tables for B2 from bench/results/b2-passB-* and b2w-passB-*. Ratio = old / new, per round then median [min-max]; above 1 the new build is lower (faster / smaller)."""
import glob, json, statistics as st
import os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
J = lambda p: json.load(open(p))
def fmt(v): return f"{st.median(v):.4g} [{min(v):.4g}-{max(v):.4g}]"
def row(name, old, new):
    ratios = [o / n for o, n in zip(old, new)]
    print(f"| {name} | {fmt(old)} | {fmt(new)} | {fmt(ratios)} |")
print("| metric | old | new | old/new per round |\n|---|---|---|---|")
for c in (1, 16):
    for k in ("p50", "p99"):
        o = [J(f"{R}b2-passB-old-warm-c{c}-run{r}.json")["first_byte_us"][k] for r in range(1, 6)]
        n = [J(f"{R}b2-passB-new-warm-c{c}-run{r}.json")["first_byte_us"][k] for r in range(1, 6)]
        row(f"warm c{c} {k} first byte us", o, n)
rows = [json.loads(l) for l in open(R + "b2-passB-wake.jsonl")]
for j in ("WAL", "TRUNCATE"):
    for k in ("p50", "p99"):
        o = [x["wake_to_first_query_us"][k] for x in sorted(rows, key=lambda x: x["run"]) if x["variant"] == "old" and x["journal"] == j]
        n = [x["wake_to_first_query_us"][k] for x in sorted(rows, key=lambda x: x["run"]) if x["variant"] == "new" and x["journal"] == j]
        row(f"wake in process {j} {k} us", o, n)
for n_ in (1, 10, 50):
    for field in ("Pss", "RssAnon", "RssFile"):
        def get(v):
            out = []
            for r in range(1, 6):
                d = J(f"{R}b2-passB-mcurve-{v}-spared-distinct-n{n_}-run{r}.json")
                s = d["snapshots_kb"]["first"]
                out.append(next(iter(x for k, x in s.items() if k.lower() == field.lower()), None) if isinstance(s, dict) else None)
            return out
        o, nw = get("old"), get("new")
        if None in o or None in nw: print("mcurve field layout:", J(f"{R}b2-passB-mcurve-old-spared-distinct-n{n_}-run1.json")["snapshots_kb"]["first"].keys() if isinstance(J(f"{R}b2-passB-mcurve-old-spared-distinct-n{n_}-run1.json")["snapshots_kb"]["first"], dict) else "?"); break
        row(f"mcurve N={n_} first {field} MiB", [x / 1024 for x in o], [x / 1024 for x in nw])
wl = sorted({p.split("-")[3] if False else p[len(R) + len("b2w-passB-old-"):].rsplit("-run", 1)[0] for p in glob.glob(R + "b2w-passB-old-*-run1.json")})
print("\n| workload | old p50 us | new p50 us | old/new p50 per round | old cpu us/req | new cpu us/req |\n|---|---|---|---|---|---|")
for w in wl:
    o = [J(f"{R}b2w-passB-old-{w}-run{r}.json") for r in range(1, 6)]
    n = [J(f"{R}b2w-passB-new-{w}-run{r}.json") for r in range(1, 6)]
    op = [x["full_body_us"]["p50"] for x in o]; np_ = [x["full_body_us"]["p50"] for x in n]
    ratios = [a / b for a, b in zip(op, np_)]
    print(f"| {w} | {fmt(op)} | {fmt(np_)} | {fmt(ratios)} | {fmt([x['server_cpu_us_per_request'] for x in o])} | {fmt([x['server_cpu_us_per_request'] for x in n])} |")
