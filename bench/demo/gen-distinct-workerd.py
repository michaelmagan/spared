#!/usr/bin/env python3
"""Generate one workerd config with N genuinely different workers (own script text, own page, own SQLite Durable Object class
and namespace each), following apps/demo-workerd/config.capnp. Worker k listens on 127.0.0.1:(9100+k), or $PORT_BASE+k.
Usage: gen-distinct-workerd.py OUT_DIR DATA_DIR [N]"""
import os, random, sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
out, data = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
N = int(sys.argv[3]) if len(sys.argv) > 3 else 50
PORT_BASE = int(os.environ.get("PORT_BASE", "9100"))
WORDS = "alpha bravo cedar delta ember fjord grove harbor indigo juniper kestrel lantern meadow nectar orchid pebble quartz ridge summit thistle umber violet willow xenon yarrow zephyr".split()
worker = open(os.path.join(ROOT, "apps", "demo-workerd", "worker.js")).read()
page = open(os.path.join(ROOT, "apps", "demo-shared", "page.html")).read()
os.makedirs(out, exist_ok=True)
os.makedirs(data, exist_ok=True)

services, sockets = [], []
for k in range(N):
    rnd = random.Random(1000 + k)
    title = " ".join(rnd.choice(WORDS) for _ in range(3)).title()
    open(os.path.join(out, f"page{k}.html"), "w").write(page.replace("<title>", f"<title>{title} - ", 1))
    nh = 6 + (k * 7) % 23 + k // 5
    methods, arms = [], []
    for i in range(nh):
        c1, c2 = rnd.randrange(3, 2**31) | 1, rnd.randrange(3, 2**31) | 1
        rot, iters, mod, x = rnd.randrange(1, 31), rnd.randrange(20, 400), rnd.randrange(3, 50), rnd.randrange(0, 3)
        w = " ".join(rnd.choice(WORDS) for _ in range(rnd.randrange(3, 12)))
        col = rnd.choice(["id", "created_at", "length(text)"])
        methods.append(f'''
  h{i}() {{
    let acc = BigInt({c1}) ^ {k}n;
    for (let j = 0n; j < {iters}n; j++) {{
      acc = BigInt.asUintN(64, acc * {c2}n);
      acc = BigInt.asUintN(64, (acc << {rot}n) | (acc >> {64 - rot}n)) ^ (j + {i}n);
    }}
    const n = this.sql.exec("SELECT count(*) AS n FROM items WHERE ({col}) % {mod} = {x}").one().n;
    return `{w} ${{acc}} ${{n}}`;
  }}
''')
        arms.append(f'    if (url.pathname === "/r{i}") return new Response(this.h{i}());')
    w = worker.replace("export class Items", f"export class Items{k}") \
              .replace('import PAGE from "page.html";', 'import PAGE from "page.html";')
    w = w.replace("  latest() {", "".join(methods) + "\n  latest() {", 1)
    w = w.replace('    if (url.pathname === "/api/items") {', "\n".join(arms) + '\n    if (url.pathname === "/api/items") {', 1)
    open(os.path.join(out, f"worker{k}.js"), "w").write(w)
    services.append(f'''    (name = "w{k}", worker = (
      modules = [
        (name = "worker.js", esModule = embed "worker{k}.js"),
        (name = "page.html", text = embed "page{k}.html"),
      ],
      compatibilityDate = "2026-11-10",
      durableObjectNamespaces = [(className = "Items{k}", uniqueKey = "distinct-items-{k}", enableSql = true)],
      durableObjectStorage = (localDisk = "disk"),
      bindings = [(name = "ITEMS", durableObjectNamespace = "Items{k}")],
    )),''')
    sockets.append(f'    (name = "http{k}", address = "127.0.0.1:{PORT_BASE + k}", http = (), service = "w{k}"),')
cfg = f'''using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "disk", disk = (path = "{data}", writable = true)),
{chr(10).join(services)}
  ],
  sockets = [
{chr(10).join(sockets)}
  ],
);
'''
open(os.path.join(out, "config.capnp"), "w").write(cfg)
print("generated", N, "workers in", out)
