#!/usr/bin/env bash
# Pass D, second binary: the wake suite (suite name d2), the warm load check, and the hot-app check, one after the other.
cd "$(dirname "$0")/../.."
SUITE=d2 ROUNDS=5 SPEC=bench/passB-scripts/d-spec.txt bash bench/passB-scripts/d-run.sh
bash bench/passB-scripts/d-load.sh
bash bench/passB-scripts/d-hot.sh
echo D-ALL-DONE
