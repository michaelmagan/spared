#!/usr/bin/env python3
import glob, json, statistics as st
import os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
J = lambda p: json.load(open(p))
fmt = lambda v: f"{st.median(v):.4g} [{min(v):.4g}-{max(v):.4g}]"
print("| config | conc | metric | spared | workerd | workerd/spared per round |\n|---|---|---|---|---|---|")
for cfg in ("pin1", "cpu2", "unpinned"):
    for c in (1, 16):
        for k in ("p50", "p99"):
            s = [J(f"{R}b4-passB-{cfg}-spared-warm-c{c}-run{r}.json") for r in range(1, 6)]
            w = [J(f"{R}b4-passB-{cfg}-workerd-warm-c{c}-run{r}.json") for r in range(1, 6)]
            sv = [x["first_byte_us"][k] for x in s]; wv = [x["first_byte_us"][k] for x in w]
            err = sum(x["errors"] for x in s + w)
            print(f"| {cfg} | {c} | {k} us | {fmt(sv)} | {fmt(wv)} | {fmt([b / a for a, b in zip(sv, wv)])} (errors {err}) |")
        s = [J(f"{R}b4-passB-{cfg}-spared-warm-c{c}-run{r}.json")["requests"] / J(f"{R}b4-passB-{cfg}-spared-warm-c{c}-run{r}.json")["detail"]["measured_wall_s"] for r in range(1, 6)]
        w = [J(f"{R}b4-passB-{cfg}-workerd-warm-c{c}-run{r}.json")["requests"] / J(f"{R}b4-passB-{cfg}-workerd-warm-c{c}-run{r}.json")["detail"]["measured_wall_s"] for r in range(1, 6)]
        print(f"| {cfg} | {c} | req/s | {fmt(s)} | {fmt(w)} | {fmt([a / b for a, b in zip(s, w)])} (spared/workerd) |")
