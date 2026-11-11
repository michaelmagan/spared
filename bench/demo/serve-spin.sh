#!/usr/bin/env bash
# Run the demo app under `spin up` on 127.0.0.1:9191. Database file: bench/work/spin-data/spin.db.
# Usage: [CPU=0] [FRESH=1] bench/demo/serve-spin.sh        (exec's spin; stop with SIGTERM)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SPIN=${SPIN:-/opt/spin-dl/spin}
D=$ROOT/bench/work/spin-data
mkdir -p "$D"
[ "${FRESH:-0}" = 1 ] && rm -rf "$D" && mkdir -p "$D"
cat >"$D/runtime-config.toml" <<TOML
[sqlite_database.default]
type = "spin"
path = "$D/spin.db"
TOML
cd "$ROOT/apps/demo-spin"
[ -f target/wasm32-wasip2/release/demo_spin.wasm ] || cargo build --target wasm32-wasip2 --release -j2
${CPU:+exec taskset -c $CPU} "$SPIN" up --listen 127.0.0.1:9191 --runtime-config-file "$D/runtime-config.toml" --state-dir "$D/state" >"$D/spin.log" 2>&1
