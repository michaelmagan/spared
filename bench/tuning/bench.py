#!/usr/bin/env python3
"""Interleaved SQLite-microbenchmark-style runs over several variants (SQLite tuning, docs/sqlite-in-wasm-speedup.md).

Binaries come from bench/tuning/build.sh and live in bench/work/tuning/bin. The host is $SPARED or
the lab build, target/lab/release/spared (cargo build --release --features lab --target-dir target/lab). Engine knobs (tune) need bench/tuning/engine-knobs.patch applied to
the host; without it only the pseudo keys below have an effect.

usage: bench.py [--reps N] [--quiet LOAD] [--n ROWS] [--journal WAL] [--out file.json] SPEC...
SPEC = label:kind:binary[:tune[:extra_sql[:vs_native_label]]]
  kind = wasm | native ; binary = name under bin/ (wasm: NAME.wasm, native: NAME.native)
  tune = SPARED_TUNE string, with pseudo keys noepoch=1 (no epoch interruption) and hop=1 (the first
         SQLite microbenchmark harness: sync WASI linker, every file call on a blocking-pool thread) ; '-' for none
The first native SPEC is the comparator for ratios (or --pair to pair wasm i with native i).
"""
import argparse, hashlib, json, os, shutil, statistics as st, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SP = os.path.join(ROOT, "bench", "work", "tuning")
BIN = os.path.join(SP, "bin")
SPARED = os.environ.get("SPARED", os.path.join(ROOT, "target", "lab", "release", "spared"))
QS = ["insert_bulk", "point_select", "index_agg", "scan", "update_bulk", "durable_insert_200"]


def wait_quiet(limit, max_s):
    t0 = time.time()
    while True:
        la = float(open("/proc/loadavg").read().split()[0])
        if la < limit:
            return la
        if time.time() - t0 > max_s:
            raise TimeoutError(f"loadavg never below {limit} within {max_s}s (now {la})")
        time.sleep(1)


def tune_env(tune):
    env = dict(os.environ)
    keys = []
    for kv in (tune or "").split(","):
        if not kv or kv == "-":
            continue
        if kv.startswith("noepoch"):
            env["SPARED_NO_EPOCH"] = "1"
        elif kv.startswith("hop"):
            env["SPARED_PROBE_LEGACY_IO"] = "1"
        else:
            keys.append(kv)
    if keys:
        env["SPARED_TUNE"] = ",".join(keys)
    return env


def cwasm_for(binary, tune):
    env = tune_env(tune)
    src = os.path.join(BIN, binary + ".wasm")
    h = hashlib.sha256((open(src, "rb").read()) + (tune or "").encode()).hexdigest()[:16]
    out = os.path.join(SP, "cw", f"{binary}-{h}.cwasm")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    if not os.path.exists(out):
        subprocess.run([SPARED, "compile", src, out], env=env, check=True)
    return out


def run_one(spec, n, journal, cpu):
    os.makedirs(os.path.join(SP, "data"), exist_ok=True)
    d = tempfile.mkdtemp(dir=os.path.join(SP, "data"))
    try:
        args = ["bench", str(n), journal] + ([spec["sql"]] if spec["sql"] else [])
        if spec["kind"] == "wasm":
            cmd = ["taskset", "-c", cpu, SPARED, "run-probe", spec["cwasm"], "--data", d, "--"] + args
            env = tune_env(spec["tune"])
        else:
            cmd = ["taskset", "-c", cpu, os.path.join(BIN, spec["bin"] + ".native")] + args
            env = dict(os.environ, DB=os.path.join(d, "app.db"))
        out = subprocess.run(cmd, env=env, check=True, capture_output=True, text=True).stdout
        return {l.split()[1]: int(l.split()[2]) for l in out.splitlines() if l.startswith("B ")}
    finally:
        shutil.rmtree(d, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("--quiet", type=float, default=0.3)
    ap.add_argument("--max-wait", type=int, default=3600)
    ap.add_argument("--n", type=int, default=100000)
    ap.add_argument("--journal", default="WAL")
    ap.add_argument("--out")
    ap.add_argument("--pair", action="store_true")
    ap.add_argument("--cpu", default="0")
    ap.add_argument("--stat", default="median", choices=["median", "min"])
    ap.add_argument("specs", nargs="+")
    a = ap.parse_args()
    specs = []
    for s in a.specs:
        p = s.split(":", 5) + [None] * 6
        sp = {"label": p[0], "kind": p[1], "bin": p[2], "tune": None if p[3] in (None, "-", "") else p[3],
              "sql": None if p[4] in (None, "-", "") else p[4], "vs": p[5]}
        if sp["kind"] == "wasm":
            sp["cwasm"] = cwasm_for(sp["bin"], sp["tune"])
            sp["wasm_bytes"] = os.path.getsize(os.path.join(BIN, sp["bin"] + ".wasm"))
            sp["cwasm_bytes"] = os.path.getsize(sp["cwasm"])
        specs.append(sp)
    runs = {s["label"]: {q: [] for q in QS} for s in specs}
    loads = []
    done_reps, note = 0, None
    for r in range(a.reps):
        order = specs if r % 2 == 0 else specs[::-1]
        rep = {}
        try:
            for s in order:
                if a.quiet > 0:
                    loads.append(wait_quiet(a.quiet, a.max_wait))
                rep[s["label"]] = run_one(s, a.n, a.journal, a.cpu)
        except TimeoutError as e:
            note = f"stopped after {done_reps} complete reps: {e}"
            print(note, file=sys.stderr)
            break
        for s in specs:
            for q in QS:
                runs[s["label"]][q].append(rep[s["label"]][q])
        done_reps += 1
    if done_reps == 0:
        sys.exit(3)
    f = st.median if a.stat == "median" else min
    natives = [s for s in specs if s["kind"] == "native"]
    wasms = [s for s in specs if s["kind"] == "wasm"]
    out = {"n": a.n, "journal": a.journal, "reps": done_reps, "stat": a.stat, "quiet_limit": a.quiet, "cpus": a.cpu, "note": note,
           "max_loadavg_before_run": max(loads) if loads else None, "variants": {}}
    for s in specs:
        out["variants"][s["label"]] = {"kind": s["kind"], "binary": s["bin"], "tune": s["tune"], "extra_sql": s["sql"],
                                       "us": {q: f(runs[s["label"]][q]) for q in QS}, "runs": runs[s["label"]]}
        for k in ("wasm_bytes", "cwasm_bytes"):
            if k in s:
                out["variants"][s["label"]][k] = s[k]
    hdr = f"{'variant':<28}" + "".join(f"{q[:12]:>13}" for q in QS) + f"{'geomean':>9}"
    print(hdr)
    for s in natives:
        u = out["variants"][s["label"]]["us"]
        print(f"{s['label']:<28}" + "".join(f"{u[q]:>13.0f}" for q in QS))
    for i, s in enumerate(wasms):
        if not natives:
            u = out["variants"][s["label"]]["us"]
            print(f"{s['label']:<28}" + "".join(f"{u[q]:>13.0f}" for q in QS))
            continue
        nat = natives[min(i, len(natives) - 1)] if a.pair else natives[0]
        if s["vs"]:
            nat = [x for x in natives if x["label"] == s["vs"]][0]
        u, nu = out["variants"][s["label"]]["us"], out["variants"][nat["label"]]["us"]
        ratio = {q: round(u[q] / nu[q], 3) for q in QS}
        gm = round(st.geometric_mean(ratio.values()), 3)
        out["variants"][s["label"]].update({"vs": nat["label"], "ratio": ratio, "geomean": gm})
        print(f"{s['label']:<28}" + "".join(f"{ratio[q]:>13.2f}" for q in QS) + f"{gm:>9.2f}")
        print(f"{'  us':<28}" + "".join(f"{u[q]:>13.0f}" for q in QS))
    if a.out:
        json.dump(out, open(a.out, "w"), indent=1)


main()
