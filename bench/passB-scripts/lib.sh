# Shared helpers for the pass-A drivers. Source this file.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
export WASI_SDK_PATH=${WASI_SDK_PATH:-$("$ROOT/scripts/wasi-sdk-default.sh")}
if [ -z "$WASI_SDK_PATH" ]; then
  echo "lib.sh: wasi-sdk not found. Fix: run 'spared setup' (installs ~/.spared/wasi-sdk), or set WASI_SDK_PATH to a wasi-sdk 25 directory" >&2
  return 1 2>/dev/null || exit 1
fi
R=$ROOT/bench/results
cd "$ROOT"
steal_snap() { awk '/^cpu /{s=0; for(i=2;i<=9;i++) s+=$i; print $9, s}' /proc/stat; }  # "steal_jiffies total_jiffies" (USER_HZ ticks, all 4 vCPUs)
leftovers() { pkill -x spared 2>/dev/null; pkill -x workerd 2>/dev/null; pkill -x loadgen 2>/dev/null; pkill -x wl-loadgen 2>/dev/null; pkill -x demo-native 2>/dev/null; pkill -f 'demo-distinct' 2>/dev/null; pkill -f mem-curve.py 2>/dev/null; true; }
wait_quiet() { # kill stray processes, wait for 1-min load < 0.3 (up to 40 min)
  leftovers; sleep 1
  for _ in $(seq 480); do awk '{exit !($1 < 0.3)}' /proc/loadavg && return 0; sleep 5; done
  echo "NOT QUIET after 40 min: $(cat /proc/loadavg)" >&2; return 1
}
# seg_begin SUITE LABEL ; seg_end SUITE LABEL  -> appends to bench/results/SUITE-passB-steal.jsonl
seg_begin() { read -r S0 T0 <<<"$(steal_snap)"; L0=$(cut -d' ' -f1 /proc/loadavg); D0=$(date -u +%FT%TZ); }
seg_end() {
  read -r S1 T1 <<<"$(steal_snap)"
  python3 - "$1" "$2" "$S0" "$T0" "$S1" "$T1" "$L0" "$D0" "$(date -u +%FT%TZ)" >>"$R/$1-passB-steal.jsonl" <<'PY'
import json, sys
suite, label, s0, t0, s1, t1, l0, d0, d1 = sys.argv[1:]
ds, dt = int(s1) - int(s0), int(t1) - int(t0)
print(json.dumps({"suite": suite, "segment": label, "start": d0, "end": d1, "loadavg1_at_start": float(l0),
                  "steal_jiffies_before": int(s0), "steal_jiffies_after": int(s1), "steal_delta": ds, "all_cpu_jiffies_delta": dt,
                  "steal_pct": round(100 * ds / dt, 3) if dt else None}))
PY
}
env_snap() { # label -> one line of env facts into bench/results/passB-env.jsonl
  python3 - "$1" >>"$R/passB-env.jsonl" <<'PY'
import json,sys,subprocess,datetime
def sh(c): return subprocess.run(c,shell=True,capture_output=True,text=True).stdout.strip()
print(json.dumps({"label":sys.argv[1],"time":datetime.datetime.utcnow().isoformat()+"Z","kernel":sh("uname -r"),"cpu":sh("grep -m1 'model name' /proc/cpuinfo"),
 "nproc":sh("nproc"),"loadavg":open("/proc/loadavg").read().strip(),"mem_free_mb":sh("awk '/MemAvailable/{print int($2/1024)}' /proc/meminfo"),
 "head":sh("git rev-parse --short HEAD")}))
PY
}
DEMO_WASM=$ROOT/apps/demo-spared/target/wasm32-wasip2/release/demo-spared.wasm
inner_quiet() { for _ in $(seq 600); do awk '{exit !($1 < 0.3)}' /proc/loadavg && return 0; sleep 1; done; return 0; }
mk_template_in() { # binary dir [admin apps path, default /v1/apps] : demo-0..99 deployed (never requested), compiled by that binary
  local bin=$1 t=$2 ap=${3:-/v1/apps}
  [ -d "$t/apps/demo-99" ] && return
  rm -rf "$t"; mkdir -p "$t"
  "$bin" serve --data-dir "$t" --listen 127.0.0.1:8080 --admin-listen 127.0.0.1:8081 --idle-secs 600 >"$t.deploy.log" 2>&1 &
  local p=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  for k in $(seq 0 99); do
    M=$(printf 'name = "demo-%s"\n[grants]\ndata = true\n' "$k" | base64 -w0)
    curl -fsS -X PUT -H "authorization: Bearer $(cat "$t/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$DEMO_WASM" http://127.0.0.1:8081$ap/demo-$k >/dev/null
  done
  kill $p; wait $p 2>/dev/null || true
}
wait_tcp() { for _ in $(seq 200); do (exec 3<>/dev/tcp/127.0.0.1/$1) 2>/dev/null && return 0; sleep 0.1; done; return 1; }
LG=$ROOT/bench/demo/loadgen/target/release/loadgen
WORKERD=$ROOT/bench/work/workerd/node_modules/.bin/workerd
