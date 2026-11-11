#!/usr/bin/env bash
# B3: celld in production mode (celld 0.6.3 against a local S3-compatible bucket, RustFS 1.0.5 on loopback, pinned to CPU 3; MinIO's
# download is gone, see the summary) vs spared. Targets: spared, celld-single (one node, bucket durability), celld-fleet (two nodes on this VM,
# CELLD_DURABILITY default `fleet`, load sent to node A). Node A on CPU 0, node B on CPU 1, load generator on CPU 2.
# Per round and target: cold (first request of 100 apps), warm c1/c16 (20,000), mixed c1/c16 (5,000, one POST in ten), restart on the existing
# state then cold again (cold-existing), then a fresh start and PSS after N = 1, 10, 50 apps served. 5 rounds, target order rotates.
# Raw: bench/results/b3-passB-<target>-<part>-run<r>.json
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off AWS_ACCESS_KEY_ID=benchkey AWS_SECRET_ACCESS_KEY=benchsecret123 AWS_REGION=us-east-1
export CELLD_ESBUILD=/opt/celld-src/tools/node_modules/.bin/esbuild
C=/opt/celld-src/celld/target/release/celld
S3=http://127.0.0.1:9000
B=$ROOT/bench/work/b3; mkdir -p "$B/bin"
cp -f "$ROOT/target/release/spared" "$B/bin/spared"
mk_template_in "$B/bin/spared" "$B/tpl"
# bucket store
pgrep -x rustfs >/dev/null || { (cd "$ROOT/bench/work/s3" && RUSTFS_ACCESS_KEY=benchkey RUSTFS_SECRET_KEY=benchsecret123 nohup taskset -c 3 ./rustfs server --address 127.0.0.1:9000 rustfs-data >rustfs.log 2>&1 &); sleep 5; }
PIDS=""
stop() { for p in $PIDS; do kill $p 2>/dev/null; done; for p in $PIDS; do for _ in $(seq 300); do kill -0 $p 2>/dev/null || break; sleep 0.1; done; kill -9 $p 2>/dev/null; wait $p 2>/dev/null; done; PIDS=""; sleep 2; }
pss_snap() { # PIDS -> "rss anon file pss pss_anon pss_file" kB, summed over each pid and its children
  local all="$PIDS" p c
  for p in $PIDS; do for c in $(pgrep -P "$p" 2>/dev/null || true); do all="$all $c"; done; done
  for p in $all; do
    awk '$1=="VmRSS:"{r=$2} $1=="RssAnon:"{a=$2} $1=="RssFile:"{f=$2} END{print "S", r+0, a+0, f+0}' /proc/$p/status
    awk '$1=="Pss:"{x=$2} $1=="Pss_Anon:"{y=$2} $1=="Pss_File:"{z=$2} END{print "P", x+0, y+0, z+0}' /proc/$p/smaps_rollup
  done | awk '$1=="S"{r+=$2;a+=$3;f+=$4} $1=="P"{x+=$2;y+=$3;z+=$4} END{print r+0, a+0, f+0, x+0, y+0, z+0}'
}
start_target() { # target fresh(1|0) prefix
  local t=$1 fresh=$2 pre=$3
  stop
  if [ $t = spared ]; then
    [ $fresh = 1 ] && { rm -rf "$B/data"; cp -a "$B/tpl" "$B/data"; }
    taskset -c 0 "$B/bin/spared" serve --data-dir "$B/data" --idle-secs 600 >"$B/spared.log" 2>&1 & PIDS="$!"
    for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8081/ && break; sleep 0.1; done
    return
  fi
  if [ $fresh = 1 ]; then
    rm -rf "$B/ca" "$B/cb"; mkdir -p "$B/ca" "$B/cb"
    (cd "$ROOT/apps/demo-celld" && $C deploy . --bucket s3://cells/$pre --endpoint $S3 >"$B/deploy-$pre.log" 2>&1) || { echo "celld deploy failed"; return 1; }
  fi
  (CELLD_WATCH=$B/ca taskset -c 0 $C --bucket s3://cells/$pre --endpoint $S3 --listen 127.0.0.1:9292 --internal-listen 127.0.0.1:9293 --advertise 127.0.0.1:9293 >"$B/celld-a.log" 2>&1 & echo $! >"$B/pid-a")
  PIDS="$(cat $B/pid-a)"
  if [ $t = celld-fleet ]; then
    (CELLD_WATCH=$B/cb taskset -c 1 $C --bucket s3://cells/$pre --endpoint $S3 --listen 127.0.0.1:9392 --internal-listen 127.0.0.1:9393 --advertise 127.0.0.1:9393 >"$B/celld-b.log" 2>&1 & echo $! >"$B/pid-b")
    PIDS="$PIDS $(cat $B/pid-b)"
    wait_tcp 9392
  fi
  wait_tcp 9292
  if [ $t = celld-fleet ]; then
    for _ in $(seq 60); do $C diagnose --bucket s3://cells/$pre --endpoint $S3 --read-only 2>&1 | grep -q '2 node lease' && break; sleep 1; done
  fi
  sleep 3
}
style() { [ $1 = spared ] && echo spared || echo celld; }
lg() { taskset -c 2 "$LG" "$@"; }
ts=(spared celld-single celld-fleet)
for r in 1 2 3 4 5; do
  for i in 0 1 2; do
    t=${ts[$(( (i + r - 1) % 3 ))]}; st=$(style $t); pre=b3-$t-r$r-$$
    wait_quiet || true
    echo "$(date +%T) round $r $t"
    seg_begin b3 "r$r-$t"
    if start_target $t 1 $pre; then
      lg --style $st --mode cold --n 100 --out "$R/b3-passB-$t-cold-run$r.json"
      for c in 1 16; do inner_quiet; lg --style $st --mode warm --conc $c --requests 20000 --out "$R/b3-passB-$t-warm-c$c-run$r.json"; done
      for c in 1 16; do inner_quiet; lg --style $st --mode mixed --conc $c --requests 5000 --out "$R/b3-passB-$t-mixed-c$c-run$r.json"; done
      stop; start_target $t 0 $pre; inner_quiet
      lg --style $st --mode cold --n 100 --out "$R/b3-passB-$t-cold-existing-run$r.json"
      stop
      start_target $t 1 $pre-mem
      e=$(pss_snap)
      lg --style $st --mode touch --n 1 --out /dev/null; sleep 2; m1=$(pss_snap)
      lg --style $st --mode touch --n 10 --out /dev/null; sleep 2; m10=$(pss_snap)
      lg --style $st --mode touch --n 50 --out /dev/null; sleep 2; m50=$(pss_snap)
      python3 - "$t" "$r" "$e" "$m1" "$m10" "$m50" >"$R/b3-passB-$t-mem-run$r.json" <<'PY'
import json,sys
t,r,*s=sys.argv[1:]
k=("rss","anon","file","pss","pss_anon","pss_file")
f=lambda x: dict(zip(k,map(int,x.split())))
print(json.dumps({"target":t,"run":int(r),"kb":{"empty":f(s[0]),"n1":f(s[1]),"n10":f(s[2]),"n50":f(s[3])}}))
PY
    else
      echo "FAILED start $t round $r"
    fi
    stop
    seg_end b3 "r$r-$t"
  done
done
echo B3-DONE
