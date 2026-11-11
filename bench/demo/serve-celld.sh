#!/usr/bin/env bash
# Start celld (single node, local object store via `celld dev`) with the demo app on :9292, pinned to one core.
# Env: LOGS=0 drops --logs (use for measuring; default 1). FRESH=1 wipes state first. CPU=<n> core to pin (default 0). CELLD=<path> binary. NOPIN=1 disables pinning.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CELLD=${CELLD:-/opt/celld-src/celld/target/release/celld}
export CELLD_ESBUILD=${CELLD_ESBUILD:-/opt/celld-src/tools/node_modules/.bin/esbuild}
[ -x "$CELLD" ] || { echo "missing $CELLD (cargo build --release -j2 in /opt/celld-src/celld)" >&2; exit 1; }
[ -x "$CELLD_ESBUILD" ] || { echo "missing esbuild (npm i esbuild in /opt/celld-src/tools)" >&2; exit 1; }
cd "$ROOT/apps/demo-celld"
args=(dev --port 9292 --no-watch)
[ "${LOGS:-1}" = 1 ] && args+=(--logs)
[ "${FRESH:-0}" = 1 ] && args+=(--clean)
pin=(taskset -c "${CPU:-0}")
[ "${NOPIN:-0}" = 1 ] && pin=()
echo "celld: http://127.0.0.1:9292/ (state: apps/demo-celld/.celld/dev)" >&2
exec "${pin[@]}" "$CELLD" "${args[@]}"
