#!/usr/bin/env bash
# Pass D driver: SUITE=name ROUNDS=5 SPEC=file ; one scenario per line: name|target|bin|NA|proto|wait|cpus|extra flags. Order rotates per round.
# Raw: bench/results/SUITE-passD-NAME-TARGET-runR.json, steal in SUITE-passD-steal.jsonl.
source "$(dirname "$0")/c-lib.sh"
source "$(dirname "$0")/d-lib.sh"
SUITE=${SUITE:?}; ROUNDS=${ROUNDS:-5}; SPEC=${SPEC:?}
mkdir -p "$ROOT/bench/work/d/bin"
mapfile -t SC < <(grep -v '^#' "$SPEC" | grep .)
n=${#SC[@]}
for r in $(seq 1 $ROUNDS); do
  for i in $(seq 0 $((n-1))); do
    line=${SC[$(( (i + (r-1) * 5) % n ))]}
    IFS='|' read -r name target bin na proto wait cpus extra <<<"$line"
    out=$R/$SUITE-passD-$name-$target-run$r.json
    [ -f "$out" ] && continue
    wait_quiet || true
    echo "$(date +%T) $SUITE r$r $name $target"
    c_seg_begin
    T=$target BIN=$bin NA=$na PROTO=$proto WAIT=$wait CPUS=${cpus:-0} EXTRA="$extra" OUT=$out.tmp "$(dirname "$0")/d-wake.sh" && mv "$out.tmp" "$out" || echo "FAILED $name"
    d_seg_end $SUITE "r$r-$name-$target"
  done
done
echo "$SUITE-DONE"
