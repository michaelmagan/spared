# Design

This note is for readers who want the detail behind spared: how it works, what it costs, and what is not built. spared hosts small Rust web apps on one machine. Each app is a Rust crate compiled to a `wasm32-wasip2` component, with SQLite compiled in. Numbers come from `bench/results/`, and [benchmarks](benchmarks.md) says how to rerun them.

spared is an early preview, so nothing here is a stable interface. The sandbox has had no outside audit (see the [FAQ](faq.md)).

## Shape of the system

One host process (`spared serve`, wasmtime 50.0.1) runs on a single node. It has two listeners:

- The public listener (default `127.0.0.1:8080`) routes by the `Host` header: `<app>.localhost` goes to the app named `<app>`. With `--allow-path-routing`, `/_/<app>/` on any host also reaches the app, at the cost of putting every app on one browser origin, so apps could read each other's data in a visitor's browser; leave it off for code you did not write.
- The admin listener (default `127.0.0.1:8081`) takes deploys, lists, logs and deletes, and serves the metrics. It needs a bearer token (the admin token, or an app token for one app's routes, see [App tokens](#app-tokens)), refuses `Host` names that are not an IP or `localhost`, and refuses requests with an `Origin` header.

The public listener answers one path itself: `GET /healthz`, and only for a host that names no app (see [Observability](#observability)). Everything else on it is an app's or a 404.

On disk, the data dir starts with `format.json`, the stamp that says which layout it uses (see [Versions and the data directory format](#versions-and-the-data-directory-format)). Each app has a directory under the data dir: the precompiled component (`component.cwasm`, a hard link into the shared store described below), the deployed wasm (`component.wasm`, also a link into the store), `component.sha256` (its key), its manifest, a version number, an `app.log` and a `data/` directory that holds the app's SQLite file.

Compiled components live once per distinct deployed binary in `<data-dir>/components/<sha256 of the deployed wasm>.cwasm`, written to a temporary name and renamed. The server keeps one `Component` and one pre-linked `ProxyPre` per hash in memory, shared by every app using it, so identical deployments map the compiled code once (the workerd comparison found about 4 MiB of resident memory per hot app when each name mapped its own copy; apps with different code still cannot share, see the memory curve in `docs/memory-methodology.md`). Limits, grants, the store and the data directory stay per app. Deleting or redeploying an app removes its link, and the stored file goes when no loaded app runs that hash and no app directory links to it (link count 1); startup also removes unreferenced, unlinked and partial files. On a file system that cannot hard link (a store on another mount) each app directory gets a copy and the store file's link count stays 1, so the link count alone never decides: a hash a loaded app names is kept, and a store file that went missing is put back from the app directory's copy at load.

The deployed wasm is stored beside its compiled file (`components/<sha256>.wasm`), because a `.cwasm` loads only into the wasmtime version and configuration that made it. When a stored `.cwasm` is refused at load, the server checks the stored wasm against its hash, compiles it again into a new file, renames that into place and relinks the app, so a wasmtime upgrade does not strand deployed apps.

## One instance per app

Requests to one app go through a single instance, guarded by a lock in the host (`crates/spared/src/server.rs`). Requests to the same app run one after another. Different apps run concurrently on the host's async runtime.

Consequences:

- Writes to an app are serialized, the way a Durable Object serializes them, and SQLite needs no cross-process locking.
- An app keeps in-memory state only until its instance is dropped. Apps must treat memory as a cache.
- One slow request delays the app's other requests, up to the deadline.
- A request runs in its own task, holding the lock until it finishes: a client that disconnects does not cancel it, so the instance is never left mid-call. A request still queued for the lock is dropped when its client goes away. An app takes at most `--max-app-queue` requests running or waiting (default 16); the next gets `503 app_busy` with `Retry-After`, so a slow app cannot use up every connection slot.

Spin 4 does it differently. It pools and reuses wasip3 instances (up to 128 requests by default; 1 for wasip2), and several instances of one app can run at once with no per-app serialization (Spin v4.3.0 source, `crates/trigger-http/src/lib.rs`, read for the comparison).

## SQLite inside the sandbox

SQLite is C compiled to Wasm with wasi-sdk 25 and linked into the app through `rusqlite` with the bundled source. The app opens `/data/app.db` itself.

Two settings are not defaults, and both come from the crash test:

- The `unix-none` VFS, which takes no lock files. This is safe only because the host guarantees one owner. With SQLite's default dotfile locking, 7 of the first 100 kill -9 trials could not reopen the database (a stale `app.db.lock` directory); with `unix-none`, 0 of 100 failed. The 7 failures have no raw file in `bench/results/`.
- `locking_mode=EXCLUSIVE` when WAL is used, because WASI has no shared memory.

The shipped helper (`src/spared.rs` in each generated app) uses the rollback journal in `TRUNCATE` mode with `synchronous=FULL`. It wakes fastest and needs no recovery step. WAL also works but each wake after an unclean unload recovers the WAL: about 0.2 ms more than TRUNCATE on the current build, about 1 ms before SQLite used `pread`/`pwrite` (`bench/results/wake-tuned.jsonl`, `wake.jsonl`).

The alternative is native SQLite in the host, reached through a query API. See the trade-offs table below.

## Grants and import validation

The manifest, `spared.toml`, declares what an app may use:

```toml
name = "my-app"

[grants]
data = true               # a /data directory; default true
fetch = ["api.example.com"]   # outbound HTTP hosts; "*.example.com" matches subdomains only

[limits]
memory_mb = 64
cpu_ms = 1000
deadline_ms = 5000
max_outbound = 10
```

At deploy, the host reads the component's declared imports (`crates/spared/src/grants.rs`):

- `wasi:cli`, `wasi:clocks`, `wasi:random`, `wasi:io` and `wasi:http/types` are always allowed.
- `wasi:filesystem` needs `data = true`.
- `wasi:http/outgoing-handler` needs a non-empty `fetch`.
- Anything else, including every `wasi:sockets` interface, is refused.

A refused deploy returns `ungranted_import`, naming the imports and the manifest line that would fix it where one exists. The host also refuses a component that does not export `wasi:http/incoming-handler`, is not a component, exceeds 16 MB, is too expensive to compile, or whose manifest name differs from the deployed name.

At run time the app's filesystem is one preopened directory. Outbound HTTP passes a hook (`crates/spared/src/sandbox.rs`) that checks the host list and the per-request call budget, resolves the name, refuses any private, loopback, link-local, CGNAT, multicast, documentation, benchmarking or reserved address (including mapped IPv6 forms), then connects to the address it checked (`pinned_send.rs`). Without that last step the stock sender in wasmtime-wasi-http resolves the name a second time, which a short-TTL DNS name can exploit.

Result: 54 escape cases, 0 failures: 46 attacks denied and 8 controls that succeeded (`bench/results/escape.json`; `escape-tuned.json` is an earlier run of 43 cases). The first run had 31 cases. The hardening pass added the rest: it refuses URLs with userinfo or a mismatched `Host`, compares hosts without case or trailing dot, and denies more embedded-IPv4 IPv6 forms. The per-app limits added one: a granted fetch after the app's egress budget is spent. The last three are about host error text. wasmtime-wasi answers `wasi:io/error.to-debug-string` with the host error's debug text, which holds its causes and, when the process captures backtraces (`RUST_BACKTRACE=1`), a host stack trace with host symbol names. The linker replaces that function (`sandbox.rs`): a guest gets a fixed line, an HTTP error code as the same value `wasi:http/types.http-error-code` returns, or the egress refusal's own text. `spared` also sets `RUST_BACKTRACE=0` and `RUST_LIB_BACKTRACE=0` at start unless `SPARED_ALLOW_HOST_BACKTRACE=1`. The cases run a child process with `RUST_BACKTRACE=1`, with and without that variable, and a guest that fills its data cap through a write stream and asks for the error's text.

## Wake and sleep

Deploy precompiles the component to native code (`.cwasm`) and pre-links it. Waking an app creates a store with the app's limits, preopens `/data`, sets stdout and stderr to the app log, and instantiates the component. The first request does the rest: the app opens its SQLite file.

A background task checks every 500 ms and drops any instance idle for `--idle-secs` (default 30). A dropped instance releases its Wasm memory and its open database, but the app is not free while idle. The compiled component stays mapped, which costs about 4.1 MiB of reclaimable file pages and 0.35 MiB of anonymous memory per different app, and about 0.09 MiB per app for copies of one component. An app that is deployed but has not been requested since the server started costs about 0.67 MiB, because the server maps every deployed component at start. Beyond that an idle app costs its files on disk and an entry in the host's app table.

Measured (`bench/wake.sh`, one pinned CPU, in process, precompiled component, 100-row database):

- Instantiating the component: about 40 to 50 us at p50.
- Wake to first query with the rollback journal, current build: p50 634 to 684 us, p99 1831 to 2881 us across three runs (`wake-tuned.jsonl`). Before the `pread`/`pwrite` change it read p50 595 to 607 us and p99 699 to 747 us on a faster VM run; within one run the current build is about 13% faster.
- With WAL, current build: p50 856 to 875 us (1420 to 1438 us before the `pread`/`pwrite` change).

Density harness (`spared bench-density`, in a `--features lab` build): an in-process harness, not the server. It built 10,000 app directories with 500 instances live, in one process with the CPU limit off and no memory cap. Creating the apps added 7.7 MiB to the process in total, and a live instance grew it by 417 KiB. These used one shared component, and the harness says nothing about what `spared serve` does at that count, which was not measured. Before compiled code was shared, 50 separately deployed names of the demo app cost 4,120 KiB per hot app by RSS, 93% of it file-backed pages, and PSS was the same (`bench/results/vs-workerd-spared-mem-run*.json`, `vs-workerd-mem-pss-before-same-run*.json`). With sharing, 50 identical deployments cost 435 KiB of PSS growth per hot app (`vs-workerd-set2-mem-pss-set2-same-run*.json`). Apps with different code cannot share. Measured as totals for N different apps (`docs/memory-methodology.md`, `bench/results/memory-curve-*.json`, from the build before the `pread`/`pwrite` change), spared costs 17.1 MiB + 4,481 KiB per hot app by PSS (87 percent file-backed code pages the kernel can reclaim) and 1.3 MiB + 561 KiB by RssAnon, and identical apps cost 288 KiB each. Per-app growth after one request (4,000 KiB against 508 KiB for workerd) leaves out workerd's startup memory, so totals are the fair comparison.

## Limits

- Memory: `StoreLimits` caps linear memory at `memory_mb`. A failed grow returns an error to the guest rather than killing it.
- CPU: epoch interruption. A host thread increments a counter every 1 ms. At each tick a running guest yields to the async runtime (a multi-thread runtime in `spared serve`), so one app's compute does not stall other apps or the admin API, and the request is charged for the wall-clock time its guest was running; the guest is interrupted at the next tick after that time reaches `cpu_ms`. The charge covers the guest's own code and the work the server does inside the calls it makes (file reads and writes, fsync, directory listings), because the server does that work on the same thread while the guest waits. It is wall-clock time, not CPU time, and it does not cover time spent waiting for the network or for another app's turn. Outbound HTTP setup (DNS, TCP, TLS) runs outside the guest's call, so each outbound call that passed the policy checks is charged a flat 1 ms of the CPU budget instead (`OUTBOUND_CALL_CPU_NS`); the number of calls is bounded by `max_outbound` and by `deadline_ms`. Epoch checks cost 12 to 34% on SQLite-heavy work depending on the query shape, about 21% across the six shapes of the SQLite microbenchmark, so they stay on and we pay for it.
- Operator maximums: `spared serve --max-memory-mb`, `--max-cpu-ms`, `--max-deadline-ms` and `--max-outbound` cap what a manifest may ask for. Deploy refuses more; load clamps.
- Per-app limits: the operator can lower one app's data and entry caps and give it a CPU budget and an outbound HTTP budget over time ([Per-app limits](#per-app-limits)).
- Deadline: a wall-clock timeout per request (`deadline_ms`), covering the wake of the instance as well as the handler. On timeout the instance is dropped and the client gets `deadline` (HTTP 504).
- Outbound: at most `max_outbound` HTTP calls per request (the count resets at each request).
- Guest file calls run on the worker thread that polls the guest, and a call in progress cannot be interrupted, so `cpu_ms` and `deadline_ms` take effect once it returns; what one call can cost is bounded by the per-app caps on entries, open files and data size. `spared serve` runs at least 4 workers so one app's slow sync cannot stall the listeners. `deadline_ms` is the bound on everything a request does, host work included: at the deadline the request is stopped and the instance dropped, which also ends any outbound call it had started.
- Connections: request headers and body within `--client-timeout-secs`, a write that makes no progress for that long closes the connection, and a public connection lives at most 30 timeouts.
- Request body 8 MB and response body 16 MB by default (`--max-request-mb`, `--max-response-mb`). The response body is read while the handler runs. Component 16 MB (`--max-component-mb`).
- A trap, a CPU limit hit, a memory failure or a deadline drops the instance. The next request wakes a fresh one.

The escape suite checked the memory cap (512 MB request under a 32 MB cap fails the allocation without killing the app) and the CPU cap (200 ms cap interrupted a loop at 255 ms in the stored run). The deadline and the outbound count have code but no test in the suite.

## WebAssembly features

The engine turns on only what the guest toolchain (Rust for `wasm32-wasip2`, wasi-sdk 25 for SQLite) emits, and switches the rest off by name, so a wasmtime upgrade cannot widen what an upload may use. A component that uses an off feature is refused at deploy (`invalid_component`).

| Feature | State | Why |
|---|---|---|
| Component model | on | every app is a component |
| Bulk memory, reference types, multi-value, sign extension, saturating float conversion | on | emitted by Rust and by the SQLite build |
| SIMD (fixed width) | on | the SQLite build uses `-msimd128` |
| Relaxed SIMD | off | nothing emits it, and results may differ between machines |
| Threads, atomics, shared memory | off | apps are single-threaded and SQLite is built without threads |
| Garbage collection, function references | off | Rust has no managed heap; a collector heap would be a second memory beside `memory_mb` |
| Exception handling | off | Rust panics abort and C builds without exceptions |
| Memory64 | off | 32-bit address space is what `memory_mb` limits are written for |
| Multiple memories | off | each module has one memory |
| Tail calls | off | not emitted by the toolchain |
| Wide arithmetic, extended constant expressions, stack switching, custom page sizes | off | not emitted by the toolchain |
| Component-model async, threading, error contexts, extra proposals | off | the WASI 0.2 interfaces apps use are synchronous |

## Compiling an upload

A component can be small and still cost gigabytes and many seconds to compile: the compiler's memory grows much faster than the size of one function with thousands of locals. So the host never compiles an upload in the server process. It first scans the wasm for the largest function (bytes and locals) and the function count, and refuses a component over `--max-function-kb`, `--max-function-locals` or `--max-component-functions`. What passes is compiled by a child process (`spared compile-worker`) that has an address-space limit (`--compile-memory-mb`), a time limit (`--compile-timeout-secs`, enforced by killing it), a CPU-time backstop, a low priority, and at most `--compile-threads` threads. It dies with the server. The server only loads the artifact the child wrote, so one compile serves both validation and storage. Compiles run one at a time. A component that trips any of these gets `422 component_too_complex`, and nothing is deployed. A component the compiler itself rejects still gets `invalid_component`.

The limits apply to new uploads. A component already in the store is not scanned again, and a recompile after a wasmtime upgrade keeps the memory and time limits but not the size scan, so an upgrade never takes down an app that deployed fine before.

## Atomic deploy

A deploy validates everything first: manifest, component, imports, handler export, and that the host can link the component (the step that used to run only when the app was loaded afterwards). Only then does it write files: the stored component, then the new manifest, hash, version and component links as synced `*.tmp` files, then a synced `release.pending` marker, then the renames into place. The marker is the commit point. A crash before it leaves the previous version; a crash after it is finished at the next start, so the app directory never settles with a new manifest beside an old component. The new in-memory app is built before the commit and swapped into the app table after it, then the old instance is dropped. The app's data directory is untouched, so state survives a redeploy.

Limits of this claim: it relies on `fsync` and `rename` doing what the file system says, and no power-loss test has been run. Requests already running on the old instance are not shown to be drained.

## Backups

`POST /v1/apps/{name}/backup` (`spared backup`) copies an app's `data/` directory to `<data-dir>/backups/<name>/<UTC timestamp>/`. It works because of the one-instance rule: the server takes the app's instance lock (the same lock a request holds, so a request in flight finishes first and queued requests wait), drops the live instance so no handle holds the database, copies every file to a `.tmp-` directory, fsyncs it, and renames it into place. With the `TRUNCATE` journal a dropped instance leaves a clean `app.db`, so the copy is a consistent database. The server does not open the copy: an earlier version ran `PRAGMA integrity_check` on it, which evaluated the app's own index expressions in the host with no limits (9 s and 734 MB for a 12 KB file) and exposed SQLite's file parsers to app-written bytes. Now each file is hashed as it is read, the copy is read back and hashed, and they must match; the response says `integrity: "copied, sha256 verified; database not opened by the host"`. That proves a faithful copy, not a healthy database; the app's own SQLite, inside its sandbox, is what reads the file after a restore. If a non-empty `app.db-journal` or `-wal` is present, the files are copied together and the response says `journal_clean: false`. Symlinks in `data/` are not followed or copied, since the app controls them. The same holds if the app swaps a directory for a link while the copy runs: every directory and file is opened relative to its parent directory without following links, so a name that has become a link is skipped, not entered. The walks that run beside a live app (the size and entry count behind `spared ps` and the quota books) and the deletion of a tree work the same way. They keep one directory handle per nesting level, so they stop at 256 levels: the size shown for a deeper tree is partial, and for the quota such a tree counts as over the entry limit. A copy is capped at 64 directory levels, 100,000 entries and `--max-app-data-mb` of apparent size (a sparse file counts at its full size, and its holes stay holes in the copy); over that the backup is refused and nothing changes.

A backup holds app data only, not code. A restore (`POST /v1/apps/{name}/restore`, `spared restore --from <id> --yes`) takes the same lock and drops the instance, copies the backup next to the data directory with the same hash check, stages the complete copy as `data.restore-ready` (so a start after a crash can finish or undo the restore), moves the current `data/` to `backups/<name>/pre-restore-<timestamp>/`, and renames the copy into place; restoring the `pre-restore` id undoes it. Ids come from the clock but never go backwards: a new id is at least one second after the newest existing one, so retention (`--max-backups-per-app`, default 20, applied to backups and, separately, to pre-restore copies) cannot make a new backup sort as the oldest. Ids are validated against the timestamp pattern, so a request cannot name a path.

Deleting an app moves its directory to `<data-dir>/trash/` with one rename and removes it with an iterative walk (no recursion, two open directories, short paths), so an app that nested its data thousands of levels deep cannot crash the server when it is removed; backup deletion and retention work the same way.

Local backups are on the same disk as the data, so on their own they protect against bad writes and bad deploys, not against losing the disk. With `spared serve --backup-to s3://<bucket>/<prefix>` the server also uploads each backup to an S3-compatible bucket, after the app serves again. The client is written for spared over the hyper, rustls and ring stack the binary already had, with no new crates: AWS Signature Version 4 over the body's SHA-256 (never `UNSIGNED-PAYLOAD`), no redirects followed, no proxy, and errors that carry the store's code, message and request id, never a header or a secret. Each backup becomes one archive (POSIX tar, zstd level 3, then ChaCha20-Poly1305 in 64 KiB STREAM segments under a key derived from the operator's key and a random salt, with the app name and remote id as associated data) and a `manifest.json` written last as the commit marker. The server refuses to start with `--backup-to` and no key unless `--backup-encryption off` is given, because the key has to be kept off the machine to be any use. A restore from the bucket downloads, verifies (size, SHA-256, every segment and the final-segment flag, a strict tar reader, each file's hash) and unpacks into a new local backup, then runs the local restore above, so a bad archive never reaches `data/`. spared never deletes in the bucket: retention is the bucket's lifecycle rule. Tested against an S3 stand-in in the test process, not against AWS S3, R2 or any other store ([operating](operating.md#off-machine-backups)).

`spared move` moves an app to another server through that bucket, from one machine that reaches both admin APIs. The parts it needs are small and usable on their own. A freeze (`POST /v1/apps/{name}/freeze`) takes the app's instance lock, drops the instance, writes a `frozen` marker in the app directory and sets a flag before it lets the lock go. A public request checks the flag before it reads its body and again once it holds the instance lock, so a request that was waiting for the lock never runs after a freeze, and the marker keeps the app frozen across restarts. A deploy over a frozen app is refused, except with `?frozen=move`. The code travels as the deployed wasm and its manifest (`GET /v1/apps/{name}/code`), because a backup holds data only. The target compiles it itself, and a compiled file never crosses machines. Then the move freezes the source, backs it up, fetches the backup on the target, compares the content digest the target unpacked with the one the source uploaded before anything replaces the target's data, restores, checks, and thaws the target. The source holds the truth until the restore has completed, so a failure before that thaws it, and a failure after it leaves it frozen. Each step reads the state both servers report, so a rerun finds where an earlier run stopped instead of keeping a journal. Epochs are advisory. A restore from the bucket sets the server's epoch for the app (`backups/<name>/.epoch`) to one above the highest in the bucket, and each upload first looks for a higher one with a single `ListObjectsV2` that starts after the server's own epoch, which the fixed-width `e<epoch>` keys make possible. Finding one refuses the upload with `remote_superseded` and freezes the app. That detects a second writer after the fact and does not prevent one: the look and the upload are two requests, and a stale server is noticed only when it uploads. Preventing it would take a fencing record written with conditional PUTs (`If-None-Match`, `If-Match`), which not every S3-compatible store supports. That is not built. There is no failover: the source has to be up for a move ([operating](operating.md#move-an-app-to-another-server)).

Tested through the real server (`scripts/server-test.py`), including a writer thread acknowledging writes while three backups run; not tested: power loss during a backup, a full disk, and backups of databases in WAL mode.

`spared bench <name>` times cold wakes (evict, then the first request's time to first byte, 20 cycles) and warm requests on one connection, and prints p50, p99 and max. It runs against a live server over HTTP, so its numbers include the loopback network path and differ from the in-process figures of the wake and workerd benchmarks.

## App tokens

The admin token can do everything. An app token can do some things to one app. `POST /v1/tokens` (admin token only) makes one: 32 bytes from `/dev/urandom`, written as `spt_` and 64 hex digits, so a secret scanner can match the prefix. The response is the only place the token appears. The server keeps `{id, app, scopes, sha256, created, expires, last_used, label, revoked, may_create}` in `<data-dir>/tokens/<id>.json` (directory 0700, file 0600, written to a temporary name, synced and renamed, then the directory synced). The id is the first 12 hex digits of the SHA-256. A file that does not parse is skipped with a warning at start, so a damaged record lets nothing in. `last_used` is written at most once a minute per token.

Authentication runs after the `Origin` and `Host` checks, which are unchanged. A bearer equal to the admin token (compared in constant time) is the admin. Otherwise its SHA-256 is compared with every stored hash in full, without an early exit. A match that is revoked or expired is refused with its own kind, and no match is `unauthorized`. Wrong tokens are counted per source address: 30 in a minute shut that address out for the rest of the minute, for every app token and unknown token, so a guess cannot be confirmed while it is shut out. The admin token is compared first and is never shut out, so the operator can always get in to find what keeps sending the wrong token. Requests with no token are not counted, because the CLI sends one before every deploy to check that the server is up.

Authorization is a table from method and path to what a route needs (`crates/spared/src/tokens.rs`, `need`): an app and a scope, the token's own app (`GET /v1/apps`, filtered), or the admin token. Anything not in the table needs the admin token, so a new route is closed to app tokens until it is added. The check runs before the route looks anything up, so a request for another app's route gets the same 403 `wrong_scope` whether that app exists or not. A deploy by an app token also passes a creation rule into the deploy, checked again under the app's name lock, where the token is also looked up again: one revoked or expired since the request began (the body can take minutes) changes nothing, and one that no longer may create the app cannot. A token made while the name was free may create the app once, and then only redeploy it. A token made for an existing app may only redeploy. Deleting an app revokes its tokens, so no token outlives its app.

Not built: tokens with a narrower scope than an app (one route, one backup), per-token rate limits, token use counted in `/metrics`, and handing tokens to another server with `spared move`.

## Per-app limits

The server-wide caps treat every app alike, and every per-request limit is chosen by whoever writes the manifest, which on a platform is the tenant or its coding agent. `spared limits` adds numbers the operator sets for one app and the app cannot touch.

Where they live. `PUT /v1/apps/{name}/limits` (admin token only) stores `{data_mb, entries, cpu_ms_per_min, egress_bytes_per_hour}`, any subset, in `<data-dir>/limits/<name>.json`: directory 0700, file 0600, written to a temporary name, synced, renamed, then the directory synced. The file sits outside `apps/<name>/` so it can exist before the app does (a platform sets limits before handing out the token that creates the app), and so `spared rm`, which an app token with the `rm` scope may run, cannot remove the operator's policy along with the app. The route table in `tokens.rs` names the limits routes as admin-only, and its default is admin-only as well, so no scope reaches them. An app token's view of its own app (`GET /v1/apps/{name}`, the deploy answer) leaves the `quota` object out. A file that does not parse at start keeps its app out of the table: it is not served until the operator sets or clears its limits, which loads it then.

What is in force. For each resource the smaller of the override and the server's cap: `--max-app-data-mb`, `--max-app-entries`, and one core (60000 ms a minute) for CPU. Egress has no server-wide cap. A manifest has no key for any of these. An override above a cap is kept as written and reported as `clamped`, so raising the cap later lets it take effect without a second command.

Disk. At each wake the store gets `AppCaps` with the per-app numbers in place of the server's, and the books (`fsguard.rs`) run as before: the recount before a refusal, at most four times a second, and no credit for the guest's word that it freed something. A change to these limits drops the app's instance so the next wake reads them. The server-test attacks on the books run a second time with the limits set per app under larger server caps.

CPU. The guest's poll time (`PollClock`, the same clock `cpu_ms` reads) is charged to a token bucket when each request ends. The bucket refills at the override's rate and holds 10 seconds of it. A request is let in while the bucket holds anything, checked before its body is read and again once it holds the instance lock, so queued requests cannot all slip in on one positive reading. Requests to one app run one at a time, so the bucket is overdrawn by at most one request's `cpu_ms`. An empty bucket answers new requests 503 `cpu_budget_exhausted` with `Retry-After` set to the seconds until it holds something again. The cost per request is an atomic load and an atomic add for an app without a budget, and two short mutex sections for one with a budget. The wake (instantiating the component, which runs its start function) is charged by the time it takes, whether it succeeds, fails or times out, and so is each outbound call that passed the policy checks, at a flat 1 ms for its DNS, TCP and TLS setup. Not charged: the server's other work on outbound HTTP (parsing responses) and requests turned away before they run.

Egress. The outbound hook (`sandbox.rs`) wraps the request body it sends and the response body it gets in a meter that charges every data frame and trailer to a bucket holding one hour of the override's rate. A call is refused before it connects once the bucket is empty, and a frame that empties it is replaced by an error, so a large body stops near the budget instead of after it. The guest sees `ErrorCode::InternalError` with text that starts with `egress_budget_exhausted`. The policy checks (host list, `max_outbound`, address rules) come first, so a call they refuse costs nothing. Each call is also charged for what precedes its body: the request's method, URI and headers (each header is name plus value plus 4 bytes) before it connects, and the response's status line (15 bytes) and headers when they arrive. A call whose request head alone spends the bucket is refused before it connects, so a call with no body still lowers the bucket. Not counted: TLS handshake bytes, unread socket buffers, and the server's responses to the app's own clients.

State. One `AppQuota` per app name, held by the server and shared by the app's `App` value, its instance's store and the outbound hook. Eviction, a trap, a freeze and a redeploy keep it, since it lives outside the instance, and an app has one instance, so many requests cannot multiply a budget. It is removed with the app and made fresh when the app is deployed again, and the counters are in memory only: they start over when the server restarts. A change to a budget keeps what was used rather than refilling it. The memory is a few hundred bytes per app and per stored override, both of which the operator controls.

`spared move` copies the source's override to a target that has none for that name, before the app is deployed frozen there, and refuses with `move_unsupported` a target without the limits routes, while nothing has changed. A target that has its own override keeps it. Copying was picked over printing a reminder because a forgotten reminder leaves the app running with no limits on the new server.

Not built: a per-app memory cap below the manifest's, disk read and write rates, the bandwidth of requests and responses on the public listener, a request rate per app, a server-wide default budget for every app, and keeping the counters across a restart.

## Versions and the data directory format

`spared --version` prints the release version from `Cargo.toml` and the git commit the binary was built from (`crates/spared/build.rs` asks git, or takes `SPARED_GIT_SHA` where the build has no checkout, as in the image build). The server prints the same line when it starts and reports the version in `/healthz` and in `spared_build_info`.

The data dir carries a stamp, `format.json`: `{"format": 1, "created_by": "<version>", "last_opened_by": "<version>"}`. `format` numbers the on-disk layout, not the release. `spared serve` and `spared dev` read it before they create the admin token or touch anything else (`crates/spared/src/datadir.rs`):

- No stamp: a new directory, or one from before stamps existed. It is format 1; the server stamps it.
- The format this build writes: opened, and `last_opened_by` updated.
- A higher format than this build reads: refused with `data_dir_too_new`. A lower one than it reads: `data_dir_too_old`. A file that does not parse: `data_dir_stamp_unreadable`. In each case the process exits 1 before writing anything, with a `fix`.

The stamp is written to a temporary name and renamed, mode 0600. Format 2 means that `<data-dir>/limits/` holds at least one per-app limit. The server writes it when the first one is set (the stamp before the file, so a limit is never on disk under a format 1 stamp) and writes format 1 again when the last is cleared or at a start that finds none. A release from before per-app limits reads format 1 only, so it refuses such a directory instead of serving its apps without the limits an operator set. A directory that never had limits stays at format 1 and older releases still open it. Formats 1 and 2 have the same layout otherwise, so there is no migration code yet. The rule is in [upgrading.md](upgrading.md): a release that changes the layout raises the number and either upgrades the directory at start or refuses it. What the stamp does not cover is the compiled code, which has its own check: a `.cwasm` from another wasmtime version is refused by wasmtime at load and recompiled from the stored wasm (above).

Tested in `scripts/server-test.py`: a directory with no stamp opens, is stamped and serves; three bad stamps are refused and the directory's files are compared before and after. Not tested: a crash between a layout upgrade and the stamp write, since there is no layout upgrade yet.

## Observability

Three things, all in `crates/spared/src/obs.rs` and the request wrapper in `server.rs`. No new dependency, and no OpenTelemetry: this is a log and a metrics endpoint, not traces.

**Access log.** One JSON object per line on stdout for every request on either listener; the server's own messages go to stderr, so the two can be split. Fields: `time` (UTC, RFC 3339, milliseconds), `listener` (`public` or `admin`), `app` (the app the request was routed to, or `null`), `method`, `path`, `status`, `duration_ms` (from the request's headers arriving to the response being ready to send) and `request_id`; plus `error`, the error kind, when spared itself produced the response (`app_trap`, `deadline`, `no_such_app`, `unauthorized` and so on), and `token_id` when an admin request carried a valid app token. The line is built from the request's method and path only. It never holds the query string, a header, a cookie or a body, and no token can reach it. Every string goes through a JSON encoder, so a request path cannot forge a second line; paths are cut at 512 characters. `--access-log off` (env `SPARED_ACCESS_LOG=off`) stops it. Not logged: connections that never send a complete request head (they time out inside the HTTP layer), and the time spent writing the response to a slow client.

The request id is 16 hex digits, different for every request and not a counter, so it does not reveal request volume. It is returned to the client as the `x-spared-request-id` response header, on app responses too (the server overwrites a header of that name set by an app), so someone reporting a failure can quote it. The app itself is not given the id.

**Health check.** `GET` or `HEAD /healthz` on the public listener answers 200 with `{"status": "ok", "version": "...", "uptime_seconds": N}`. It is the only path the public listener answers itself. It is matched after host routing, so a request whose `Host` names an app goes to that app, `/healthz` included. The body names no app, path or count of apps. It does tell anyone who can reach the listener which version runs and when it started, which is what a health check and a bug report need; put a proxy in front if that is too much.

**Metrics.** `GET /metrics` on the admin listener only, in the Prometheus text format (version 0.0.4). It goes through the same refusals as every admin route, in the same order: an `Origin` header (403), a `Host` that is not an IP or `localhost` (403), a missing or wrong bearer token (401). The public listener has no such route: `/metrics` there is routed like any other path and ends in an app or a 404.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `spared_build_info` | gauge | `version` | Always 1; carries the version |
| `spared_uptime_seconds` | gauge | | Seconds since start |
| `spared_requests_total` | counter | `listener`, `status` (`1xx` to `5xx`) | Requests answered |
| `spared_errors_total` | counter | `listener`, `kind` | Error responses spared itself produced, by error kind |
| `spared_wakes_total` | counter | | App instances started |
| `spared_evictions_total` | counter | `reason`: `idle`, `error`, `admin`, `deploy` | Live instances dropped: idle timeout; a trap, limit or deadline; `spared evict`, a backup or a restore; a redeploy or delete |
| `spared_apps` | gauge | `state`: `deployed`, `hot` | Apps on the server, and those with a live instance |
| `spared_components_loaded` | gauge | | Distinct compiled components in memory |
| `spared_prefetch_bytes_total` | counter | | Bytes of stored components read into the page cache ahead of requests, at start and at a component's first wake |
| `process_resident_memory_bytes` | gauge | | Resident memory of the server process (Linux) |

No metric has an app name as a label: with thousands of apps that would be thousands of series, and `spared ps` already has per-app requests, wakes and errors. The error kinds are string literals in the server, so that label set is small and fixed. Counters reset when the server restarts.

Cost: the access log is one `write` to stdout per request. Measured on a warm GET with the log on against off, it added +5.6% at p50 with one connection and +5.9% with 16 connections, and a line is about 163 bytes (`docs/benchmarks.md`). Only warm GET and one app were measured, and stdout went to a file. The benchmark scripts that start a server pass `--access-log off`, which is the configuration the published numbers were measured in (they predate the log).

Tested in `scripts/server-test.py` and `scripts/e2e.sh`: the fields of a line; that a secret placed in a query string, a header, a cookie and a body appears nowhere in the server's output; that the admin token is never printed; `/metrics` refused without the token, with an `Origin`, with a foreign `Host`, and absent from the public listener even with the token; `/healthz` naming no app.

## Trade-offs

| | SQLite inside Wasm (spared) | SQLite on the host |
|---|---|---|
| A SQLite memory-safety bug reached by a hostile app | Contained to that app's sandbox | Reaches the host and every other app |
| Memory and CPU caps on database work | Cover it, because it runs as guest code | Need separate accounting; a heavy query runs outside the app's limits |
| What the host must defend | A directory grant | A query API: which file, result size, query time |
| Crash domain | One app | The host process |
| Same engine in the browser | Yes, so a local-first sync path stays open | No |
| SQLite speed | About 1.85x native CPU with CPU limits on (1.5x without them) | Native |

The cost, measured:

- SQLite microbenchmark, current build: geometric mean 1.85x native across six query shapes, from 1.07x (durable inserts) to 2.37x (point select); 1.54x with epochs off. What is left is Cranelift code against native C and the epoch checks.
- Request-cost benchmark, current build: a request with 10 reads and 1 durable write has p50 1.13 ms in the sandbox against 0.80 ms with SQLite on the host, and uses 2.05x the CPU (560 us against 276 us), medians of five runs from `bench/results/request-cost-passB-*.json`. One core serves about half as many write-carrying requests.
- Before the `pread`/`pwrite` change SQLite measured 3.22x (2.21x to 5.14x) in the SQLite microbenchmark and 3x the CPU in the request-cost benchmark, a much larger ratio than today (`docs/benchmarks.md`). Most of that gap was SQLite on WASI reading and writing every page through a WASI stream that wasmtime-wasi runs on another thread; the guest SQLite is now built with `-DHAVE_PREAD=1 -DHAVE_PWRITE=1` (`scripts/guest-cflags.txt`).

If you trust your apps, a host-side SQLite is faster. spared exists to host code nobody reviewed.

## How it differs from others

Only what we read or ran.

- Spin (v4.3.0). Verified from its source: SQLite runs in the host (`crates/sqlite-inproc`, a host-side `rusqlite` connection, exposed as `spin:sqlite@3.1.0`). Instances are pooled and reused, with no per-app serialization. spared keeps SQLite in the sandbox and runs one instance per app. Measured on the same demo app in a separate run (`docs/benchmarks.md`): warm GET at one connection, p50 456 us on Spin against 109 us on spared. Spin's SQLite ran on the host with the delete journal, the app was a wasip3 component that opened its database on every request, and Spin was not rerun alongside workerd and celld.
- celld (v0.6.3). Read from its source and docs: `crates/celld` depends on `rusqlite` with the bundled SQLite, native in the node process, and `docs/security.md` says "celld trusts the application code" and "Do not run code from mutually distrusting tenants in one fleet". Measured on the same demo app in the second workerd comparison run, in `celld dev` mode (single node, local object store), so its cold and write numbers are not like for like. It was also run in production mode against RustFS, an S3-compatible server on loopback with local disk. That is a best case for celld with no network, and celld's docs do not qualify RustFS for production.
- workerd. Measured in the same-app comparison against workerd 1.20261110.1, JavaScript worker with a SQLite-backed Durable Object, same app. Waking an idle app with data on disk and its code in the page cache is close (890 us against 1067 us at p50 in the latest session, 1.08x slower in the first, so no advantage is claimed. After a reboot (cold page cache) the first request took 5.4 ms with no read-ahead, 3.3 ms with the 1 GB default and 1.8 ms against workerd's 2.5 ms with `--prefetch-mb 4096` covering the apps' code), and a first request that seeds the database was slower on spared in all four sessions. Warm requests at one connection were 2.34x, 2.51x, 2.88x and 2.62x lower at p50 across four sessions (121 against 283 us, 108 against 271 us, 162 against 467 us, 148 against 388 us). With 16 connections on one saturated CPU, p99 was 2.85x to 4.57x lower, and with 10% writes at one connection spared's p99 was higher in two of the four sessions. Memory for N different apps, measured as totals: spared 15.4 MiB + 4.4 MiB per app by PSS against workerd 50.9 MiB + 2.6 MiB, so spared uses less up to about 20 apps and workerd less beyond (889 against 574 MiB at 200). By unreclaimable memory (RssAnon) spared is lower at every count (108 against 516 MiB at 200), but a cgroup limit counts page cache, and there spared used 1.84x workerd's memory at 200 apps. Identical apps cost 0.28 MiB each. See `docs/benchmarks.md`. workerd runs JavaScript on V8; spared runs Rust on wasmtime; the comparison is not like for like.
- Cloudflare Durable Objects. spared's one-owner-per-app rule serializes writes the way a Durable Object does. That is a design reference, not a tested equivalence. 'Better data isolation' means one thing: in workerd, the open-source Workers runtime, the SQLite engine behind a Durable Object runs natively in the one process all apps share, while in spared the engine and the database file sit inside each app's own sandbox, so a SQLite bug one app reaches stays in that app. It is a claim about where the database engine runs, not about Cloudflare's hosted service, which was not run.

## Not built yet

- Point-in-time restore. Backups exist (see above), so you can go back to a backup you took, not to an arbitrary moment. Backups can be uploaded to an S3-compatible bucket ([operating](operating.md#off-machine-backups)), but nothing schedules them or deletes old ones in the bucket.
- Failover. `spared move` moves an app while both servers are up. Nothing moves an app off a server that is down, and nothing stops two servers from serving one app: a stale one is only detected when it uploads a backup.
- Multi-node. One host process on one machine.
- WebSockets.
- Cron and scheduled jobs.
- Browser sync. SQLite in Wasm leaves the path open; no code exists.
- Code sharing between apps with different binaries. Identical deployments share one compiled copy (see memory above).
- Users, roles or TLS on the admin API. It takes the admin token, or an app token for one app's routes ([App tokens](#app-tokens)).
- Tracing and OpenTelemetry export. There is an access log and a Prometheus metrics endpoint (see [Observability](#observability)); nothing else.
- Migration code for the data directory. Formats 1 and 2 differ only in whether per-app limits are stored. The stamp and the refusal are built, and an in-place upgrade is not needed yet.
- Limits on disk and network bandwidth per app, and a per-app memory cap set by the operator ([Per-app limits](#per-app-limits)).
- Power-loss durability testing.
- An outside security review.
