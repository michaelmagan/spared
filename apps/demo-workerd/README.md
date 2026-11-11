# demo-workerd

JavaScript worker with a SQLite-backed Durable Object, the workerd side of the same-app comparison. Rust twin: `apps/demo-spared/`. Both embed `apps/demo-shared/page.html`.

Pinned: `workerd@1.20261110.1` (npm), `compatibilityDate = "2026-11-10"` (accepted by that build).

Install: `mkdir -p bench/work/workerd && cd bench/work/workerd && npm init -y && npm i workerd@1.20261110.1`.

Run both targets: `bench/demo/serve-both.sh [idle-secs]` (workerd :9090, spared :8080 with Host `demo.localhost`). Verify: `bench/demo/check.sh`.

Not like for like:

- Language and engine: JS on V8 (workerd) vs Rust compiled to wasm on wasmtime.
- Storage: Durable Object SQLite storage (`ctx.storage.sql`, localDisk) vs a SQLite file opened through the unix-none VFS with journal_mode=TRUNCATE, synchronous=FULL. workerd's durability and write coalescing (output gates) differ from an explicit FULL sync per commit.
- Process model: one long-lived workerd process serving the worker, with the DO instance created on first request, vs spared instantiating a wasm component per app and idling it out after `--idle-secs`.
- Routing: workerd routes every request to one DO instance (one hop through the stub); spared routes by Host header.
- Seed and first-use cost: both seed 100 rows at first use; the DO seeds in its constructor, the Rust app on first DB access.
