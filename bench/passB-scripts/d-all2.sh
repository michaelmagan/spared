#!/usr/bin/env bash
# Pass D follow-up; run it after d-all.sh has finished: time to ready, then the warm load check a second time (tag load2; the first run lost its steal lines to a variable clash).
cd "$(dirname "$0")/../.."
bash bench/passB-scripts/d-start.sh
TAG=load2 bash bench/passB-scripts/d-load.sh
echo D-ALL2-DONE
