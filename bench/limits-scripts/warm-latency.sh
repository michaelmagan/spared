#!/usr/bin/env bash
# Warm-request latency before and after per-app limits, after the method of bench/passE-scripts:
# server pinned to CPU 0, client (`spared bench`, 20,000 warm GETs on one connection after 20 cold
# cycles) pinned to CPU 2, access log off, rounds rotate the order of the variants.
#   before         a release build of the commit before per-app limits ($BEFORE_BIN)
#   after          this build, no limits set
#   after-limited  this build, with a CPU budget, an egress budget and a data cap set high enough
#                  never to throttle, so the accounting runs on every request
# The app is examples/dist/notes (scripts/build-examples.sh), GET /api/notes with one row.
# Results: bench/results/limits-<variant>-run<N>.json and a summary on stdout.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CLI=${CLI:-$ROOT/target/release/spared}
BEFORE_BIN=${BEFORE_BIN:?set BEFORE_BIN to a release build of the commit before per-app limits}
WASM=$ROOT/examples/dist/notes/app.wasm
MAN=$ROOT/examples/dist/notes/spared.toml
OUT=${OUT:-$ROOT/bench/results}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/spared-limits-bench.XXXXXX")
ROUNDS=${ROUNDS:-6}
N=${N:-20000}
export SPARED_ACCESS_LOG=off
trap 'rm -rf "$WORK"' EXIT
run() { # variant round
  local v=$1 r=$2 bin=$CLI d=$WORK/data-$1
  [ "$v" = before ] && bin=$BEFORE_BIN
  rm -rf "$d"; mkdir -p "$d/home"
  HOME=$d/home taskset -c 0 "$bin" serve --data-dir "$d/data" --listen 127.0.0.1:9380 --admin-listen 127.0.0.1:9381 --idle-secs 3600 >"$d/serve.log" 2>&1 &
  local pid=$!
  for _ in $(seq 100); do [ -s "$d/data/admin-token" ] && curl -s -o /dev/null http://127.0.0.1:9380/healthz && break; sleep 0.1; done
  local tok; tok=$(cat "$d/data/admin-token")
  curl -fsS -X PUT -H "authorization: Bearer $tok" -H "x-spared-manifest: $(base64 -w0 "$MAN")" --data-binary @"$WASM" http://127.0.0.1:9381/v1/apps/notes >/dev/null
  if [ "$v" = after-limited ]; then
    HOME=$d/home SPARED_TOKEN=$tok "$CLI" limits set notes --cpu-percent 100 --egress-mb-per-hour 1000 --data-mb 512 --server http://127.0.0.1:9381 >/dev/null
  fi
  curl -fsS -H 'content-type: application/json' -H 'host: notes.localhost' -d '{"text":"bench"}' http://127.0.0.1:9380/api/notes >/dev/null
  HOME=$d/home SPARED_TOKEN=$tok taskset -c 2 "$CLI" --json bench notes --requests "$N" --path /api/notes --server http://127.0.0.1:9381 >"$OUT/limits-$v-run$r.json"
  kill "$pid"; wait "$pid" 2>/dev/null || true
}
for r in $(seq 1 "$ROUNDS"); do
  case $((r % 3)) in
    1) order="before after after-limited" ;;
    2) order="after-limited before after" ;;
    0) order="after after-limited before" ;;
  esac
  for v in $order; do run "$v" "$r"; done
done
python3 - "$OUT" <<'PY'
import json, os, statistics, sys
d = sys.argv[1]
for v in ["before", "after", "after-limited"]:
    runs = [json.load(open(os.path.join(d, f))) for f in sorted(os.listdir(d)) if f.startswith(f"limits-{v}-run")]
    p50 = [r["warm"]["p50_ms"] for r in runs]
    p99 = [r["warm"]["p99_ms"] for r in runs]
    print(f"{v:14} warm p50 median {statistics.median(p50):.3f} ms (runs {', '.join(f'{x:.3f}' for x in p50)}), p99 median {statistics.median(p99):.3f} ms")
PY
