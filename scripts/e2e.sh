#!/usr/bin/env bash
# End-to-end smoke test: run a real server, scaffold an app, test it, deploy it, call it, tear down.
# Along the way: `spared --version`, GET /healthz, the access log (and that it holds no query
# string or body), the admin /metrics endpoint, the data dir's format stamp, and a restart on the
# same data dir.
# Then run scripts/server-test.py (admin token, limits, routing, traps, slow clients) against the
# same binary; SKIP_SERVER_TEST=1 skips it.
#
# Needs: a built host binary (cargo build --release -p spared), the wasm32-wasip2 Rust target,
# wasi-sdk 25 with WASI_SDK_PATH pointing at it, curl and python3.
#
# Run it locally the way CI does:
#   cargo build --release -p spared
#   spared setup                                  # installs wasi-sdk into ~/.spared/wasi-sdk, which the script finds
#   export WASI_SDK_PATH=/path/to/wasi-sdk-25.0   # or point at another copy
#   scripts/e2e.sh
#
# Environment:
#   SPARED_BIN          path to the spared binary (default: target/release/spared in this repo)
#   CARGO_TARGET_DIR    build cache for the scaffolded app (default: a temp dir, so the first
#                       build compiles SQLite from scratch, about a minute; point it at a
#                       persistent dir to make reruns fast)
#   KEEP_TMP=1          keep the temp dir (server log, data dir, app) after the run
#
# Uses random free localhost ports, a temp data dir and a temp $HOME for the server; never touches
# 8080/8081, ~/.spared data or ~/.spared/token. The CLI gets the token through $SPARED_TOKEN.
set -eu
set -o pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
SPARED_BIN=${SPARED_BIN:-$root/target/release/spared}
[ -x "$SPARED_BIN" ] || { echo "e2e: $SPARED_BIN not found; run cargo build --release -p spared" >&2; exit 2; }
[ -n "${WASI_SDK_PATH:-}" ] || WASI_SDK_PATH=$("$root/scripts/wasi-sdk-default.sh")
[ -n "$WASI_SDK_PATH" ] || { echo "e2e: wasi-sdk not found. Fix: run 'spared setup' (installs ~/.spared/wasi-sdk), or set WASI_SDK_PATH to a wasi-sdk 25 directory" >&2; exit 2; }
export WASI_SDK_PATH
command -v curl >/dev/null || { echo "e2e: curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "e2e: python3 is required" >&2; exit 2; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/spared-e2e.XXXXXX")
server_pid=
# shellcheck disable=SC2329  # invoked by the EXIT trap
cleanup() {
  status=$?
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ -f "$tmp/serve.log" ]; then
    echo "--- server log ---" >&2
    tail -n 40 "$tmp/serve.log" >&2 || true
  fi
  if [ "${KEEP_TMP:-0}" = 1 ]; then
    echo "e2e: kept $tmp" >&2
  else
    rm -rf "$tmp"
  fi
  if [ "$status" -eq 0 ]; then echo "e2e: PASS"; else echo "e2e: FAIL (exit $status)" >&2; fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

fail() { echo "e2e: FAIL: $*" >&2; exit 1; }
step() { echo "e2e: $*"; }

free_port() {
  python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}
pub=$(free_port)
adm=$(free_port)
[ "$pub" != "$adm" ] || adm=$(free_port)

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$tmp/target}
export SPARED_URL=http://127.0.0.1:$adm

step "spared --version"
version_line=$("$SPARED_BIN" --version) || fail "spared --version exited nonzero"
printf '%s\n' "$version_line" | grep -Eq '^spared [0-9]+\.[0-9]+\.[0-9]+[^ ]*( \([0-9a-f]{7,12}\))?$' || fail "unexpected --version output: $version_line"
version=${version_line#spared }
version=${version%% *}
if "$SPARED_BIN" --help | grep -Eq '^ +(bench-3b|run-probe|escape-test|bench-wake|bench-density|compile) '; then fail "spared --help lists a lab command"; fi

# The server's stdout (the access log) and stderr (its own messages) go to separate files.
start_server() {
  HOME="$tmp/home" "$SPARED_BIN" serve --data-dir "$tmp/data" --listen "127.0.0.1:$pub" --admin-listen "127.0.0.1:$adm" >>"$tmp/access.log" 2>>"$tmp/serve.log" &
  server_pid=$!
  up=0
  for _ in $(seq 1 100); do
    kill -0 "$server_pid" 2>/dev/null || fail "server exited during startup"
    if [ -s "$tmp/data/admin-token" ]; then
      SPARED_TOKEN=$(cat "$tmp/data/admin-token")
      export SPARED_TOKEN
      if "$SPARED_BIN" ps >/dev/null 2>&1; then up=1; break; fi
    fi
    sleep 0.2
  done
  [ "$up" = 1 ] || fail "server did not answer on the admin port within 20 s"
}

step "starting server (apps on $pub, admin on $adm)"
mkdir -p "$tmp/home"
start_server
grep -q "^spared $version" "$tmp/serve.log" || fail "the server's startup output does not name version $version"
grep -q "^apps:  http://127.0.0.1:$pub " "$tmp/serve.log" && grep -q "^admin: http://127.0.0.1:$adm " "$tmp/serve.log" && grep -q '^data dir: ' "$tmp/serve.log" || fail "startup output lacks the listen addresses or the data dir"

step "GET /healthz"
code=$(curl -sS --max-time 10 -o "$tmp/body" -w '%{http_code}' "http://127.0.0.1:$pub/healthz") || fail "healthz request failed"
[ "$code" = 200 ] || { cat "$tmp/body" >&2; fail "/healthz answered $code, want 200"; }
python3 -I -c '
import json, sys
v = json.load(open(sys.argv[1]))
assert sorted(v) == ["status", "uptime_seconds", "version"], v
assert v["status"] == "ok" and v["version"] == sys.argv[2], v
' "$tmp/body" "$version" || fail "/healthz body is not {status, version, uptime_seconds} with version $version"

step "data dir format stamp"
python3 -I -c '
import json, sys
s = json.load(open(sys.argv[1]))
assert s["format"] == 1 and s["created_by"] == sys.argv[2], s
' "$tmp/data/format.json" "$version" || fail "format.json is missing or wrong"

mkdir -p "$tmp/work"
cd "$tmp/work"

step "spared new smoke-app"
"$SPARED_BIN" new smoke-app >/dev/null
cd smoke-app

step "a build script runs without the admin token in its environment"
cat >build.rs <<'RS'
fn main() {
    let seen = |name: &str| if std::env::var_os(name).is_some() { "set" } else { "unset" };
    let report = format!("SPARED_TOKEN={}\nSPARED_ADMIN_TOKEN={}\n", seen("SPARED_TOKEN"), seen("SPARED_ADMIN_TOKEN"));
    if let Some(path) = std::env::var_os("E2E_BUILD_PROBE") {
        std::fs::write(path, report).unwrap();
    }
}
RS

step "spared test"
E2E_BUILD_PROBE="$tmp/build-probe.txt" SPARED_ADMIN_TOKEN="$SPARED_TOKEN" "$SPARED_BIN" test --path / --path /api/notes >"$tmp/test.out" 2>"$tmp/test.err" || { cat "$tmp/test.out" "$tmp/test.err" >&2; fail "spared test exited nonzero"; }
[ "$(cat "$tmp/build-probe.txt" 2>/dev/null)" = "$(printf 'SPARED_TOKEN=unset\nSPARED_ADMIN_TOKEN=unset')" ] || { cat "$tmp/build-probe.txt" >&2; fail "the build script saw SPARED_TOKEN or SPARED_ADMIN_TOKEN in its environment"; }

step "spared deploy"
"$SPARED_BIN" deploy >"$tmp/deploy.out" 2>"$tmp/deploy.err" || { cat "$tmp/deploy.out" "$tmp/deploy.err" >&2; fail "spared deploy exited nonzero"; }
url=$(sed -n 's/^url: //p' "$tmp/deploy.out" | head -n 1)
[ -n "$url" ] || { cat "$tmp/deploy.out" >&2; fail "deploy printed no url"; }
step "deployed at $url"

step "built component uses positional file reads and writes (SQLite built with HAVE_PREAD/HAVE_PWRITE)"
# Without those flags SQLite reads and writes pages through WASI streams, which wasmtime-wasi runs
# on a blocking-pool thread (the SQLite microbenchmark went from 1.8x to 3.2x native); the component then imports no
# [method]descriptor.read or .write. The names are matched with their length byte (23, 24).
wasm=$(ls "$CARGO_TARGET_DIR"/wasm32-wasip2/release/smoke*app.wasm | head -n 1)
python3 -I -c '
import sys
b = open(sys.argv[1], "rb").read()
assert b"\x17[method]descriptor.read" in b and b"\x18[method]descriptor.write" in b, "no positional file I/O imports"
' "$wasm" || fail "$wasm does not import descriptor.read/write: guest CFLAGS lost HAVE_PREAD/HAVE_PWRITE"
host=${url#http://}
host=${host%%/*}
host=${host%%:*}
url=${url%/}

http() {
  curl -sS --max-time 30 --resolve "$host:$pub:127.0.0.1" -o "$tmp/body" -w '%{http_code}' "$@"
}

step "two app directories with the same crate name each build their own code"
# Every app builds into one target dir and cargo names the final unit after the crate, so a second
# directory with the same name used to run the first one's build. Old mtimes make the stale unit look fresh.
mkdir -p "$tmp/dup/a" "$tmp/dup/b"
(cd "$tmp/dup/a" && "$SPARED_BIN" new dup-app >/dev/null)
(cd "$tmp/dup/b" && "$SPARED_BIN" new dup-app --template counter >/dev/null)
find "$tmp/dup" -type f -exec touch -d '2020-01-01 00:00:00' {} +
dup_deploy() {
  (cd "$tmp/dup/$1/dup-app" && "$SPARED_BIN" deploy >"$tmp/dup.out" 2>"$tmp/dup.err") || { cat "$tmp/dup.out" "$tmp/dup.err" >&2; fail "deploying dup-app from $1 failed"; }
}
dup_status() {
  curl -sS --max-time 30 --resolve "dup-app.localhost:$pub:127.0.0.1" -o /dev/null -w '%{http_code}' "http://dup-app.localhost:$pub$1"
}
for dir in a b a; do
  dup_deploy "$dir"
  if [ "$dir" = a ]; then want_ok=/api/notes; want_missing=/api/count; else want_ok=/api/count; want_missing=/api/notes; fi
  [ "$(dup_status "$want_ok")" = 200 ] && [ "$(dup_status "$want_missing")" = 404 ] || fail "after deploying $dir/dup-app the server runs the other directory's code ($want_ok should answer 200 and $want_missing 404)"
done
"$SPARED_BIN" rm dup-app --yes >/dev/null || fail "spared rm dup-app failed"

step "spared logs on an app that has written nothing"
"$SPARED_BIN" logs smoke-app >"$tmp/logs.out" 2>"$tmp/logs.err" || fail "spared logs exited nonzero"
[ ! -s "$tmp/logs.out" ] && grep -q 'no output yet' "$tmp/logs.err" || { cat "$tmp/logs.out" "$tmp/logs.err" >&2; fail "spared logs on an empty log did not say 'no output yet'"; }

step "POST /api/notes"
code=$(http -X POST -H 'content-type: application/json' -H 'x-api-key: E2EHEADERSECRET' -d '{"text":"hello from e2e"}' "$url/api/notes?token=E2EQUERYSECRET") || fail "POST request failed"
[ "$code" = 201 ] || { cat "$tmp/body" >&2; fail "POST answered $code, want 201"; }
python3 -I -c '
import json, sys
n = json.load(open(sys.argv[1]))
assert n["text"] == "hello from e2e", n
assert isinstance(n["id"], int), n
' "$tmp/body" || fail "POST body is not the created note"

step "GET /api/notes"
code=$(http "$url/api/notes") || fail "GET request failed"
[ "$code" = 200 ] || { cat "$tmp/body" >&2; fail "GET answered $code, want 200"; }
python3 -I -c '
import json, sys
notes = json.load(open(sys.argv[1]))
assert [n["text"] for n in notes] == ["hello from e2e"], notes
' "$tmp/body" || fail "GET body does not list the posted note"

step "GET /"
code=$(http "$url/") || fail "GET / failed"
[ "$code" = 200 ] || fail "GET / answered $code, want 200"

step "GET /_/smoke-app/api/notes (path form is off by default)"
path_code=$(curl -sS --max-time 30 -o "$tmp/body" -w '%{http_code}' "http://127.0.0.1:$pub/_/smoke-app/api/notes") || fail "path-form request failed"
[ "$path_code" = 404 ] && grep -q path_routing_off "$tmp/body" || { cat "$tmp/body" >&2; fail "path form answered $path_code, want 404 path_routing_off"; }

step "spared ps"
"$SPARED_BIN" ps >"$tmp/ps.out" || fail "spared ps exited nonzero"
grep -q 'smoke-app' "$tmp/ps.out" || { cat "$tmp/ps.out" >&2; fail "spared ps does not list smoke-app"; }
"$SPARED_BIN" --json ps >"$tmp/ps.json" || fail "spared --json ps exited nonzero"
python3 -I -c '
import json, sys
apps = json.load(open(sys.argv[1]))["apps"]
app = next(a for a in apps if a["name"] == "smoke-app")
assert app["requests"] >= 3, app
' "$tmp/ps.json" || fail "ps --json does not show smoke-app with at least 3 requests"

step "spared logs"
"$SPARED_BIN" logs smoke-app --lines 20 >/dev/null 2>&1 || fail "spared logs exited nonzero"

step "app tokens: a token for smoke-app redeploys it, a token for another app cannot"
"$SPARED_BIN" token create smoke-app --name e2e >"$tmp/apptoken" 2>"$tmp/apptoken.err" || { cat "$tmp/apptoken.err" >&2; fail "spared token create exited nonzero"; }
apptoken=$(cat "$tmp/apptoken")
case "$apptoken" in spt_*) ;; *) fail "spared token create did not print an spt_ token alone on stdout";; esac
SPARED_TOKEN="$apptoken" "$SPARED_BIN" deploy >"$tmp/appdeploy.out" 2>"$tmp/appdeploy.err" || { cat "$tmp/appdeploy.out" "$tmp/appdeploy.err" >&2; fail "spared deploy with an app token for the app failed"; }
grep -q '^deployed smoke-app' "$tmp/appdeploy.out" || { cat "$tmp/appdeploy.out" >&2; fail "spared deploy with an app token printed no deployed line"; }
SPARED_TOKEN="$apptoken" "$SPARED_BIN" ps | grep -q 'smoke-app' || fail "spared ps with the app token does not list its app"
"$SPARED_BIN" token create other-app >"$tmp/othertoken" 2>/dev/null || fail "spared token create other-app exited nonzero"
if SPARED_TOKEN=$(cat "$tmp/othertoken") "$SPARED_BIN" deploy >"$tmp/wrong.out" 2>"$tmp/wrong.err"; then fail "a token for other-app deployed smoke-app"; fi
grep -q '^error\[wrong_scope\]: ' "$tmp/wrong.err" && grep -q '^fix: .*spared token create smoke-app' "$tmp/wrong.err" || { cat "$tmp/wrong.err" >&2; fail "deploying with another app's token did not say wrong_scope with a fix naming the app"; }
for f in "$tmp/apptoken" "$tmp/othertoken"; do
  if grep -rqF "$(cat "$f")" "$tmp/data" "$tmp/serve.log" "$tmp/access.log" "$tmp/home"; then fail "an app token reached the data dir, the server's output or its home"; fi
done

step "access log"
sleep 0.3
python3 -I -c '
import json, sys
text = open(sys.argv[1]).read()
lines = [json.loads(l) for l in text.splitlines()]
fields = {"time", "listener", "app", "method", "path", "status", "duration_ms", "request_id"}
posts = [l for l in lines if l["method"] == "POST" and l["app"] == "smoke-app"]
assert len(posts) == 1, posts
p = posts[0]
assert set(p) == fields and p["path"] == "/api/notes" and p["status"] == 201 and p["listener"] == "public", p
assert any(l["path"] == "/healthz" and l["app"] is None for l in lines), "no /healthz line"
assert any(l["listener"] == "admin" for l in lines), "no admin line"
for secret in ("E2EQUERYSECRET", "E2EHEADERSECRET", "hello from e2e", sys.argv[2]):
    assert secret not in text, "the access log holds a query string, header, body or the token"
' "$tmp/access.log" "$SPARED_TOKEN" || fail "the access log is missing a line or holds something it must not"
if grep -q "$SPARED_TOKEN" "$tmp/serve.log"; then fail "the server printed the admin token"; fi

step "GET /metrics (admin listener, with the token)"
code=$(curl -sS --max-time 10 -o "$tmp/body" -w '%{http_code}' "http://127.0.0.1:$adm/metrics") || fail "metrics request failed"
[ "$code" = 401 ] || fail "/metrics without the token answered $code, want 401"
code=$(curl -sS --max-time 10 -o "$tmp/body" -w '%{http_code}' "http://127.0.0.1:$pub/metrics") || fail "public metrics request failed"
[ "$code" = 404 ] || fail "/metrics on the public listener answered $code, want 404"
code=$(curl -sS --max-time 10 -H "Authorization: Bearer $SPARED_TOKEN" -o "$tmp/body" -w '%{http_code}' "http://127.0.0.1:$adm/metrics") || fail "metrics request failed"
[ "$code" = 200 ] || { cat "$tmp/body" >&2; fail "/metrics with the token answered $code, want 200"; }
grep -q '^spared_requests_total{listener="public",status="2xx"} [1-9]' "$tmp/body" && grep -q '^spared_wakes_total [1-9]' "$tmp/body" && grep -q '^spared_apps{state="deployed"} 1$' "$tmp/body" || { cat "$tmp/body" >&2; fail "/metrics lacks the request, wake or app counters"; }
if grep -q 'smoke-app' "$tmp/body"; then fail "/metrics names an app"; fi

step "restart on the same data dir: the app and its data are still there"
kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true
start_server
grep -q ', 1 apps loaded' "$tmp/serve.log" || fail "the restarted server did not report 1 app loaded"
code=$(http "$url/api/notes") || fail "GET after restart failed"
[ "$code" = 200 ] && grep -q 'hello from e2e' "$tmp/body" || { cat "$tmp/body" >&2; fail "the note did not survive a restart"; }

step "a second server is refused: same data dir (data_dir_in_use), same ports (address_in_use)"
if HOME="$tmp/home" "$SPARED_BIN" serve --data-dir "$tmp/data" --listen "127.0.0.1:$(free_port)" --admin-listen "127.0.0.1:$(free_port)" >"$tmp/second.out" 2>&1; then fail "a second server started on a data dir that is in use"; fi
grep -q '^error\[data_dir_in_use\]: ' "$tmp/second.out" && grep -q '^fix: ' "$tmp/second.out" && grep -q "process $server_pid" "$tmp/second.out" || { cat "$tmp/second.out" >&2; fail "the refusal does not name data_dir_in_use, the holder and a fix"; }
if HOME="$tmp/home" "$SPARED_BIN" serve --data-dir "$tmp/data-other" --listen "127.0.0.1:$pub" --admin-listen "127.0.0.1:$(free_port)" >"$tmp/second.out" 2>&1; then fail "a second server started on a port that is in use"; fi
grep -q "^error\[address_in_use\]: 127.0.0.1:$pub is already in use" "$tmp/second.out" && grep -q '^fix: ' "$tmp/second.out" || { cat "$tmp/second.out" >&2; fail "the refusal does not name the address and a fix"; }
[ ! -e "$tmp/data-other" ] || fail "a server that could not bind left a data dir behind"
code=$(http "$url/api/notes") || fail "GET after the refusals failed"
[ "$code" = 200 ] || fail "the first server stopped answering after the refusals ($code)"

step "a data dir from a newer spared is refused with data_dir_too_new"
kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true
server_pid=
cp "$tmp/data/format.json" "$tmp/format.json.good"
printf '{"format":99,"created_by":"9.0.0","last_opened_by":"9.0.0"}\n' >"$tmp/data/format.json"
if HOME="$tmp/home" "$SPARED_BIN" serve --data-dir "$tmp/data" --listen "127.0.0.1:$pub" --admin-listen "127.0.0.1:$adm" >"$tmp/refused.out" 2>&1; then fail "a server started on a data dir with format 99"; fi
grep -q 'data_dir_too_new' "$tmp/refused.out" && grep -q '^fix: ' "$tmp/refused.out" || { cat "$tmp/refused.out" >&2; fail "the refusal does not name data_dir_too_new and a fix"; }
cp "$tmp/format.json.good" "$tmp/data/format.json"
start_server

step "spared rm"
if "$SPARED_BIN" --json rm smoke-app >"$tmp/rm-no-yes.out" 2>&1; then fail "spared rm without --yes succeeded"; fi
if ! grep -q '"kind":"confirm_required"' "$tmp/rm-no-yes.out" || ! grep -q -- '--yes' "$tmp/rm-no-yes.out"; then
  cat "$tmp/rm-no-yes.out" >&2
  fail "rm without --yes did not return confirm_required with a fix"
fi
"$SPARED_BIN" ps | grep -q 'smoke-app' || fail "rm without --yes deleted the app"
"$SPARED_BIN" rm smoke-app --yes >/dev/null || fail "spared rm exited nonzero"
"$SPARED_BIN" ps | grep -q 'smoke-app' && fail "smoke-app still listed after rm"

step "spared ps without a token names the fix"
if SPARED_TOKEN='' HOME="$tmp/home-empty" "$SPARED_BIN" ps >"$tmp/notoken.out" 2>&1; then fail "ps without a token succeeded"; fi
grep -q 'SPARED_TOKEN' "$tmp/notoken.out" || { cat "$tmp/notoken.out" >&2; fail "no-token error does not name SPARED_TOKEN"; }

if [ "${SKIP_SERVER_TEST:-0}" != 1 ]; then
  step "scripts/server-test.py"
  SPARED_BIN="$SPARED_BIN" python3 -I "$root/scripts/server-test.py" || fail "server-test.py failed"
fi

exit 0
