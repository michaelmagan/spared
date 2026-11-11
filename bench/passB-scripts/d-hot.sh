#!/usr/bin/env bash
# Pass D: what the start-up pass costs a request that is already hot. 500 seeded apps, page cache dropped, server started, app 0 served once, then
# 3,000 sequential warm requests to app 0 (about the length of the pass), alone on the server. before = old binary, off = new binary with --prefetch-mb 0,
# after = new binary, 4096 MB (the pass reads all 500 components while the requests run), after-default = new binary, 1024 MB.
# CPU sets: 0 (server on one CPU, the pass shares it) and 0,1. Load generator CPU 2. 5 rounds, order rotates.
source "$(dirname "$0")/c-lib.sh"
source "$(dirname "$0")/d-lib.sh"
export SPARED_ACCESS_LOG=off
D=$ROOT/bench/work/d; SEED=$C/seed-n500-u500; RUN=$D/hotrun
spared_seed "$SEED" 500 500
PID=""
stop() { [ -n "$PID" ] && { kill $PID 2>/dev/null; wait $PID 2>/dev/null; }; PID=""; sleep 1; }
SC=("before|before|0|" "off|after|0|--prefetch-mb 0" "after|after|0|--prefetch-mb 4096" "afterdefault|after|0|" "before|before|0,1|" "after|after|0,1|--prefetch-mb 4096")
n=${#SC[@]}
for r in 1 2 3 4 5; do
  for i in $(seq 0 $((n-1))); do
    IFS='|' read -r name bin cpus extra <<<"${SC[$(( (i + (r-1) * 2) % n ))]}"
    tag=${name}-cpu${cpus//,/}
    out=$R/d-passD-hot-$tag-run$r.json
    [ -f "$out" ] && continue
    wait_quiet || true
    echo "$(date +%T) hot r$r $tag"
    seg_begin
    rm -rf "$RUN"; cp -al "$SEED" "$RUN"
    for a in "$RUN"/apps/*/; do rm -rf "$a/data"; cp -a "$SEED/apps/$(basename $a)/data" "$a/data"; done
    sync; echo 3 >/proc/sys/vm/drop_caches; sleep 1
    taskset -c $cpus "$D/bin/spared-$bin" serve --data-dir "$RUN" --idle-secs 600 $extra >"$D/spared-hot.log" 2>&1 & PID=$!
    for _ in $(seq 2000); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.005; done
    python3 "$FETCH" spared 0 0 /dev/null
    taskset -c 2 "$LG" --style spared --mode warm --app 0 --conc 1 --requests 3000 --out "$out.tmp" && mv "$out.tmp" "$out"
    sleep 3
    grep '^prefetch:' "$D/spared-hot.log" >"$out.prefetch" || true
    stop; rm -rf "$RUN"
    d_seg_end d "hot-r$r-$tag"
  done
done
echo D-HOT-DONE
