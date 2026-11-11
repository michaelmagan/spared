#!/usr/bin/env bash
# Correctness check for the three demo targets (workerd, spared, spin). Starts serve-both.sh and serve-spin.sh with fresh data, stops them afterwards.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
FRESH=1 "$ROOT/bench/demo/serve-both.sh" "${IDLE:-30}" >"$ROOT/bench/work/demo-check.log" 2>&1 &
SP=$!
FRESH=1 "$ROOT/bench/demo/serve-spin.sh" &
SPN=$!
stop_spin() { kill $SPN 2>/dev/null; pkill -f 'spin trigger http --listen 127.0.0.1:9191' 2>/dev/null; pkill -f 'spin up --listen 127.0.0.1:9191' 2>/dev/null; wait $SPN 2>/dev/null || true; }
trap 'kill $SP 2>/dev/null; wait $SP 2>/dev/null || true; stop_spin' EXIT
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }

get() { # target path
  case $1 in
    workerd) curl -s -m 20 -w '\n%{http_code}' "http://127.0.0.1:9090$2" ;;
    spin) curl -s -m 20 -w '\n%{http_code}' "http://127.0.0.1:9191$2" ;;
    spared) curl -s -m 20 -w '\n%{http_code}' -H 'Host: demo.localhost' "http://127.0.0.1:8080$2" ;;
  esac
}
post() {
  case $1 in
    workerd) curl -s -m 20 -w '\n%{http_code}' -H 'content-type: application/json' -d "$2" http://127.0.0.1:9090/api/items ;;
    spin) curl -s -m 20 -w '\n%{http_code}' -H 'content-type: application/json' -d "$2" http://127.0.0.1:9191/api/items ;;
    spared) curl -s -m 20 -w '\n%{http_code}' -H 'content-type: application/json' -H 'Host: demo.localhost' -d "$2" http://127.0.0.1:8080/api/items ;;
  esac
}

for i in $(seq 100); do
  curl -s -o /dev/null http://127.0.0.1:9090/ 2>/dev/null && break; sleep 0.1
done
for i in $(seq 100); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: demo.localhost' http://127.0.0.1:8080/ 2>/dev/null)" = 200 ] && break; sleep 0.2
done

for i in $(seq 100); do
  curl -s -o /dev/null http://127.0.0.1:9191/ 2>/dev/null && break; sleep 0.1
done

MARK="check-$$-$(date +%s)"
for t in workerd spared spin; do
  out=$(get $t /); code=${out##*$'\n'}; body=${out%$'\n'*}
  [ "$code" = 200 ] && ok "$t GET / 200" || bad "$t GET / -> $code"
  grep -q '<h1>Demo List</h1>' <<<"$body" && ok "$t heading" || bad "$t heading missing"
  [ "$(grep -o '<li>' <<<"$body" | wc -l)" = 20 ] && ok "$t 20 items" || bad "$t item count"
  grep -q 'Seed item 100' <<<"$body" && ok "$t seeded" || bad "$t not seeded"
  out=$(post $t "{\"text\":\"$MARK <b>&\"}"); code=${out##*$'\n'}
  [ "$code" = 201 ] && ok "$t POST 201" || bad "$t POST -> $code"
  out=$(get $t /api/items); body=${out%$'\n'*}
  grep -q "$MARK" <<<"$body" && ok "$t GET /api/items has new item" || bad "$t api missing item"
  out=$(get $t /); body=${out%$'\n'*}
  grep -q "$MARK &lt;b&gt;&amp;" <<<"$body" && ok "$t page shows escaped new item" || bad "$t page missing item"
done

# same HTML modulo item list
strip() { sed '/^<li>/d'; }
a=$(get workerd / | sed '$d' | strip | md5sum); b=$(get spared / | sed '$d' | strip | md5sum); c=$(get spin / | sed '$d' | strip | md5sum)
[ "$a" = "$b" ] && ok "HTML identical modulo items (workerd, spared)" || bad "HTML differs outside items (workerd, spared)"
[ "$a" = "$c" ] && ok "HTML identical modulo items (workerd, spin)" || bad "HTML differs outside items (workerd, spin)"

# durability: kill -9 spin, restart on the same data, the marked item is still there
pkill -9 -f 'spin trigger http --listen 127.0.0.1:9191'; kill -9 $SPN 2>/dev/null; wait $SPN 2>/dev/null || true
"$ROOT/bench/demo/serve-spin.sh" &
SPN=$!
for i in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:9191/ 2>/dev/null && break; sleep 0.1; done
out=$(get spin /api/items); body=${out%$'\n'*}
grep -q "$MARK" <<<"$body" && ok "spin item survives kill -9 and restart" || bad "spin lost item after kill -9"
[ "$(grep -o '"id"' <<<"$body" | wc -l)" = 20 ] && ok "spin no reseed after restart" || bad "spin item count after restart"
n=$(python3 -c "import sqlite3;print(sqlite3.connect('$ROOT/bench/work/spin-data/spin.db').execute('select count(*) from items').fetchone()[0])")
[ "$n" = 101 ] && ok "spin db has 101 rows (100 seed + 1)" || bad "spin db rows $n"
exit $fail
