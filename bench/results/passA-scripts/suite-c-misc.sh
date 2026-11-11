#!/usr/bin/env bash
# Suite c: request-cost, sqlite-overhead (epoch on and off), wake, workloads, native baseline. Each piece is one steal segment.
# PIECES selects a subset (default: all, in this order).
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
source bench/env.sh
PIECES=${PIECES:-request-cost sqlite-overhead sqlite-overhead-noepoch wake native-baseline workloads}
run() { # name, command...
  local name=$1; shift
  [[ " $PIECES " == *" $name "* ]] || return 0
  wait_quiet || true
  echo "$(date +%T) piece $name"
  seg_begin "$name" "$name"
  "$@" >"$R/$name-passA.log" 2>&1 || echo "FAILED $name (see $name-passA.log)"
  seg_end "$name" "$name"
}
wake_piece() { # 5 runs per journal mode, alternating which mode goes first
  BIN=$SPARED_LAB_BIN
  $BIN compile apps/shared-list/target/wasm32-wasip2/release/shared-list.wasm bench/work/shared-list.cwasm
  : >"$R/wake-passA.jsonl"
  for r in 1 2 3 4 5; do
    if [ $((r % 2)) = 1 ]; then js="WAL TRUNCATE"; else js="TRUNCATE WAL"; fi
    for j in $js; do
      for _ in $(seq 60); do awk '{exit !($1 < 0.3)}' /proc/loadavg && break; sleep 2; done
      d=bench/work/wake/g4_$j; rm -rf "$d"
      SPARED_JOURNAL=$j taskset -c 0 $BIN bench-wake bench/work/shared-list.cwasm --data "$d" --iters 2000 | python3 -c "import json,sys;d=json.load(sys.stdin);d['journal']='$j';d['run']=$r;d['loadavg1']=float(open('/proc/loadavg').read().split()[0]);print(json.dumps(d))" >>"$R/wake-passA.jsonl"
    done
  done
}
run request-cost env STAGE=run SUFFIX=-passA RUNS=5 bench/request-cost.sh
run sqlite-overhead env SUFFIX=-passA-epoch REPS=7 bench/sqlite-overhead.sh
run sqlite-overhead-noepoch env SUFFIX=-passA-noepoch REPS=7 SPARED_NO_EPOCH=1 bench/sqlite-overhead.sh
run wake wake_piece
run native-baseline env PARTS="check latency" LAT_TARGETS="spared native" bench/results/passA-scripts/native-baseline-passA.sh 5
run workloads env OUT=workloads-passA bench/workloads/run.sh 5
echo SUITE-C-DONE
