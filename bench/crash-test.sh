#!/usr/bin/env bash
# Crash test: kill -9 the host mid-write; every row acknowledged on stdout must survive.
# Needs the lab binary (built on first use, see bench/env.sh).
# Usage: [JOURNAL=WAL|TRUNCATE] [OUT=bench/results/<name>.json] bench/crash-test.sh [trials]
set -u
cd "$(dirname "$0")/.."
source bench/env.sh
lab_build
BIN=$SPARED_LAB_BIN
WASM=bench/work/probe.cwasm
TRIALS=${1:-100}
JOURNAL=${JOURNAL:-WAL}
if [ -n "${OUT:-}" ]; then :; elif [ "$JOURNAL" = WAL ]; then OUT=bench/results/crash-test.json; else OUT=bench/results/crash-test-$(echo "$JOURNAL" | tr A-Z a-z).json; fi
mkdir -p bench/work bench/results; $BIN compile probes/sqlite-probe/target/wasm32-wasip2/release/sqlite-probe.wasm $WASM
lost=0; bad=0; acked_total=0
for t in $(seq 1 "$TRIALS"); do
  d=bench/work/crash-$t; rm -rf "$d"; mkdir -p "$d"
  $BIN run-probe $WASM --data "$d" -- writes 100000 $JOURNAL > "$d.out" 2>/dev/null &
  pid=$!
  sleep "0.$(printf "%03d" $((RANDOM % 700 + 50)))"
  kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null
  acked=$(grep -c '^C ' "$d.out")
  res=$($BIN run-probe $WASM --data "$d" -- verify 0 $JOURNAL 2>&1 | grep '^V')
  ic=$(echo "$res" | awk '{print $2}'); cnt=$(echo "$res" | awk '{print $3}')
  acked_total=$((acked_total+acked))
  fail=0
  [ "$ic" != "ok" ] && bad=$((bad+1)) && fail=1
  [ "${cnt:-0}" -lt "$acked" ] && lost=$((lost+1)) && fail=1
  if [ $fail = 1 ]; then echo "trial $t acked=$acked verify='$res'" >&2; ls -la "$d" >&2; else rm -rf "$d" "$d.out"; fi
done
printf '{"journal_mode":"%s","trials":%d,"integrity_failures":%d,"trials_with_lost_commits":%d,"acked_commits_total":%d}\n' "$JOURNAL" "$TRIALS" "$bad" "$lost" "$acked_total" | tee $OUT
