#!/usr/bin/env bash
# Same-app comparison against workerd. Server pinned to CPU 0, load generator to CPU 2, 3 rounds, target order alternates.
# PARTS=pss: memory per hot app with 50 apps, including PSS from /proc/PID/smaps_rollup, for spared (same wasm under 50
# names, and 3 different components cycled over 50 names) and workerd (50 Durable Objects). Needs no quiet machine (memory only).
# Env for pss: PSS_TAG (label in the file names, default "current"), PSS_N (default 50), SPARED (binary to measure).
# PARTS=distinct: memory per hot app with 50 genuinely different apps (no code to share), spared (50 distinct components built from
# generated variants of apps/demo-spared, bench/demo/gen-distinct.py, built one at a time under bench/work/distinct) and workerd (one process,
# 50 distinct workers each with its own SQLite Durable Object class, bench/demo/gen-distinct-workerd.py), one GET per app, same Rss/RssAnon/
# RssFile/Pss fields as pss. Raw JSON -> bench/results/vs-workerd-mem-distinct-{spared,workerd}-run*.json plus vs-workerd-mem-distinct-components.json.
# COMPARE_CELLD=1: the three-way comparison of spared, workerd and celld 0.6.3 (celld dev, single node, local object store, durability waits for the
# object store on writes) in one run: TARGETS="spared workerd celld", OUT=vs-workerd-set2, target order rotates over rounds, phase 1 runs
# main (cold, warm, mixed) and cold-existing, phase 2 runs PARTS=pss (50 hot apps, Rss/RssAnon/RssFile/Pss). Fresh spared template per run.
# Usage: [PARTS="main mem cold-existing mem-hardlink spin-first pss"] [TARGETS="spared workerd spin"] [OUT=vs-workerd] bench/demo/measure.sh [rounds]
# Raw JSON -> bench/results/$OUT-*.json. With spin in TARGETS the order rotates across rounds (spin spared workerd / workerd spared spin /
# spared spin workerd), and spared and workerd run only the warm parts (a same-run sanity check); spin runs everything.
set -euo pipefail
# The published numbers were measured without the server's request log; keep it off.
export SPARED_ACCESS_LOG=${SPARED_ACCESS_LOG:-off}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
ROUNDS=${1:-3}
if [ "${COMPARE_CELLD:-0}" = 1 ] && [ -z "${PHASE:-}" ]; then
  rm -rf "$ROOT/bench/work/vs-workerd/spared-template" "$ROOT/bench/work/vs-workerd/pss-set2-same-template" "$ROOT/bench/work/vs-workerd/pss-set2-distinct-template"
  export TARGETS="spared workerd celld" OUT=vs-workerd-set2 PSS_TAG=set2
  PHASE=1 PARTS="main cold-existing" "$0" "$ROUNDS"
  PHASE=2 PARTS=pss "$0" "$ROUNDS"
  exit 0
fi
TARGETS=${TARGETS:-spared workerd}
OUT=${OUT:-vs-workerd}
DEFAULT_PARTS="main mem cold-existing mem-hardlink"
[[ " $TARGETS " == *" spin "* ]] && DEFAULT_PARTS="main mem cold-existing spin-first"
PARTS=${PARTS:-$DEFAULT_PARTS}
has() { [[ " $PARTS " == *" $1 "* ]]; }
W=$ROOT/bench/work/vs-workerd
R=$ROOT/bench/results
WASM=$ROOT/apps/demo-spared/target/wasm32-wasip2/release/demo-spared.wasm
WORKERD=$ROOT/bench/work/workerd/node_modules/.bin/workerd
SPARED=${SPARED:-$ROOT/target/release/spared}
SPIN_SERVE=$ROOT/bench/demo/serve-spin.sh
CELLD_SERVE=$ROOT/bench/demo/serve-celld.sh
CELLD_DIR=$ROOT/apps/demo-celld/.celld/dev
LG=$ROOT/bench/demo/loadgen/target/release/loadgen
N=100
SRV_CPU=0
LG_CPU=2
mkdir -p "$W" "$R"
[ -f "$LG" ] || (cd "$ROOT/bench/demo/loadgen" && cargo build --release -j2)

quiet() { # bounded wait for loadavg1 < 0.3
  for _ in $(seq 600); do
    awk '{exit !($1 < 0.3)}' /proc/loadavg && return 0
    sleep 1
  done
  echo "machine not quiet: $(cat /proc/loadavg)" >&2; exit 1
}
lg() { taskset -c $LG_CPU "$LG" "$@"; }

PID=""
stop() {
  if [ -n "$PID" ]; then
    pkill -P "$PID" 2>/dev/null || true
    kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null || true
  fi
  PID=""; sleep 1
}
trap stop EXIT

# Pristine spared data dir with demo-0..N-1 deployed, no app ever requested (so no database yet).
deploy_template() {
  [ -d "$W/spared-template/apps" ] && return
  rm -rf "$W/spared-template"; mkdir -p "$W/spared-template"
  "$SPARED" serve --data-dir "$W/spared-template" --listen 127.0.0.1:8080 --admin-listen 127.0.0.1:8081 --idle-secs 600 >"$W/deploy.log" 2>&1 &
  local p=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  for k in $(seq 0 $((N-1))); do
    M=$(printf 'name = "demo-%s"\n[grants]\ndata = true\n' "$k" | base64 -w0)
    curl -fsS -X PUT -H "authorization: Bearer $(cat "$W/spared-template/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$WASM" http://127.0.0.1:8081/v1/apps/demo-$k >/dev/null
  done
  kill $p; wait $p 2>/dev/null || true
}

start_spin() { # $1 = 1 for fresh data; spin up (parent) pid -> PID, its child `spin trigger http` serves
  stop
  FRESH=$1 CPU=$SRV_CPU "$SPIN_SERVE" &
  PID=$!
  for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/9191) 2>/dev/null && break; sleep 0.1; done
}

start_celld() { # $1 = 1 for fresh state; the script exec's celld, so PID is celld dev (the node process is its child). No request is sent while waiting.
  stop
  FRESH=$1 LOGS=0 CPU=$SRV_CPU "$CELLD_SERVE" >"$W/celld.log" 2>&1 &
  PID=$!
  for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/9292) 2>/dev/null && break; sleep 0.1; done
  sleep 2
}

start() { # target -> sets PID with fresh data, pinned to SRV_CPU
  stop
  if [ "$1" = spin ]; then
    start_spin 1
  elif [ "$1" = celld ]; then
    start_celld 1
  elif [ "$1" = spared ]; then
    rm -rf "$W/spared-data"; cp -a "$W/spared-template" "$W/spared-data"
    taskset -c $SRV_CPU "$SPARED" serve --data-dir "$W/spared-data" --idle-secs 600 >"$W/spared.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  else
    rm -rf "$W/workerd-data"; mkdir -p "$W/workerd-data"
    sed "s#../../bench/work/demo/workerd-data#$W/workerd-data#" "$ROOT/apps/demo-workerd/config.capnp" >"$ROOT/apps/demo-workerd/config.measure.capnp"
    (cd "$ROOT/apps/demo-workerd" && exec taskset -c $SRV_CPU "$WORKERD" serve config.measure.capnp) >"$W/workerd.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/9090) 2>/dev/null && break; sleep 0.1; done
    sleep 1
  fi
}

rss_field() { # field -> sum over process tree of PID, kB
  local pids="$PID" p c
  for p in $pids; do for c in $(pgrep -P "$p" 2>/dev/null || true); do pids="$pids $c"; done; done
  for p in $pids; do awk -v f="$1:" '$1==f{print $2}' /proc/$p/status; done | awk '{s+=$1} END{print s+0}'
}
rss_kb() { rss_field VmRSS; }
rss_procs() { # per-process "pid comm VmRSS RssAnon RssFile" for PID and its children
  local p c
  for p in $PID $(pgrep -P "$PID" 2>/dev/null || true); do
    echo "$p $(awk '$1=="Name:"{n=$2} $1=="VmRSS:"{r=$2} $1=="RssAnon:"{a=$2} $1=="RssFile:"{f=$2} END{print n, r, a, f}' /proc/$p/status)"
  done
}
rss_triple() { echo "$(rss_field VmRSS) $(rss_field RssAnon) $(rss_field RssFile)"; }

measure_mem() { # target round name
  local t=$1 r=$2 name=$3 b a1 a50
  quiet
  b=$(rss_triple)
  lg --style "$t" --mode touch --n 1 --out /dev/null; sleep 1; a1=$(rss_triple)
  lg --style "$t" --mode touch --n 50 --out /dev/null; sleep 1; a50=$(rss_triple)
  local procs; procs=$(rss_procs | tr '\n' ';')
  python3 - "$t" "$r" "$b" "$a1" "$a50" "$procs" >"$R/$OUT-$t-$name-run$r.json" <<'PY'
import json,sys
t,r,b,a1,a50,procs=sys.argv[1:]
f=lambda x: dict(zip(("rss","anon","file"),map(int,x.split())))
b,a1,a50=f(b),f(a1),f(a50)
print(json.dumps({"target":t,"run":int(r),"before_kb":b,"after_1_hot_kb":a1,"after_50_hot_kb":a50,
 "rss_kb_before":b["rss"],"rss_kb_after_1_hot":a1["rss"],"rss_kb_after_50_hot":a50["rss"],
 "procs_after_50_pid_name_rss_anon_file":procs,
 "kb_per_hot_app":(a50["rss"]-b["rss"])//50,"anon_kb_per_hot_app":(a50["anon"]-b["anon"])//50,"file_kb_per_hot_app":(a50["file"]-b["file"])//50}))
PY
}

run_target() { # target round
  local t=$1 r=$2 out="$R/$OUT"
  has main || return 0
  start "$t"; quiet
  if [ "$t" != spin ] && [ "$ONLY_WARM_OTHERS" = 1 ]; then
    for c in 1 16; do
      quiet; lg --style "$t" --mode warm --conc $c --requests 20000 --out "$out-$t-warm-c$c-run$r.json"
    done
    stop; return 0
  fi
  lg --style "$t" --mode cold --n $N --out "$out-$t-cold-run$r.json"
  for c in 1 16; do
    quiet; lg --style "$t" --mode warm --conc $c --requests 20000 --out "$out-$t-warm-c$c-run$r.json"
  done
  for c in 1 16; do
    quiet; lg --style "$t" --mode mixed --conc $c --requests 20000 --out "$out-$t-mixed-c$c-run$r.json"
  done
  stop
}

run_mem() { has mem || return 0; [ "$1" != spin ] && [ "$ONLY_WARM_OTHERS" = 1 ] && return 0; start "$1"; measure_mem "$1" "$2" mem; stop; }

# Cold as defined for the comparison: apps already hold their data (seeded, then server restarted so nothing is loaded).
run_cold_existing() {
  has cold-existing || return 0
  local t=$1 r=$2
  [ "$t" != spin ] && [ "$ONLY_WARM_OTHERS" = 1 ] && return 0
  start "$t"; quiet
  lg --style "$t" --mode touch --n $N --out /dev/null; sleep 3
  local ddir="$W/$t-data"; [ "$t" = spin ] && ddir="$ROOT/bench/work/spin-data"; [ "$t" = celld ] && ddir="$CELLD_DIR"
  echo "seeded files: $(find "$ddir" -type f \( -name '*.db' -o -name '*.sqlite*' \) | wc -l)"
  stop
  if [ "$t" = spin ]; then
    start_spin 0
  elif [ "$t" = celld ]; then
    start_celld 0
  elif [ "$t" = spared ]; then
    taskset -c $SRV_CPU "$SPARED" serve --data-dir "$W/spared-data" --idle-secs 600 >"$W/spared.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  else
    (cd "$ROOT/apps/demo-workerd" && exec taskset -c $SRV_CPU "$WORKERD" serve config.measure.capnp) >"$W/workerd.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/9090) 2>/dev/null && break; sleep 0.1; done
    sleep 1
  fi
  quiet
  lg --style "$t" --mode cold --n $N --out "$R/$OUT-$t-cold-existing-run$r.json"
  stop
}

# 50 hot apps whose component.cwasm files are hardlinks to one file (spared only).
run_mem_hardlink() {
  has mem-hardlink || return 0
  [ "$1" = spared ] || return 0
  [ "$ONLY_WARM_OTHERS" = 1 ] && return 0
  stop
  rm -rf "$W/spared-data"; cp -a "$W/spared-template" "$W/spared-data"
  for k in $(seq 1 $((N-1))); do ln -f "$W/spared-data/apps/demo-0/component.cwasm" "$W/spared-data/apps/demo-$k/component.cwasm"; done
  taskset -c $SRV_CPU "$SPARED" serve --data-dir "$W/spared-data" --idle-secs 600 >"$W/spared.log" 2>&1 &
  PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  measure_mem spared "$2" mem-hardlink
  stop
}

# PSS memory: sum over the process tree of status RssAnon/RssFile/VmRSS and smaps_rollup Pss/Pss_Anon/Pss_File.
pss_snap() { # -> "rss anon file pss pss_anon pss_file" (kB)
  local pids="$PID" p c
  for p in $pids; do for c in $(pgrep -P "$p" 2>/dev/null || true); do pids="$pids $c"; done; done
  for p in $pids; do
    awk '$1=="VmRSS:"{r=$2} $1=="RssAnon:"{a=$2} $1=="RssFile:"{f=$2} END{print "S", r+0, a+0, f+0}' /proc/$p/status
    awk '$1=="Pss:"{x=$2} $1=="Pss_Anon:"{y=$2} $1=="Pss_File:"{z=$2} END{print "P", x+0, y+0, z+0}' /proc/$p/smaps_rollup
  done | awk '$1=="S"{r+=$2;a+=$3;f+=$4} $1=="P"{x+=$2;y+=$3;z+=$4} END{print r+0, a+0, f+0, x+0, y+0, z+0}'
}

pss_json() { # target variant round empty before after1 afterN apps
  python3 - "$@" >"${PSS_FILE:-$R/$OUT-mem-pss-$PSS_TAG-$2-run$3.json}" <<'PY'
import json,sys
t,v,r,e,b,a1,an,n=sys.argv[1:]
k=("rss","anon","file","pss","pss_anon","pss_file")
f=lambda x: dict(zip(k,map(int,x.split())))
e,b,a1,an=f(e),f(b),f(a1),f(an); n=int(n)
print(json.dumps({"target":t,"variant":v,"run":int(r),"apps_hot":n,"kb_total":{"empty_server":e,"before_touch":b,"after_1_hot":a1,"after_n_hot":an},
 "kb_per_hot_app_growth":{x:(an[x]-b[x])//n for x in k},
 "kb_per_app_over_empty_server":{x:(an[x]-e[x])//n for x in k}}))
PY
}

pss_spared_template() { # variant: same | distinct. Deploys PSS_N apps under demo-K (same: one wasm; distinct: 3 wasm cycled).
  local v=$1 t="$W/pss-$PSS_TAG-$v-template"
  [ -d "$t/apps" ] && return
  rm -rf "$t"; mkdir -p "$t"
  "$SPARED" serve --data-dir "$t" --listen 127.0.0.1:8080 --admin-listen 127.0.0.1:8081 --idle-secs 600 >"$W/pss-deploy.log" 2>&1 &
  local p=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  local wasms=("$ROOT/examples/dist/demo/app.wasm" "$ROOT/examples/dist/list/app.wasm" "$ROOT/examples/dist/notes/app.wasm") k w
  for k in $(seq 0 $((PSS_N-1))); do
    if [ "$v" = same ]; then w=${wasms[0]}; else w=${wasms[$((k % 3))]}; fi
    M=$(printf 'name = "demo-%s"\n[grants]\ndata = true\n' "$k" | base64 -w0)
    curl -fsS -X PUT -H "authorization: Bearer $(cat "$t/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$w" http://127.0.0.1:8081/v1/apps/demo-$k >/dev/null
  done
  kill $p; wait $p 2>/dev/null || true
}

run_mem_pss() { # target round
  local t=$1 r=$2 v e b a1 an
  if [ "$t" = spared ]; then
    for v in same distinct; do
      stop; pss_spared_template "$v"
      rm -rf "$W/spared-data"; cp -a "$W/pss-$PSS_TAG-$v-template" "$W/spared-data"
      taskset -c $SRV_CPU "$SPARED" serve --data-dir "$W/spared-data" --idle-secs 600 >"$W/spared.log" 2>&1 &
      PID=$!
      for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
      sleep 1; b=$(pss_snap)
      lg --style spared --mode touch --n 1 --out /dev/null; sleep 1; a1=$(pss_snap)
      lg --style spared --mode touch --n "$PSS_N" --out /dev/null; sleep 1; an=$(pss_snap)
      stop
      rm -rf "$W/spared-empty"; mkdir -p "$W/spared-empty"
      taskset -c $SRV_CPU "$SPARED" serve --data-dir "$W/spared-empty" --idle-secs 600 >"$W/spared.log" 2>&1 &
      PID=$!
      for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
      sleep 1; e=$(pss_snap); stop
      pss_json spared "$v" "$r" "$e" "$b" "$a1" "$an" "$PSS_N"
    done
  else
    stop
    if [ "$t" = celld ]; then
      start_celld 1
    else
      rm -rf "$W/workerd-data"; mkdir -p "$W/workerd-data"
      sed "s#../../bench/work/demo/workerd-data#$W/workerd-data#" "$ROOT/apps/demo-workerd/config.capnp" >"$ROOT/apps/demo-workerd/config.measure.capnp"
      (cd "$ROOT/apps/demo-workerd" && exec taskset -c $SRV_CPU "$WORKERD" serve config.measure.capnp) >"$W/workerd.log" 2>&1 &
      PID=$!
      for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/9090) 2>/dev/null && break; sleep 0.1; done
      sleep 1
    fi
    b=$(pss_snap); e=$b
    lg --style "$t" --mode touch --n 1 --out /dev/null; sleep 1; a1=$(pss_snap)
    lg --style "$t" --mode touch --n "$PSS_N" --out /dev/null; sleep 1; an=$(pss_snap)
    stop
    pss_json "$t" "$([ "$t" = celld ] && echo cells || echo durable-objects)" "$r" "$e" "$b" "$a1" "$an" "$PSS_N"
  fi
}

# Spin only: the first request after a fresh `spin up` process start, on existing data, repeated over restarts (no quiet wait between restarts, one per run).
run_spin_first() {
  has spin-first || return 0
  [ "$1" = spin ] || return 0
  local r=$2 f="$W/spin-first-$2.txt"
  : >"$f"
  start_spin 1; quiet; lg --style spin --mode touch --n 1 --out /dev/null; stop
  quiet
  for i in $(seq 20); do
    start_spin 0
    lg --style spin --mode cold --n 1 --out "$W/spin-first-one.json"
    python3 -c "import json;d=json.load(open('$W/spin-first-one.json'));print(d['first_byte_us']['p50'], d['full_body_us']['p50'])" >>"$f"
    stop
  done
  python3 - "$r" "$f" >"$R/$OUT-spin-first-request-run$r.json" <<'PY'
import json,sys,statistics
r,f=sys.argv[1:]
rows=[tuple(map(int,l.split())) for l in open(f) if l.strip()]
fb=sorted(x[0] for x in rows); fu=sorted(x[1] for x in rows)
q=lambda v,p: v[min(len(v)-1,int(len(v)*p))]
print(json.dumps({"target":"spin","run":int(r),"restarts":len(rows),"first_byte_us":{"p50":q(fb,.5),"p99":q(fb,.99),"min":fb[0],"max":fb[-1]},"full_body_us":{"p50":q(fu,.5),"max":fu[-1]},"first_byte_us_all":[x[0] for x in rows]}))
PY
}

# 50 genuinely different apps. spared: deploy 50 distinct wasm under demo-K; workerd: 50 distinct workers on ports 9100+K.
DD=$ROOT/bench/work/distinct
distinct_build() {
  [ -f "$DD/wasm/v49.wasm" ] && return 0
  python3 "$ROOT/bench/demo/gen-distinct.py" 50
  ( cd "$ROOT" && source bench/env.sh && export CARGO_TARGET_DIR="$DD/target"; mkdir -p "$DD/wasm"
    for k in $(seq 0 49); do
      (cd "$DD/v$k" && cargo build --release --target wasm32-wasip2 -j2 2>&1 | tail -1)
      cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/demo-distinct-$k.wasm" "$DD/wasm/v$k.wasm"
    done )
}
distinct_components() { # sha256 and size of the 50 wasm, and how many are unique
  python3 - "$DD/wasm" >"$R/$OUT-mem-distinct-components.json" <<'PY'
import hashlib,json,sys
d=sys.argv[1]; rows=[]
for k in range(50):
    b=open(f"{d}/v{k}.wasm","rb").read(); rows.append({"app":f"demo-{k}","wasm_bytes":len(b),"sha256":hashlib.sha256(b).hexdigest()})
print(json.dumps({"apps":rows,"unique_sha256":len({r["sha256"] for r in rows}),"unique_sizes":len({r["wasm_bytes"] for r in rows}),
 "min_bytes":min(r["wasm_bytes"] for r in rows),"max_bytes":max(r["wasm_bytes"] for r in rows)}))
PY
}
touch_distinct() { # target first last
  local k
  for k in $(seq "$2" "$3"); do
    if [ "$1" = spared ]; then curl -fsS -o /dev/null -H "Host: demo-$k.localhost" http://127.0.0.1:8080/
    else curl -fsS -o /dev/null http://127.0.0.1:$((9100+k))/; fi
  done
}
run_mem_distinct() { # target round
  local t=$1 r=$2 e b a1 an k
  stop
  if [ "$t" = spared ]; then
    local tpl="$W/distinct-spared-template"
    if [ ! -d "$tpl/apps" ]; then
      rm -rf "$tpl"; mkdir -p "$tpl"
      "$SPARED" serve --data-dir "$tpl" --listen 127.0.0.1:8080 --admin-listen 127.0.0.1:8081 --idle-secs 600 >"$W/pss-deploy.log" 2>&1 &
      local p=$!
      for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
      for k in $(seq 0 $((PSS_N-1))); do
        M=$(printf 'name = "demo-%s"\n[grants]\ndata = true\n' "$k" | base64 -w0)
        curl -fsS -X PUT -H "authorization: Bearer $(cat "$tpl/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$DD/wasm/v$k.wasm" http://127.0.0.1:8081/v1/apps/demo-$k >/dev/null
      done
      kill $p; wait $p 2>/dev/null || true
    fi
    rm -rf "$W/spared-data"; cp -a "$tpl" "$W/spared-data"
    taskset -c $SRV_CPU "$SPARED" serve --data-dir "$W/spared-data" --idle-secs 600 >"$W/spared.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  else
    rm -rf "$W/workerd-data" "$W/workerd-distinct"; mkdir -p "$W/workerd-data"
    python3 "$ROOT/bench/demo/gen-distinct-workerd.py" "$W/workerd-distinct" "$W/workerd-data" "$PSS_N" >/dev/null
    (cd "$W/workerd-distinct" && exec taskset -c $SRV_CPU "$WORKERD" serve config.capnp) >"$W/workerd.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/9100) 2>/dev/null && break; sleep 0.1; done
  fi
  sleep 1; b=$(pss_snap); e=$b
  touch_distinct "$t" 0 0; sleep 1; a1=$(pss_snap)
  touch_distinct "$t" 1 $((PSS_N-1)); sleep 1; an=$(pss_snap)
  stop
  if [ "$t" = spared ]; then # empty server for the over-empty figure (apps deployed at startup cost memory before any request)
    rm -rf "$W/spared-empty"; mkdir -p "$W/spared-empty"
    taskset -c $SRV_CPU "$SPARED" serve --data-dir "$W/spared-empty" --idle-secs 600 >"$W/spared.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
    sleep 1; e=$(pss_snap); stop
  fi
  if [ "$t" = workerd ]; then # baseline: a workerd with one distinct worker (an empty workerd config has no meaning); startup compiles every configured worker
    rm -rf "$W/workerd-data" "$W/workerd-distinct"; mkdir -p "$W/workerd-data"
    python3 "$ROOT/bench/demo/gen-distinct-workerd.py" "$W/workerd-distinct" "$W/workerd-data" 1 >/dev/null
    (cd "$W/workerd-distinct" && exec taskset -c $SRV_CPU "$WORKERD" serve config.capnp) >"$W/workerd.log" 2>&1 &
    PID=$!
    for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/9100) 2>/dev/null && break; sleep 0.1; done
    sleep 1; e=$(pss_snap); stop
  fi
  PSS_FILE="$R/$OUT-mem-distinct-$t-run$r.json" pss_json "$t" distinct-apps "$r" "$e" "$b" "$a1" "$an" "$PSS_N"
}

PSS_TAG=${PSS_TAG:-current}
PSS_N=${PSS_N:-50}
rotate() { # round -> TARGETS rotated left by (round-1) so the first target changes every round
  local a=($TARGETS) n i out="" r=$1; n=${#a[@]}
  for i in $(seq 0 $((n-1))); do out="$out ${a[$(( (i + r - 1) % n ))]}"; done
  echo $out
}
# PARTS=curve: total memory of one host serving N different apps, N = 0 1 5 10 25 50 100 200 (CURVE_NS), spared (distinct, lean, same) and
# workerd (distinct workers), one fresh server per point in its own memory cgroup; method in docs/memory-methodology.md, harness
# bench/demo/mem-curve.py. Needs bench/work/distinct/wasm/v0..199 and bench/work/distinct-lean/wasm/v0..199 (gen-distinct.py 200 [--lean],
# built as in distinct_build). 3 runs at N in CURVE_3RUNS, idle snapshots on run 1. Raw JSON -> bench/results/memory-curve-*.json.
if [ "$PARTS" = curve ]; then
  CURVE_NS=${CURVE_NS:-0 1 5 10 25 50 100 200}
  CURVE_3RUNS=" ${CURVE_3RUNS:-0 1 10 50 100} "
  CURVE_TARGETS=${CURVE_TARGETS:-spared-distinct workerd-distinct spared-lean spared-same}
  mkdir -p "$ROOT/bench/work/curve/bin"
  [ -f "$ROOT/bench/work/curve/bin/spared" ] || cp "$SPARED" "$ROOT/bench/work/curve/bin/spared"
  [ -f "$ROOT/bench/work/curve/bin/workerd" ] || cp -L "$WORKERD" "$ROOT/bench/work/curve/bin/workerd"
  for n in $CURVE_NS; do
    runs=1; [[ "$CURVE_3RUNS" == *" $n "* ]] && runs=3
    for r in $(seq 1 $runs); do
      idle=""; [ "$r" = 1 ] && idle=--idle
      if [ "$n" = 0 ]; then ts="spared-distinct workerd-empty"; [ $((r % 2)) = 0 ] && ts="workerd-empty spared-distinct"
      else ts=$(TARGETS="$CURVE_TARGETS" rotate "$r"); fi
      for t in $ts; do python3 "$ROOT/bench/demo/mem-curve.py" "$t" "$n" "$r" $idle; done
      if [ "$n" = 1 ]; then python3 "$ROOT/bench/demo/mem-curve.py" workerd-trivial 1 "$r"; python3 "$ROOT/bench/demo/mem-curve.py" workerd-trivial 2 "$r"; fi
    done
  done
  python3 "$ROOT/bench/demo/mem-curve.py" spared-distinct 50 1 --idle-timer 10  # the real idle timer, against the evict endpoint
  exit 0
fi
if [ "$PARTS" = distinct ]; then
  for t in $TARGETS; do [ "$t" = spared ] && { distinct_build; distinct_components; break; }; done
  for r in $(seq 1 "$ROUNDS"); do
    for t in $TARGETS; do echo "distinct round $r target $t"; run_mem_distinct "$t" "$r"; done
  done
  exit 0
fi
if [ "$PARTS" = pss ]; then
  for r in $(seq 1 "$ROUNDS"); do
    for t in $( [[ " $TARGETS " == *" celld "* ]] && rotate "$r" || echo "$TARGETS" ); do echo "pss round $r target $t"; run_mem_pss "$t" "$r"; done
  done
  rm -f "$ROOT/apps/demo-workerd/config.measure.capnp"
  exit 0
fi
ONLY_WARM_OTHERS=0
[[ " $TARGETS " == *" spin "* ]] && ONLY_WARM_OTHERS=1
[[ " $TARGETS " == *" spared "* ]] && deploy_template
for r in $(seq 1 "$ROUNDS"); do
  if [[ " $TARGETS " == *" spin "* ]]; then
    case $((r % 3)) in 1) order="spin spared workerd";; 2) order="workerd spared spin";; 0) order="spared spin workerd";; esac
  elif [[ " $TARGETS " == *" celld "* ]]; then order=$(rotate "$r")
  elif [ $((r % 2)) = 1 ]; then order="spared workerd"; else order="workerd spared"; fi
  for t in $order; do [[ " $TARGETS " == *" $t "* ]] || continue; echo "round $r target $t"; run_target "$t" "$r"; run_mem "$t" "$r"; run_cold_existing "$t" "$r"; run_spin_first "$t" "$r"; run_mem_hardlink "$t" "$r"; done
done
rm -f "$ROOT/apps/demo-workerd/config.measure.capnp"
