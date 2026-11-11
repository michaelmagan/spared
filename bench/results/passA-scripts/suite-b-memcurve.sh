#!/usr/bin/env bash
# Suite b: memory curve, N = 0 1 10 20 50 100 200 different apps, spared-distinct and workerd-distinct (workerd-empty at N=0),
# 5 repetitions, point order alternates every point, same harness as `PARTS=curve bench/demo/measure.sh` (bench/demo/mem-curve.py),
# no idle snapshots. Files: bench/results/mcurve-passA-<target>-n<N>-run<rep>.json
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off CURVE_PREFIX=mcurve-passA
mkdir -p bench/work/curve/bin
cp -f target/release/spared bench/work/curve/bin/spared
cp -fL bench/work/workerd/node_modules/.bin/workerd bench/work/curve/bin/workerd
NS="${NS:-0 1 10 20 50 100 200}"
for rep in 1 2 3 4 5; do
  i=0
  for n in $NS; do
    i=$((i+1))
    wait_quiet || true
    if [ "$n" = 0 ]; then a=spared-distinct; b=workerd-empty; else a=spared-distinct; b=workerd-distinct; fi
    if [ $(( (rep + i) % 2 )) = 0 ]; then ts="$a $b"; else ts="$b $a"; fi
    seg_begin mcurve "rep$rep-n$n"
    for t in $ts; do python3 bench/demo/mem-curve.py "$t" "$n" "$rep" 2>&1 | tail -3; done
    seg_end mcurve "rep$rep-n$n"
  done
done
echo SUITE-B-DONE
