#!/usr/bin/env bash
# Suite g: gVisor (runsc release-20261102.0, installed from the official bucket, sha512 checked) as an extra container runtime on the same Docker.
# dockerd was restarted with `--add-runtime runsc=/usr/local/bin/runsc`; the default runtime stays runc. Same harness as suite f with --runtime runsc.
source "$(dirname "$0")/lib.sh"
export SPARED_ACCESS_LOG=off
W=$ROOT/bench/work/ctr; mkdir -p "$W"
LG=$ROOT/bench/demo/loadgen/target/release/loadgen
docker info 2>/dev/null | grep -E 'Server Version|Runtimes|Default Runtime' | tee "$R/container-passA-gvisor-docker-info.txt"; /usr/local/bin/runsc --version | tee -a "$R/container-passA-gvisor-docker-info.txt"
lgq() { for _ in $(seq 600); do awk '{exit !($1 < 0.3)}' /proc/loadavg && break; sleep 1; done; taskset -c 2 "$LG" "$@"; }
for r in 1 2 3 4 5; do
  for t in gvisor-published gvisor-ip; do
    wait_quiet || true; echo "$(date +%T) lat rep $r $t"
    seg_begin container-gvisor "lat-r$r-$t"
    rm -rf "$W/lat-data"; mkdir -p "$W/lat-data"; docker rm -f ctr-lat >/dev/null 2>&1
    docker run -d --name ctr-lat --runtime runsc --cpuset-cpus 0 -p 127.0.0.1:18095:8090 -v "$W/lat-data:/data" spared-ctr-demo:bench >/dev/null
    if [ $t = gvisor-ip ]; then addr=$(docker inspect -f '{{(index .NetworkSettings.Networks "bridge").IPAddress}}' ctr-lat):8090; else addr=127.0.0.1:18095; fi
    for _ in $(seq 100); do curl -s -o /dev/null http://$addr/ && break; sleep 0.1; done
    for c in 1 16; do lgq --style native --addr $addr --mode warm --conc $c --requests 20000 --out "$R/container-passA-warm-$t-c$c-run$r.json"; done
    docker rm -f ctr-lat >/dev/null 2>&1
    seg_end container-gvisor "lat-r$r-$t"
  done
done
for r in 1 2 3 4 5; do
  wait_quiet || true; echo "$(date +%T) start rep $r"
  seg_begin container-gvisor "start-r$r"; taskset -c 2 python3 "$(dirname "$0")/ctr-gv.py" start $r; seg_end container-gvisor "start-r$r"
done
for r in 1 2 3 4 5; do
  for n in 1 10 20; do
    wait_quiet || true; echo "$(date +%T) mem rep $r n $n"
    seg_begin container-gvisor "mem-r$r-n$n"; python3 "$(dirname "$0")/ctr-gv.py" mem $n $r; seg_end container-gvisor "mem-r$r-n$n"
  done
done
docker rm -f $(docker ps -aq) >/dev/null 2>&1
echo SUITE-G-DONE
