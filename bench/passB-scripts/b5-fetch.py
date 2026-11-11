#!/usr/bin/env python3
"""Time one GET per app, sequentially, first byte and full body in microseconds. Usage: b5-fetch.py spared|workerd FIRST LAST OUT.json
spared: 127.0.0.1:8080 with Host demo-K.localhost. workerd: 127.0.0.1:(9100+K). One keep-alive connection for spared, one per port for workerd."""
import http.client, json, sys, time
t, first, last, out = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
rows = []
sp = http.client.HTTPConnection("127.0.0.1", 8080) if t == "spared" else None
for k in range(first, last + 1):
    c = sp or http.client.HTTPConnection("127.0.0.1", 9100 + k)
    hdr = {"Host": f"demo-{k}.localhost"} if sp else {}
    t0 = time.perf_counter_ns()
    c.request("GET", "/", headers=hdr)
    r = c.getresponse()
    t1 = time.perf_counter_ns()
    r.read()
    t2 = time.perf_counter_ns()
    rows.append((k, r.status, (t1 - t0) // 1000, (t2 - t0) // 1000))
fb = sorted(x[2] for x in rows)
q = lambda p: fb[min(len(fb) - 1, max(0, -(-int(len(fb) * p * 1000) // 1000) - 1))]
json.dump({"target": t, "apps": len(rows), "errors": sum(1 for x in rows if x[1] != 200), "first_byte_us": {"p50": q(.5), "p99": q(.99), "max": fb[-1], "min": fb[0], "mean": sum(fb) // len(fb)},
           "per_app": rows}, open(out, "w"))
