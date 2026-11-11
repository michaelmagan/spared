#!/usr/bin/env bash
# Suite d: access-log cost. Same server, same app, `--access-log on` vs `off`, warm GET at 1 and 16 connections (20,000 requests each,
# server CPU 0, load generator CPU 2, quiet machine before each run), 5 rounds, order alternates. Server stdout (the log) goes to a file.
source "$(dirname "$0")/lib.sh"
W=$ROOT/bench/work/accesslog; mkdir -p "$W"
SPARED=$ROOT/target/release/spared
LG=$ROOT/bench/demo/loadgen/target/release/loadgen
TPL=$ROOT/bench/work/vs-workerd/spared-template
[ -d "$TPL/apps/demo-99" ] || { echo "template missing; run suite a first" >&2; exit 1; }
for r in 1 2 3 4 5; do
  if [ $((r % 2)) = 1 ]; then order="on off"; else order="off on"; fi
  for m in $order; do
    wait_quiet || true
    echo "$(date +%T) round $r access-log $m"
    seg_begin accesslog "r$r-$m"
    rm -rf "$W/data"; cp -a "$TPL" "$W/data"
    taskset -c 0 "$SPARED" serve --data-dir "$W/data" --idle-secs 600 --access-log $m >"$W/spared-$m.log" 2>&1 &
    P=$!
    for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
    taskset -c 2 "$LG" --style spared --mode touch --n 100 --out /dev/null
    for c in 1 16; do
      wait_quiet_inner() { for _ in $(seq 600); do awk '{exit !($1 < 0.3)}' /proc/loadavg && return 0; sleep 1; done; }
      wait_quiet_inner
      taskset -c 2 "$LG" --style spared --mode warm --conc $c --requests 20000 --out "$R/accesslog-passA-$m-warm-c$c-run$r.json"
    done
    echo "access log bytes: $(stat -c %s "$W/spared-$m.log")" >>"$R/accesslog-passA-logsize.txt"
    kill $P; wait $P 2>/dev/null || true
    seg_end accesslog "r$r-$m"
  done
done
echo SUITE-D-DONE
