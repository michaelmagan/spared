#!/usr/bin/env python3
"""Pass A summaries: median [min-max] over runs, per suite. Usage: summarize.py SECTION ...  (a b c d e f steal). Prints markdown."""
import glob, json, math, os, statistics as st, sys

R = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))


def load(pat):
    return [json.load(open(f)) for f in sorted(glob.glob(os.path.join(R, pat)))]


def S(v):
    v = [x for x in v if x is not None]
    return (st.median(v), min(v), max(v), len(v)) if v else None


def f(s, d=0):
    if not s:
        return "n/a"
    return f"{s[0]:.{d}f} [{s[1]:.{d}f}-{s[2]:.{d}f}]"


def spread(s):
    return (s[2] - s[1]) / s[0] * 100 if s and s[0] else 0


def lat(pat, key="first_byte_us"):
    ds = load(pat)
    return {"p50": S([d[key]["p50"] for d in ds]), "p99": S([d[key]["p99"] for d in ds]), "n": len(ds), "errors": sum(d.get("errors", 0) for d in ds)}


def steal(suite, prefix=None):
    p = os.path.join(R, f"{suite}-passA-steal.jsonl")
    if not os.path.exists(p):
        return None
    rows = [json.loads(l) for l in open(p)]
    if prefix:
        rows = [r for r in rows if r["segment"].startswith(prefix)]
    if not rows:
        return None
    pct = [r["steal_pct"] for r in rows if r["steal_pct"] is not None]
    tot_s, tot_t = sum(r["steal_delta"] for r in rows), sum(r["all_cpu_jiffies_delta"] for r in rows)
    return {"segments": len(rows), "min_pct": min(pct), "max_pct": max(pct), "pooled_pct": round(100 * tot_s / tot_t, 3) if tot_t else None,
            "over5": [r["segment"] for r in rows if (r["steal_pct"] or 0) > 5], "worst": max(rows, key=lambda r: r["steal_pct"] or 0)["segment"]}


def steal_line(suite, prefix=None):
    s = steal(suite, prefix)
    if not s:
        return "steal: n/a"
    return f"steal {s['min_pct']:.2f}-{s['max_pct']:.2f}% per segment (pooled {s['pooled_pct']}%, {s['segments']} segments" + (f", over 5%: {', '.join(s['over5'])}" if s['over5'] else "") + ")"


def sec_a():
    print("### a. spared vs workerd (bench/demo/measure.sh), p50 / p99 in us, median [min-max] of per-run values\n")
    rows = [("Wake from idle, data on disk (cold-existing), first byte", "cold-existing"), ("First request of a new app, seeds 100 rows (cold)", "cold"),
            ("Warm GET, 1 connection", "warm-c1"), ("Warm GET, 16 connections", "warm-c16"), ("90% GET 10% POST, 1 connection", "mixed-c1"),
            ("90% GET 10% POST, 16 connections", "mixed-c16")]
    print("| metric | spared p50 | workerd p50 | workerd/spared p50 | spared p99 | workerd p99 | workerd/spared p99 | runs |\n|---|---|---|---|---|---|---|---|")
    out = {}
    for name, k in rows:
        a, b = lat(f"vs-workerd-passA-r*-spared-{k}-run1.json"), lat(f"vs-workerd-passA-r*-workerd-{k}-run1.json")
        out[k] = {"spared": a, "workerd": b}
        r50 = b["p50"][0] / a["p50"][0] if a["p50"] and b["p50"] else None
        r99 = b["p99"][0] / a["p99"][0] if a["p99"] and b["p99"] else None
        print(f"| {name} | {f(a['p50'])} | {f(b['p50'])} | {r50:.2f}x | {f(a['p99'])} | {f(b['p99'])} | {r99:.2f}x | {a['n']}/{b['n']} |")
    print("\nSpread (max-min over median) of p50 per cell, % : " + ", ".join(f"{k} spared {spread(v['spared']['p50']):.0f} workerd {spread(v['workerd']['p50']):.0f}" for k, v in out.items()))
    for t in ("spared", "workerd"):
        print(f"\n{t}: {steal_line('vs-workerd', '')}" if False else "", end="")
    print("\n" + steal_line("vs-workerd") + "\n")
    # per-round ratios
    print("Per-round workerd/spared p50 ratio, warm 1 connection: " + ", ".join(
        f"r{r}: {load(f'vs-workerd-passA-r{r}-workerd-warm-c1-run1.json')[0]['first_byte_us']['p50'] / load(f'vs-workerd-passA-r{r}-spared-warm-c1-run1.json')[0]['first_byte_us']['p50']:.2f}"
        for r in range(1, 6) if glob.glob(os.path.join(R, f'vs-workerd-passA-r{r}-spared-warm-c1-run1.json')) and glob.glob(os.path.join(R, f'vs-workerd-passA-r{r}-workerd-warm-c1-run1.json'))))
    print()


def mem_fit_note():
    pass


def sec_d():
    print("### d. access log on vs off, spared warm GET, us, median [min-max]\n")
    print("| connections | on p50 | off p50 | on - off p50 | on p99 | off p99 | on - off p99 | runs |\n|---|---|---|---|---|---|---|---|")
    for c in (1, 16):
        on, off = lat(f"accesslog-passA-on-warm-c{c}-run*.json"), lat(f"accesslog-passA-off-warm-c{c}-run*.json")
        if not on["p50"] or not off["p50"]:
            continue
        print(f"| {c} | {f(on['p50'])} | {f(off['p50'])} | {on['p50'][0]-off['p50'][0]:+.0f} ({(on['p50'][0]/off['p50'][0]-1)*100:+.1f}%) | {f(on['p99'])} | {f(off['p99'])} | {on['p99'][0]-off['p99'][0]:+.0f} ({(on['p99'][0]/off['p99'][0]-1)*100:+.1f}%) | {on['n']}/{off['n']} |")
    print("\n" + steal_line("accesslog") + "\n")


def sec_steal():
    for p in sorted(glob.glob(os.path.join(R, "*-passA-steal.jsonl"))):
        suite = os.path.basename(p).split("-passA-")[0]
        print(f"- {suite}: {steal_line(suite)}")


if __name__ == "__main__":
    for s in sys.argv[1:]:
        globals()["sec_" + s]()
