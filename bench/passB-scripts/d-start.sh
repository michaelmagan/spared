#!/usr/bin/env bash
# Pass D: time to ready (first accepted connection on the admin port) with 500 seeded apps, old binary against new binary with the pass off and on.
# cache warm = every file pre-read, cache cold = sync + drop_caches before the start. Server on CPU 0. 8 rounds, order rotates.
source "$(dirname "$0")/c-lib.sh"
D=$ROOT/bench/work/d; SEED=$C/seed-n500-u500; RUN=$D/startrun; READY=$ROOT/bench/passB-scripts/d-ready.py
spared_seed "$SEED" 500 500
OUT=$R/d-passD-start.jsonl
V=("before|before|" "off|after|--prefetch-mb 0" "on|after|")
for r in 1 2 3 4 5 6 7 8; do
  for cache in warm cold; do
    for i in 0 1 2; do
      IFS='|' read -r name bin extra <<<"${V[$(( (i + r) % 3 ))]}"
      wait_quiet || true
      rm -rf "$RUN"; cp -al "$SEED" "$RUN"
      if [ $cache = warm ]; then find "$RUN" -type f -print0 | xargs -0 cat >/dev/null 2>&1; cat "$D/bin/spared-$bin" >/dev/null; else sync; echo 3 >/proc/sys/vm/drop_caches; sleep 1; fi
      rm -f "$D/start.ts" "$D/start.ts.tmp"
      python3 "$READY" "$D/start.ts" 8081 >"$D/ready.out" & rp=$!
      date +%s%N >"$D/start.ts.tmp"; mv "$D/start.ts.tmp" "$D/start.ts"
      taskset -c 0 "$D/bin/spared-$bin" serve --data-dir "$RUN" --idle-secs 600 $extra >"$D/spared-start.log" 2>&1 & pid=$!
      wait $rp
      echo "{\"round\":$r,\"cache\":\"$cache\",\"variant\":\"$name\",\"ready_ms\":$(cat "$D/ready.out")}" >>"$OUT"
      kill $pid; wait $pid 2>/dev/null; rm -rf "$RUN"
    done
  done
done
echo D-START-DONE
