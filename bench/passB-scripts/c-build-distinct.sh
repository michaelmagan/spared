#!/usr/bin/env bash
# Build distinct variants v200..v499 (gen-distinct.py 500 first) into bench/work/distinct/wasm. 3 parallel cargo builds.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd); source $ROOT/bench/env.sh
export CARGO_TARGET_DIR=$ROOT/bench/work/distinct/target
one() { k=$1; D=$ROOT/bench/work/distinct; [ -f $D/wasm/v$k.wasm ] && return; (cd $D/v$k && cargo build --release --target wasm32-wasip2 -j1 >/dev/null 2>&1) && cp $CARGO_TARGET_DIR/wasm32-wasip2/release/demo-distinct-$k.wasm $D/wasm/v$k.wasm; }
export -f one; export ROOT CARGO_TARGET_DIR
seq 200 499 | xargs -P3 -I{} bash -c 'one {}'
echo BUILD-DONE
