#!/usr/bin/env python3
import glob, json, statistics as st
import os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
J = lambda p: json.load(open(p))
fmt = lambda v: f"{st.median(v):.4g} [{min(v):.4g}-{max(v):.4g}]"
print("| part | metric | off | on | on/off per round |\n|---|---|---|---|---|")
for part in ("cold", "mixed-c1", "mixed-c16", "cold-existing"):
    for k in ("p50", "p99"):
        o = [J(f"{R}b7-passB-off-{part}-run{r}.json") for r in range(1, 6)]
        n = [J(f"{R}b7-passB-on-{part}-run{r}.json") for r in range(1, 6)]
        ov = [x["first_byte_us"][k] for x in o]; nv = [x["first_byte_us"][k] for x in n]
        print(f"| {part} | first byte {k} us | {fmt(ov)} | {fmt(nv)} | {fmt([b / a for a, b in zip(ov, nv)])} (errors {sum(x['errors'] for x in o + n)}) |")
for part in ("mixed-c1", "mixed-c16"):
    o = [J(f"{R}b7-passB-off-{part}-run{r}.json") for r in range(1, 6)]
    n = [J(f"{R}b7-passB-on-{part}-run{r}.json") for r in range(1, 6)]
    ov = [x["requests"] / x["detail"]["measured_wall_s"] for x in o]; nv = [x["requests"] / x["detail"]["measured_wall_s"] for x in n]
    print(f"| {part} | req/s | {fmt(ov)} | {fmt(nv)} | {fmt([b / a for a, b in zip(ov, nv)])} |")
ex = [json.loads(l) for l in open(R + "b7-passB-extra.jsonl")]
for m in ("off", "on"):
    print(m, "PSS MiB after mixed:", fmt([x["pss_kb_after_mixed"] / 1024 for x in ex if x["mode"] == m]), "log bytes:", fmt([x["log_bytes"] for x in ex if x["mode"] == m]))
