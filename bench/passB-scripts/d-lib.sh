# Pass D helpers. Source after c-lib.sh.
d_seg_end() { # suite label
  read -r S1 T1 <<<"$(steal_snap)"
  python3 - "$1" "$2" "$S0" "$T0" "$S1" "$T1" "$L0" "$D0" "$(date -u +%FT%TZ)" >>"$R/$1-passD-steal.jsonl" <<'PY'
import json, sys
suite, label, s0, t0, s1, t1, l0, d0, d1 = sys.argv[1:]
ds, dt = int(s1) - int(s0), int(t1) - int(t0)
print(json.dumps({"suite": suite, "segment": label, "start": d0, "end": d1, "loadavg1_at_start": float(l0), "steal_delta": ds, "all_cpu_jiffies_delta": dt, "steal_pct": round(100 * ds / dt, 3) if dt else None}))
PY
}
