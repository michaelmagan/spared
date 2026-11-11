#!/usr/bin/env bash
# Pass C driver: SUITE=name ROUNDS=5 SPEC=file ; scenarios one per line: name|target|NA|UNIQ|SEEDED|DROP|WARM. Order rotates per round.
# Raw: bench/results/SUITE-passC-NAME-TARGET-runR.json, steal in SUITE-passC-steal.jsonl.
source "$(dirname "$0")/c-lib.sh"
SUITE=${SUITE:?}; ROUNDS=${ROUNDS:-5}; SPEC=${SPEC:?}
cp -f "$ROOT/target/release/spared" "$C/bin/spared"
mapfile -t SC < <(grep -v '^#' "$SPEC" | grep .)
n=${#SC[@]}
for r in $(seq 1 $ROUNDS); do
  for i in $(seq 0 $((n-1))); do
    line=${SC[$(( (i + (r-1) * 3) % n ))]}
    IFS='|' read -r name target na uniq seeded drop warm ra faults <<<"$line"
    out=$R/$SUITE-passC-$name-$target-run$r.json
    [ -f "$out" ] && continue
    wait_quiet || true
    echo "$(date +%T) $SUITE r$r $name $target"
    c_seg_begin
    T=$target NA=$na UNIQ=$uniq SEEDED=$seeded DROP=$drop WARM=$warm RA=$ra FAULTS=$faults OUT=$out.tmp "$(dirname "$0")/c-wake.sh" && mv "$out.tmp" "$out" || echo "FAILED $name"
    c_seg_end $SUITE "r$r-$name-$target"
  done
done
echo "$SUITE-DONE"
