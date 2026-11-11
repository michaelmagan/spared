#!/usr/bin/env python3
import glob, json, statistics as st
import os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
J = lambda p: json.load(open(p))
fmt = lambda v: f"{st.median(v):.4g} [{min(v):.4g}-{max(v):.4g}]"
print("| target | cache | p50 us | p99 us | max us | pooled p50 over all apps | pooled p99 |\n|---|---|---|---|---|---|---|")
for t in ("spared", "workerd"):
    for m in ("nodrop", "drop"):
        ds = [J(f) for f in sorted(glob.glob(f"{R}b5-passB-{t}-{m}-run*.json"))]
        pool = sorted(x[2] for d in ds for x in d["per_app"])
        q = lambda p: pool[min(len(pool) - 1, int(len(pool) * p))]
        print(f"| {t} | {m} | {fmt([d['first_byte_us']['p50'] for d in ds])} | {fmt([d['first_byte_us']['p99'] for d in ds])} | {fmt([d['first_byte_us']['max'] for d in ds])} | {q(.5)} | {q(.99)} |")
print("\ndrop/nodrop ratio of per-round p50 (rounds paired):")
for t in ("spared", "workerd"):
    a = [J(f)["first_byte_us"]["p50"] for f in sorted(glob.glob(f"{R}b5-passB-{t}-drop-run*.json"))]
    b = [J(f)["first_byte_us"]["p50"] for f in sorted(glob.glob(f"{R}b5-passB-{t}-nodrop-run*.json"))]
    print(" ", t, fmt([x / y for x, y in zip(a, b)]))
