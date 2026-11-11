#!/usr/bin/env bash
# Start workerd (:9090) and spared (:8080 public, :8081 admin) with the demo app deployed as `demo`.
# Usage: bench/demo/serve-both.sh [idle-secs]   (default 30). Ctrl-C stops both.
# Env: FRESH=1 wipes the data dirs first.
set -euo pipefail
# The published numbers were measured without the server's request log; keep it off.
export SPARED_ACCESS_LOG=${SPARED_ACCESS_LOG:-off}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
IDLE=${1:-30}
W=$ROOT/bench/work/demo
WASM=$ROOT/apps/demo-spared/target/wasm32-wasip2/release/demo-spared.wasm
WORKERD=$ROOT/bench/work/workerd/node_modules/.bin/workerd
[ -f "$WASM" ] || { echo "missing $WASM (build demo-spared first)" >&2; exit 1; }
[ -x "$WORKERD" ] || { echo "missing workerd (see apps/demo-workerd/README.md)" >&2; exit 1; }
[ "${FRESH:-0}" = 1 ] && rm -rf "$W"
mkdir -p "$W/workerd-data" "$W/spared-data"
pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT INT TERM

(cd "$ROOT/apps/demo-workerd" && exec "$WORKERD" serve config.capnp) >"$W/workerd.log" 2>&1 &
pids+=($!)
"$ROOT/target/release/spared" serve --data-dir "$W/spared-data" --idle-secs "$IDLE" >"$W/spared.log" 2>&1 &
pids+=($!)

for i in $(seq 100); do
  curl -fs -o /dev/null http://127.0.0.1:8081/ 2>/dev/null || curl -s -o /dev/null http://127.0.0.1:8081/ && break
  sleep 0.1
done
M=$(printf 'name = "demo"\n[grants]\ndata = true\n' | base64 -w0)
curl -fsS -X PUT -H "authorization: Bearer $(cat "$W/spared-data/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$WASM" http://127.0.0.1:8081/v1/apps/demo >/dev/null
echo "workerd: http://127.0.0.1:9090/   spared: curl -H 'Host: demo.localhost' http://127.0.0.1:8080/ (idle-secs=$IDLE)"
wait
