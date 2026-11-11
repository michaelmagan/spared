#!/usr/bin/env python3
import json, statistics as st
import os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
J = lambda v, r: json.load(open(f"{R}request-cost-passB-{v}-run{r}.json"))
fmt = lambda v: f"{st.median(v):.4g} [{min(v):.4g}-{max(v):.4g}]"
S = [J("sandbox", r) for r in range(1, 6)]; H = [J("host", r) for r in range(1, 6)]
print("| metric | (a) SQLite in the sandbox | (b) SQLite in the host | a/b per round |\n|---|---|---|---|")
for k in ("p50", "p99"):
    a = [x["latency_us"][k] for x in S]; b = [x["latency_us"][k] for x in H]
    print(f"| latency {k} us | {fmt(a)} | {fmt(b)} | {fmt([x / y for x, y in zip(a, b)])} |")
a = [x["cpu_us_per_request"] for x in S]; b = [x["cpu_us_per_request"] for x in H]
print(f"| CPU us per request | {fmt(a)} | {fmt(b)} | {fmt([x / y for x, y in zip(a, b)])} |")
a = [x["latency_us"]["p50"] for x in S]; b = [x["latency_us"]["p50"] for x in H]
print(f"| p50 gap (a - b) us | {fmt([x - y for x, y in zip(a, b)])} | | |")
print(f"| max latency us | {fmt([x['latency_us']['max'] for x in S])} | {fmt([x['latency_us']['max'] for x in H])} | |")
