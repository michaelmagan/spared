#!/usr/bin/env python3
"""Pass A container harness (default Docker settings, distinct image per container). Subcommands:
  start REP        10x `docker run -d` and 10x `docker start` of a created container, timed from the start of the docker command to the first
                   response byte (published loopback port, polled after the docker command returns), plus the same polled via the container IP.
  mem N REP        N containers, each from its own image spared-ctr-dist:vK, one GET each; PSS/RssAnon of app, containerd-shim, docker-proxy,
                   dockerd+containerd (+ containerd-shim-runc-v2 ... see groups), and each container's memory cgroup. Snapshots 2 s and 15 s after the last first byte.
Files: bench/results/container-passA-<what>-...json"""
import glob, http.client, json, math, os, shutil, subprocess, sys, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
R = os.path.join(ROOT, "bench", "results")
W = os.path.join(ROOT, "bench", "work", "ctr")
PREFIX = "spared-ctr-dist:v"
CPU = "0"
KS = ("VmRSS", "RssAnon", "RssFile")
KP = ("Pss", "Pss_Anon", "Pss_File")


def sh(*a, **k):
    return subprocess.run(a, check=True, text=True, capture_output=True, **k).stdout.strip()


def kv(path):
    d = {}
    for line in open(path):
        p = line.split()
        if len(p) >= 2 and p[0].endswith(":") and p[1].isdigit():
            d[p[0][:-1]] = int(p[1])
    return d


def ppid(p):
    f = open(f"/proc/{p}/stat").read()
    return int(f[f.rindex(")") + 2:].split()[1])


def comm(p):
    return open(f"/proc/{p}/comm").read().strip()


def pids_named(*names):
    out = []
    for c in glob.glob("/proc/[0-9]*/comm"):
        try:
            if open(c).read().strip() in names:
                out.append(int(c.split("/")[2]))
        except OSError:
            pass
    return out


def group(pids):
    s = {k: 0 for k in KS + KP}
    s["pids"] = len(pids)
    for p in pids:
        try:
            st, sr = kv(f"/proc/{p}/status"), kv(f"/proc/{p}/smaps_rollup")
        except OSError:
            continue
        for k in KS:
            s[k] += st.get(k, 0)
        for k in KP:
            s[k] += sr.get(k, 0)
    return s


def poll(addr, port, t0, timeout=30):
    while time.perf_counter() - t0 < timeout:
        try:
            c = http.client.HTTPConnection(addr, port, timeout=5)
            c.request("GET", "/")
            r = c.getresponse()
            t = time.perf_counter()
            r.read(); c.close()
            if r.status == 200:
                return t
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.0005)
    raise SystemExit(f"no response from {addr}:{port}")


def pct(v):
    v = sorted(v)
    q = lambda p: v[min(len(v), max(1, math.ceil(len(v) * p))) - 1]
    return {"p50": q(.5), "p99": q(.99), "min": v[0], "max": v[-1]}


def cmd_start(rep):
    port = 18095
    data = os.path.join(W, "start-data")
    shutil.rmtree(data, ignore_errors=True); os.makedirs(data)
    img = "spared-ctr-demo:bench"
    subprocess.run(["docker", "rm", "-f", "ctr-start"], capture_output=True)
    # seed the database once
    sh("docker", "run", "-d", "--runtime", "runsc", "--name", "ctr-start", "--cpuset-cpus", CPU, "-p", f"127.0.0.1:{port}:8090", "-v", f"{data}:/data", img)
    poll("127.0.0.1", port, time.perf_counter()); sh("docker", "rm", "-f", "ctr-start")
    out = {"target": "container-gvisor", "runtime": "runsc", "rep": rep, "image": img, "what": "docker run -d / docker start, FROM scratch static binary, default bridge network, published loopback port, --cpuset-cpus 0, database already seeded; first response byte measured from the start of the docker command, polled every 0.5 ms after the command returns"}
    run_first, run_ret, run_ip = [], [], []
    for i in range(10):
        t0 = time.perf_counter()
        sh("docker", "run", "-d", "--runtime", "runsc", "--name", "ctr-start", "--cpuset-cpus", CPU, "-p", f"127.0.0.1:{port}:8090", "-v", f"{data}:/data", img)
        t1 = time.perf_counter()
        t = poll("127.0.0.1", port, t0)
        run_first.append(round((t - t0) * 1e6)); run_ret.append(round((t1 - t0) * 1e6))
        sh("docker", "rm", "-f", "ctr-start"); time.sleep(0.5)
    out["docker_run"] = {"first_byte_us": pct(run_first), "first_byte_us_all": run_first, "docker_command_returned_us_all": run_ret}
    sh("docker", "create", "--runtime", "runsc", "--name", "ctr-start", "--cpuset-cpus", CPU, "-p", f"127.0.0.1:{port}:8090", "-v", f"{data}:/data", img)
    st_first, st_ret, st_ip = [], [], []
    try:
        for i in range(10):
            t0 = time.perf_counter()
            sh("docker", "start", "ctr-start")
            t1 = time.perf_counter()
            t = poll("127.0.0.1", port, t0)
            st_first.append(round((t - t0) * 1e6)); st_ret.append(round((t1 - t0) * 1e6))
            sh("docker", "kill", "ctr-start"); time.sleep(0.5)
        # same, polled through the container IP (no docker-proxy)
        for i in range(10):
            t0 = time.perf_counter()
            sh("docker", "start", "ctr-start")
            ip = sh("docker", "inspect", "-f", '{{(index .NetworkSettings.Networks "bridge").IPAddress}}', "ctr-start")
            t = poll(ip, 8090, t0)
            st_ip.append(round((t - t0) * 1e6))
            sh("docker", "kill", "ctr-start"); time.sleep(0.5)
    finally:
        subprocess.run(["docker", "rm", "-f", "ctr-start"], capture_output=True)
    out["docker_start"] = {"first_byte_us": pct(st_first), "first_byte_us_all": st_first, "docker_command_returned_us_all": st_ret}
    out["docker_start_via_container_ip"] = {"first_byte_us": pct(st_ip), "first_byte_us_all": st_ip}
    out["docker_version"] = sh("docker", "version", "-f", "{{.Server.Version}}")
    f = os.path.join(R, f"container-passA-gvisor-start-run{rep}.json")
    json.dump(out, open(f, "w"), indent=1)
    print("start rep", rep, "run -d p50", out["docker_run"]["first_byte_us"]["p50"], "start p50", out["docker_start"]["first_byte_us"]["p50"], "start via ip p50", out["docker_start_via_container_ip"]["first_byte_us"]["p50"])


def descendants(roots):
    kids = {}
    for st_ in glob.glob("/proc/[0-9]*/stat"):
        try:
            f_ = open(st_).read()
            kids.setdefault(int(f_[f_.rindex(")") + 2:].split()[1]), []).append(int(st_.split("/")[2]))
        except (OSError, ValueError):
            pass
    out, todo = [], list(roots)
    while todo:
        p = todo.pop(); out.append(p); todo += kids.get(p, [])
    return out


def snapshot(ids):
    apps = [int(sh("docker", "inspect", "-f", "{{.State.Pid}}", i)) for i in ids]
    shims = sorted({ppid(p) for p in apps})
    cg = []
    for i in ids:
        d = f"/sys/fs/cgroup/memory/docker/{i}"
        ms = {a: int(b) // 1024 for a, b in (l.split() for l in open(os.path.join(d, "memory.stat")))}
        cg.append({"usage": int(open(os.path.join(d, "memory.usage_in_bytes")).read()) // 1024, **{k: ms[k] for k in ("rss", "cache", "mapped_file")}})
    return {"app": group(apps), "shim": group(shims), "shim_names": sorted({comm(p) for p in shims}), "tree_of_shims": group(descendants(shims)), "tree_comms": sorted({comm(p) for p in descendants(shims) if os.path.exists(f"/proc/{p}/comm")}), "proxy": group(pids_named("docker-proxy")),
            "daemons": group(pids_named("dockerd", "containerd")), "cgroup_sum_kb": {k: sum(x[k] for x in cg) for k in ("usage", "rss", "cache", "mapped_file")}}


def cmd_mem(n, rep):
    base = 21000
    names = [f"ctr-mem-{k}" for k in range(n)]
    subprocess.run(["docker", "rm", "-f", *names], capture_output=True)
    d = os.path.join(W, "mem-data"); shutil.rmtree(d, ignore_errors=True)
    out = {"target": "container-gvisor", "runtime": "runsc", "n": n, "rep": rep, "images": f"{PREFIX}0..{n - 1} (one distinct static binary per container)", "server_cpu": CPU,
           "docker_version": sh("docker", "version", "-f", "{{.Server.Version}}")}
    time.sleep(1)
    out["daemons_before_kb"] = group(pids_named("dockerd", "containerd"))
    ids, starts = [], []
    try:
        for k, name in enumerate(names):
            os.makedirs(os.path.join(d, f"d{k}"))
            t0 = time.perf_counter()
            ids.append(sh("docker", "run", "-d", "--runtime", "runsc", "--name", name, "--cpuset-cpus", CPU, "-p", f"127.0.0.1:{base + k}:8090", "-v", f"{d}/d{k}:/data", PREFIX + str(k)))
            starts.append(round((poll("127.0.0.1", base + k, t0) - t0) * 1e6))
        out["start_to_first_byte_us"] = starts
        time.sleep(2)
        out["snapshots_kb"] = {"first": snapshot(ids)}
        time.sleep(13)
        out["snapshots_kb"]["idle15"] = snapshot(ids)
    finally:
        subprocess.run(["docker", "rm", "-f", *names], capture_output=True)
        shutil.rmtree(d, ignore_errors=True)
    f = os.path.join(R, f"container-passA-gvisor-memory-n{n}-run{rep}.json")
    json.dump(out, open(f, "w"), indent=1)
    a = out["snapshots_kb"]["first"]
    print(f"mem n={n} rep={rep}: app Pss {a['app']['Pss']} shim {a['shim']['Pss']} proxy {a['proxy']['Pss']} daemons {a['daemons']['Pss']}")


if sys.argv[1] == "start":
    cmd_start(int(sys.argv[2]))
elif sys.argv[1] == "mem":
    cmd_mem(int(sys.argv[2]), int(sys.argv[3]))
