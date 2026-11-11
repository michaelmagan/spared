#!/usr/bin/env python3
"""Usage: c-summarize.py SUITE  -> per scenario/target: median [min-max] over rounds of p50, p99, max, mean (us), plus split by first-seen vs shared component."""
import glob, json, os, re, statistics, sys, collections
suite = sys.argv[1]
G = collections.defaultdict(list)
for f in sorted(glob.glob(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results", f"{suite}-passC-*-run*.json"))):
    m = re.match(rf".*/{suite}-passC-(.+)-(spared|workerd)-run(\d+)\.json", f)
    if not m: continue
    G[(m.group(1), m.group(2))].append(json.load(open(f)))
def q(v, p):
    v = sorted(v); return v[min(len(v) - 1, max(0, -(-int(len(v) * p * 1000) // 1000) - 1))]
def ms(v): return f"{statistics.median(v):.0f} [{min(v):.0f}-{max(v):.0f}]"
print("| scenario | target | rounds | apps | p50 us | p99 us | max us | mean us | first app us | p50 shared-component apps | p50 first-seen apps |")
print("|---|---|---|---|---|---|---|---|---|---|---|")
for (name, t), runs in G.items():
    p50, p99, mx, mean, first, sh, fs = [], [], [], [], [], [], []
    for j in runs:
        rows = j["per_app"]; fb = [r[2] for r in rows]; u = j["meta"]["uniq"]
        p50.append(q(fb, .5)); p99.append(q(fb, .99)); mx.append(max(fb)); mean.append(sum(fb) / len(fb)); first.append(rows[0][2])
        s = [r[2] for r in rows if r[0] >= u]; f_ = [r[2] for r in rows if r[0] < u]
        if s: sh.append(q(s, .5))
        if f_: fs.append(q(f_, .5))
    mj = [statistics.median(x[0] for x in j["faults_majflt_minflt_readbytes"]) for j in runs if "faults_majflt_minflt_readbytes" in j]
    rb = [statistics.median(x[2] for x in j["faults_majflt_minflt_readbytes"]) / 1024 for j in runs if "faults_majflt_minflt_readbytes" in j]
    ftxt = f" majflt/wake {ms(mj)}, read KiB/wake {ms(rb)}" if mj else ""
    err = sum(j["errors"] for j in runs)
    print(f"| {name} | {t} | {len(runs)} | {runs[0]['apps']}{' ERR%d' % err if err else ''} | {ms(p50)} | {ms(p99)} | {ms(mx)} | {ms(mean)} | {ms(first)} | {ms(sh) if sh else '-'} | {ms(fs) if fs else '-'} |{ftxt} |")
