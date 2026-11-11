#!/usr/bin/env bash
# Sandbox escape suite. Needs the lab binary (`cargo build --release -p spared --features lab
# --target-dir target/lab`; built on first use) and builds the escape-probe guests.
# SPARED_EXTRA_CA_FILE is only for networks that intercept TLS (the HTTPS control needs it there).
set -eu
cd "$(dirname "$0")/.."
# Honour the same overrides as scripts/e2e.sh: WASI_SDK_PATH for the SDK, SPARED_BIN for the host binary.
if [ -n "${WASI_SDK_PATH:-}" ]; then export WASI="$WASI_SDK_PATH"; fi
source bench/env.sh
lab_build
SPARED_BIN=${SPARED_BIN:-$SPARED_LAB_BIN}
rm -rf bench/work/escape
(cd probes/escape-probe && cargo build --release --target wasm32-wasip2)
"$SPARED_BIN" escape-test --probes probes/escape-probe/target/wasm32-wasip2/release --root bench/work/escape
