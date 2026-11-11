#!/usr/bin/env bash
# Suite a: bench/demo/measure.sh (parts main + cold-existing, as session 3), 5 rounds, one target per invocation so that steal time is
# logged per (round, target) and the order alternates: spared workerd / workerd spared / ...
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
TARGETS=spared PARTS=none bench/demo/measure.sh 1 >/dev/null 2>&1   # deploys the 100-app template once, outside any segment
for r in 1 2 3 4 5; do
  if [ $((r % 2)) = 1 ]; then order="spared workerd"; else order="workerd spared"; fi
  for t in $order; do
    wait_quiet || true
    echo "$(date +%T) round $r $t"
    seg_begin vs-workerd "r$r-$t"
    TARGETS=$t OUT=vs-workerd-passA-r$r PARTS="main cold-existing" bench/demo/measure.sh 1 >"$R/vs-workerd-passA-r$r-$t.log" 2>&1 || echo "FAILED r$r $t (see log)"
    seg_end vs-workerd "r$r-$t"
  done
done
echo SUITE-A-DONE
