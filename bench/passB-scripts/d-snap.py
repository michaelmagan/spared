#!/usr/bin/env python3
"""One JSON line of process facts: CPU seconds (all threads and the spared-prefetch thread), VmRSS split, major faults.
Usage: d-snap.py PID"""
import json, os, sys
pid = sys.argv[1]; hz = os.sysconf("SC_CLK_TCK")
def stat(p):
    f = open(p).read().rsplit(")", 1)[1].split()
    return (int(f[11]) + int(f[12])) / hz, int(f[9])
cpu, maj = stat(f"/proc/{pid}/stat")
th = {}
for t in os.listdir(f"/proc/{pid}/task"):
    try:
        name = open(f"/proc/{pid}/task/{t}/comm").read().strip()
        th[name] = th.get(name, 0) + stat(f"/proc/{pid}/task/{t}/stat")[0]
    except OSError: pass
st = {}
for l in open(f"/proc/{pid}/status"):
    k, _, v = l.partition(":")
    if k in ("VmRSS", "RssAnon", "RssFile", "RssShmem"): st[k] = int(v.split()[0])
print(json.dumps({"cpu_s": cpu, "prefetch_thread_cpu_s": th.get("spared-prefetch", 0), "majflt": maj, **{k + "_kb": v for k, v in st.items()}}))
