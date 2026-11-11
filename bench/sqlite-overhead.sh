#!/usr/bin/env bash
# SQLite microbenchmark: identical queries, SQLite in Wasm (wasmtime, cwasm) vs native. Median of REPS runs per query.
set -eu
cd "$(dirname "$0")/.."
source bench/env.sh
lab_build
N=${N:-100000}; REPS=${REPS:-7}
BIN=$SPARED_LAB_BIN
NATIVE=probes/sqlite-probe/target/native/release/sqlite-probe
mkdir -p bench/work/g3w bench/work/g3n bench/results
$BIN compile probes/sqlite-probe/target/wasm32-wasip2/release/sqlite-probe.wasm bench/work/probe.cwasm
for i in $(seq $REPS); do
  rm -f bench/work/g3w/app.db* bench/work/g3n/app.db*
  $BIN run-probe bench/work/probe.cwasm --data bench/work/g3w -- bench $N | grep '^B' | sed 's/^/wasm /' 
  DB=bench/work/g3n/app.db $NATIVE bench $N | grep '^B' | sed 's/^/native /'
done > bench/work/g3.raw
python3 - <<'PY' | tee bench/results/sqlite-overhead${SUFFIX:-}.json
import json,statistics as st,collections
d=collections.defaultdict(list)
for l in open('bench/work/g3.raw'):
    t,_,q,us=l.split(); d[(t,q)].append(int(us))
qs=sorted({q for _,q in d}); out={}
for q in qs:
    w=st.median(d[('wasm',q)]); n=st.median(d[('native',q)])
    out[q]={'wasm_us':w,'native_us':n,'ratio':round(w/n,2),'wasm_runs':d[('wasm',q)],'native_runs':d[('native',q)]}
print(json.dumps(out,indent=1))
PY
