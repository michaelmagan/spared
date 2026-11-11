#!/usr/bin/env bash
# seed-n500-u200 without a second 1.8 GB of files: app k gets hardlinks to the component files of app k%200 from seed-n500-u500 (databases are real copies).
C=$(cd "$(dirname "$0")/../.." && pwd)/bench/work/c; S=$C/seed-n500-u500; D=$C/seed-n500-u200
rm -rf "$D"; mkdir -p "$D/apps" "$D/components" "$D/backups" "$D/trash"; cp -a "$S/admin-token" "$S/format.json" "$D/"
for k in $(seq 0 499); do
  j=$((k % 200)); mkdir "$D/apps/demo-$k"
  for f in component.cwasm component.sha256 component.wasm version; do ln "$S/apps/demo-$j/$f" "$D/apps/demo-$k/$f"; done
  printf 'name = "demo-%s"\n[grants]\ndata = true\n' $k >"$D/apps/demo-$k/manifest.toml"; : >"$D/apps/demo-$k/app.log"
  cp -a "$S/apps/demo-$k/data" "$D/apps/demo-$k/data"
done
for j in $(seq 0 199); do h=$(cat "$S/apps/demo-$j/component.sha256"); for e in cwasm wasm; do ln -f "$S/components/$h.$e" "$D/components/$h.$e"; done; done
touch "$D/.seeded-500-200"
