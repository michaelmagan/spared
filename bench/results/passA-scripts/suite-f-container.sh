#!/usr/bin/env bash
# Suite f: container baseline (Docker defaults, FROM scratch static binary per container), container-first, native process and spared as reference rows.
# Starts dockerd/containerd itself (they were not running before) and stops them at the end.
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
W=$ROOT/bench/work/ctr; mkdir -p "$W"
LG=$ROOT/bench/demo/loadgen/target/release/loadgen
SPARED=$ROOT/target/release/spared
STATIC=$ROOT/bench/work/native-static/x86_64-unknown-linux-gnu/release/demo-native
TPL=$ROOT/bench/work/vs-workerd/spared-template
PHASES=${PHASES:-lat start mem}
has() { [[ " $PHASES " == *" $1 "* ]]; }
if ! docker info >/dev/null 2>&1; then
  nohup dockerd >"$W/dockerd.log" 2>&1 &
  for _ in $(seq 120); do docker info >/dev/null 2>&1 && break; sleep 1; done
fi
docker info >/dev/null 2>&1 || { echo "dockerd did not start; see $W/dockerd.log" >&2; exit 1; }
docker info 2>/dev/null | grep -E 'Server Version|Cgroup|Storage Driver|Default Runtime|Runtimes|Security Options' | tee "$R/container-passA-docker-info.txt"
# images
if ! docker image inspect spared-ctr-dist:v49 >/dev/null 2>&1; then
  for k in $(seq 0 49); do
    c=$W/ctx-v$k; rm -rf "$c"; mkdir -p "$c"; cp "$ROOT/bench/work/distinct-native/static-bin/v$k" "$c/demo-native"; cp "$ROOT/apps/demo-native/Dockerfile" "$c/"
    docker build -q -t spared-ctr-dist:v$k "$c" >/dev/null
  done
fi
c=$W/ctx-demo; rm -rf "$c"; mkdir -p "$c"; cp "$STATIC" "$c/demo-native"; cp "$ROOT/apps/demo-native/Dockerfile" "$c/"
docker build -q -t spared-ctr-demo:bench "$c" >/dev/null
mkdir -p "$ROOT/bench/work/distinct-native/bin"; cp -f "$ROOT/bench/work/distinct-native/static-bin/"v* "$ROOT/bench/work/distinct-native/bin/"
mkdir -p "$ROOT/bench/work/curve/bin"; cp -f target/release/spared "$ROOT/bench/work/curve/bin/spared"; cp -f "$STATIC" "$ROOT/bench/work/curve/bin/demo-native"

lgq() { for _ in $(seq 600); do awk '{exit !($1 < 0.3)}' /proc/loadavg && break; sleep 1; done; taskset -c 2 "$LG" "$@"; }
PID=""
lat_target() { # target rep
  local t=$1 r=$2 addr c
  case $t in
    container-published|container-ip)
      rm -rf "$W/lat-data"; mkdir -p "$W/lat-data"; docker rm -f ctr-lat >/dev/null 2>&1
      docker run -d --name ctr-lat --cpuset-cpus 0 -p 127.0.0.1:18095:8090 -v "$W/lat-data:/data" spared-ctr-demo:bench >/dev/null
      if [ $t = container-ip ]; then addr=$(docker inspect -f '{{(index .NetworkSettings.Networks "bridge").IPAddress}}' ctr-lat):8090; else addr=127.0.0.1:18095; fi
      for _ in $(seq 100); do curl -s -o /dev/null http://$addr/ && break; sleep 0.1; done ;;
    native)
      rm -rf "$W/nat-data"; mkdir -p "$W/nat-data"
      taskset -c 0 "$STATIC" --listen 127.0.0.1:18090 --db "$W/nat-data/app.db" >/dev/null 2>&1 & PID=$!
      addr=127.0.0.1:18090; for _ in $(seq 100); do curl -s -o /dev/null http://$addr/ && break; sleep 0.1; done ;;
    spared)
      rm -rf "$W/sp-data"; cp -a "$TPL" "$W/sp-data"
      taskset -c 0 "$SPARED" serve --data-dir "$W/sp-data" --idle-secs 600 --listen 127.0.0.1:18080 --admin-listen 127.0.0.1:18081 >/dev/null 2>&1 & PID=$!
      addr=127.0.0.1:18080; for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:18081/ && break; sleep 0.1; done
      taskset -c 2 "$LG" --style spared --addr $addr --mode touch --n 100 --out /dev/null ;;
  esac
  local style=native; [ $t = spared ] && style=spared
  for c in 1 16; do lgq --style $style --addr $addr --mode warm --conc $c --requests 20000 --out "$R/container-passA-warm-$t-c$c-run$r.json"; done
  docker rm -f ctr-lat >/dev/null 2>&1
  if [ -n "$PID" ]; then kill $PID 2>/dev/null; wait $PID 2>/dev/null; PID=""; fi
}
if has lat; then
  ts=(container-published container-ip native spared)
  for r in 1 2 3 4 5; do
    rot=$(( (r-1) % 4 ))
    for i in 0 1 2 3; do
      t=${ts[$(( (i+rot) % 4 ))]}
      wait_quiet || true; echo "$(date +%T) lat rep $r $t"
      seg_begin container "lat-r$r-$t"; lat_target $t $r; seg_end container "lat-r$r-$t"
    done
  done
fi
if has start; then
  for r in 1 2 3 4 5; do
    wait_quiet || true; echo "$(date +%T) start rep $r"
    seg_begin container "start-r$r"; taskset -c 2 python3 "$(dirname "$0")/ctr.py" start $r; seg_end container "start-r$r"
  done
fi
if has mem; then
  for r in 1 2 3 4 5; do
    for n in 1 10 20 50; do
      wait_quiet || true; echo "$(date +%T) mem rep $r n $n"
      seg_begin container "mem-r$r-n$n"
      if [ $(( (r+n) % 2 )) = 0 ]; then order="ctr native spared"; else order="native spared ctr"; fi
      for t in $order; do
        case $t in
          ctr) python3 "$(dirname "$0")/ctr.py" mem $n $r ;;
          native) CURVE_PREFIX=ctrmem-passA python3 bench/demo/mem-curve.py native-distinct $n $r | tail -1 ;;
          spared) CURVE_PREFIX=ctrmem-passA python3 bench/demo/mem-curve.py spared-distinct $n $r | tail -1 ;;
        esac
      done
      seg_end container "mem-r$r-n$n"
    done
  done
fi
docker rm -f $(docker ps -aq) >/dev/null 2>&1
echo SUITE-F-DONE
