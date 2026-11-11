#!/usr/bin/env python3
"""Total memory of a host serving N apps, spared against workerd (docs/memory-methodology.md).

One point = one fresh server with N apps, in its own memory cgroup (v1), page cache dropped before start. Snapshots of the process
tree (status Rss*, smaps_rollup Pss*, per-mapping breakdown) and of the cgroup at: startup, first (every app served one GET), steady
(every app served 100 requests: 90 GET /, 10 POST /api/items), and, with --idle, idle15/idle65 (spared instances evicted through the
admin API, workerd left alone). Requests come from this process, outside the server's tree and cgroup.

Targets: spared-distinct (bench/work/distinct/wasm/vK.wasm), spared-lean (bench/work/distinct-lean/wasm/vK.wasm), spared-same (v0 under
N names), workerd-distinct (gen-distinct-workerd.py), workerd-empty (no worker, N must be 0), workerd-trivial (N trivial workers, no DO).
native-distinct: N native processes, each its own binary bench/work/distinct-native/bin/vK (gen-distinct.py --native); native-same: N
processes of one binary (apps/demo-native). Each native app is one process with its own database; the N processes share the cgroup and
the pinned CPU, and their PSS is summed.
Usage: mem-curve.py TARGET N RUN [--idle] [--idle-timer SECS]  -> bench/results/memory-curve-TARGET-nN-runRUN.json
CURVE_PREFIX (default memory-curve) replaces the file prefix; a check run with another prefix stays out of mem-curve-fit.py's glob."""
import base64, glob, hashlib, http.client, json, os, shutil, signal, subprocess, sys, time, urllib.request

# The published numbers were measured without the server's request log; keep it off.
os.environ.setdefault("SPARED_ACCESS_LOG", "off")

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
W = os.environ.get("CURVE_W", os.path.join(ROOT, "bench", "work", "curve"))
R = os.path.join(ROOT, "bench", "results")
# Private copies of the binaries (bench/work/curve/bin): a binary file mapped by another process on the machine would have its
# pages split by PSS, and a rebuild of target/release/spared mid-campaign would change what is measured.
SPARED = os.environ.get("SPARED", os.path.join(ROOT, "bench", "work", "curve", "bin", "spared"))
WORKERD = os.environ.get("WORKERD", os.path.join(ROOT, "bench", "work", "curve", "bin", "workerd"))
NATIVE = os.environ.get("NATIVE", os.path.join(ROOT, "bench", "work", "curve", "bin", "demo-native"))
NATIVE_DISTINCT = os.path.join(ROOT, "bench", "work", "distinct-native", "bin")
APPS = os.environ.get("APPS_PATH", "/v1/apps")
SP_PUB, SP_ADM, WD_BASE, NT_BASE = 18080, 18081, 19100, 20100  # ports away from the other harnesses (8080/8081, 9100+)
WASM = {"distinct": os.path.join(ROOT, "bench", "work", "distinct", "wasm"), "lean": os.path.join(ROOT, "bench", "work", "distinct-lean", "wasm")}
CG = "/sys/fs/cgroup/memory"
SRV_CPU = int(os.environ.get("SRV_CPU", "0"))

args = [a for a in sys.argv[1:] if not a.startswith("--")]
target, n, run = args[0], int(args[1]), int(args[2])
IDLE = "--idle" in sys.argv
STEADY = int(os.environ.get("STEADY_REQS", "100"))  # requests per app by the steady snapshot, first included
IDLE_TIMER = int(sys.argv[sys.argv.index("--idle-timer") + 1]) if "--idle-timer" in sys.argv else 0
os.makedirs(W, exist_ok=True)


def wasm_for(kind, k):
    return os.path.join(WASM["distinct"], "v0.wasm") if kind == "same" else os.path.join(WASM[kind], f"v{k % int(os.environ.get('WRAP', '100000'))}.wasm")


def wait_http(host, port, path="/", tries=300):
    for _ in range(tries):
        try:
            c = http.client.HTTPConnection(host, port, timeout=2)
            c.request("GET", path)
            c.getresponse().read()
            return True
        except OSError:
            time.sleep(0.1)
    return False


def wait_tcp(port, tries=600):
    import socket
    for _ in range(tries):
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return True
        except OSError:
            time.sleep(0.1)
    return False


def spared_template(kind, n):
    """Data dir holding apps demo-0..n-1 (at least) deployed and compiled to cwasm, never requested; extended on demand."""
    t = os.path.join(W, f"tpl-spared-{kind}")
    missing = [k for k in range(n) if not os.path.isfile(os.path.join(t, "apps", f"demo-{k}", "component.sha256"))]
    if not missing:
        return t
    os.makedirs(t, exist_ok=True)
    p = subprocess.Popen([SPARED, "serve", "--data-dir", t, "--listen", f"127.0.0.1:{SP_PUB}", "--admin-listen", f"127.0.0.1:{SP_ADM}", "--idle-secs", "600"],
                         stdout=open(os.path.join(W, "deploy.log"), "a"), stderr=subprocess.STDOUT)
    try:
        assert wait_http("127.0.0.1", SP_ADM)
        tok = open(os.path.join(t, "admin-token")).read().strip()
        for k in missing:
            m = base64.b64encode(f'name = "demo-{k}"\n[grants]\ndata = true\n'.encode()).decode()
            req = urllib.request.Request(f"http://127.0.0.1:{SP_ADM}{APPS}/demo-{k}", data=open(wasm_for(kind, k), "rb").read(), method="PUT",
                                         headers={"authorization": f"Bearer {tok}", "x-spared-manifest": m})
            urllib.request.urlopen(req, timeout=300).read()
    finally:
        p.terminate(); p.wait()
    return t


def spared_datadir(kind, n):
    """Fresh data dir holding demo-0..n-1 from the template; compiled components are hard links (read-only, content-addressed)."""
    d = os.path.join(W, "spared-data")
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "apps")); os.makedirs(os.path.join(d, "components")); os.makedirs(os.path.join(d, "backups"))
    if n == 0:
        return d, []
    t = spared_template(kind, n)
    shutil.copy2(os.path.join(t, "admin-token"), d)
    comps = []
    for k in range(n):
        src, dst = os.path.join(t, "apps", f"demo-{k}"), os.path.join(d, "apps", f"demo-{k}")
        os.makedirs(dst)
        for e in os.listdir(src):
            s = os.path.join(src, e)
            if e == "component.cwasm":
                os.link(s, os.path.join(dst, e))
            elif os.path.isdir(s):
                shutil.copytree(s, os.path.join(dst, e))
            else:
                shutil.copy2(s, os.path.join(dst, e))
        sha = open(os.path.join(src, "component.sha256")).read().strip()
        cdst = os.path.join(d, "components", f"{sha}.cwasm")
        if not os.path.exists(cdst):
            os.link(os.path.join(t, "components", f"{sha}.cwasm"), cdst)
        comps.append({"app": f"demo-{k}", "sha256": sha, "wasm_bytes": os.path.getsize(wasm_for(kind, k)), "cwasm_bytes": os.path.getsize(cdst)})
    return d, comps


TRIVIAL = 'export default { async fetch() { return new Response("ok\\n"); } };\n'


def workerd_config(kind, n):
    d, data = os.path.join(W, "workerd-cfg"), os.path.join(W, "workerd-data")
    shutil.rmtree(d, ignore_errors=True); shutil.rmtree(data, ignore_errors=True)
    os.makedirs(d); os.makedirs(data)
    if kind == "distinct":
        subprocess.run([sys.executable, os.path.join(ROOT, "bench", "demo", "gen-distinct-workerd.py"), d, data, str(n)], check=True, stdout=subprocess.DEVNULL,
                       env={**os.environ, "PORT_BASE": str(WD_BASE)})
        comps = [{"app": f"w{k}", "js_bytes": os.path.getsize(os.path.join(d, f"worker{k}.js")),
                  "sha256": hashlib.sha256(open(os.path.join(d, f"worker{k}.js"), "rb").read()).hexdigest()} for k in range(n)]
        return d, comps
    services, sockets = [f'    (name = "disk", disk = (path = "{data}", writable = true)),'], []
    if n == 0:  # workerd exits with no socket; a socket on the disk service keeps a process with no worker alive
        sockets.append(f'    (name = "files", address = "127.0.0.1:{WD_BASE - 1}", http = (), service = "disk"),')
    for k in range(n):
        open(os.path.join(d, f"t{k}.js"), "w").write(TRIVIAL.replace("ok", f"ok {k}"))
        services.append(f'    (name = "t{k}", worker = (modules = [(name = "worker.js", esModule = embed "t{k}.js")], compatibilityDate = "2026-11-10")),')
        sockets.append(f'    (name = "http{k}", address = "127.0.0.1:{WD_BASE + k}", http = (), service = "t{k}"),')
    open(os.path.join(d, "config.capnp"), "w").write('using Workerd = import "/workerd/workerd.capnp";\n\nconst config :Workerd.Config = (\n  services = [\n'
                                                     + "\n".join(services) + "\n  ],\n  sockets = [\n" + "\n".join(sockets) + "\n  ],\n);\n")
    return d, []


def tree(pid):
    kids = {}
    for st in glob.glob("/proc/[0-9]*/stat"):
        try:
            f = open(st).read()
            pp = int(f[f.rindex(")") + 2:].split()[1])
            kids.setdefault(pp, []).append(int(st.split("/")[2]))
        except (OSError, ValueError):
            pass
    out, todo = [], [pid]
    while todo:
        p = todo.pop(); out.append(p); todo += kids.get(p, [])
    return out


def kv(path):
    d = {}
    for line in open(path):
        parts = line.split()
        if len(parts) >= 2 and parts[0].endswith(":") and parts[1].isdigit():
            d[parts[0][:-1]] = int(parts[1])
    return d


def category(name, exe, datadir):
    if not name or name.startswith("[anon") or name in ("[heap]",) or name.startswith("/memfd:"):
        return "anon"
    if name.startswith("["):
        return "other_special"
    if name in exe:
        return "server_binary"
    if ".so" in os.path.basename(name):
        return "shared_libs"
    if name.endswith(".cwasm"):
        return "components"
    if datadir and name.startswith(datadir):
        return "data_files"
    if name.startswith("/dev/shm"):
        return "tmpfs"
    return "other_files"


def breakdown(pids, exe, datadir):
    cats = {}
    for p in pids:
        cur = None
        for line in open(f"/proc/{p}/smaps"):
            f = line.split()
            if not f[0].endswith(":") or "-" in f[0]:
                cur = category(f[5] if len(f) > 5 else "", exe, datadir)
                if len(f) > 5 and f[5].endswith(" (deleted)"):
                    cur = "other_files"
                c = cats.setdefault(cur, {"maps": 0, "rss": 0, "pss": 0, "anon": 0})
                c["maps"] += 1
            elif f[0] in ("Rss:", "Pss:", "Anonymous:"):
                cats[cur][{"Rss:": "rss", "Pss:": "pss", "Anonymous:": "anon"}[f[0]]] += int(f[1])
    return cats


def snap(pid, cg, exe, datadir, with_breakdown=False):
    pids = [q for r in (pid if isinstance(pid, list) else [pid]) for q in tree(r)]
    s = {"pids": len(pids), "threads": 0}
    for k in ("VmRSS", "RssAnon", "RssFile", "RssShmem", "Pss", "Pss_Anon", "Pss_File", "Pss_Shmem", "Shared_Clean", "Shared_Dirty", "Private_Clean", "Private_Dirty"):
        s[k] = 0
    for p in pids:
        st = kv(f"/proc/{p}/status")
        s["threads"] += st.get("Threads", 0)
        for k in ("VmRSS", "RssAnon", "RssFile", "RssShmem"):
            s[k] += st.get(k, 0)
        sr = kv(f"/proc/{p}/smaps_rollup")
        for k in ("Pss", "Pss_Anon", "Pss_File", "Pss_Shmem", "Shared_Clean", "Shared_Dirty", "Private_Clean", "Private_Dirty"):
            s[k] += sr.get(k, 0)
    if cg:
        ms = {}
        for line in open(os.path.join(cg, "memory.stat")):
            a, b = line.split()
            ms[a] = int(b)
        s["cgroup_kb"] = {"usage": int(open(os.path.join(cg, "memory.usage_in_bytes")).read()) // 1024,
                          **{k: ms[k] // 1024 for k in ("rss", "cache", "mapped_file", "shmem", "active_file", "inactive_file") if k in ms}}
    sh = os.statvfs("/dev/shm")
    s["dev_shm_used_kb"] = (sh.f_blocks - sh.f_bfree) * sh.f_frsize // 1024
    if with_breakdown:
        s["breakdown_kb"] = breakdown(pids, exe, datadir)
    return s


class Client:
    def __init__(self, sys_, n):
        self.sys, self.conns, self.errors, self.requests = sys_, {}, 0, 0

    def conn(self, k):
        if k not in self.conns:
            port = SP_PUB if self.sys == "spared" else NT_BASE + k if self.sys == "native" else WD_BASE + k
            self.conns[k] = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
        return self.conns[k]

    def req(self, k, method, path, body=None):
        h = {"host": f"demo-{k}.localhost"} if self.sys == "spared" else {}
        if body is not None:
            h["content-type"] = "application/json"
        for attempt in range(2):
            try:
                c = self.conn(k)
                c.request(method, path, body=body, headers=h)
                r = c.getresponse(); r.read()
                self.requests += 1
                if not 200 <= r.status < 300:
                    self.errors += 1
                return r.status
            except (OSError, http.client.HTTPException):
                self.conns.pop(k, None)
        self.errors += 1
        return 0

    def close(self):
        for c in self.conns.values():
            c.close()
        self.conns = {}


def main():
    sys_ = target.split("-", 1)[0]
    kind = target.split("-", 1)[1]
    cgname = f"memcurve-{os.getpid()}"
    cg = os.path.join(CG, cgname)
    os.makedirs(cg, exist_ok=True)
    if sys_ == "spared":
        datadir, comps = spared_datadir(kind, n)
        cmd = [SPARED, "serve", "--data-dir", datadir, "--listen", f"127.0.0.1:{SP_PUB}", "--admin-listen", f"127.0.0.1:{SP_ADM}", "--idle-secs", str(IDLE_TIMER or 600)]
        cwd, exe = ROOT, {os.path.realpath(SPARED)}
    elif sys_ == "native":
        datadir = os.path.join(W, "native-data")
        shutil.rmtree(datadir, ignore_errors=True)
        bins = [NATIVE if kind == "same" else os.path.join(NATIVE_DISTINCT, f"v{k}") for k in range(n)]
        cmds = []
        for k in range(n):
            os.makedirs(os.path.join(datadir, f"d{k}"))
            cmds.append([bins[k], "--listen", f"127.0.0.1:{NT_BASE + k}", "--db", os.path.join(datadir, f"d{k}", "app.db")])
        comps = [{"app": f"n{k}", "sha256": hashlib.sha256(open(b, "rb").read()).hexdigest(), "bin_bytes": os.path.getsize(b)} for k, b in enumerate(bins)]
        cmd, cwd, exe = cmds[0], ROOT, {os.path.realpath(b) for b in bins}
    else:
        assert not (kind == "empty" and n), "workerd-empty is N=0 only"
        cwd, comps = workerd_config("distinct" if kind == "distinct" else "trivial", n)
        datadir, cmd, exe = os.path.join(W, "workerd-data"), [WORKERD, "serve", "config.capnp"], {os.path.realpath(WORKERD)}
    subprocess.run(["sync"])
    try:
        open("/proc/sys/vm/drop_caches", "w").write("1\n")
        dropped = True
    except OSError:
        dropped = False

    def pre():
        open(os.path.join(cg, "cgroup.procs"), "w").write(str(os.getpid()))
        os.sched_setaffinity(0, {SRV_CPU})

    t0 = time.time()
    log = open(os.path.join(W, f"{sys_}.log"), "w")
    if sys_ == "native":
        procs = [subprocess.Popen(c, cwd=cwd, stdout=log, stderr=subprocess.STDOUT, preexec_fn=pre) for c in cmds]
        p = procs[0]
    else:
        p = subprocess.Popen(cmd, cwd=cwd, stdout=log, stderr=subprocess.STDOUT, preexec_fn=pre)
        procs = [p]
    roots = [q.pid for q in procs]
    out = {"steady_requests_per_app": STEADY, "binary_sha256": hashlib.sha256(open(cmd[0], "rb").read()).hexdigest(), "target": target, "system": sys_, "kind": kind, "n": n, "run": run, "page_cache_dropped_before_start": dropped,
           "server_cpu": SRV_CPU, "cmd": cmd, "apps": {"count": len(comps), "unique_sha256": len({c["sha256"] for c in comps})}}
    if comps and sys_ == "spared":
        out["apps"].update(wasm_bytes_total=sum(c["wasm_bytes"] for c in comps), cwasm_bytes_unique_total=sum({c["sha256"]: c["cwasm_bytes"] for c in comps}.values()))
    elif comps and sys_ == "native":
        out["apps"].update(bin_bytes_unique_total=sum({c["sha256"]: c["bin_bytes"] for c in comps}.values()))
    elif comps:
        out["apps"].update(js_bytes_total=sum(c["js_bytes"] for c in comps))
    cl = Client(sys_, n)
    try:
        if sys_ == "spared":
            ok = wait_http("127.0.0.1", SP_ADM)
        elif sys_ == "native":
            ok = all(wait_tcp(NT_BASE + k) for k in range(n))
        elif n:
            ok = wait_tcp(WD_BASE + n - 1)
        else:
            ok = wait_tcp(WD_BASE - 1) and p.poll() is None
        assert ok and all(q.poll() is None for q in procs), f"server did not start, see {W}/{sys_}.log"
        out["ready_s"] = round(time.time() - t0, 3)
        time.sleep(1)
        snaps = {"startup": snap(roots, cg, exe, datadir, True)}
        t = time.time()
        for k in range(n):
            cl.req(k, "GET", "/")
        out["first_wall_s"] = round(time.time() - t, 3)
        time.sleep(1)
        snaps["first"] = snap(roots, cg, exe, datadir, True)
        t = time.time()
        for k in range(n):
            for i in range(1, STEADY):
                if i % 10 == 9:
                    cl.req(k, "POST", "/api/items", json.dumps({"text": f"curve {k} {i}"}))
                else:
                    cl.req(k, "GET", "/")
        out["steady_wall_s"] = round(time.time() - t, 3)
        cl.close()
        time.sleep(1)
        snaps["steady"] = snap(roots, cg, exe, datadir, True)
        if IDLE or IDLE_TIMER:
            t = time.time()
            if sys_ == "spared" and n and not IDLE_TIMER:
                tok = open(os.path.join(datadir, "admin-token")).read().strip()
                for k in range(n):
                    urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{SP_ADM}{APPS}/demo-{k}/evict", data=b"", method="POST",
                                                                  headers={"authorization": f"Bearer {tok}"}), timeout=30).read()
            for label, at in (("idle15", 15 + IDLE_TIMER), ("idle65", 65 + IDLE_TIMER)):
                time.sleep(max(0, at - (time.time() - t)))
                snaps[label] = snap(roots, cg, exe, datadir, True)
            out["idle_method"] = ("idle timer --idle-secs %d, snapshots at timer+15 s and timer+65 s" % IDLE_TIMER) if IDLE_TIMER else \
                ("admin evict of every app, then no requests" if sys_ == "spared" else "no requests (workerd decides what to free)" if sys_ == "workerd" else "no requests")
        out["requests"], out["errors"] = cl.requests, cl.errors
        out["snapshots_kb"] = snaps
        assert all(q.poll() is None for q in procs), "server exited during the point"
    finally:
        for p in procs:
            p.send_signal(signal.SIGTERM)
        for p in procs:
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill(); p.wait()
            for q in tree(p.pid)[1:]:
                try:
                    os.kill(q, signal.SIGKILL)
                except OSError:
                    pass
        time.sleep(0.5)
        try:
            os.rmdir(cg)
        except OSError:
            pass
    f = os.path.join(R, f"{os.environ.get('CURVE_PREFIX', 'memory-curve')}-{target}-n{n}-run{run}{'-idletimer' if IDLE_TIMER else ''}{f'-steady{STEADY}' if STEADY != 100 else ''}.json")
    json.dump(out, open(f, "w"), indent=1)
    s = snaps["first"]
    print(f"{target} n={n} run={run}: first Pss {s['Pss']} RssAnon {s['RssAnon']} RssFile {s['RssFile']} errors {cl.errors} -> {os.path.basename(f)}")


main()
