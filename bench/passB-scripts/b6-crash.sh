#!/usr/bin/env bash
# B6: (1) the existing crash test (bench/crash-test.sh: run-probe, kill -9 during writes, WAL and TRUNCATE), 100 trials each;
# (2) the same idea against `spared serve` over HTTP (b6-crash-serve.py): 100 trials with 1 writer and 100 trials with 16 writers.
source "$(dirname "$0")/lib.sh"
source bench/env.sh
export SPARED_ACCESS_LOG=off
wait_quiet || true; env_snap crash
seg_begin b6 existing-crash-test
for j in WAL TRUNCATE; do JOURNAL=$j OUT=$R/crash-test-passB-$(echo $j | tr A-Z a-z).json bench/crash-test.sh 100 2>&1 | tail -3 >>"$R/crash-test-passB.log"; done
seg_end b6 existing-crash-test
B=$ROOT/bench/work/b6; mkdir -p "$B/bin"; cp -f "$ROOT/target/release/spared" "$B/bin/spared"
rm -rf "$B/tpl"; mkdir -p "$B/tpl"
"$B/bin/spared" serve --data-dir "$B/tpl" --listen 127.0.0.1:8080 --admin-listen 127.0.0.1:8081 --idle-secs 600 >"$B/deploy.log" 2>&1 & p=$!
for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
M=$(printf 'name = "demo-0"\n[grants]\ndata = true\n' | base64 -w0)
curl -fsS -X PUT -H "authorization: Bearer $(cat "$B/tpl/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$DEMO_WASM" http://127.0.0.1:8081/v1/apps/demo-0 >/dev/null
kill $p; wait $p 2>/dev/null || true
for w in 1 16; do
  wait_quiet || true
  seg_begin b6 "serve-w$w"
  python3 bench/passB-scripts/b6-crash-serve.py "$B/bin/spared" "$B/tpl" 100 $w "$R/crash-serve-passB-w$w.json" 2>>"$R/crash-serve-passB.log" | tee -a "$R/crash-serve-passB.log"
  seg_end b6 "serve-w$w"
done
echo B6-DONE
