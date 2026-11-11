#!/usr/bin/env bash
# Workload comparison: the same backend tasks as a Rust app on spared (apps/workloads-spared) and as a JavaScript worker on
# workerd (apps/workloads-workerd). Method as the same-app comparison (bench/demo/measure.sh): server under test pinned `taskset -c 0`, load
# generator `taskset -c 2`, concurrency 1 on one keep-alive connection, WARMUP requests discarded then REQS timed per workload,
# ROUNDS rounds with the target order alternating, a fresh server and fresh data per target per round.
# Before every timed workload run it waits for the 1-minute load average to drop below LOAD_MAX (default 0.3), sharing one
# budget of QUIET_BUDGET_S seconds (default 5400) across the whole run. When the budget runs out it measures anyway and marks
# that run `"quiet": false` in its JSON.
# Steps: build what is missing, verify (both servers, bench/workloads/verify.py), measure, summarize (summarize.py).
# Usage: bench/workloads/run.sh [rounds]   env: WORKLOADS="sha256 json ...", REQS (200), WARMUP (20), SKIP_VERIFY=1, OUT (workloads)
# Raw JSON -> bench/results/$OUT-<target>-<workload>-run<r>.json, $OUT-verify.json, $OUT-summary.json, $OUT-env.json.
set -euo pipefail
# The published numbers were measured without the server's request log; keep it off.
export SPARED_ACCESS_LOG=${SPARED_ACCESS_LOG:-off}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
ROUNDS=${1:-3}
OUT=${OUT:-workloads}
REQS=${REQS:-200}
WARMUP=${WARMUP:-20}
LOAD_MAX=${LOAD_MAX:-0.3}
QUIET_BUDGET_S=${QUIET_BUDGET_S:-5400}
WORKLOADS=${WORKLOADS:-noop sha256 json sql-agg sql-write markdown sieve nbody pixels sort wordfreq html gzip regex pbkdf2}
W=$ROOT/bench/work/workloads
R=$ROOT/bench/results
SPARED=${SPARED:-$ROOT/target/release/spared}
WORKERD=$ROOT/bench/work/workerd/node_modules/.bin/workerd
MARKED=$ROOT/bench/work/workerd/node_modules/marked/lib/marked.esm.js
WASM=$ROOT/apps/workloads-spared/target/wasm32-wasip2/release/workloads-spared.wasm
LG=$ROOT/bench/workloads/loadgen/target/release/wl-loadgen
SP_PORT=8180 SP_ADMIN=8181 WD_PORT=9095
SRV_CPU=0 LG_CPU=2
mkdir -p "$W" "$R"

# ---- build what is missing (never during measurement) ----
[ -x "$SPARED" ] || (cd "$ROOT" && cargo build --release -p spared -j2)
[ -x "$LG" ] || (cd "$ROOT/bench/workloads/loadgen" && cargo build --release -j2)
[ -f "$WASM" ] || (cd "$ROOT/apps/workloads-spared" && source "$ROOT/bench/env.sh" && cargo build --release --target wasm32-wasip2 -j2)
[ -x "$WORKERD" ] && [ -f "$MARKED" ] || { echo "install: cd bench/work/workerd && npm i workerd@1.20261110.1 marked@18.2.0" >&2; exit 1; }

QUIET_SPENT=0
quiet() { # bounded wait for loadavg1 < LOAD_MAX; sets Q=true if quiet, Q=false if the shared budget ran out
  while :; do
    if awk -v m="$LOAD_MAX" '{exit !($1 < m)}' /proc/loadavg; then Q=true; return; fi
    if [ "$QUIET_SPENT" -ge "$QUIET_BUDGET_S" ]; then Q=false; return; fi
    sleep 5; QUIET_SPENT=$((QUIET_SPENT + 5))
  done
}

PID=""
stop() {
  if [ -n "$PID" ]; then kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null || true; fi
  PID=""; sleep 1
}
trap stop EXIT
wait_port() { for _ in $(seq 200); do (exec 3<>/dev/tcp/127.0.0.1/$1) 2>/dev/null && return 0; sleep 0.1; done; echo "port $1 never opened" >&2; exit 1; }

start_spared() { # $1 = cpu list for taskset
  rm -rf "$W/spared-data" "$W/home"; mkdir -p "$W/spared-data" "$W/home"
  HOME=$W/home taskset -c "$1" "$SPARED" serve --data-dir "$W/spared-data" --listen 127.0.0.1:$SP_PORT \
    --admin-listen 127.0.0.1:$SP_ADMIN --idle-secs 3600 >"$W/spared.log" 2>&1 &
  PID=$!
  wait_port $SP_ADMIN
  for _ in $(seq 50); do [ -s "$W/spared-data/admin-token" ] && break; sleep 0.1; done
  curl -fsS -X PUT -H "authorization: Bearer $(cat "$W/spared-data/admin-token")" \
    -H "x-spared-manifest: $(base64 -w0 "$ROOT/apps/workloads-spared/spared.toml")" \
    --data-binary @"$WASM" http://127.0.0.1:$SP_ADMIN/v1/apps/workloads >"$W/deploy.json"
  curl -fsS -H "Host: workloads.localhost" http://127.0.0.1:$SP_PORT/w/sql-seed >/dev/null
}

start_workerd() { # $1 = cpu list
  rm -rf "$W/workerd-data"; mkdir -p "$W/workerd-data"
  sed "s#../../bench/work/workloads/workerd-data#$W/workerd-data#; s#127.0.0.1:9095#127.0.0.1:$WD_PORT#" \
    "$ROOT/apps/workloads-workerd/config.capnp" >"$ROOT/apps/workloads-workerd/config.run.capnp"
  (cd "$ROOT/apps/workloads-workerd" && exec taskset -c "$1" "$WORKERD" serve config.run.capnp) >"$W/workerd.log" 2>&1 &
  PID=$!
  wait_port $WD_PORT
  curl -fsS http://127.0.0.1:$WD_PORT/w/sql-seed >/dev/null
}

# ---- environment record ----
python3 - "$R/$OUT-env.json" "$ROOT" <<'PY'
import json, subprocess, sys, re
out, root = sys.argv[1:]
sh = lambda c: subprocess.run(c, shell=True, capture_output=True, text=True, cwd=root).stdout.strip()
lock = open(f"{root}/apps/workloads-spared/Cargo.lock").read()
crates = {m[0]: m[1] for m in re.findall(r'name = "([^"]+)"\nversion = "([^"]+)"', lock)}
keep = ["wstd", "rusqlite", "libsqlite3-sys", "serde_json", "sha2", "hmac", "pbkdf2", "pulldown-cmark", "flate2", "miniz_oxide", "regex", "regex-automata", "base64"]
pkg = lambda p: json.load(open(f"{root}/bench/work/workerd/node_modules/{p}/package.json"))["version"]
json.dump({"kernel": sh("uname -sr"), "cpu": sh("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2").strip(), "nproc": sh("nproc"),
  "rustc": sh("rustc --version"), "workerd_npm": pkg("workerd"), "workerd_version": sh("bench/work/workerd/node_modules/.bin/workerd --version"),
  "marked": pkg("marked"), "rust_crates": {k: crates.get(k) for k in keep},
  "release_profile": "opt-level=3, lto=true, codegen-units=1, strip=true (same as apps/demo-spared)",
  "spared_limits": open(f"{root}/apps/workloads-spared/spared.toml").read()}, open(out, "w"), indent=1)
PY

# ---- verify: both servers up (unpinned), every route compared ----
if [ "${SKIP_VERIFY:-0}" != 1 ]; then
  start_spared 0-3; SPID=$PID; PID=""
  start_workerd 0-3
  python3 "$ROOT/bench/workloads/verify.py" --spared http://127.0.0.1:$SP_PORT --workerd http://127.0.0.1:$WD_PORT --out "$R/$OUT-verify.json"
  stop; PID=$SPID; stop
fi

# ---- measure ----
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) = 1 ]; then order="spared workerd"; else order="workerd spared"; fi
  for t in $order; do
    if [ "$t" = spared ]; then start_spared $SRV_CPU; addr=127.0.0.1:$SP_PORT; host=workloads.localhost
    else start_workerd $SRV_CPU; addr=127.0.0.1:$WD_PORT; host=127.0.0.1:$WD_PORT; fi
    for wl in $WORKLOADS; do
      n=$REQS; [ "$wl" = noop ] && n=$((REQS * 10))
      quiet; q=$Q; load=$(cut -d' ' -f1 /proc/loadavg)
      f="$R/$OUT-$t-$wl-run$r.json"
      taskset -c $LG_CPU "$LG" --addr "$addr" --host "$host" --path "/w/$wl" --warmup "$WARMUP" --requests "$n" --pids "$PID" --out "$f.tmp"
      python3 - "$f.tmp" "$f" "$t" "$wl" "$r" "$q" "$load" "$(cut -d' ' -f1 /proc/loadavg)" <<'PY'
import json, sys
src, dst, t, wl, r, q, l0, l1 = sys.argv[1:]
d = json.load(open(src))
d.update({"target": t, "workload": wl, "run": int(r), "quiet": q == "true", "loadavg1_before": float(l0), "loadavg1_after": float(l1)})
json.dump(d, open(dst, "w")); print(f"run {r} {t:8s} {wl:10s} p50 {d['full_body_us']['p50']:>8} us  p99 {d['full_body_us']['p99']:>8} us  cpu {d['server_cpu_us_per_request']:>8} us  load {l0}{'' if q == 'true' else ' (NOT QUIET)'}")
PY
      rm -f "$f.tmp"
    done
    stop
  done
done
echo "quiet wait used: ${QUIET_SPENT}s of ${QUIET_BUDGET_S}s"
python3 "$ROOT/bench/workloads/summarize.py" --results "$R" --prefix "$OUT" --rounds "$ROUNDS"
