#!/usr/bin/env bash
# B4: spared and workerd on the same CPU sets: 1 CPU pinned (pass A reference), 2 CPUs (0,1), unpinned (0-3, load generator unpinned too).
# Warm GET at 1 and 16 connections, 20,000 requests, one app, 5 rounds; target and config order rotate every round.
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
B=$ROOT/bench/work/b4; mkdir -p "$B/bin"
cp -f "$ROOT/target/release/spared" "$B/bin/spared"
mk_template_in "$B/bin/spared" "$B/tpl"
cfgs=("pin1 0 2" "cpu2 0,1 2" "unpinned 0-3 none")
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
start_spared() {
  rm -rf "$B/data"; cp -a "$B/tpl" "$B/data"
  taskset -c "$1" "$B/bin/spared" serve --data-dir "$B/data" --idle-secs 600 >"$B/spared.log" 2>&1 &
  PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
}
start_workerd() {
  rm -rf "$B/wd-data"; mkdir -p "$B/wd-data"
  sed "s#../../bench/work/demo/workerd-data#$B/wd-data#" "$ROOT/apps/demo-workerd/config.capnp" >"$ROOT/apps/demo-workerd/config.measure.capnp"
  (cd "$ROOT/apps/demo-workerd" && exec taskset -c "$1" "$WORKERD" serve config.measure.capnp) >"$B/workerd.log" 2>&1 &
  PID=$!
  wait_tcp 9090; sleep 1
}
lg() { if [ "$LGCPU" = none ]; then "$LG" "$@"; else taskset -c "$LGCPU" "$LG" "$@"; fi; }
for r in 1 2 3 4 5; do
  for ci in 0 1 2; do
    cfg=(${cfgs[$(( (ci + r - 1) % 3 ))]}); name=${cfg[0]}; SRV=${cfg[1]}; LGCPU=${cfg[2]}
    if [ $(( (r + ci) % 2 )) = 0 ]; then ts="spared workerd"; else ts="workerd spared"; fi
    for t in $ts; do
      wait_quiet || true
      echo "$(date +%T) round $r $name $t"
      seg_begin b4 "r$r-$name-$t"
      if [ $t = spared ]; then start_spared $SRV; else start_workerd $SRV; fi
      lg --style $t --mode touch --n 1 --out /dev/null
      for c in 1 16; do inner_quiet; lg --style $t --mode warm --conc $c --requests 20000 --out "$R/b4-passB-$name-$t-warm-c$c-run$r.json"; done
      stop
      seg_end b4 "r$r-$name-$t"
    done
  done
done
echo B4-DONE
