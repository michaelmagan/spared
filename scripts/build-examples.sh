#!/bin/sh
# Build the three example apps to examples/dist/<name>/{app.wasm,spared.toml}.
#
#   notes  examples/notes      (the `spared new` template, with relative URLs)
#   list   apps/shared-list    (JSON list API)
#   demo   apps/demo-spared    (page app)
#
# Each example's spared.toml lives in examples/<name>/. The Dockerfile runs this script and
# `spared serve --examples-dir` deploys the result.
#
# Needs: cargo with the wasm32-wasip2 target, and wasi-sdk 25 (SQLite is C). The SDK comes from
# $WASI_SDK_PATH, then ~/.spared/wasi-sdk (see `spared setup`), then /opt/wasi-sdk*.
# Environment: CARGO_TARGET_DIR (default: target/examples in this repo), OUT (default: examples/dist).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out=${OUT:-$root/examples/dist}
target=${CARGO_TARGET_DIR:-$root/target/examples}

sdk=${WASI_SDK_PATH:-}
if [ -z "$sdk" ]; then
  sdk=$("$root/scripts/wasi-sdk-default.sh")
fi
if [ -z "$sdk" ] || [ ! -x "$sdk/bin/clang" ]; then
  echo "build-examples: wasi-sdk not found. Fix: run 'spared setup', or set WASI_SDK_PATH to a wasi-sdk 25 directory" >&2
  exit 1
fi
if ! rustc --print target-libdir --target wasm32-wasip2 >/dev/null 2>&1; then
  echo "build-examples: the wasm32-wasip2 Rust target is missing. Fix: rustup target add wasm32-wasip2" >&2
  exit 1
fi

export CC_wasm32_wasip2="$sdk/bin/clang"
export AR_wasm32_wasip2="$sdk/bin/llvm-ar"
# The guest C flags live in one file, shared with `spared deploy` and bench/env.sh.
CFLAGS_wasm32_wasip2="--sysroot=$sdk/share/wasi-sysroot $(grep -v '^#' "$root/scripts/guest-cflags.txt" | tr '\n' ' ')"
export CFLAGS_wasm32_wasip2
export CARGO_TARGET_DIR="$target"

build() {
  name=$1 src=$2 pkg=$3
  echo "build-examples: $name (from $src)" >&2
  locked=
  if [ -f "$root/$src/Cargo.lock" ]; then locked=--locked; fi
  (cd "$root/$src" && cargo build --release --target wasm32-wasip2 $locked)
  mkdir -p "$out/$name"
  cp "$target/wasm32-wasip2/release/$pkg.wasm" "$out/$name/app.wasm"
  cp "$root/examples/$name/spared.toml" "$out/$name/spared.toml"
}

rm -rf "$out"
build notes examples/notes notes
build list apps/shared-list shared-list
build demo apps/demo-spared demo-spared
echo "build-examples: wrote $out" >&2
