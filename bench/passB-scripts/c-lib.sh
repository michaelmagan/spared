# Pass C helpers. Source after lib.sh (lib.sh cd's to the repo root and defines wait_quiet, steal_snap, WORKERD, DEMO paths).
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
PC=$ROOT/bench/passB-scripts/c-pcache.py
C=$ROOT/bench/work/c; mkdir -p "$C/bin"
FETCH=$ROOT/bench/passB-scripts/b5-fetch.py
c_seg_begin() { seg_begin; }
c_seg_end() { # suite label
  read -r S1 T1 <<<"$(steal_snap)"
  python3 - "$1" "$2" "$S0" "$T0" "$S1" "$T1" "$L0" "$D0" "$(date -u +%FT%TZ)" >>"$R/$1-passC-steal.jsonl" <<'PY'
import json, sys
suite, label, s0, t0, s1, t1, l0, d0, d1 = sys.argv[1:]
ds, dt = int(s1) - int(s0), int(t1) - int(t0)
print(json.dumps({"suite": suite, "segment": label, "start": d0, "end": d1, "loadavg1_at_start": float(l0), "steal_delta": ds, "all_cpu_jiffies_delta": dt, "steal_pct": round(100 * ds / dt, 3) if dt else None}))
PY
}
spared_seed() { # DIR NA UNIQ : NA apps demo-K using distinct wasm v(K % UNIQ), each requested once (database exists). Idempotent.
  local d=$1 na=$2 u=$3
  [ -f "$d/.seeded-$na-$u" ] && return
  rm -rf "$d"; mkdir -p "$d"
  "$C/bin/spared" serve --data-dir "$d" --listen 127.0.0.1:8080 --admin-listen 127.0.0.1:8081 --idle-secs 600 >"$d.seed.log" 2>&1 & local p=$!
  for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
  for k in $(seq 0 $((na-1))); do
    local M; M=$(printf 'name = "demo-%s"\n[grants]\ndata = true\n' "$k" | base64 -w0)
    curl -fsS -X PUT -H "authorization: Bearer $(cat "$d/admin-token")" -H "x-spared-manifest: $M" --data-binary @"$ROOT/bench/work/distinct/wasm/v$((k % u)).wasm" http://127.0.0.1:8081/v1/apps/demo-$k >/dev/null
  done
  python3 "$FETCH" spared 0 $((na-1)) /dev/null
  kill $p; wait $p 2>/dev/null || true; sleep 1
  touch "$d/.seeded-$na-$u"
}
workerd_seed() { # NA : config in $C/wd-cfg-NA, pristine seeded data in $C/wd-seed-NA. Idempotent.
  local na=$1
  [ -f "$C/wd-seed-$na/.seeded" ] && return
  rm -rf "$C/wd-cfg-$na" "$C/wd-seed-$na" "$C/wd-data-$na"; mkdir -p "$C/wd-seed-$na"
  python3 "$ROOT/bench/demo/gen-distinct-workerd.py" "$C/wd-cfg-$na" "$C/wd-data-$na" $na >/dev/null
  (cd "$C/wd-cfg-$na" && exec "$WORKERD" serve config.capnp) >"$C/wd-seed-$na.log" 2>&1 & local p=$!
  wait_tcp 9100; wait_tcp $((9100+na-1)); sleep 1
  python3 "$FETCH" workerd 0 $((na-1)) /dev/null
  kill $p; wait $p 2>/dev/null || true; sleep 1
  cp -a "$C/wd-data-$na/." "$C/wd-seed-$na/"; touch "$C/wd-seed-$na/.seeded"
}
