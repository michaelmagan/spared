#!/usr/bin/env bash
# Pass D warm regression check (shape of b7): before vs after binary on a 100-app template (demo-spared), page cache warm.
# Per round and binary: fresh copy of the template, `cold` (first request of each of 100 apps), mixed (1 in 10 is a POST) at 1 and 16
# connections (20,000 requests each), restart on the seeded data, `cold` again (cold-existing). Server CPU 0, load generator CPU 2. 5 rounds, order alternates.
source "$(dirname "$0")/c-lib.sh"
source "$(dirname "$0")/d-lib.sh"
B=$ROOT/bench/work/d; mkdir -p "$B/bin"
export SPARED_ACCESS_LOG=off
TAG=${TAG:-load}
mk_template_in "$B/bin/spared-before" "$B/tpl"
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
start() { taskset -c 0 "$B/bin/spared-$1" serve --data-dir "$B/data" --idle-secs 600 >"$B/spared-load-$1.log" 2>&1 & PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done; }
snap() { python3 "$ROOT/bench/passB-scripts/d-snap.py" $PID; }
for r in 1 2 3 4 5; do
  if [ $((r % 2)) = 1 ]; then order="before after"; else order="after before"; fi
  for m in $order; do
    wait_quiet || true
    echo "$(date +%T) round $r $m"
    seg_begin
    rm -rf "$B/data"; cp -a "$B/tpl" "$B/data"
    cat "$B/data"/components/* >/dev/null
    start $m
    SN0=$(snap)
    taskset -c 2 "$LG" --style spared --mode cold --n 100 --out "$R/d-passD-$TAG-$m-cold-run$r.json"
    for c in 1 16; do inner_quiet; taskset -c 2 "$LG" --style spared --mode mixed --conc $c --requests 20000 --out "$R/d-passD-$TAG-$m-mixed-c$c-run$r.json"; done
    echo "{\"mode\":\"$m\",\"run\":$r,\"snap_at_ready\":$SN0,\"snap_after_mixed\":$(snap),\"pss_kb_after_mixed\":$(awk '$1=="Pss:"{print $2}' /proc/$PID/smaps_rollup)}" >>"$R/d-passD-$TAG-extra.jsonl"
    stop
    start $m; inner_quiet
    taskset -c 2 "$LG" --style spared --mode cold --n 100 --out "$R/d-passD-$TAG-$m-cold-existing-run$r.json"
    stop
    d_seg_end d "load-r$r-$m"
  done
done
echo D-LOAD-DONE
