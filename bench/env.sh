# shellcheck shell=bash
# Build environment for compiling SQLite (C) to wasm32-wasip2. Source before building guests.
_spared_root=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)
export WASI=${WASI:-$("$_spared_root/scripts/wasi-sdk-default.sh")}
if [ -z "$WASI" ]; then
  echo "bench/env.sh: wasi-sdk not found. Fix: run 'spared setup' (installs ~/.spared/wasi-sdk), or set WASI to a wasi-sdk 25 directory" >&2
  return 1 2>/dev/null || exit 1
fi
export CC_wasm32_wasip2="$WASI/bin/clang" AR_wasm32_wasip2="$WASI/bin/llvm-ar"
# The lab binary: `spared` built with --features lab, which adds run-probe, compile, bench-wake,
# bench-density, bench-3b and escape-test. It goes in its own target directory so it never replaces
# the release binary. `lab_build` builds it if it is missing; extra arguments go to cargo (e.g. -j2).
export SPARED_LAB_BIN=${SPARED_LAB_BIN:-$_spared_root/target/lab/release/spared}
SPARED_REPO_ROOT=$_spared_root
lab_build() {
  [ -x "$SPARED_LAB_BIN" ] || (cd "$SPARED_REPO_ROOT" && cargo build --release --locked -p spared --features lab --target-dir target/lab "$@")
}
# Guest C flags: one file, shared with `spared deploy` and scripts/build-examples.sh.
export CFLAGS_wasm32_wasip2="--sysroot=$WASI/share/wasi-sysroot $(grep -v '^#' "$_spared_root/scripts/guest-cflags.txt" | tr '\n' ' ')"
unset _spared_root
