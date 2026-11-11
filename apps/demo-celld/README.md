# demo-celld

The celld side of the demo comparison: same worker, same `apps/demo-shared/page.html` (symlinked as `page.html`), same endpoints (`GET /`, `GET /api/items`, `POST /api/items`), same schema, 100 seed rows in the Durable Object constructor, one object per `?id=K`. `worker.js` differs from `apps/demo-workerd/worker.js` in one line: `import PAGE from "./page.html"` (esbuild needs a relative path; `wrangler.jsonc` maps `**/*.html` to a Text rule).

Version: celld 0.6.3, commit `8702f6d186c77698dbfb30cf29e3b1922a88f008` (tag v0.6.3), Apache-2.0, source in `/opt/celld-src/celld`.

Build: `cd /opt/celld-src/celld && cargo build --release -j2` (output in `target/release/celld`; V8 came prebuilt, about 5 minutes). celld bundles the worker with esbuild, which is not on the machine's PATH: `cd /opt/celld-src/tools && npm i esbuild` (0.28.2), passed to celld through `CELLD_ESBUILD`.

Start: `bench/demo/serve-celld.sh` (port 9292; `FRESH=1` wipes state; `CPU=n` picks the core). It runs `taskset -c 0 celld dev --port 9292 --no-watch --logs` from this directory with `CELLD_ESBUILD` set. Verify: `bench/demo/check-celld.sh`.

Differences from workerd:

- Mode: single-node `celld dev` with the local SQLite object store standing in for the bucket. Non-dev single-node mode needs an S3, GCS or Azure bucket; local storage is only offered by `dev`.
- Process model: one celld process embedding V8, one isolate per cell (Durable Object), activated on first request, evicted only under pressure or `CELLD_IDLE_EVICT_S` (unset here). workerd runs every object inside one process and isolate group.
- Storage path: each cell has a SQLite database under `apps/demo-celld/.celld/dev`, with every commit captured as an LTX replication log and written to the object store. Nothing is stored at `bench/work/demo`.
- Replication is off in the sense that there is no peer. The default durability mode `fleet` with zero peers makes every write wait for the object store ("fleet durability requested and no peer is available" in the log). Every POST therefore pays an object-store write plus a local fsync, which is stronger than workerd's local-disk storage and different from spared's synchronous=FULL file.
- Config: `compatibility_date` is 2026-01-01 (copied from the celld examples), not workerd's 2026-11-10.

Caveats for a like-for-like comparison:

- `--logs` enables INFO logging; drop it from the script when measuring.
- The `celld dev` HTTP listener passes through celld's routing layer before reaching the cell; request routing, ownership checks and the readiness gate have no counterpart in workerd or spared.
- `celld dev` is a development mode; its local object store latency is not representative of S3 or R2.
- Core pinning with `taskset -c 0` covers the process, but V8 and celld use extra threads that share that core.
- First request to each `?id=K` includes cell activation, isolate start and seeding.
