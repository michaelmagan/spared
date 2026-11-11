#!/usr/bin/env python3
"""Group compiled .text bytes of a cwasm by origin (needs the wasm name section). usage: textsize.py file.cwasm"""
import re, subprocess, sys, collections
out = subprocess.run(["objdump", "-t", sys.argv[1]], capture_output=True, text=True).stdout
g = collections.Counter()
big = []
for l in out.splitlines():
    p = l.split()
    if len(p) < 6 or p[3] != ".text":
        continue
    sz = int(p[4], 16)
    n = re.sub(r".*function\[\d+\]::", "", " ".join(p[5:]))
    big.append((sz, n))
    if re.match(r"(sqlite3Fts5|fts5)", n, re.I): k = "fts5"
    elif re.match(r"(sqlite3Fts3|fts3)", n, re.I): k = "fts3"
    elif re.match(r"(rtree|geopoly|sqlite3Rtree)", n, re.I): k = "rtree+geopoly"
    elif re.match(r"(json|sqlite3Json)", n): k = "json"
    elif "::" in n or n.startswith(("_R", "_Z", "<", "__rust", "rust_", "cabi", "__rdl", "__rg")): k = "rust"
    elif re.match(r"(wasm\[|component|trampoline|wasmtime|array_to_wasm|wasm_to)", n) or "trampoline" in n: k = "trampolines/builtins"
    else: k = "sqlite-core+libc"
    g[k] += sz
t = sum(g.values())
for k, v in g.most_common():
    print(f"{k:<24}{v/1024:9.0f} KB {100*v/t:5.1f}%")
print(f"{'total .text':<24}{t/1024:9.0f} KB")
if len(sys.argv) > 2:
    for sz, n in sorted(big, reverse=True)[:int(sys.argv[2])]:
        print(f"  {sz:8d} {n[:100]}")
