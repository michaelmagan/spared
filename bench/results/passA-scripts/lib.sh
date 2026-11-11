# Shared helpers for the pass-A drivers. Source this file.
export WASI_SDK_PATH=/opt/wasi-sdk-dl/wasi-sdk-25.0-x86_64-linux
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
R=$ROOT/bench/results
cd "$ROOT"
steal_snap() { awk '/^cpu /{s=0; for(i=2;i<=9;i++) s+=$i; print $9, s}' /proc/stat; }  # "steal_jiffies total_jiffies" (USER_HZ ticks, all 4 vCPUs)
leftovers() { pkill -x spared 2>/dev/null; pkill -x workerd 2>/dev/null; pkill -x loadgen 2>/dev/null; pkill -x wl-loadgen 2>/dev/null; pkill -x demo-native 2>/dev/null; pkill -f 'demo-distinct' 2>/dev/null; pkill -f mem-curve.py 2>/dev/null; true; }
wait_quiet() { # kill stray processes, wait for 1-min load < 0.3 (up to 40 min)
  leftovers; sleep 1
  for _ in $(seq 480); do awk '{exit !($1 < 0.3)}' /proc/loadavg && return 0; sleep 5; done
  echo "NOT QUIET after 40 min: $(cat /proc/loadavg)" >&2; return 1
}
# seg_begin SUITE LABEL ; seg_end SUITE LABEL  -> appends to bench/results/SUITE-passA-steal.jsonl
seg_begin() { read -r S0 T0 <<<"$(steal_snap)"; L0=$(cut -d' ' -f1 /proc/loadavg); D0=$(date -u +%FT%TZ); }
seg_end() {
  read -r S1 T1 <<<"$(steal_snap)"
  python3 - "$1" "$2" "$S0" "$T0" "$S1" "$T1" "$L0" "$D0" "$(date -u +%FT%TZ)" >>"$R/$1-passA-steal.jsonl" <<'PY'
import json, sys
suite, label, s0, t0, s1, t1, l0, d0, d1 = sys.argv[1:]
ds, dt = int(s1) - int(s0), int(t1) - int(t0)
print(json.dumps({"suite": suite, "segment": label, "start": d0, "end": d1, "loadavg1_at_start": float(l0),
                  "steal_jiffies_before": int(s0), "steal_jiffies_after": int(s1), "steal_delta": ds, "all_cpu_jiffies_delta": dt,
                  "steal_pct": round(100 * ds / dt, 3) if dt else None}))
PY
}
