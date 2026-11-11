#!/usr/bin/env python3
"""Per-app memory growth with N distinct hot apps (same procedure as PARTS=distinct in bench/demo/measure.sh,
own ports so it can run beside other measurements).

usage: mem.py --label L --out file.json [--n 50] [--tune SPARED_TUNE] (--wasm one.wasm | --dir dir_with_v{k}.wasm)
--wasm: N copies of one component made distinct by a unique custom section (different sha256, so the server
compiles and maps each separately; nothing is shared between them).
"""
import argparse, base64, hashlib, json, os, shutil, subprocess, sys, tempfile, time, urllib.request

# The published numbers were measured without the server's request log; keep it off.
os.environ.setdefault("SPARED_ACCESS_LOG", "off")

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SP = os.path.join(ROOT, "bench", "work", "tuning")
SPARED = os.environ.get("SPARED", os.path.join(ROOT, "target", "release", "spared"))
PUB, ADM = "127.0.0.1:28417", "127.0.0.1:28418"
urllib.request.install_opener(urllib.request.build_opener(urllib.request.ProxyHandler({})))


def leb(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)


def distinct(wasm, k):
    name = b"spared-distinct"
    payload = leb(len(name)) + name + f"variant {k}".encode()
    return wasm + b"\x00" + leb(len(payload)) + payload


def snap(pid):
    r = {}
    for l in open(f"/proc/{pid}/status"):
        p = l.split()
        if p[0] in ("VmRSS:", "RssAnon:", "RssFile:"):
            r[{"VmRSS:": "rss", "RssAnon:": "anon", "RssFile:": "file"}[p[0]]] = int(p[1])
    for l in open(f"/proc/{pid}/smaps_rollup"):
        p = l.split()
        if p[0] in ("Pss:", "Pss_Anon:", "Pss_File:"):
            r[{"Pss:": "pss", "Pss_Anon:": "pss_anon", "Pss_File:": "pss_file"}[p[0]]] = int(p[1])
    return r


def serve(data, env):
    p = subprocess.Popen(["taskset", "-c", "0", SPARED, "serve", "--data-dir", data, "--listen", PUB, "--admin-listen", ADM,
                          "--idle-secs", "600"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        try:
            urllib.request.urlopen(f"http://{ADM}/", timeout=1)
            break
        except urllib.error.HTTPError:
            break
        except Exception:
            time.sleep(0.1)
    assert p.poll() is None, "server did not start"
    return p


def stop(p):
    p.terminate()
    p.wait()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--n", type=int, default=50)
    ap.add_argument("--tune")
    ap.add_argument("--wasm")
    ap.add_argument("--dir")
    a = ap.parse_args()
    env = dict(os.environ)
    if a.tune:
        env["SPARED_TUNE"] = a.tune
    os.makedirs(os.path.join(SP, "data"), exist_ok=True)
    root = tempfile.mkdtemp(dir=os.path.join(SP, "data"))
    try:
        data = os.path.join(root, "d")
        os.makedirs(data)
        p = serve(data, env)
        tok = open(os.path.join(data, "admin-token")).read().strip()
        sizes, hashes = [], set()
        base = open(a.wasm, "rb").read() if a.wasm else None
        for k in range(a.n):
            w = distinct(base, k) if base else open(os.path.join(a.dir, f"v{k}.wasm"), "rb").read()
            sizes.append(len(w))
            hashes.add(hashlib.sha256(w).hexdigest())
            m = base64.b64encode(f'name = "demo-{k}"\n[grants]\ndata = true\n'.encode()).decode()
            req = urllib.request.Request(f"http://{ADM}/v1/apps/demo-{k}", data=w, method="PUT",
                                         headers={"authorization": f"Bearer {tok}", "x-spared-manifest": m})
            urllib.request.urlopen(req, timeout=300).read()
        stop(p)
        cw = []
        for dp, _, fs in os.walk(data):
            cw += [os.path.getsize(os.path.join(dp, f)) for f in fs if f.endswith(".cwasm") and not os.path.islink(os.path.join(dp, f))]
        p = serve(data, env)
        time.sleep(1)
        before = snap(p.pid)

        def touch(k):
            req = urllib.request.Request(f"http://{PUB}/", headers={"Host": f"demo-{k}.localhost"})
            body = urllib.request.urlopen(req, timeout=60).read()
            assert len(body) > 100, body[:200]

        touch(0)
        time.sleep(1)
        a1 = snap(p.pid)
        for k in range(1, a.n):
            touch(k)
        time.sleep(1)
        an = snap(p.pid)
        stop(p)
        res = {"label": a.label, "apps_hot": a.n, "tune": a.tune, "source": a.wasm or a.dir, "unique_sha256": len(hashes),
               "wasm_bytes_median": sorted(sizes)[len(sizes) // 2],
               "cwasm_bytes_median": sorted(cw)[len(cw) // 2] if cw else None, "cwasm_files": len(cw),
               "kb_total": {"before_touch": before, "after_1_hot": a1, "after_n_hot": an},
               "kb_per_hot_app_growth": {x: (an[x] - before[x]) // a.n for x in before}}
        json.dump(res, open(a.out, "w"), indent=1)
        g = res["kb_per_hot_app_growth"]
        print(f"{a.label}: wasm {res['wasm_bytes_median']} cwasm {res['cwasm_bytes_median']} ({len(cw)} files) per-app KB: pss {g['pss']} "
              f"pss_file {g['pss_file']} pss_anon {g['pss_anon']} rss_file {g['file']} rss_anon {g['anon']}")
    finally:
        shutil.rmtree(root, ignore_errors=True)


main()
