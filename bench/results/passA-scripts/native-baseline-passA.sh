#!/usr/bin/env bash
# Baselines for the sandbox: the demo app as a native Rust process (apps/demo-native) and in a container, against spared on the same machine.
# Usage: [PARTS="build check latency memory container"] bench/native-baseline.sh [rounds]   (default 3 rounds)
#   build      demo-native, demo-spared (wasm), the load generator, 50 different variants of each app for the memory part
#              (bench/demo/gen-distinct.py, --native), and, when Docker answers, a static build in a FROM scratch image.
#   check      starts both apps and compares status, content-type and body on every route; exits 1 on a difference.
#   latency    per round, spared and native alternate first: warm GET at 1 and 16 connections (20,000 requests, bench/demo/loadgen),
#              spared's cold wake of 100 apps that already hold data, 20 spawns of the native binary timed to the first response byte,
#              and, with Docker, 10 `docker run` timed to the first response byte (LAT_TARGETS picks targets, default all three). Server on CPU 0, load generator on CPU 2.
#   memory     bench/demo/mem-curve.py at N = 1, 10, 50 for native-distinct (N processes, N different binaries), native-same (N processes
#              of one binary) and spared-distinct (one server, N different apps), 3 runs, rotated order.
#   container  bench/demo/mem-container.py at N = 1, 10, 30 containers, 3 runs.
# SKIP_DONE=1 keeps memory points that already have a result file.
# Raw JSON -> bench/results/native-{warm,cold,memory}-*.json and container-*.json; summary: bench/demo/native-summary.py.
set -euo pipefail
# The published numbers were measured without the server's request log; keep it off.
export SPARED_ACCESS_LOG=${SPARED_ACCESS_LOG:-off}
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
ROUNDS=${1:-3}
PARTS=${PARTS:-build check latency memory container}
has() { [[ " $PARTS " == *" $1 "* ]]; }
W=$ROOT/bench/work/native
R=$ROOT/bench/results
SPARED=${SPARED:-$ROOT/target/release/spared}
NATIVE=$ROOT/apps/demo-native/target/release/demo-native
STATIC=$ROOT/bench/work/native-static/x86_64-unknown-linux-gnu/release/demo-native
WASM=$ROOT/apps/demo-spared/target/wasm32-wasip2/release/demo-spared.wasm
LG=$ROOT/bench/demo/loadgen/target/release/loadgen
IMAGE=spared-demo-native:bench
SP_PUB=18080 SP_ADM=18081 NT_PORT=18090 CT_PORT=18095
SRV_CPU=0 LG_CPU=2
N=100
mkdir -p "$W" "$R"
docker_ok() { docker info >/dev/null 2>&1; }

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
  if [ -n "$PID" ]; then kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null || true; fi
  PID=""; sleep 1
}
trap stop EXIT
wait_port() { for _ in $(seq 200); do (exec 3<>/dev/tcp/127.0.0.1/"$1") 2>/dev/null && return 0; sleep 0.05; done; return 1; }

build() {
  (cd "$ROOT" && cargo build --release -p spared)
  (cd "$ROOT/apps/demo-native" && cargo build --release --locked)
  (cd "$ROOT" && source bench/env.sh && cd apps/demo-spared && cargo build --release --locked --target wasm32-wasip2)
  (cd "$ROOT/bench/demo/loadgen" && cargo build --release --locked)
  local d
  python3 "$ROOT/bench/demo/gen-distinct.py" 50
  python3 "$ROOT/bench/demo/gen-distinct.py" 50 --native
  ( cd "$ROOT" && source bench/env.sh && export CARGO_TARGET_DIR=$ROOT/bench/work/distinct/target; mkdir -p bench/work/distinct/wasm
    for k in $(seq 0 49); do
      [ -f "bench/work/distinct/wasm/v$k.wasm" ] && continue
      (cd "bench/work/distinct/v$k" && cargo build --release --target wasm32-wasip2 -j2 2>&1 | tail -1)
      cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/demo-distinct-$k.wasm" "bench/work/distinct/wasm/v$k.wasm"
    done )
  d=$ROOT/bench/work/distinct-native; mkdir -p "$d/bin"
  for k in $(seq 0 49); do
    [ -f "$d/bin/v$k" ] && continue
    (cd "$d/v$k" && CARGO_TARGET_DIR=$d/target cargo build --release -j2 2>&1 | tail -1)
    cp "$d/target/release/demo-distinct-$k" "$d/bin/v$k"
  done
  # Private copies, so no other process maps the measured binaries (see bench/demo/mem-curve.py).
  mkdir -p "$ROOT/bench/work/curve/bin"
  cp "$SPARED" "$ROOT/bench/work/curve/bin/spared"
  cp "$NATIVE" "$ROOT/bench/work/curve/bin/demo-native"
  if docker_ok; then
    (cd "$ROOT/apps/demo-native" && RUSTFLAGS="-C target-feature=+crt-static" CARGO_TARGET_DIR=$ROOT/bench/work/native-static \
      cargo build --release --locked --target x86_64-unknown-linux-gnu)
    rm -rf "$W/ctx"; mkdir -p "$W/ctx"
    cp "$STATIC" "$ROOT/apps/demo-native/Dockerfile" "$W/ctx/"
    docker build -q -t "$IMAGE" "$W/ctx"
  else
    echo "docker: no daemon answers; the container parts will be skipped" >&2
  fi
}

start_spared() { # $1 = data dir (used as is)
  stop
  taskset -c $SRV_CPU "$SPARED" serve --data-dir "$1" --listen 127.0.0.1:$SP_PUB --admin-listen 127.0.0.1:$SP_ADM --idle-secs 600 >"$W/spared.log" 2>&1 &
  PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:$SP_ADM/ && break; sleep 0.1; done
}
start_native() { # $1 = database file
  stop
  taskset -c $SRV_CPU "$NATIVE" --listen 127.0.0.1:$NT_PORT --db "$1" >"$W/native.log" 2>&1 &
  PID=$!
  wait_port $NT_PORT
}

spared_template() { # data dir with demo-0..N-1 deployed, never requested
  [ -d "$W/spared-template/apps/demo-$((N-1))" ] && return 0
  rm -rf "$W/spared-template"; mkdir -p "$W/spared-template"
  start_spared "$W/spared-template"
  local k M
  for k in $(seq 0 $((N-1))); do
    M=$(printf 'name = "demo-%s"\n[grants]\ndata = true\n' "$k" | base64 -w0)
    curl -fsS -X PUT -H "authorization: Bearer $(cat "$W/spared-template/admin-token")" -H "x-spared-manifest: $M" \
      --data-binary @"$WASM" http://127.0.0.1:$SP_ADM/v1/apps/demo-$k >/dev/null
  done
  stop
}

check() {
  spared_template
  rm -rf "$W/check"; mkdir -p "$W/check"; cp -a "$W/spared-template" "$W/check/spared"
  start_spared "$W/check/spared"; local sp=$PID; PID=""
  start_native "$W/check/native.db"; local np=$PID; PID=""
  local fail=0 m p b a s1 s2
  while IFS='|' read -r m p b; do
    a=(-X "$m"); [ -n "$b" ] && a+=(-H 'content-type: application/json' --data-binary "$b")
    s1=$(curl -s -o "$W/check/b1" -w '%{http_code} %{content_type}' "${a[@]}" -H 'Host: demo-0.localhost' http://127.0.0.1:$SP_PUB"$p")
    s2=$(curl -s -o "$W/check/b2" -w '%{http_code} %{content_type}' "${a[@]}" http://127.0.0.1:$NT_PORT"$p")
    if [ "$s1" = "$s2" ] && cmp -s "$W/check/b1" "$W/check/b2"; then echo "same: $m $p ($s1, $(wc -c <"$W/check/b1") bytes)"
    else echo "DIFFERENT: $m $p spared [$s1] native [$s2]"; fail=1; fi
  done <<'EOF'
GET|/|
GET|/api/items|
GET|/missing|
POST|/api/items|{not json
POST|/api/items|{"text":"a <b> & 'c'"}
GET|/|
GET|/api/items|
EOF
  kill "$sp" "$np"; wait "$sp" "$np" 2>/dev/null || true
  return $fail
}

samples_json() { # name file-of-"first full"-lines extra-json -> stdout
  python3 - "$@" <<'PY'
import json,math,sys
name,f,extra=sys.argv[1:]
rows=[tuple(map(int,l.split())) for l in open(f) if l.strip()]
fb=sorted(x[0] for x in rows)
q=lambda v,p: v[min(len(v),max(1,math.ceil(len(v)*p)))-1]
print(json.dumps({"target":name,**json.loads(extra),"samples":len(rows),"errors":sum(1 for x in rows if x[2]==0),
 "first_byte_us":{"p50":q(fb,.5),"p99":q(fb,.99),"min":fb[0],"max":fb[-1]},"first_byte_us_all":[x[0] for x in rows]}))
PY
}

lat_spared() { # round
  local r=$1
  rm -rf "$W/spared-data"; cp -a "$W/spared-template" "$W/spared-data"
  start_spared "$W/spared-data"
  lg --style spared --addr 127.0.0.1:$SP_PUB --mode touch --n $N --out /dev/null
  local c
  for c in 1 16; do quiet; lg --style spared --addr 127.0.0.1:$SP_PUB --mode warm --conc $c --requests 20000 --out "$R/native-passA-warm-spared-c$c-run$r.json"; done
  stop
  start_spared "$W/spared-data"; quiet  # every app holds its data, nothing is loaded
  lg --style spared --addr 127.0.0.1:$SP_PUB --mode cold --n $N --out "$R/native-passA-cold-spared-run$r.json"
  stop
}

lat_native() { # round
  local r=$1 c i f="$W/native-spawn.txt"
  rm -f "$W/native.db"*
  start_native "$W/native.db"
  for c in 1 16; do quiet; lg --style native --addr 127.0.0.1:$NT_PORT --mode warm --conc $c --requests 20000 --out "$R/native-passA-warm-native-c$c-run$r.json"; done
  stop; quiet; : >"$f"
  for i in $(seq 20); do # the database exists (seeded above); each spawn is a new process on it
    lg --style native --addr 127.0.0.1:$NT_PORT --mode spawn --spawn-cpu $SRV_CPU -- "$NATIVE" --listen 127.0.0.1:$NT_PORT --db "$W/native.db" \
      | python3 -c "import json,sys;d=json.load(sys.stdin);print(d['detail']['first_byte_from_spawn_us'],d['detail']['full_body_from_spawn_us'],0 if d['errors'] else 1)" >>"$f"
    sleep 0.2
  done
  samples_json native "$f" '{"run":'"$r"',"what":"spawn the binary, connect when it listens, GET /, first response byte; database already seeded","spawns":20}' >"$R/native-passA-cold-native-run$r.json"
}

lat_container() { # round
  docker_ok || return 0
  local r=$1
  rm -rf "$W/ctr-data"; mkdir -p "$W/ctr-data"
  docker rm -f ctr-bench >/dev/null 2>&1 || true
  docker run -d --name ctr-bench -p 127.0.0.1:$CT_PORT:8090 -v "$W/ctr-data:/data" "$IMAGE" >/dev/null  # seeds the database
  wait_port $CT_PORT; curl -fsS -o /dev/null http://127.0.0.1:$CT_PORT/; docker rm -f ctr-bench >/dev/null
  quiet
  # `docker run -d` returns once the container runs; then GET / is polled every 0.5 ms until it answers. Polling the published port
  # while `docker run` is still starting the container measured about 1.17 s instead, because a connection that reaches the port
  # before the container's network is up waits for a TCP retransmission.
  taskset -c $LG_CPU python3 - "$r" "$W/ctr-data" "$IMAGE" "$CT_PORT" "$SRV_CPU" >"$R/container-passA-unused-run$r.json" <<'PY'
import http.client, json, math, subprocess, sys, time
r, data, image, port, cpu = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
first, ran = [], []
for i in range(10):
    t0 = time.perf_counter()
    subprocess.run(["docker", "run", "-d", "--name", "ctr-bench", "--cpuset-cpus", cpu, "-p", f"127.0.0.1:{port}:8090", "-v", f"{data}:/data", image],
                   check=True, stdout=subprocess.DEVNULL)
    ran.append(round((time.perf_counter() - t0) * 1e6))
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=5); c.request("GET", "/"); resp = c.getresponse()
            t = time.perf_counter(); resp.read(); c.close()
            if resp.status == 200:
                break
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.0005)
    first.append(round((t - t0) * 1e6))
    subprocess.run(["docker", "rm", "-f", "ctr-bench"], check=True, stdout=subprocess.DEVNULL)
    time.sleep(0.5)
v = sorted(first)
q = lambda p: v[min(len(v), max(1, math.ceil(len(v) * p))) - 1]
print(json.dumps({"target": "container", "run": int(r), "starts": 10, "errors": 0,
                  "what": "docker run -d (FROM scratch, static binary, bridge network, published loopback port, CPU 0), then GET / polled every 0.5 ms; first response byte from the start of docker run; database already seeded",
                  "first_byte_us": {"p50": q(.5), "p99": q(.99), "min": v[0], "max": v[-1]}, "first_byte_us_all": first, "docker_run_returned_us_all": ran}))
PY
}

if has build; then build; fi
if has check; then check; fi
if has latency; then
  spared_template
  for r in $(seq 1 "$ROUNDS"); do
    if [ $((r % 2)) = 1 ]; then order="spared native container"; else order="container native spared"; fi
    for t in $order; do [[ " ${LAT_TARGETS:-spared native container} " == *" $t "* ]] || continue; echo "latency round $r: $t"; "lat_$t" "$r"; done
  done
fi
if has memory; then
  for n in 1 10 50; do
    for r in $(seq 1 "$ROUNDS"); do
      case $((r % 3)) in 1) ts="native-distinct native-same spared-distinct";; 2) ts="spared-distinct native-distinct native-same";; 0) ts="native-same spared-distinct native-distinct";; esac
      for t in $ts; do
        [ "${SKIP_DONE:-0}" = 1 ] && [ -f "$R/native-memory-$t-n$n-run$r.json" ] && continue  # resume an interrupted run
        CURVE_PREFIX=native-memory python3 "$ROOT/bench/demo/mem-curve.py" "$t" "$n" "$r"
      done
    done
  done
fi
if has container && docker_ok; then
  for n in 1 10 30; do for r in $(seq 1 "$ROUNDS"); do python3 "$ROOT/bench/demo/mem-container.py" "$n" "$r"; done; done
fi
python3 "$ROOT/bench/demo/native-summary.py"
