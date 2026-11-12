#!/usr/bin/env bash
# Proves the SQLite object cache (crates/spared/src/sqlite_cache.rs) in a throwaway HOME:
#   1. `spared setup` compiles sqlite3.c once, and a second `spared setup` compiles nothing.
#   2. The first build of a new app in an empty target dir does not run the C compiler on sqlite3.c.
#   3. With the cache off (SPARED_NO_SQLITE_CACHE=1) the same build does compile it, so the counter works.
#   4. The cached and the uncached build produce the same component, byte for byte, and the same
#      `PRAGMA compile_options` and `sqlite_compileoption_get` list when the guest asks SQLite.
#   5. A missing cache, a damaged object and a changed flag (LIBSQLITE3_FLAGS) each fall back to one
#      plain compile and a working build; a changed flag keeps its own entry.
#   6. A relative SPARED_LIB_DIR is made absolute, so the build finds the wrapper.
# The compiler is counted by a wrapper script in a copy of the wasi-sdk directory (symlinks to the real files).
#
# Needs: a built host binary (cargo build --release -p spared), the wasm32-wasip2 Rust target,
# wasi-sdk 25 ($WASI_SDK_PATH or what scripts/wasi-sdk-default.sh finds), python3, network access
# or a warm cargo registry. Takes about three minutes: it builds SQLite six times.
# Environment: SPARED_BIN (default: target/release/spared in this repo), KEEP_TMP=1 keeps the temp dir.
set -eu
set -o pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
SPARED_BIN=${SPARED_BIN:-$root/target/release/spared}
[ -x "$SPARED_BIN" ] || { echo "sqlite-cache-test: $SPARED_BIN not found; run cargo build --release -p spared" >&2; exit 2; }
[ -n "${WASI_SDK_PATH:-}" ] || WASI_SDK_PATH=$("$root/scripts/wasi-sdk-default.sh")
[ -n "$WASI_SDK_PATH" ] || { echo "sqlite-cache-test: wasi-sdk not found; run 'spared setup' or set WASI_SDK_PATH" >&2; exit 2; }
command -v python3 >/dev/null || { echo "sqlite-cache-test: python3 is required" >&2; exit 2; }
real_sdk=$WASI_SDK_PATH

CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
export CARGO_HOME RUSTUP_HOME

tmp=$(mktemp -d "${TMPDIR:-/tmp}/spared-sqlite-cache.XXXXXX")
cleanup() {
  status=$?
  if [ "${KEEP_TMP:-0}" = 1 ]; then echo "sqlite-cache-test: kept $tmp" >&2; else rm -rf "$tmp"; fi
  if [ "$status" -eq 0 ]; then echo "sqlite-cache-test: PASS"; else echo "sqlite-cache-test: FAIL (exit $status)" >&2; fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
fail() { echo "sqlite-cache-test: FAIL: $*" >&2; exit 1; }
step() { echo "sqlite-cache-test: $*"; }

mkdir -p "$tmp/home"
export HOME="$tmp/home"
export SPARED_LIB_DIR="$tmp/lib"
unset SPARED_NO_SQLITE_CACHE LIBSQLITE3_FLAGS CARGO_TARGET_DIR

sdk=$tmp/sdk
mkdir -p "$sdk/bin"
for f in "$real_sdk"/bin/*; do
  [ "$(basename "$f")" = clang ] || ln -s "$f" "$sdk/bin/$(basename "$f")"
done
for d in share lib VERSION; do
  [ -e "$real_sdk/$d" ] && ln -s "$real_sdk/$d" "$sdk/$d"
done
cat >"$sdk/bin/clang" <<EOF
#!/bin/sh
for a in "\$@"; do
  case "\$a" in */sqlite3.c|sqlite3.c) echo compile >>"$tmp/compiles" ;; esac
done
exec "$real_sdk/bin/clang" "\$@"
EOF
chmod +x "$sdk/bin/clang"
export WASI_SDK_PATH=$sdk

compiles() { if [ -f "$tmp/compiles" ]; then wc -l <"$tmp/compiles" | tr -d ' '; else echo 0; fi; }
expect_compiles() {
  [ "$(compiles)" = "$1" ] || fail "$2: sqlite3.c was compiled $(compiles) times in total, want $1"
}
entries() { find "$SPARED_LIB_DIR" -maxdepth 1 -type d -name 'sqlite-*' | wc -l | tr -d ' '; }

step "spared setup --dry-run names the SQLite step and changes nothing"
"$SPARED_BIN" setup --dry-run >"$tmp/dry.out" || fail "setup --dry-run failed"
grep -q '^sqlite precompile: would run' "$tmp/dry.out" || { cat "$tmp/dry.out" >&2; fail "the dry run does not mention the SQLite step"; }
[ ! -e "$SPARED_LIB_DIR" ] || fail "the dry run created $SPARED_LIB_DIR"
"$SPARED_BIN" doctor >"$tmp/doctor.out" 2>&1 || true
grep -q '^WARN sqlite-cache: ' "$tmp/doctor.out" || { cat "$tmp/doctor.out" >&2; fail "doctor does not warn about the missing cache"; }

step "spared setup compiles SQLite once"
"$SPARED_BIN" setup >"$tmp/setup.out" 2>"$tmp/setup.err" || { cat "$tmp/setup.out" "$tmp/setup.err" >&2; fail "setup failed"; }
grep -q '^sqlite precompile: built' "$tmp/setup.out" || { cat "$tmp/setup.out" >&2; fail "setup did not report the SQLite step as built"; }
expect_compiles 1 "setup"
[ "$(entries)" = 1 ] || fail "want one cache entry after setup, found $(entries)"
"$SPARED_BIN" doctor >"$tmp/doctor.out" 2>&1 || true
grep -q '^OK   sqlite-cache: ' "$tmp/doctor.out" || { cat "$tmp/doctor.out" >&2; fail "doctor does not report the cache"; }
"$SPARED_BIN" setup >"$tmp/setup.out" 2>&1 || fail "second setup failed"
grep -q '^sqlite precompile: already built' "$tmp/setup.out" || { cat "$tmp/setup.out" >&2; fail "a second setup did not say already built"; }
expect_compiles 1 "second setup"

mkdir -p "$tmp/work"
cd "$tmp/work"
"$SPARED_BIN" new optapp --template counter >/dev/null
cat >optapp/src/main.rs <<'RS'
#[allow(dead_code)]
mod spared;
use wstd::http::{Body, Error, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS t(x);";
const CHUNK: usize = 150;

fn listing() -> (String, bool, String) {
    spared::with_db(|c| {
        let mut pragma: Vec<String> = vec![];
        let mut st = c.prepare("PRAGMA compile_options").unwrap();
        for row in st.query_map([], |r| r.get::<_, String>(0)).unwrap() {
            pragma.push(row.unwrap());
        }
        let mut get: Vec<String> = vec![];
        for n in 0..1000 {
            let o: Option<String> = c.query_row("SELECT sqlite_compileoption_get(?1)", [n], |r| r.get(0)).unwrap();
            match o {
                Some(o) => get.push(o),
                None => break,
            }
        }
        let version: String = c.query_row("SELECT sqlite_version()", [], |r| r.get(0)).unwrap();
        let agree = pragma == get;
        let used = |name: &str| -> bool { c.query_row("SELECT sqlite_compileoption_used(?1)", [name], |r| r.get::<_, i64>(0)).unwrap() == 1 };
        let mut text = format!("sqlite {version} ;");
        for o in &pragma {
            text.push(' ');
            text.push_str(o);
            text.push(';');
        }
        let used_line = format!(
            "used: threadsafe0={} fts5={} omit_shared_cache={} omit_load_extension={}",
            used("THREADSAFE=0"),
            used("ENABLE_FTS5"),
            used("OMIT_SHARED_CACHE"),
            used("OMIT_LOAD_EXTENSION")
        );
        (text, agree, used_line)
    })
}

// `spared test` prints the first 200 bytes of a body, so the listing is served in chunks:
// /0, /1, ... each hold CHUNK bytes, and /summary reports the length and whether the PRAGMA and
// sqlite_compileoption_get lists agree.
#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let (text, agree, used_line) = listing();
    let path = req.uri().path().trim_start_matches('/').to_string();
    let body = if path == "summary" {
        format!("length {} pragma_and_get_agree {agree} {used_line}\n", text.len())
    } else {
        let n: usize = path.parse().unwrap_or(0);
        text.get(n * CHUNK..((n + 1) * CHUNK).min(text.len())).unwrap_or("").to_string()
    };
    Ok(spared::text_response(200, body))
}
RS

chunks=$(for i in $(seq 0 19); do printf -- '--path /%s ' "$i"; done)
build_app() {
  # build_app <target-dir> <out-file>: spared test in optapp; the component's sha256 goes to
  # <out-file>.sha. The target dir is removed afterwards to keep the disk use small.
  (cd optapp && CARGO_TARGET_DIR="$1" "$SPARED_BIN" test $chunks --path /summary >"$2" 2>"$2.err") || { cat "$2" "$2.err" >&2; fail "spared test failed (target $1)"; }
  ls "$1"/wasm32-wasip2/release/build/libsqlite3-sys-*/out/libsqlite3.a >/dev/null 2>&1 || fail "$1 has no libsqlite3.a: libsqlite3-sys did not build"
  python3 -I -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1/wasm32-wasip2/release/optapp.wasm" >"$2.sha"
  rm -rf "$1"
}

step "the first build in an empty target dir does not compile sqlite3.c"
start=$(date +%s)
build_app "$tmp/t-cached" "$tmp/cached.txt"
elapsed=$(( $(date +%s) - start ))
expect_compiles 1 "first build with a warm cache"
step "  first build, warm cache: ${elapsed}s"

step "the same build with the cache off compiles sqlite3.c (the counter sees it)"
start=$(date +%s)
SPARED_NO_SQLITE_CACHE=1 build_app "$tmp/t-bundled" "$tmp/bundled.txt"
elapsed=$(( $(date +%s) - start ))
expect_compiles 2 "build with SPARED_NO_SQLITE_CACHE=1"
step "  first build, cache off: ${elapsed}s"

step "both builds give the same component and the same SQLite settings"
[ "$(cat "$tmp/cached.txt.sha")" = "$(cat "$tmp/bundled.txt.sha")" ] || fail "the cached and the uncached component differ"
cmp -s "$tmp/cached.txt" "$tmp/bundled.txt" || { diff "$tmp/cached.txt" "$tmp/bundled.txt" >&2 || true; fail "the guest reports different SQLite settings"; }
grep -q 'length [0-9]* pragma_and_get_agree true' "$tmp/cached.txt" || { cat "$tmp/cached.txt" >&2; fail "the PRAGMA and sqlite_compileoption_get lists disagree"; }
grep -q 'GET /19 -> 200' "$tmp/cached.txt" && [ "$(grep -c . "$tmp/cached.txt")" -gt 20 ] || fail "the listing output is incomplete"
grep -q 'threadsafe0=true fts5=true omit_shared_cache=true omit_load_extension=true' "$tmp/cached.txt" || { cat "$tmp/cached.txt" >&2; fail "an expected compile option is missing"; }
python3 -I - "$tmp/cached.txt" <<'PY' || fail "the listing is longer than the 20 chunks the test reads"
import re, sys
t = open(sys.argv[1]).read()
n = int(re.search(r"length (\d+)", t).group(1))
assert n <= 20 * 150, n
PY

step "a missing cache falls back to one compile and fills the cache again"
rm -rf "$SPARED_LIB_DIR"/sqlite-*
build_app "$tmp/t-missing" "$tmp/missing.txt"
expect_compiles 3 "build with the cache entry removed"
[ "$(entries)" = 1 ] || fail "the build did not refill the cache"
[ "$(cat "$tmp/missing.txt.sha")" = "$(cat "$tmp/cached.txt.sha")" ] || fail "the refilled build differs"

step "a damaged object is not trusted"
printf 'not an object file' >"$(ls -d "$SPARED_LIB_DIR"/sqlite-*)/sqlite3.o"
build_app "$tmp/t-damaged" "$tmp/damaged.txt"
expect_compiles 4 "build with a damaged object"
[ "$(cat "$tmp/damaged.txt.sha")" = "$(cat "$tmp/cached.txt.sha")" ] || fail "the build after the damaged object differs"
build_app "$tmp/t-healed" "$tmp/healed.txt"
expect_compiles 4 "build after the entry was rebuilt"

step "a changed C flag is a different object, kept next to the first"
LIBSQLITE3_FLAGS="-DSQLITE_DEFAULT_CACHE_SIZE=-4000" build_app "$tmp/t-flag" "$tmp/flag.txt"
expect_compiles 5 "build with an extra SQLite flag"
[ "$(entries)" = 2 ] || fail "want two entries after a build with another flag, found $(entries)"
grep -q 'DEFAULT_CACHE_SIZE=-4000' "$tmp/flag.txt" || { cat "$tmp/flag.txt" >&2; fail "the extra flag did not reach SQLite (a stale object was used)"; }
build_app "$tmp/t-plain-again" "$tmp/plain-again.txt"
expect_compiles 5 "build after the flagged one"
cmp -s "$tmp/plain-again.txt" "$tmp/cached.txt" || fail "the plain build changed after a flagged one"

step "a relative SPARED_LIB_DIR still builds, in its own absolute directory"
SPARED_LIB_DIR=rel-lib build_app "$tmp/t-relative" "$tmp/relative.txt"
expect_compiles 6 "build with a relative SPARED_LIB_DIR"
[ -x optapp/rel-lib/bin/spared-cc ] || fail "the wrapper was not set up under the relative SPARED_LIB_DIR"
ls optapp/rel-lib/sqlite-*/sqlite3.o >/dev/null 2>&1 || fail "the relative SPARED_LIB_DIR got no cache entry"
[ "$(cat "$tmp/relative.txt.sha")" = "$(cat "$tmp/cached.txt.sha")" ] || fail "the build with a relative SPARED_LIB_DIR differs"

exit 0
