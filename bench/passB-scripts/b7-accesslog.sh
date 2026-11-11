#!/usr/bin/env bash
# B7: access log on vs off under mixed load and wake. Per round and mode: fresh copy of a 100-app template, `cold` (first request of
# each of 100 apps), mixed (1 in 10 is a POST) at 1 and 16 connections (20,000 requests), restart on the now-seeded data, `cold` again
# (cold-existing). Server pinned to CPU 0, load generator CPU 2, stdout (the log) to a file. 5 rounds, order alternates.
source "$(dirname "$0")/lib.sh"
B=$ROOT/bench/work/b7; mkdir -p "$B/bin"
cp -f "$ROOT/target/release/spared" "$B/bin/spared"
mk_template_in "$B/bin/spared" "$B/tpl"
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
start() { taskset -c 0 "$B/bin/spared" serve --data-dir "$B/data" --idle-secs 600 --access-log $1 >"$B/spared-$1.log" 2>&1 & PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done; }
rss() { awk '$1=="Pss:"{print $2}' /proc/$PID/smaps_rollup; }
for r in 1 2 3 4 5; do
  if [ $((r % 2)) = 1 ]; then order="on off"; else order="off on"; fi
  for m in $order; do
    wait_quiet || true
    echo "$(date +%T) round $r access-log $m"
    seg_begin b7 "r$r-$m"
    rm -rf "$B/data"; cp -a "$B/tpl" "$B/data"
    start $m
    taskset -c 2 "$LG" --style spared --mode cold --n 100 --out "$R/b7-passB-$m-cold-run$r.json"
    for c in 1 16; do inner_quiet; taskset -c 2 "$LG" --style spared --mode mixed --conc $c --requests 20000 --out "$R/b7-passB-$m-mixed-c$c-run$r.json"; done
    echo "{\"mode\":\"$m\",\"run\":$r,\"pss_kb_after_mixed\":$(rss),\"log_bytes\":$(stat -c %s "$B/spared-$m.log")}" >>"$R/b7-passB-extra.jsonl"
    stop
    start $m; inner_quiet
    taskset -c 2 "$LG" --style spared --mode cold --n 100 --out "$R/b7-passB-$m-cold-existing-run$r.json"
    stop
    seg_end b7 "r$r-$m"
  done
done
echo B7-DONE
