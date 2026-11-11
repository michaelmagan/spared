#!/usr/bin/env python3
"""Memory and start time of N containers, each running the native demo app (apps/demo-native, static, FROM scratch) with its own data.

One point: record the Docker daemons with no container, start N containers one after another (`docker run -d`, bridge network, a
published loopback port each, CPU 0, the data directory bind-mounted at /data), time each from `docker run` to the first response byte
of GET /, then snapshot 2 s and 15 s after the last first response. Snapshots sum, per group of processes, VmRSS/RssAnon/RssFile from
/proc/PID/status and Pss/Pss_Anon/Pss_File from /proc/PID/smaps_rollup (as bench/demo/mem-curve.py does):
  app      the N app processes (each container's init process; the app is the only process in its container)
  shim     the containerd-shim processes that parent the app processes
  proxy    the docker-proxy processes (one per published port)
  daemons  dockerd and containerd
and read each container's memory cgroup (v1) memory.stat (rss, cache, mapped_file). Requests come from this process.
Usage: mem-container.py N RUN  -> bench/results/container-memory-nN-runRUN.json"""
import glob, http.client, json, os, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
W = os.path.join(ROOT, "bench", "work", "native", "ctr-mem")
R = os.path.join(ROOT, "bench", "results")
IMAGE = os.environ.get("IMAGE", "spared-demo-native:bench")
BASE = 21000
SRV_CPU = os.environ.get("SRV_CPU", "0")
n, run = int(sys.argv[1]), int(sys.argv[2])
KEYS_STATUS = ("VmRSS", "RssAnon", "RssFile")
KEYS_SMAPS = ("Pss", "Pss_Anon", "Pss_File")


def kv(path):
    d = {}
    for line in open(path):
        parts = line.split()
        if len(parts) >= 2 and parts[0].endswith(":") and parts[1].isdigit():
            d[parts[0][:-1]] = int(parts[1])
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
    s = {k: 0 for k in KEYS_STATUS + KEYS_SMAPS}
    s["pids"] = len(pids)
    for p in pids:
        st, sr = kv(f"/proc/{p}/status"), kv(f"/proc/{p}/smaps_rollup")
        for k in KEYS_STATUS:
            s[k] += st.get(k, 0)
        for k in KEYS_SMAPS:
            s[k] += sr.get(k, 0)
    return s


def first_byte(port, t0, timeout=30):
    while time.perf_counter() - t0 < timeout:
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            c.request("GET", "/")
            r = c.getresponse()
            t = time.perf_counter()
            r.read(); c.close()
            if r.status == 200:
                return t
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.0005)
    raise SystemExit(f"no response on port {port}")


def snapshot(ids):
    apps = [int(subprocess.check_output(["docker", "inspect", "-f", "{{.State.Pid}}", i], text=True)) for i in ids]
    shims = sorted({ppid(p) for p in apps})
    cg = []
    for i in ids:
        d = f"/sys/fs/cgroup/memory/docker/{i}"
        ms = {a: int(b) // 1024 for a, b in (line.split() for line in open(os.path.join(d, "memory.stat")))}
        cg.append({"usage": int(open(os.path.join(d, "memory.usage_in_bytes")).read()) // 1024, **{k: ms[k] for k in ("rss", "cache", "mapped_file")}})
    cgsum = {k: sum(x[k] for x in cg) for k in ("usage", "rss", "cache", "mapped_file")}
    return {"app": group(apps), "shim": group(shims), "shim_names": sorted({comm(p) for p in shims}), "proxy": group(pids_named("docker-proxy")),
            "daemons": group(pids_named("dockerd", "containerd")), "cgroup_sum_kb": cgsum}


def main():
    names = [f"ctr-mem-{k}" for k in range(n)]
    subprocess.run(["docker", "rm", "-f", *names], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.rmtree(W, ignore_errors=True)
    out = {"target": "container", "n": n, "run": run, "image": IMAGE, "server_cpu": SRV_CPU,
           "image_id": subprocess.check_output(["docker", "image", "inspect", "-f", "{{.Id}}", IMAGE], text=True).strip(),
           "docker_version": subprocess.check_output(["docker", "version", "-f", "{{.Server.Version}}"], text=True).strip()}
    time.sleep(1)
    out["daemons_before_kb"] = group(pids_named("dockerd", "containerd"))
    ids, starts = [], []
    try:
        for k, name in enumerate(names):
            os.makedirs(os.path.join(W, f"d{k}"))
            t0 = time.perf_counter()
            cid = subprocess.check_output(["docker", "run", "-d", "--name", name, "--cpuset-cpus", SRV_CPU, "-p", f"127.0.0.1:{BASE + k}:8090",
                                           "-v", f"{W}/d{k}:/data", IMAGE], text=True).strip()
            starts.append(round((first_byte(BASE + k, t0) - t0) * 1e6))
            ids.append(cid)
        out["start_to_first_byte_us"] = starts
        s = sorted(starts)
        out["start_to_first_byte_us_p50"] = s[(len(s) - 1) // 2] if len(s) % 2 else (s[len(s) // 2 - 1] + s[len(s) // 2]) // 2
        time.sleep(2)
        out["snapshots_kb"] = {"first": snapshot(ids)}
        time.sleep(13)
        out["snapshots_kb"]["idle15"] = snapshot(ids)
    finally:
        subprocess.run(["docker", "rm", "-f", *names], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        shutil.rmtree(W, ignore_errors=True)
    f = os.path.join(R, f"container-memory-n{n}-run{run}.json")
    json.dump(out, open(f, "w"), indent=1)
    a = out["snapshots_kb"]["first"]
    print(f"container n={n} run={run}: app Pss {a['app']['Pss']} shim Pss {a['shim']['Pss']} proxy Pss {a['proxy']['Pss']} "
          f"daemons Pss {a['daemons']['Pss']} start p50 {out['start_to_first_byte_us_p50']} us -> {os.path.basename(f)}")


main()
