#!/usr/bin/env bash
# Request-cost benchmark: cost per realistic request (10 point reads + 1 durable write, live instance, one pinned CPU).
# (a) SQLite inside the Wasm sandbox, (b) SQLite native in the host behind a WIT call.
# Stages: STAGE=build builds and compiles only; STAGE=run measures only; default does both.
# SUFFIX (default empty) goes into the result names: bench/results/request-cost$SUFFIX-{sandbox,host}-run<r>.json.
set -eu
cd "$(dirname "$0")/.."
source bench/env.sh
SUFFIX=${SUFFIX:-}; N=${N:-10000}; WARMUP=${WARMUP:-500}; RUNS=${RUNS:-3}; STAGE=${STAGE:-all}
BIN=$SPARED_LAB_BIN
SB=apps/bench3b-sandbox/target/wasm32-wasip2/release/bench3b-sandbox.wasm
HO=apps/bench3b-host/target/wasm32-wasip2/release/bench3b-host.wasm
mkdir -p bench/work/g3b bench/results

if [ "$STAGE" != run ]; then
  for app in bench3b-sandbox bench3b-host; do
    (cd apps/$app && cargo build --release --target wasm32-wasip2 -j2)
  done
  lab_build -j2
  $BIN compile $SB bench/work/g3b/sandbox.cwasm
  $BIN compile $HO bench/work/g3b/host.cwasm
fi
[ "$STAGE" = build ] && exit 0

wait_idle() {
  for _ in $(seq 120); do
    awk '{exit !($1 < 0.5)}' /proc/loadavg && return 0
    sleep 5
  done
  echo "loadavg stayed >= 0.5 for 10 minutes: $(cat /proc/loadavg)" >&2
  exit 1
}

for r in $(seq "$RUNS"); do
  if [ $((r % 2)) -eq 1 ]; then order="sandbox host"; else order="host sandbox"; fi
  for v in $order; do
    wait_idle
    d=bench/work/g3b/data-$v; rm -rf "$d"
    taskset -c 0 $BIN bench-3b bench/work/g3b/$v.cwasm --variant $v --data "$d" \
      --requests "$N" --warmup "$WARMUP" --run "$r" --out bench/results/request-cost$SUFFIX-$v-run$r.json >/dev/null
    echo "run $r $v done (loadavg $(cut -d' ' -f1 /proc/loadavg))" >&2
  done
done

python3 - "$RUNS" "$SUFFIX" <<'PY'
import json, statistics as st, sys
runs, suffix = int(sys.argv[1]), sys.argv[2]
def load(v): return [json.load(open(f'bench/results/request-cost{suffix}-{v}-run{r}.json')) for r in range(1, runs + 1)]
A, B = load('sandbox'), load('host')
print(f"{'variant':<10}{'run':>4}{'p50 us':>10}{'p99 us':>10}{'max us':>10}{'cpu us/req':>12}")
for name, rs in (('sandbox', A), ('host', B)):
    for x in rs:
        l = x['latency_us']
        print(f"{name:<10}{x['run']:>4}{l['p50']:>10.1f}{l['p99']:>10.1f}{l['max']:>10.1f}{x['cpu_us_per_request']:>12.1f}")
med = lambda rs, f: st.median(f(x) for x in rs)
a_p50, a_p99 = med(A, lambda x: x['latency_us']['p50']), med(A, lambda x: x['latency_us']['p99'])
b_p50 = med(B, lambda x: x['latency_us']['p50'])
a_cpu = med(A, lambda x: x['cpu_us_per_request'])
print(f"\nmedian over {runs} runs: (a) p50 {a_p50:.1f} us, p99 {a_p99:.1f} us, cpu {a_cpu:.1f} us/req; (b) p50 {b_p50:.1f} us")
gap = a_p50 - b_p50
print(f"(a).p50 - (b).p50 = {gap:.1f} us")
need = a_cpu * 5000
print(f"500 hot apps x 10 req/s = 5000 req/s; cpu needed = {need:.0f} us per second of one core (limit 1,000,000)")
p99_ms = a_p99 / 1000
passed = p99_ms <= 5 and gap < 1000
k_lat = p99_ms > 20
k_cpu = need > 1_000_000
print(f"PASS  (a).p99 <= 5 ms [{p99_ms:.3f} ms] AND (a).p50-(b).p50 < 1 ms [{gap/1000:.3f} ms]: {passed}")
print(f"FAIL  (a).p99 > 20 ms: {k_lat}; cpu_us_per_request*5000 > 1,000,000: {k_cpu}; fail: {k_lat or k_cpu}")
PY
