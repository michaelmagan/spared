#!/usr/bin/env bash
# One cold-wake measurement. Env: T=spared|workerd NA UNIQ SEEDED=1|0 DROP WARM=1|0 OUT=json-path
# DROP: none (everything pre-read into cache) | all (sync; echo 3 > drop_caches) | cwasm | db | cwasmdb | wasm (fadvise DONTNEED on those files only)
#       | allbutcwasm (drop_caches then re-read cwasm files) | allbutdb (drop_caches then re-read the databases)
# WARM=1: serve app 0 before the drop (runtime instantiated); WARM=0: the first measured request is also the runtime's first.
# Measured: one GET per app 1..NA-1, sequential. spared server pinned to CPU 0 (loadgen CPU 2 not pinned separately; python on the other cores).
source "$(dirname "$0")/c-lib.sh"
export SPARED_ACCESS_LOG=off
: "${T:?}" "${NA:?}" "${UNIQ:=$NA}" "${SEEDED:=1}" "${DROP:?}" "${WARM:=1}" "${OUT:?}"
PID=""
RAF=/sys/block/vda/queue/read_ahead_kb; RA0=$(cat $RAF)
cleanup() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; echo $RA0 >$RAF; }
trap cleanup EXIT
if [ $T = spared ]; then
  SEED=$C/seed-n$NA-u$UNIQ; RUN=$C/run; ROOTD=$RUN
  spared_seed "$SEED" $NA $UNIQ
  rm -rf "$RUN"; cp -al "$SEED" "$RUN"
  if [ "$SEEDED" = 1 ]; then # databases are real copies so the pristine seed never changes
    for a in "$RUN"/apps/*/; do rm -rf "$a/data"; cp -a "$SEED/apps/$(basename $a)/data" "$a/data"; done
  else rm -rf "$RUN"/apps/*/data; fi
  CWG="*.cwasm"; DBG="app.db*"; WG="*.wasm"
  start() { taskset -c 0 "$C/bin/spared" serve --data-dir "$RUN" --idle-secs 600 >"$C/spared.log" 2>&1 & PID=$!
            for _ in $(seq 200); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done; }
else
  workerd_seed $NA
  ROOTD=$C/wd-data-$NA; rm -rf "$ROOTD"; mkdir -p "$ROOTD"
  if [ "$SEEDED" = 1 ]; then cp -a "$C/wd-seed-$NA/." "$ROOTD/"; rm -f "$ROOTD/.seeded"; fi
  CWG="__none__"; DBG="*"; WG="__none__"
  start() { (cd "$C/wd-cfg-$NA" && exec taskset -c 0 "$WORKERD" serve config.capnp) >"$C/workerd.log" 2>&1 & PID=$!
            wait_tcp 9100; wait_tcp $((9100+NA-1)); sleep 1; }
fi
if [ $DROP = none ]; then # warm everything the run could touch
  find "$ROOTD" -type f -print0 | xargs -0 cat >/dev/null 2>&1
  cat "$C/bin/spared" "$WORKERD" >/dev/null
fi
[ -n "${RA:-}" ] && echo $RA >$RAF   # read_ahead_kb for this run (restored by cleanup to the value at start)
start
[ "$WARM" = 1 ] && python3 "$FETCH" $T 0 0 /dev/null
sync
RES0=$(python3 "$PC" resident "$ROOTD" "$CWG" 2>/dev/null | head -1)
case $DROP in
  none) ;;
  all) echo 3 >/proc/sys/vm/drop_caches ;;
  cwasm) python3 "$PC" evict "$ROOTD" "$CWG" >/dev/null ;;
  db) python3 "$PC" evict "$ROOTD" "$DBG" '*-journal' '*-wal' '*-shm' >/dev/null ;;
  cwasmdb) python3 "$PC" evict "$ROOTD" "$CWG" "$DBG" >/dev/null ;;
  wasm) python3 "$PC" evict "$ROOTD" "$WG" >/dev/null ;;
  allbutcwasm) echo 3 >/proc/sys/vm/drop_caches; find "$ROOTD" -name '*.cwasm' -print0 | xargs -0 cat >/dev/null ;;
  allbutdb) echo 3 >/proc/sys/vm/drop_caches; find "$ROOTD" -type f \( -name 'app.db*' \) -print0 | xargs -0 cat >/dev/null ;;
  *) echo "bad DROP $DROP" >&2; exit 2 ;;
esac
sleep 1
RES1=$(python3 "$PC" resident "$ROOTD" "$CWG" 2>/dev/null | head -1)
if [ "${FAULTS:-0}" = 1 ]; then python3 "$ROOT/bench/passB-scripts/c-fetch-faults.py" $T 1 $((NA-1)) "$OUT" $PID; else python3 "$FETCH" $T 1 $((NA-1)) "$OUT"; fi
python3 - "$OUT" "$T" "$NA" "$UNIQ" "$SEEDED" "$DROP" "$WARM" "$RES0" "$RES1" "${RA:-$RA0}" <<'PY'
import json, sys
o, t, na, u, s, d, w, r0, r1, ra = sys.argv[1:]
j = json.load(open(o)); j["meta"] = dict(target=t, na=int(na), uniq=int(u), seeded=int(s), drop=d, warm=int(w), cwasm_resident_before_drop=r0, cwasm_resident_before_measure=r1, read_ahead_kb=ra)
json.dump(j, open(o, "w"))
PY
cleanup
[ $T = spared ] && rm -rf "$RUN"
exit 0
