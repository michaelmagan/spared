#!/usr/bin/env python3
import glob, json, statistics as st
import os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
J = lambda p: json.load(open(p))
fmt = lambda v: f"{st.median(v):.4g} [{min(v):.4g}-{max(v):.4g}]"
T = ["spared", "celld-single", "celld-fleet"]
print("| part | metric | " + " | ".join(T) + " |\n|---|---|---|---|---|")
for part in ["cold", "cold-existing", "warm-c1", "warm-c16", "mixed-c1", "mixed-c16"]:
    for k in ("p50", "p99"):
        cells = []
        for t in T:
            fs = sorted(glob.glob(f"{R}b3-passB-{t}-{part}-run*.json"))
            cells.append(f"{fmt([J(f)['first_byte_us'][k] for f in fs])} (n={len(fs)}, errors {sum(J(f)['errors'] for f in fs)})")
        print(f"| {part} | first byte {k} us | " + " | ".join(cells) + " |")
print("\nPSS MiB (sum over the process tree; celld-fleet = both nodes; bucket store not included):")
print("| point | " + " | ".join(T) + " |\n|---|---|---|---|---|")
for pt in ("empty", "n1", "n10", "n50"):
    cells = []
    for t in T:
        v = [J(f)["kb"][pt]["pss"] / 1024 for f in sorted(glob.glob(f"{R}b3-passB-{t}-mem-run*.json"))]
        cells.append(fmt(v))
    print(f"| {pt} | " + " | ".join(cells) + " |")
print("per-app growth between N=1 and N=50 (MiB/app, (pss50-pss1)/49):")
for t in T:
    v = [(J(f)["kb"]["n50"]["pss"] - J(f)["kb"]["n1"]["pss"]) / 49 / 1024 for f in sorted(glob.glob(f"{R}b3-passB-{t}-mem-run*.json"))]
    print(" ", t, fmt(v))
print("RSS MiB at n50:", {t: fmt([J(f)["kb"]["n50"]["rss"] / 1024 for f in sorted(glob.glob(f"{R}b3-passB-{t}-mem-run*.json"))]) for t in T})
