#!/usr/bin/env bash
# Build one variant of a guest for the SQLite tuning runs (docs/sqlite-in-wasm-speedup.md).
#   bench/tuning/build.sh <name>            probes/sqlite-probe  -> bench/work/tuning/bin/<name>.wasm
#   APP=demo bench/tuning/build.sh <name>   apps/demo-spared     -> bench/work/tuning/bin/<name>.wasm
# Levers, all optional, from the environment:
#   OPT     optimisation level for SQLite's C (default -O3)
#   FEAT    wasm feature flags (default: the shipped set)
#   XC      extra C flags for the wasm build only, e.g. "-DHAVE_PREAD=1 -DHAVE_PWRITE=1"
#   SQL     LIBSQLITE3_FLAGS: SQLite options applied to the wasm build and to the native comparator
#   RUSTP   cargo profile overrides, e.g. "CARGO_PROFILE_RELEASE_OPT_LEVEL=s"
#   RF      RUSTFLAGS for the wasm build (-C link-arg=--skip-wit-component gives a core module for wasm-opt)
#   NATIVE=1 also builds the native comparator (<name>.native); WASM=0 skips the wasm build
#   NRF     RUSTFLAGS for the native build ("--cfg sqlite_nomutex" with
#           SQL="-USQLITE_THREADSAFE -DSQLITE_THREADSAFE=0 -Dsqlite3_threadsafe=sqlite3_threadsafe_real")
# The source is copied to bench/work/tuning first so the build does not depend on which Cargo
# workspace the checkout sits in.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
W=$ROOT/bench/work/tuning
WASI=${WASI:-/opt/wasi-sdk-dl/wasi-sdk-25.0-x86_64-linux}
name=$1
OPT=${OPT:--O3}
FEAT=${FEAT--mbulk-memory -msimd128 -mnontrapping-fptoint -mmultivalue -mreference-types}
if [ "${APP:-probe}" = demo ]; then src=apps/demo-spared; pkg=demo-spared; else src=probes/sqlite-probe; pkg=sqlite-probe; fi
mkdir -p "$W/bin" "$W/src/$pkg/src" "$W/src/demo-shared"
cp "$ROOT/$src"/src/*.rs "$W/src/$pkg/src/"
cp "$ROOT/$src/Cargo.lock" "$W/src/$pkg/"
cp -r "$ROOT/apps/demo-shared/." "$W/src/demo-shared/"
{ grep -v '^\[workspace\]$' "$ROOT/$src/Cargo.toml"; printf '\n[workspace]\n'; } > "$W/src/$pkg/Cargo.toml"
cd "$W/src/$pkg"
if [ "${WASM:-1}" = 1 ]; then
  env ${RUSTP:-} CC_wasm32_wasip2="$WASI/bin/clang" AR_wasm32_wasip2="$WASI/bin/llvm-ar" \
    CFLAGS_wasm32_wasip2="--sysroot=$WASI/share/wasi-sysroot $OPT $FEAT -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION=1 -D_WASI_EMULATED_GETPID -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS ${XC:-}" \
    LIBSQLITE3_FLAGS="${SQL:-}" RUSTFLAGS="${RF:-}" \
    cargo build --release --target wasm32-wasip2 -j2 --target-dir "$W/target-wasm-$pkg"
  cp "$W/target-wasm-$pkg/wasm32-wasip2/release/$pkg.wasm" "$W/bin/$name.wasm"
  ls -la "$W/bin/$name.wasm"
fi
if [ "${NATIVE:-0}" = 1 ]; then
  env LIBSQLITE3_FLAGS="${SQL:-}" RUSTFLAGS="${NRF:-}" cargo build --release -j2 --target-dir "$W/target-native-$pkg"
  cp "$W/target-native-$pkg/release/$pkg" "$W/bin/$name.native"
  ls -la "$W/bin/$name.native"
fi
