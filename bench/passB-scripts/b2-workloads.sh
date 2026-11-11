#!/usr/bin/env bash
# B2 workloads piece: the 15 backend workloads (apps/workloads-spared) on the old (6b52139) and new (HEAD) spared, 5 rounds, order alternates.
# Method as bench/workloads/run.sh (server CPU 0, load generator CPU 2, 20 warmup + 200 timed requests per workload, 2000 for noop),
# spared only. Raw: bench/results/b2w-passB-<old|new>-<workload>-run<r>.json
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
B=$ROOT/bench/work/b2; mkdir -p "$B/bin"
W=$ROOT/bench/work/b2w; mkdir -p "$W"
WASM=$ROOT/apps/workloads-spared/target/wasm32-wasip2/release/workloads-spared.wasm
WLG=$ROOT/bench/workloads/loadgen/target/release/wl-loadgen
WORKLOADS="noop sha256 json sql-agg sql-write markdown sieve nbody pixels sort wordfreq html gzip regex pbkdf2"
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
for r in 1 2 3 4 5; do
  if [ $((r % 2)) = 1 ]; then order="old new"; else order="new old"; fi
  for v in $order; do
    ap=/v1/apps; [ $v = old ] && ap=/apps
    wait_quiet || true
    echo "$(date +%T) round $r $v"
    seg_begin b2w "r$r-$v"
    rm -rf "$W/data" "$W/home"; mkdir -p "$W/data" "$W/home"
    HOME=$W/home taskset -c 0 "$B/bin/$v" serve --data-dir "$W/data" --listen 127.0.0.1:8180 --admin-listen 127.0.0.1:8181 --idle-secs 3600 >"$W/spared.log" 2>&1 &
    PID=$!
    wait_tcp 8181
    for _ in $(seq 50); do [ -s "$W/data/admin-token" ] && break; sleep 0.1; done
    curl -fsS -X PUT -H "authorization: Bearer $(cat "$W/data/admin-token")" -H "x-spared-manifest: $(base64 -w0 "$ROOT/apps/workloads-spared/spared.toml")" --data-binary @"$WASM" http://127.0.0.1:8181$ap/workloads >"$W/deploy.json"
    curl -fsS -H "Host: workloads.localhost" http://127.0.0.1:8180/w/sql-seed >/dev/null
    for wl in $WORKLOADS; do
      n=200; [ "$wl" = noop ] && n=2000
      inner_quiet
      f="$R/b2w-passB-$v-$wl-run$r.json"
      taskset -c 2 "$WLG" --addr 127.0.0.1:8180 --host workloads.localhost --path "/w/$wl" --warmup 20 --requests "$n" --pids "$PID" --out "$f" || echo "FAILED $v $wl"
    done
    stop
    seg_end b2w "r$r-$v"
  done
done
echo B2W-DONE
