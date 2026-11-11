#!/usr/bin/env python3
"""Like b5-fetch.py but also records, around each request, the server's major/minor faults (/proc/PID/stat fields 12, 10) and read_bytes (/proc/PID/io).
Usage: c-fetch-faults.py spared|workerd FIRST LAST OUT.json PID   -> per_app rows (k,status,first_byte_us,total_us) plus faults rows (majflt,minflt,read_bytes deltas)."""
import http.client, json, sys, time
t, first, last, out, pid = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], sys.argv[5]
def snap():
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    rb = 0
    for l in open(f"/proc/{pid}/io"):
        if l.startswith("read_bytes"): rb = int(l.split()[1])
    return int(f[7]), int(f[9]), rb  # minflt=field10 -> idx 7 after comm/state shift; majflt=field12 -> idx 9
rows, faults = [], []
sp = http.client.HTTPConnection("127.0.0.1", 8080) if t == "spared" else None
for k in range(first, last + 1):
    c = sp or http.client.HTTPConnection("127.0.0.1", 9100 + k)
    hdr = {"Host": f"demo-{k}.localhost"} if sp else {}
    a = snap()
    t0 = time.perf_counter_ns()
    c.request("GET", "/", headers=hdr)
    r = c.getresponse()
    t1 = time.perf_counter_ns()
    r.read()
    t2 = time.perf_counter_ns()
    b = snap()
    rows.append((k, r.status, (t1 - t0) // 1000, (t2 - t0) // 1000))
    faults.append((b[1] - a[1], b[0] - a[0], b[2] - a[2]))
fb = sorted(x[2] for x in rows)
q = lambda p: fb[min(len(fb) - 1, max(0, -(-int(len(fb) * p * 1000) // 1000) - 1))]
json.dump({"target": t, "apps": len(rows), "errors": sum(1 for x in rows if x[1] != 200), "first_byte_us": {"p50": q(.5), "p99": q(.99), "max": fb[-1], "min": fb[0], "mean": sum(fb) // len(fb)},
           "per_app": rows, "faults_majflt_minflt_readbytes": faults}, open(out, "w"))
