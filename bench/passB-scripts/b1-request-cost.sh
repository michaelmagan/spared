#!/usr/bin/env bash
# B1: request-cost, run stage only (build stage was run first: STAGE=build bench/request-cost.sh). 5 runs per variant, interleaved by the script itself.
source "$(dirname "$0")/lib.sh"
source bench/env.sh
export SPARED_ACCESS_LOG=off
wait_quiet || true
env_snap request-cost
seg_begin b1 request-cost
env STAGE=run SUFFIX=-passB RUNS=5 bench/request-cost.sh >"$R/request-cost-passB.log" 2>&1 || echo FAILED
seg_end b1 request-cost
cat "$R/request-cost-passB.log" | tail -25
