#!/usr/bin/env bash
# B5: first request of 49 different apps (apps 1..49 of 50 distinct apps with data already on disk), after the server has served app 0.
# nodrop: page cache as it is. drop: `echo 3 > /proc/sys/vm/drop_caches` after the server started and served app 0, before apps 1..49.
# Server pinned to CPU 0. 5 rounds; order of (target, mode) rotates. spared: distinct wasm bench/work/distinct/wasm/v0..v49.
source "$(dirname "$0")/c-lib.sh"; seg_end() { c_seg_end "$@"; }
export SPARED_ACCESS_LOG=off
B=$ROOT/bench/work/b5c; mkdir -p "$B/bin"
cp -f "$ROOT/target/release/spared" "$B/bin/spared"
NA=50
# seeded spared data dir: 50 distinct apps deployed and each requested once (database exists), server stopped
if [ ! -d "$B/spared-seed/apps/demo-49" ] || [ ! -d "$B/spared-seed/apps/demo-49/data" ]; then
  rm -rf "$B/spared-seed"; mkdir -p "$B/spared-seed"
  "$B/bin/spared" serve --data-dir "$B/spared-seed" --listen 127.0.0.1:8080 --admin-listen 127.0.0.1:8081 --idle-secs 600 >"$B/seed.log" 2>&1 & p=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  for k in $(seq 0 $((NA-1))); do
    M=$(printf 'name = "demo-%s"\n[grants]\ndata = true\n' "$k" | base64 -w0)
    curl -fsS -X PUT -H "authorization: Bearer $(cat "$B/spared-seed/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$ROOT/bench/work/distinct/wasm/v$k.wasm" http://127.0.0.1:8081/v1/apps/demo-$k >/dev/null
    curl -fsS -o /dev/null -H "Host: demo-$k.localhost" http://127.0.0.1:8080/
  done
  kill $p; wait $p 2>/dev/null || true
fi
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
for r in 1 2 3 4 5; do
  for ti in 0 1; do
    if [ $(( (r + ti) % 2 )) = 0 ]; then ts="spared workerd"; else ts="workerd spared"; fi
    for t in $ts; do
      for m in $( [ $(( r % 2 )) = 1 ] && echo "nodrop drop" || echo "drop nodrop" ); do
        wait_quiet || true
        echo "$(date +%T) round $r $t $m"
        seg_begin b5 "r$r-$t-$m"
        if [ $t = spared ]; then
          rm -rf "$B/data"; cp -a "$B/spared-seed" "$B/data"
          taskset -c 0 "$B/bin/spared" serve --data-dir "$B/data" --idle-secs 600 >"$B/spared.log" 2>&1 & PID=$!
          for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
        else
          rm -rf "$B/wd-data" "$B/wd-cfg"; mkdir -p "$B/wd-data"
          python3 "$ROOT/bench/demo/gen-distinct-workerd.py" "$B/wd-cfg" "$B/wd-data" $NA >/dev/null
          # seed: start unpinned, touch every app once so each has a database, stop
          (cd "$B/wd-cfg" && exec "$WORKERD" serve config.capnp) >"$B/wd-seed.log" 2>&1 & PID=$!
          wait_tcp 9100; wait_tcp $((9100+NA-1)); sleep 1
          python3 "$ROOT/bench/passB-scripts/b5-fetch.py" workerd 0 $((NA-1)) /dev/null
          stop
          (cd "$B/wd-cfg" && exec taskset -c 0 "$WORKERD" serve config.capnp) >"$B/workerd.log" 2>&1 & PID=$!
          wait_tcp 9100; wait_tcp $((9100+NA-1)); sleep 1
        fi
        python3 "$ROOT/bench/passB-scripts/b5-fetch.py" $t 0 0 /dev/null
        sync
        [ $m = drop ] && echo 3 >/proc/sys/vm/drop_caches
        sleep 1
        python3 "$ROOT/bench/passB-scripts/b5-fetch.py" $t 1 $((NA-1)) "$R/b5-passC-$t-$m-run$r.json"
        stop
        seg_end b5 "r$r-$t-$m"
      done
    done
  done
done
echo B5-DONE
