#!/usr/bin/env python3
import glob, json, statistics as st
import os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
J = lambda p: json.load(open(p))
fmt = lambda v: f"{st.median(v):.4g} [{min(v):.4g}-{max(v):.4g}]"
print("| N | snapshot | PSS MiB | RssAnon MiB | RssFile MiB |\n|---|---|---|---|---|")
for n in (200, 500):
    ds = [J(f) for f in sorted(glob.glob(f"{R}b8-passB-mcurve-spared-distinct-n{n}-run*.json"))]
    for snap in ("startup", "first", "steady"):
        row = []
        for k in ("Pss", "RssAnon", "RssFile"):
            row.append(fmt([next(v for kk, v in d["snapshots_kb"][snap].items() if kk.lower() == k.lower()) / 1024 for d in ds]))
        print(f"| {n} | {snap} | " + " | ".join(row) + " |")
    print(f"| {n} | ready_s / first_wall_s | {fmt([d['ready_s'] for d in ds])} | {fmt([d['first_wall_s'] for d in ds])} | unique components {ds[0]['apps']['unique_sha256']} |")
print("\nwake at N=500 (apps 1..499, first request each):")
for m in ("nodrop", "drop"):
    ds = [J(f) for f in sorted(glob.glob(f"{R}b8-passB-wake500-{m}-run*.json"))]
    print(f"| {m} | p50 {fmt([d['first_byte_us']['p50'] for d in ds])} | p99 {fmt([d['first_byte_us']['p99'] for d in ds])} | max {fmt([d['first_byte_us']['max'] for d in ds])} |")
