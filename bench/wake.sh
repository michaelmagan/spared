#!/usr/bin/env bash
# Wake benchmark: wake to first query = instantiate + open DB + one query, in-process, pinned to 1 CPU.
# SUFFIX (default empty) goes into the result name: bench/results/wake$SUFFIX.jsonl.
set -eu
cd "$(dirname "$0")/.."
source bench/env.sh
lab_build
BIN=$SPARED_LAB_BIN
OUT=bench/results/wake${SUFFIX:-}.jsonl
$BIN compile apps/shared-list/target/wasm32-wasip2/release/shared-list.wasm bench/work/shared-list.cwasm
mkdir -p bench/results
for j in WAL TRUNCATE; do
  d=bench/work/wake/g4_$j; rm -rf "$d"
  for r in 1 2 3; do
    rm -rf "$d"
    SPARED_JOURNAL=$j taskset -c 0 $BIN bench-wake bench/work/shared-list.cwasm --data "$d" --iters 2000 | python3 -c "import json,sys;d=json.load(sys.stdin);d['journal']='$j';d['run']=$r;print(json.dumps(d))"
  done
done > "$OUT"
python3 - "$OUT" <<'PY'
import json,statistics as st,sys
rows=[json.loads(l) for l in open(sys.argv[1])]
for j in ('WAL','TRUNCATE'):
    r=[x for x in rows if x['journal']==j]
    print(j,'p50',[x['wake_to_first_query_us']['p50'] for x in r],'p99',[x['wake_to_first_query_us']['p99'] for x in r])
PY
