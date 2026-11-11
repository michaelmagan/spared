#!/usr/bin/env python3
"""Pass D tables from bench/results/d-passD-*. Usage: d-summarize.py [wake|load|steal]
Every figure is the median [min-max] over the rounds that exist for that scenario. Latency is first-byte time in microseconds."""
import glob, json, os, re, statistics as st, sys
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results") + "/"
SUITE = os.environ.get("SUITE", "d2")
TAG = os.environ.get("TAG", "load")
MAXRUN = int(os.environ.get("MAXRUN", "99"))
J = lambda p: json.load(open(p))
def fmt(v, d=4):
    v = [x for x in v if x is not None]
    if not v: return "-"
    return f"{st.median(v):.{d}g} [{min(v):.{d}g}-{max(v):.{d}g}]"

def wake():
    names = {}
    for p in sorted(glob.glob(R + f"{SUITE}-passD-*-run*.json")):
        m = re.match(r".*/" + SUITE + r"-passD-(.+)-(spared|workerd)-run(\d+)\.json$", p)
        if m and int(m.group(3)) <= MAXRUN and not m.group(1).startswith(("load-", "hot-")): names.setdefault((m.group(1), m.group(2)), []).append(J(p))
    print("| scenario | target | rounds | p50 us | p99 us | max us | first 50 apps p50 us | majflt per wake | read KiB per wake | ready ms | prefetch pass ms (CPU ms) | cwasm resident MiB before |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for (n, t), rs in sorted(names.items()):
        f = lambda k: fmt([r["first_byte_us"][k] for r in rs])
        first50 = [st.median(x[2] for x in r["per_app"] if x[0] < 50) for r in rs]
        mf = [st.median(x[0] for x in r["faults_majflt_minflt_readbytes"]) for r in rs if "faults_majflt_minflt_readbytes" in r]
        rb = [st.median(x[2] for x in r["faults_majflt_minflt_readbytes"]) / 1024 for r in rs if "faults_majflt_minflt_readbytes" in r]
        ready = [r["meta"]["ready_ms"] for r in rs]
        pl = [re.search(r"in (\d+) ms \((\d+) ms of CPU\)", r["meta"]["prefetch_line"] or "") if t == "spared" else None for r in rs]
        pl = [(int(m.group(1)), int(m.group(2))) for m in pl if m]
        pas = f"{fmt([a for a, _ in pl], 3)} ({fmt([b for _, b in pl], 3)})" if pl else "-"
        res = [float(re.search(r"resident ([\d.]+) of", r["meta"]["cwasm_resident_before_measure"]).group(1)) for r in rs if r["meta"]["cwasm_resident_before_measure"]]
        err = sum(r["errors"] for r in rs)
        print(f"| {n} | {t} | {len(rs)} | {f('p50')} | {f('p99')} | {f('max')} | {fmt(first50)} | {fmt(mf, 3)} | {fmt(rb, 4)} | {fmt(ready, 4)} | {pas} | {fmt(res, 4)} |" + (f" errors {err}" if err else ""))
    print()
    print("| scenario | rounds | CPU s: ready to first request | CPU s: serving the requests | VmRSS MiB at ready | VmRSS MiB after | RssAnon MiB at ready | RssAnon MiB after | RssFile MiB after | prefetch_bytes_total MiB |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    for (n, t), rs in sorted(names.items()):
        if t != "spared": continue
        g = lambda r, a, k: r["meta"][a].get(k)
        pre = [g(r, "snap_before_requests", "cpu_s") - g(r, "snap_at_ready", "cpu_s") for r in rs]
        srv = [g(r, "snap_after_requests", "cpu_s") - g(r, "snap_before_requests", "cpu_s") for r in rs]
        mib = lambda a, k: [g(r, a, k) / 1024 for r in rs]
        pb = [r["meta"]["prefetch_bytes_total"] / 2**20 for r in rs if r["meta"]["prefetch_bytes_total"] is not None]
        print(f"| {n} | {len(rs)} | {fmt(pre, 3)} | {fmt(srv, 3)} | {fmt(mib('snap_at_ready', 'VmRSS_kb'), 4)} | {fmt(mib('snap_after_requests', 'VmRSS_kb'), 4)} | {fmt(mib('snap_at_ready', 'RssAnon_kb'), 4)} | {fmt(mib('snap_after_requests', 'RssAnon_kb'), 4)} | {fmt(mib('snap_after_requests', 'RssFile_kb'), 4)} | {fmt(pb, 4)} |")

def load():
    print("| part | metric | before | after | after/before per round |\n|---|---|---|---|---|")
    for part in ("cold", "mixed-c1", "mixed-c16", "cold-existing"):
        for k in ("p50", "p99"):
            o = [J(f"{R}d-passD-{TAG}-before-{part}-run{r}.json") for r in range(1, 6)]
            n = [J(f"{R}d-passD-{TAG}-after-{part}-run{r}.json") for r in range(1, 6)]
            ov = [x["first_byte_us"][k] for x in o]; nv = [x["first_byte_us"][k] for x in n]
            print(f"| {part} | first byte {k} us | {fmt(ov)} | {fmt(nv)} | {fmt([b / a for a, b in zip(ov, nv)])} (errors {sum(x['errors'] for x in o + n)}) |")
    for part in ("mixed-c1", "mixed-c16"):
        o = [J(f"{R}d-passD-{TAG}-before-{part}-run{r}.json") for r in range(1, 6)]
        n = [J(f"{R}d-passD-{TAG}-after-{part}-run{r}.json") for r in range(1, 6)]
        ov = [x["requests"] / x["detail"]["measured_wall_s"] for x in o]; nv = [x["requests"] / x["detail"]["measured_wall_s"] for x in n]
        print(f"| {part} | req/s | {fmt(ov)} | {fmt(nv)} | {fmt([b / a for a, b in zip(ov, nv)])} |")
    ex = [json.loads(l) for l in open(R + f"d-passD-{TAG}-extra.jsonl")]
    for m in ("before", "after"):
        e = [x for x in ex if x["mode"] == m]
        print(m, "PSS MiB after mixed:", fmt([x["pss_kb_after_mixed"] / 1024 for x in e]),
              "VmRSS MiB:", fmt([x["snap_after_mixed"]["VmRSS_kb"] / 1024 for x in e]),
              "RssAnon MiB:", fmt([x["snap_after_mixed"]["RssAnon_kb"] / 1024 for x in e]),
              "CPU s at ready:", fmt([x["snap_at_ready"]["cpu_s"] for x in e], 3))

def steal():
    for suite in ("d", "d2"):
        for p in glob.glob(R + f"{suite}-passD-steal.jsonl"):
            rows = [json.loads(l) for l in open(p)]
            v = [r["steal_pct"] for r in rows if r["steal_pct"] is not None]
            ld = [r["loadavg1_at_start"] for r in rows]
            print(f"{p.split('/')[-1]}: {len(rows)} segments, steal % min/median/max {min(v):.2f} / {st.median(v):.2f} / {max(v):.2f}, load at start {min(ld):.2f}-{max(ld):.2f}, segments above 2%: {sum(1 for x in v if x > 2)}")

{"wake": wake, "load": load, "steal": steal}[sys.argv[1] if len(sys.argv) > 1 else "wake"]()
