#!/usr/bin/env bash
# One pass D measurement. Env: T=spared|workerd BIN=before|after|wakeonly NA PROTO WAIT OUT=json-path EXTRA="server flags"
# PROTO restart: sync, drop_caches, start the server, WAIT (finished = until the prefetch line is in the log, none = no wait, N = seconds),
#                serve app 0, then one GET to each of apps 1..NA-1 (the representative cold case: host restarted or cache lost, then traffic)
# PROTO b5:      start, serve app 0, sync, drop_caches, then the same GETs (pass B/C protocol; the cache is dropped after the start)
# PROTO warm:    every file pre-read, start, serve app 0, then the same GETs
# CPUS: cpu list for the server (default 0). Load generator: python, not pinned.
source "$(dirname "$0")/c-lib.sh"
export SPARED_ACCESS_LOG=off
: "${T:?}" "${NA:?}" "${PROTO:?}" "${OUT:?}"; BIN=${BIN:-after}; WAIT=${WAIT:-none}; EXTRA=${EXTRA:-}; CPUS=${CPUS:-0}
D=$ROOT/bench/work/d; mkdir -p "$D"
SNAP=$ROOT/bench/passB-scripts/d-snap.py; READY=$ROOT/bench/passB-scripts/d-ready.py
PID=""
cleanup() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; }
trap cleanup EXIT
ready_ms=-1; seen_ms=-1
if [ $T = spared ]; then
  SEED=$C/seed-n$NA-u$NA; RUN=$D/run; ROOTD=$RUN; BINF=$D/bin/spared-$BIN
  spared_seed "$SEED" $NA $NA
  rm -rf "$RUN"; cp -al "$SEED" "$RUN"
  for a in "$RUN"/apps/*/; do rm -rf "$a/data"; cp -a "$SEED/apps/$(basename $a)/data" "$a/data"; done
  start() {
    rm -f "$D/start.ts" "$D/start.ts.tmp"
    python3 "$READY" "$D/start.ts" 8081 >"$D/ready.out" & local rp=$!
    date +%s%N >"$D/start.ts.tmp"; mv "$D/start.ts.tmp" "$D/start.ts"
    taskset -c $CPUS "$BINF" serve --data-dir "$RUN" --idle-secs 600 $EXTRA >"$D/spared.log" 2>&1 & PID=$!
    wait $rp; ready_ms=$(cat "$D/ready.out")
    T_START=$(date +%s%N)
  }
else
  workerd_seed $NA
  ROOTD=$C/wd-data-$NA; rm -rf "$ROOTD"; mkdir -p "$ROOTD"; cp -a "$C/wd-seed-$NA/." "$ROOTD/"; rm -f "$ROOTD/.seeded"
  start() { local t0; t0=$(date +%s%N)
    (cd "$C/wd-cfg-$NA" && exec taskset -c $CPUS "$WORKERD" serve config.capnp) >"$D/workerd.log" 2>&1 & PID=$!
    wait_tcp 9100; wait_tcp $((9100+NA-1)); ready_ms=$(( ($(date +%s%N) - t0) / 1000000 )).0; sleep 1; T_START=$(date +%s%N); }
fi
dropc() { sync; echo 3 >/proc/sys/vm/drop_caches; sleep 1; }
SN0="{}"; SN1="{}"; RES0=""; RES1=""
case $PROTO in
  restart)
    dropc
    start
    SN0=$(python3 "$SNAP" $PID)
    case $WAIT in
      finished) for _ in $(seq 600); do grep -q '^prefetch:' "$D/spared.log" && break; sleep 0.1; done
                seen_ms=$(( ($(date +%s%N) - T_START) / 1000000 )); sleep 0.5 ;;
      none) ;;
      *) sleep $WAIT ;;
    esac
    SN1=$(python3 "$SNAP" $PID)
    python3 "$FETCH" $T 0 0 /dev/null
    [ $WAIT != none ] && { sleep 1; RES1=$(python3 "$PC" resident "$ROOTD" "*.cwasm" 2>/dev/null | head -1); }
    ;;
  b5)
    start
    python3 "$FETCH" $T 0 0 /dev/null
    SN0=$(python3 "$SNAP" $PID); SN1=$SN0
    dropc
    RES1=$(python3 "$PC" resident "$ROOTD" "*.cwasm" 2>/dev/null | head -1)
    ;;
  warm)
    find "$ROOTD" -type f -print0 | xargs -0 cat >/dev/null 2>&1
    cat "$D/bin/spared-$BIN" "$WORKERD" >/dev/null
    start
    SN0=$(python3 "$SNAP" $PID); sleep 2; SN1=$(python3 "$SNAP" $PID)
    python3 "$FETCH" $T 0 0 /dev/null
    sleep 1
    ;;
  *) echo "bad PROTO $PROTO" >&2; exit 2 ;;
esac
if [ $T = spared ]; then python3 "$ROOT/bench/passB-scripts/c-fetch-faults.py" $T 1 $((NA-1)) "$OUT" $PID; else python3 "$FETCH" $T 1 $((NA-1)) "$OUT"; fi
SN2=$( [ $T = spared ] && python3 "$SNAP" $PID || echo '{}')
PBYTES=""
[ $T = spared ] && PBYTES=$(curl -s -H "Authorization: Bearer $(cat "$RUN/admin-token")" http://127.0.0.1:8081/metrics | awk '$1=="spared_prefetch_bytes_total"{print $2}')
PLINE=$(grep '^prefetch:' "$D/spared.log" 2>/dev/null | head -1)
python3 - "$OUT" "$T" "$BIN" "$NA" "$PROTO" "$WAIT" "$EXTRA" "$ready_ms" "$seen_ms" "$SN0" "$SN1" "$SN2" "$RES1" "$PBYTES" "$PLINE" "$CPUS" <<'PY'
import json, sys
o, t, b, na, pr, w, ex, rdy, seen, s0, s1, s2, res1, pb, pl, cpus = sys.argv[1:]
j = json.load(open(o))
j["meta"] = dict(target=t, bin=b, na=int(na), proto=pr, wait=w, extra=ex, cpus=cpus, ready_ms=float(rdy), prefetch_line_seen_ms_after_ready=int(seen),
                 snap_at_ready=json.loads(s0), snap_before_requests=json.loads(s1), snap_after_requests=json.loads(s2), cwasm_resident_before_measure=res1,
                 prefetch_bytes_total=(int(pb) if pb else None), prefetch_line=pl)
json.dump(j, open(o, "w"))
PY
cleanup
[ $T = spared ] && rm -rf "$RUN"
exit 0
