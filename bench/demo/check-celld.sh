#!/usr/bin/env bash
# Correctness check for the celld demo target (:9292), same assertions as check.sh plus a restart durability check.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$ROOT/bench/work"
FRESH=1 "$ROOT/bench/demo/serve-celld.sh" >"$ROOT/bench/work/celld-check.log" 2>&1 &
SP=$!
stop() { pkill -f 'celld dev --port 9292' 2>/dev/null; kill $SP 2>/dev/null; wait $SP 2>/dev/null || true; }
trap stop EXIT
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }
B=http://127.0.0.1:9292
get() { curl -s -m 20 -w '\n%{http_code}' "$B$1"; }
post() { curl -s -m 20 -w '\n%{http_code}' -H 'content-type: application/json' -d "$1" "$B/api/items"; }
wait_up() { for i in $(seq 300); do [ "$(curl -s -o /dev/null -w '%{http_code}' $B/ 2>/dev/null)" = 200 ] && return; sleep 0.2; done; }
wait_up

MARK="check-$$-$(date +%s)"
t=celld
out=$(get /); code=${out##*$'\n'}; body=${out%$'\n'*}
[ "$code" = 200 ] && ok "$t GET / 200" || bad "$t GET / -> $code"
grep -q '<h1>Demo List</h1>' <<<"$body" && ok "$t heading" || bad "$t heading missing"
[ "$(grep -o '<li>' <<<"$body" | wc -l)" = 20 ] && ok "$t 20 items" || bad "$t item count"
grep -q 'Seed item 100' <<<"$body" && ok "$t seeded" || bad "$t not seeded"
out=$(post "{\"text\":\"$MARK <b>&\"}"); code=${out##*$'\n'}
[ "$code" = 201 ] && ok "$t POST 201" || bad "$t POST -> $code"
out=$(post 'nope'); code=${out##*$'\n'}
[ "$code" = 400 ] && ok "$t POST bad json 400" || bad "$t bad json -> $code"
out=$(get /api/items); body=${out%$'\n'*}
grep -q "$MARK" <<<"$body" && ok "$t GET /api/items has new item" || bad "$t api missing item"
out=$(get /); body=${out%$'\n'*}
grep -q "$MARK &lt;b&gt;&amp;" <<<"$body" && ok "$t page shows escaped new item" || bad "$t page missing item"
out=$(get "/?id=other"); body=${out%$'\n'*}
grep -q "$MARK" <<<"$body" && bad "$t ?id=other shares state" || ok "$t ?id=other is a separate object"
grep -q 'Seed item 100' <<<"$body" && ok "$t ?id=other seeded" || bad "$t ?id=other not seeded"

# durability: kill -9, restart on the same state, the marked item is still there
pkill -9 -f 'celld dev --port 9292'; kill -9 $SP 2>/dev/null; wait $SP 2>/dev/null || true
"$ROOT/bench/demo/serve-celld.sh" >>"$ROOT/bench/work/celld-check.log" 2>&1 &
SP=$!
wait_up
out=$(get /api/items); body=${out%$'\n'*}
grep -q "$MARK" <<<"$body" && ok "$t item survives kill -9 and restart" || bad "$t lost item after kill -9"
[ "$(grep -o '"id"' <<<"$body" | wc -l)" = 20 ] && ok "$t no reseed after restart" || bad "$t item count after restart"
exit $fail
