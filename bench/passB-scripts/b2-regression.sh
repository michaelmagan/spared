#!/usr/bin/env bash
# B2: paired old/new regression check of the resource-limit commits. old = 6b52139 (parent of b589cd9, the first resource-cap commit),
# new = HEAD. Per round the order of old/new alternates for every piece. Pieces: warm c1/c16, wake in process (WAL, TRUNCATE), mcurve N=1/10/50.
# Per-binary templates (cwasm is engine-config specific). Server pinned to CPU 0, load generator CPU 2.
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
B=$ROOT/bench/work/b2; mkdir -p "$B/bin"
cp -f "$ROOT/target/old/release/spared" "$B/bin/old"
cp -f "$ROOT/target/release/spared" "$B/bin/new"
cp -f "$ROOT/target/lab/release/spared" "$B/bin/new-lab"
cp -f "$ROOT/target/old/release/spared" "$B/bin/old-lab"
SHL=$ROOT/apps/shared-list/target/wasm32-wasip2/release/shared-list.wasm
PIECES=${PIECES:-warm wake mcurve}
ROUNDS=${ROUNDS:-5}
for v in old new; do sha256sum "$B/bin/$v" | cut -c1-16 | sed "s/^/$v /" ; done >"$R/b2-passB-binaries.txt"
mk_template() { local ap=/v1/apps; [ "$1" = old ] && ap=/apps; mk_template_in "$B/bin/$1" "$B/tpl-$1" $ap; }
piece_warm() { # variant round
  local v=$1 r=$2
  rm -rf "$B/data"; cp -a "$B/tpl-$v" "$B/data"
  taskset -c 0 "$B/bin/$v" serve --data-dir "$B/data" --idle-secs 600 >"$B/serve-$v.log" 2>&1 &
  local P=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  taskset -c 2 "$LG" --style spared --mode touch --n 100 --out /dev/null
  for c in 1 16; do inner_quiet; taskset -c 2 "$LG" --style spared --mode warm --conc $c --requests 20000 --out "$R/b2-passB-$v-warm-c$c-run$r.json"; done
  kill $P; wait $P 2>/dev/null || true
}
piece_wake() {
  local v=$1 r=$2 j
  $B/bin/$v-lab compile $SHL $B/shared-list-$v.cwasm >/dev/null
  for j in WAL TRUNCATE; do
    inner_quiet
    d=$B/wake-$v-$j; rm -rf "$d"
    SPARED_JOURNAL=$j taskset -c 0 $B/bin/$v-lab bench-wake $B/shared-list-$v.cwasm --data "$d" --iters 2000 | python3 -c "import json,sys;d=json.load(sys.stdin);d['journal']='$j';d['run']=$r;d['variant']='$v';d['loadavg1']=float(open('/proc/loadavg').read().split()[0]);print(json.dumps(d))" >>"$R/b2-passB-wake.jsonl"
  done
}
piece_mcurve() {
  local v=$1 r=$2 n
  for n in 1 10 50; do
    inner_quiet
    APPS_PATH=$([ $v = old ] && echo /apps || echo /v1/apps) SPARED=$B/bin/$v CURVE_W=$B/curve-$v CURVE_PREFIX=b2-passB-mcurve-$v python3 "$ROOT/bench/passB-scripts/mem-curve.py" spared-distinct $n $r 2>&1 | tail -1
  done
}
for r in $(seq 1 $ROUNDS); do
  if [ $((r % 2)) = 1 ]; then order="old new"; else order="new old"; fi
  for p in $PIECES; do
    for v in $order; do
      [ "$p" = warm ] && mk_template $v
      wait_quiet || true
      echo "$(date +%T) round $r piece $p $v"
      seg_begin b2 "r$r-$p-$v"
      piece_$p $v $r >>"$R/b2-passB-$p.log" 2>&1 || echo "FAILED r$r $p $v"
      seg_end b2 "r$r-$p-$v"
    done
  done
done
echo B2-DONE
