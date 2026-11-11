#!/usr/bin/env python3
"""B6: kill -9 `spared serve` during concurrent HTTP writes; every POST that returned 201 must be in the app's SQLite file afterwards,
integrity_check must be ok, and the restarted server must answer. Usage: b6-crash-serve.py BIN TEMPLATE_DIR TRIALS WRITERS OUT.json
TEMPLATE_DIR: data dir with app demo-0 deployed (never requested). Server pinned to CPU 0 (taskset)."""
import http.client, json, os, random, shutil, signal, sqlite3, subprocess, sys, threading, time

bin_, tpl, trials, writers, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
W = os.path.dirname(tpl.rstrip("/")) + "/b6-data"
env = dict(os.environ, SPARED_ACCESS_LOG="off")


def start(d):
    p = subprocess.Popen(["taskset", "-c", "0", bin_, "serve", "--data-dir", d, "--idle-secs", "600"], stdout=subprocess.DEVNULL, stderr=open(W + ".log", "a"), env=env)
    for _ in range(200):
        try:
            c = http.client.HTTPConnection("127.0.0.1", 8080, timeout=5)
            c.request("GET", "/api/items", headers={"Host": "demo-0.localhost"})
            r = c.getresponse(); r.read()
            if r.status == 200:
                return p
        except OSError:
            time.sleep(0.05)
    raise SystemExit("server did not start")


res = {"trials": trials, "writers": writers, "acked_total": 0, "trials_with_lost_acked": 0, "integrity_failures": 0, "restart_failures": 0, "post_restart_write_failures": 0, "details": []}
for t in range(1, trials + 1):
    shutil.rmtree(W, ignore_errors=True)
    shutil.copytree(tpl, W, symlinks=True)
    p = start(W)
    acked, lock, stop = [], threading.Lock(), threading.Event()

    def writer(i):
        seq = 0
        try:
            c = http.client.HTTPConnection("127.0.0.1", 8080, timeout=10)
            while not stop.is_set():
                seq += 1
                txt = f"t{t}-w{i}-{seq}"
                c.request("POST", "/api/items", body=json.dumps({"text": txt}), headers={"Host": "demo-0.localhost", "content-type": "application/json"})
                r = c.getresponse(); r.read()
                if r.status == 201:
                    with lock:
                        acked.append(txt)
        except Exception:
            pass

    ths = [threading.Thread(target=writer, args=(i,), daemon=True) for i in range(writers)]
    for th in ths:
        th.start()
    time.sleep(random.uniform(0.3, 1.5))
    os.kill(p.pid, signal.SIGKILL)
    p.wait()
    stop.set()
    for th in ths:
        th.join(timeout=5)
    with lock:
        ack = list(acked)
    db = os.path.join(W, "apps", "demo-0", "data", "app.db")
    lost, integ, nrows = None, None, None
    try:
        con = sqlite3.connect(db)
        integ = con.execute("PRAGMA integrity_check").fetchone()[0]
        have = {r[0] for r in con.execute("SELECT text FROM items")}
        nrows = len(have)
        lost = [a for a in ack if a not in have]
        con.close()
    except Exception as e:
        integ = f"error {e}"
    res["acked_total"] += len(ack)
    if lost:
        res["trials_with_lost_acked"] += 1
    if integ != "ok":
        res["integrity_failures"] += 1
    ok_restart = ok_write = False
    try:
        p2 = start(W)
        ok_restart = True
        c = http.client.HTTPConnection("127.0.0.1", 8080, timeout=10)
        c.request("POST", "/api/items", body=json.dumps({"text": f"after-{t}"}), headers={"Host": "demo-0.localhost", "content-type": "application/json"})
        r = c.getresponse(); r.read()
        ok_write = r.status == 201
        p2.send_signal(signal.SIGTERM); p2.wait(timeout=30)
    except Exception as e:
        print("restart error", e, file=sys.stderr)
    if not ok_restart:
        res["restart_failures"] += 1
    elif not ok_write:
        res["post_restart_write_failures"] += 1
    res["details"].append({"trial": t, "acked": len(ack), "rows": nrows, "lost": len(lost or []), "integrity": integ, "restart_ok": ok_restart, "write_after_ok": ok_write})
    if lost or integ != "ok" or not ok_restart or not ok_write:
        print(f"trial {t} PROBLEM acked={len(ack)} rows={nrows} lost={len(lost or [])} integ={integ} restart={ok_restart} write={ok_write}", file=sys.stderr)
        shutil.copytree(W, f"{W}-fail-{t}", symlinks=True)
json.dump(res, open(out, "w"))
print(json.dumps({k: v for k, v in res.items() if k != "details"}))
