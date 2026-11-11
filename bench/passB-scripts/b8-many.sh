#!/usr/bin/env bash
# B8: spared serve with hundreds of apps. N = 200 different apps (bench/work/distinct/wasm/v0..v199) and N = 500 apps made from those 200 wasm
# files under 500 names (WRAP=200: apps k and k+200 share a component, so code is shared by the content-addressed store and N = 500 under-counts
# what 500 different apps would cost). mem-curve harness (one fresh server per point, page cache dropped before start), 3 reps, memory after
# every app served one request (`first`) and 100 requests each (`steady`). Then wake at N = 500 on seeded data: server started, app 0 served,
# apps 1..499 requested once each, with and without a page-cache drop before them, 3 reps, order alternates.
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
B=$ROOT/bench/work/b8; mkdir -p "$B/bin"
cp -f "$ROOT/target/release/spared" "$B/bin/spared"
for rep in 1 2 3; do
  for n in 200 500; do
    wait_quiet || true
    seg_begin b8 "mcurve-n$n-rep$rep"
    WRAP=200 SPARED=$B/bin/spared CURVE_W=$B CURVE_PREFIX=b8-passB-mcurve python3 "$ROOT/bench/passB-scripts/mem-curve-b8.py" spared-distinct $n $rep 2>&1 | tail -1
    seg_end b8 "mcurve-n$n-rep$rep"
  done
done
# seeded 500-app data dir (apps from the mem-curve template, each requested once so each has a database)
T=$B/tpl-spared-distinct; D=$B/seed500
if [ ! -d "$D/apps/demo-499/data" ] || [ -z "$(ls $D/apps/demo-499/data 2>/dev/null)" ]; then
  rm -rf "$D"; mkdir -p "$D/apps" "$D/components" "$D/backups"
  cp "$T/admin-token" "$D/"
  for k in $(seq 0 499); do mkdir "$D/apps/demo-$k"; for e in $(ls "$T/apps/demo-$k"); do if [ -d "$T/apps/demo-$k/$e" ]; then cp -a "$T/apps/demo-$k/$e" "$D/apps/demo-$k/$e"; else ln "$T/apps/demo-$k/$e" "$D/apps/demo-$k/$e"; fi; done; done
  for f in "$T"/components/*; do ln "$f" "$D/components/$(basename $f)"; done
  [ -f "$T/format" ] && cp "$T/format" "$D/"; for f in $(ls "$T" | grep -v -e '^apps$' -e '^components$' -e '^backups$' -e '^admin-token$'); do cp -a "$T/$f" "$D/" 2>/dev/null; done
  taskset -c 0 "$B/bin/spared" serve --data-dir "$D" --idle-secs 600 >"$B/seed.log" 2>&1 & p=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  python3 "$ROOT/bench/passB-scripts/b5-fetch.py" spared 0 499 /dev/null
  kill $p; wait $p 2>/dev/null
fi
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
for r in 1 2 3; do
  for m in $( [ $((r % 2)) = 1 ] && echo "nodrop drop" || echo "drop nodrop" ); do
    wait_quiet || true
    echo "$(date +%T) wake round $r $m"
    seg_begin b8 "wake-r$r-$m"
    t0=$(date +%s.%N)
    taskset -c 0 "$B/bin/spared" serve --data-dir "$D" --idle-secs 600 >"$B/wake.log" 2>&1 & PID=$!
    for _ in $(seq 600); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.05; done
    t1=$(date +%s.%N)
    python3 "$ROOT/bench/passB-scripts/b5-fetch.py" spared 0 0 /dev/null
    sync; [ $m = drop ] && echo 3 >/proc/sys/vm/drop_caches; sleep 1
    python3 "$ROOT/bench/passB-scripts/b5-fetch.py" spared 1 499 "$R/b8-passB-wake500-$m-run$r.json"
    echo "{\"mode\":\"$m\",\"run\":$r,\"ready_s\":$(python3 -c "print(round($t1-$t0,3))"),\"pss_kb\":$(awk '$1=="Pss:"{print $2}' /proc/$PID/smaps_rollup)}" >>"$R/b8-passB-wake500-extra.jsonl"
    stop
    seg_end b8 "wake-r$r-$m"
  done
done
echo B8-DONE
