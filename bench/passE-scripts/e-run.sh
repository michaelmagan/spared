#!/usr/bin/env bash
# Pass E: did the security and server changes since 28d4336 (fsguard per-write fstat and recount, file stream slot counting, request body budget,
# flock on the data dir, lenient manifest) cost throughput or latency? before = lab build of 28d4336 (git worktree bench/work/before, built with
# RUSTUP_TOOLCHAIN=1.99.0 so both use one compiler), after = target/lab/release/spared at the branch tip. Rounds alternate which binary goes first.
# Segment A: 100 demo apps, warm GET at 1 and 16 connections (20,000 each) and mixed (1 in 10 POST) at 1 and 16 (20,000 each).
# Segment B: workloads app (noop, sql-agg, sql-write) and bench/passE-guest (file-write, file-small, POST 1 KB and 1 MB), wl-loadgen at concurrency 1.
# Server pinned to CPU 0, load generator to CPU 2. Steal is logged per segment to bench/results/e-passE-steal.jsonl.
source "$(dirname "$0")/../passB-scripts/lib.sh"
export SPARED_ACCESS_LOG=off
E=$ROOT/bench/work/e; mkdir -p "$E/bin"
cp -f "$ROOT/bench/work/before/target/lab/release/spared" "$E/bin/before"
cp -f "$ROOT/target/lab/release/spared" "$E/bin/after"
for v in before after; do sha256sum "$E/bin/$v" | cut -c1-16 | sed "s/^/$v /"; done >"$R/e-passE-binaries.txt"
ROUNDS=${ROUNDS:-6}
WLG=$ROOT/bench/workloads/loadgen/target/release/wl-loadgen
WL_WASM=$ROOT/apps/workloads-spared/target/wasm32-wasip2/release/workloads-spared.wasm
PE_WASM=$ROOT/bench/passE-guest/target/wasm32-wasip2/release/passe-guest.wasm
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
ejson() { python3 - "$1" "$2" "$S0" "$T0" "$S1" "$T1" "$L0" "$D0" "$(date -u +%FT%TZ)" >>"$R/e-passE-steal.jsonl" <<'PY'
import json, sys
label, _x, s0, t0, s1, t1, l0, d0, d1 = sys.argv[1:]
ds, dt = int(s1) - int(s0), int(t1) - int(t0)
print(json.dumps({"segment": label, "start": d0, "end": d1, "loadavg1_at_start": float(l0), "steal_delta": ds, "all_cpu_jiffies_delta": dt, "steal_pct": round(100 * ds / dt, 3) if dt else None}))
PY
}
seg_close() { read -r S1 T1 <<<"$(steal_snap)"; ejson "$1" x; }
piece_a() { # variant round
  local v=$1 r=$2 out
  rm -rf "$E/data"; cp -a "$E/tpl-$v" "$E/data"
  cat "$E/data"/components/* >/dev/null
  taskset -c 0 "$E/bin/$v" serve --data-dir "$E/data" --idle-secs 600 >"$E/serve-a-$v.log" 2>&1 &
  PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  taskset -c 2 "$LG" --style spared --mode touch --n 100 --out /dev/null
  for c in 1 16; do
    inner_quiet; taskset -c 2 "$LG" --style spared --mode warm --conc $c --requests 20000 --out "$R/e-passE-$v-warm-c$c-run$r.json"
    inner_quiet; taskset -c 2 "$LG" --style spared --mode mixed --conc $c --requests 20000 --out "$R/e-passE-$v-mixed-c$c-run$r.json"
  done
  stop
}
deploy() { # name wasm manifest
  curl -fsS -X PUT -H "authorization: Bearer $(cat "$E/datab/admin-token")" -H "x-spared-manifest: $(base64 -w0 "$3")" --data-binary @"$2" http://127.0.0.1:8181/v1/apps/$1 >/dev/null
}
piece_b() {
  local v=$1 r=$2 spec name host path n extra
  rm -rf "$E/datab" "$E/home"; mkdir -p "$E/datab" "$E/home"
  HOME=$E/home taskset -c 0 "$E/bin/$v" serve --data-dir "$E/datab" --listen 127.0.0.1:8180 --admin-listen 127.0.0.1:8181 --idle-secs 3600 >"$E/serve-b-$v.log" 2>&1 &
  PID=$!
  wait_tcp 8181
  for _ in $(seq 50); do [ -s "$E/datab/admin-token" ] && break; sleep 0.1; done
  deploy workloads "$WL_WASM" "$ROOT/apps/workloads-spared/spared.toml"
  deploy passe "$PE_WASM" "$ROOT/bench/passE-guest/spared.toml"
  curl -fsS -H "Host: workloads.localhost" http://127.0.0.1:8180/w/sql-seed >/dev/null
  for spec in "noop|workloads|/w/noop|2000|" "sql-agg|workloads|/w/sql-agg|300|" "sql-write|workloads|/w/sql-write|300|" \
              "file-write|passe|/file-write|1000|" "file-small|passe|/file-small|500|" "post-1kb|passe|/body|2000|--post-bytes 1024" "post-1mb|passe|/body|500|--post-bytes 1048576"; do
    IFS='|' read -r name host path n extra <<<"$spec"
    inner_quiet
    taskset -c 2 "$WLG" --addr 127.0.0.1:8180 --host $host.localhost --path "$path" --warmup 20 --requests "$n" --pids "$PID" $extra --out "$R/e-passE-$v-$name-run$r.json" || echo "FAILED $v $name"
  done
  stop
}
for v in before after; do mk_template_in "$E/bin/$v" "$E/tpl-$v"; done
for r in $(seq 1 $ROUNDS); do
  if [ $((r % 2)) = 1 ]; then order="before after"; else order="after before"; fi
  for v in $order; do
    for p in a b; do
      wait_quiet || true
      echo "$(date +%T) round $r piece $p $v"
      seg_begin
      piece_$p $v $r >>"$E/run.log" 2>&1 || echo "FAILED r$r $p $v"
      seg_close "r$r-$p-$v"
    done
  done
done
echo E-DONE
