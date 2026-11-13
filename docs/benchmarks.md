# Benchmarks

Every spared benchmark is here with what it measures, how to rerun it, the current result and its caveats. Raw output is in `bench/results/`, and each result below names its files. Files with `-tuned` in the name come from the current build, where SQLite is compiled with `pread`/`pwrite` (see [SQLite in Wasm: the pread/pwrite speedup](sqlite-in-wasm-speedup.md)). Files with `oldflags` in the name come from a build without it. Results marked as having no raw file are described in prose only.

Sizes are in KiB and MiB (powers of 1,024) unless a figure is a file size in decimal MB. Percentile columns are p50 and p99 of the samples in a run. Where a run had only 10 or 20 samples, p99 is the maximum. Several results were taken on different builds or in different sessions, and each says which.

## Machine

From `bench/MACHINE.md`. All committed results come from one machine unless noted.

- Linux 6.18.44, x86_64, 4 vCPU Intel Xeon at 2.10 GHz, 16 GB RAM.
- rustc 1.97.0, wasmtime 50.0.1, wasi-sdk 25.0 (clang 19), rusqlite 0.41.1 with libsqlite3-sys 0.39.1, Spin 4.3.0 (83b512f).
- Comparison with workerd: workerd 1.20261110.1 (npm), compatibility date 2026-11-10, Durable Object SQLite on local disk, spared with its access log off (`SPARED_ACCESS_LOG=off`). Server pinned with `taskset -c 0`, load generator with `taskset -c 2`. Loopback HTTP/1.1 keep-alive, no TLS. Latency runs start only when the one-minute load average is under 0.3.
- Native and container baselines: the same Rust toolchain for the host target; Docker 29.9.2 with default settings (cgroup v1, overlayfs storage).
- Not a 1 vCPU / 1 GB cgroup. The wake and density benchmarks were pinned to one CPU by affinity only.

It is a virtual machine. No file measures interference from other tenants of the host, so treat small differences as noise, and compare ratios within one run rather than absolute times across runs. The disk is the VM's virtual disk; whether fsync reaches stable storage was not verified, so latencies that include a durable write (the crash test, the request-cost benchmark and the mixed load in the workerd comparison) may be optimistic for real disks.

## Build once

```sh
cargo build --release -p spared
cargo build --release -p spared --features lab --target-dir target/lab   # the lab binary the harness scripts use; they build it if it is missing
source bench/env.sh        # wasi-sdk 25; override with WASI=/path
```

The harness commands below need the lab build. `cargo build --release -p spared --features lab --target-dir target/lab` puts it at `target/lab/release/spared`, which is where `bench/env.sh` looks (`SPARED_LAB_BIN` overrides the path). Sourcing `bench/env.sh` defines `lab_build`, and the scripts call it to build the lab binary when it is missing.

Guests (`probes/`, `apps/`) build with `cargo build --release --target wasm32-wasip2` in their directory. Scripts write into `bench/work/`, which is gitignored.

Every script that starts `spared serve` sets `SPARED_ACCESS_LOG=off`. The results below were measured before the server had a request log, and the scripts keep that configuration. The log costs one write to stdout per request. Measured on a warm GET with the log on against off (20,000 requests per run, 5 rounds, same server and app), it added 8 us at p50 with one connection (151 against 143 us, +5.6%) and about 109 us with 16 connections (1,968 against 1,859 us, +5.9%). The one-connection difference was not consistent in sign across rounds, while the 16-connection one was positive in every round. A line is about 163 bytes. Only warm GET and one app were measured, and stdout went to a file, so a blocked or slow stdout consumer was not tested.

## Lab subcommands

The benchmark and test harnesses below use subcommands that exist only in a lab build of `spared` (`--features lab`). The release binary does not have them, and never serve apps from a lab build. The `bench/` scripts build the lab binary on first use into `target/lab`, so it does not replace the release binary. `spared <name> --help` prints a harness's flags.

| Subcommand | What it does | Used by |
|---|---|---|
| `spared compile <component> <out>` | Precompile a component to a `.cwasm` file | `bench/wake.sh`, `bench/sqlite-overhead.sh` and others |
| `spared run-probe <component> --data <dir> [args]` | Run a command-style component with a granted data directory | `bench/crash-test.sh`, `bench/sqlite-overhead.sh` |
| `spared bench-wake <component> --data <dir>` | Wake an app repeatedly and time instantiate plus first query | `bench/wake.sh` |
| `spared bench-density <component> --root <dir>` | Deploy many apps, keep some hot, report memory and latency | the density benchmark |
| `spared bench-3b <component> --variant <v> --data <dir>` | The request-cost benchmark against a live instance | `bench/request-cost.sh` |
| `spared escape-test --probes <dir> --root <dir>` | Run the sandbox escape suite | `bench/escape-tests.sh`, CI |

`spared bench <app>`, which measures a deployed app over HTTP, is a user command and is listed in `--help`.

## Consistency after a process kill

- Measures: whether a write acknowledged to the client survives the process being killed mid-write, and whether the database file is still consistent afterwards. The first four rows kill the `spared run-probe` harness. The last two kill `spared serve` itself while clients write over HTTP.
- Rerun: `bench/crash-test.sh`; set `JOURNAL=TRUNCATE` for the shipped mode, `OUT=` names the result file.
- Method: `kill -9` the host mid-write, restart, compare rows against commits acknowledged on stdout. 100 trials per run.
- Result: no committed write was lost and no integrity check failed in 400 harness trials. In 200 `spared serve` trials (100 with one writer, 100 with 16), 138,598 acknowledged writes were lost in none, and no trial failed an integrity check or the restart (`bench/passB-scripts/b6-crash.sh`).

| build | journal | acknowledged commits | trials with lost commits | integrity failures | file |
|---|---|---|---|---|---|
| without `pread`/`pwrite` | WAL | 140,341 | 0 | 0 | `bench/results/crash-test.json` |
| without `pread`/`pwrite` | TRUNCATE | 49,307 | 0 | 0 | `bench/results/crash-test-truncate.json` |
| current | TRUNCATE | 50,620 | 0 | 0 | `bench/results/crash-test-truncate-tuned.json` |
| current | WAL | 165,003 | 0 | 0 | `bench/results/crash-test-wal-tuned.json` |
| current, `spared serve` over HTTP, 1 writer | TRUNCATE | 60,512 | 0 | 0 | `bench/results/crash-serve-passB-w1.json` |
| current, `spared serve` over HTTP, 16 writers | TRUNCATE | 78,086 | 0 | 0 | `bench/results/crash-serve-passB-w16.json` |

- Caveats: killing a process does not drop the OS page cache, so committed pages survive even without an fsync. The test would pass with `synchronous=OFF`, so it checks that the journal leaves a consistent file and does not exercise fsync. It is not a power-loss test. Whether WASI fsync reaches host fsync is untested. The WAL and TRUNCATE runs differ in commit rate and are not like for like. With SQLite's default dotfile locking, 7 of the first 100 trials could not reopen the database, which is why spared uses the `unix-none` VFS; that first run has no raw file in the repo.

## SQLite in Wasm against native

- Measures: CPU cost of SQLite running inside the Wasm sandbox relative to the same SQLite compiled natively.
- Rerun: `bench/sqlite-overhead.sh` (`N=100000`, `REPS=7` by default; `SUFFIX` names the result file, `SPARED_NO_EPOCH=1` turns epoch interruption off).
- Method: the same Rust source and bundled SQLite compiled to Wasm and to native, median of 7 runs, six query shapes, Wasm from a precompiled `.cwasm` at Cranelift opt level Speed.
- Result (`bench/results/sqlite-overhead-passA-epoch.json`, `sqlite-overhead-passA-noepoch.json`): geometric mean 1.85x native with epoch interruption on, 1.54x off, from 1.07x (durable_insert_200) to 2.37x (point_select). SQLite inside the sandbox costs about 1.85x native CPU. The epoch checks cost 12.2% to 33.7% depending on the query shape, 21.0% as a geometric mean over the six shapes. The 1.07x for durable inserts is dominated by fsync noise: the seven Wasm runs span 31,387 to 34,533 us and the native runs 28,493 to 33,245 us, and the tuning harness read 0.95 to 1.1 for the same query.
- Before and after the `pread`/`pwrite` change: SQLite on WASI was originally built without `pread`/`pwrite`, so every page went through a WASI stream that wasmtime-wasi 50 runs on a blocking-pool thread (47,006 futex calls per run against 41 after). That build measured a geometric mean of 3.22x with epochs on and 2.96x off in this unpinned microbenchmark (most of that gap was the harness, see the speedup page), from 2.21x (insert_bulk) to 5.14x (full scan) (`sqlite-overhead-epoch.json`, `sqlite-overhead-noepoch.json`). The guest flags now include `-DHAVE_PREAD=1 -DHAVE_PWRITE=1` (`scripts/guest-cflags.txt`). The write-up is in [sqlite-in-wasm-speedup.md](sqlite-in-wasm-speedup.md).
- Caveats: the comparator is native SQLite with default flags (THREADSAFE=1), which if anything favours Wasm. Native and Wasm use different VFSs. The VM ran the unchanged native binary 30 to 40% slower in some runs than in others, so compare ratios within a run. The ratio is within 2x on the mean but not on every query. A point select is about 1.3 us, so per-request latency barely moves. The epoch-off case (1.54x) cannot ship because the CPU limit needs epochs.

## Request cost

- Measures: latency and CPU for a realistic request, with SQLite inside the sandbox against SQLite on the host behind a WIT call.
- Rerun: `bench/request-cost.sh` (`SUFFIX` names the result files). Host command `spared bench-3b`; guests in `apps/bench3b-sandbox` and `apps/bench3b-host`.
- Method: one request does 10 point reads and 1 durable write (`synchronous=FULL`, TRUNCATE journal) on a 100-row database. Live instance, one pinned CPU, 10,000 requests per run, 5 runs per variant, variants alternated.
- Result (`bench/results/request-cost-passB-{sandbox,host}-run*.json`), medians of 5 runs:

| variant | p50 us | p99 us | CPU us per request |
|---|---|---|---|
| SQLite inside the sandbox | 1132 | 3378 | 560 |
| SQLite on the host | 802 | 2928 | 276 |

- The sandbox adds about 0.33 ms at p50 and uses 2.05x the CPU, so a core serves about half as many write-carrying requests.
- Caveats: this benchmark was designed after the SQLite microbenchmark result was known. It assumes every request is a durable write, which a real disk does not sustain at high rates. Maximum latency shows outliers (39 ms once), and the one-minute load average read 0.54 to 0.64 between runs, from the benchmark itself. The native build uses cc defaults and the default VFS, while the guest uses `unix-none`. The host variant pays JSON encoding per call. CPU is process-wide. Sessions drift: the host-side variant, which the SQLite change did not touch, read 580 us p50 and about 207 us CPU in an earlier session and 802 us and 276 us in this one. Compare variants within a session only, and treat every absolute number here, including the 0.33 ms, as specific to this session.

## Wake to first query

- Measures: how long an idle app takes to wake and answer its first query.
- Rerun: `bench/wake.sh` (`SUFFIX` names the result file). Host command `spared bench-wake`; app `apps/shared-list`.
- Method: per iteration, build a store with a granted `/data`, instantiate the precompiled component, deliver `GET /items`, wait for the response. The app opens its database and runs one query. 100 seeded rows, `taskset -c 0`, 3 runs of 2,000 iterations.
- Result (`bench/results/wake-tuned.jsonl`, repeat `wake-tuned-run2.jsonl`), medians of 3 runs:

| journal mode | p50 us | p99 us |
|---|---|---|
| TRUNCATE | 654 | 2751 |
| WAL, exclusive | 873 | 3063 |

- Waking an idle app to its first query takes about 0.65 ms at p50 with the rollback journal. Instantiation is about 40 to 50 us of that. With WAL the `pread`/`pwrite` change halved the wake (recovery reads and writes pages; `wake-oldflags-paired.jsonl` has the earlier WAL figure of 1745 us at p50).
- Caveats: in process, so no TCP accept or router; one CPU by affinity, not a cgroup; a small database; warm OS page cache.

## Density harness

- Measures: memory, disk and wake cost of 10,000 app directories with a small set of live instances, inside one process. This is an in-process harness and not `spared serve`. It never starts the server.
- Rerun: `taskset -c 0 target/lab/release/spared bench-density <component> --root <dir>` (defaults: 10,000 deployed, 500 hot, 20 rows). Code in `crates/spared/src/lab/density.rs`. There is no wrapper script, and the exact component and flags of the committed run are not recorded in a file, so the run cannot be reproduced exactly as made.
- Method: load one component, then for each of 10,000 apps create a data directory, build a bare store and write 20 rows through an in-process call. Keep 500 instances live, then send 10,000 requests across them with 10% writes. There is no app table, router, listener, deploy API or request log. The CPU limit is off (the epoch deadline is set far in the future) and no memory cap applies. "Deployed" means a directory with a small database exists.
- Result (`bench/results/density.json`): 417 KiB RSS growth per live instance (422 KiB after the requests). Process RSS peaked at 224.6 MiB with 500 instances live, from a 13.0 MiB baseline. Creating the 10,000 apps grew RSS by 7.7 MiB in total. Each app's database took 93 KiB of disk, without a component, manifest or log. Waking a live-set app took p50 0.80 ms and p99 2.2 ms, and a warm request p50 33 us and p99 452 us.
- Caveats: this is not a capacity claim and does not show what `spared serve` does with 10,000 apps. The real server maps every deployed component at start and was never run at that count. One shared component serves all apps, so the result does not predict many different apps, where an evicted app keeps about 4.1 MiB mapped and a never-requested one about 0.67 MiB (see the memory section of the workerd comparison below). Whole-process RSS, not a cgroup. Sequential requests. Tiny databases.

## Sandbox escape tests

- Measures: whether a guest can read another app's data, reach the host, or exceed its limits.
- Rerun: `bench/escape-tests.sh`. Code in `crates/spared/src/lab/escape.rs`; guests in `probes/escape-probe`.
- Method: three probe guests run as app B with its own data directory, with app A's secret beside it. A case passes only on positive evidence.
- Result (`bench/results/escape-tuned.json`, transcript `escape-run-tuned.txt`): 43 cases, 0 failures: 37 attacks denied and 6 controls that had to succeed and did. The same result on the build without `pread`/`pwrite` (`escape.json`). The cases are in `crates/spared/src/lab/escape.rs`.
- Caveats: black-box tests of one configuration against wasmtime 50.0.1, not fuzzing and not an audit. DNS rebinding is fixed by construction, with no rebinding test. The outbound count limit is not in the suite; the deadline is tested by `scripts/server-test.py`. The CPU cap is wall-clock epochs.

## The same app against workerd

- Measures: warm latency, waking an idle app, a first request that seeds the database, mixed read/write load and memory for one small app on spared against the same app on workerd, with celld as an additional reference point. Spin, measured separately, is under [Other hosts](#other-hosts).
- Rerun: `bench/demo/measure.sh [rounds]`. Variables: `TARGETS`, `PARTS`, `OUT`. Check the apps first with `bench/demo/check.sh`. Load generator: `bench/demo/loadgen`. Apps: `apps/demo-spared`, `apps/demo-workerd` (and `apps/demo-celld`). workerd is installed with `npm`, as in the first step of the rerun in [workloads](workloads.md#rerun), into `bench/work/workerd`.
- Method: one small web app (a page from a SQLite-backed list, and a JSON write endpoint), written once for each target. 3 runs per session (5 in session 4), order alternated, median across runs. Warm tests use 20,000 requests against one hot app.
- What "cold" means here: waking an idle app whose data is already on disk. It is not a cold process start. The wake rows send 100 sequential requests on one connection, one to each of 100 apps. For spared those are 100 deployments of the same compiled component, so after the first request the code is already loaded and paged in. For workerd they are 100 Durable Objects of one class in a worker that is already compiled. Both sides measure making one more instance of code that is already in memory. The first request to a different app, which also has to page in about 3.8 MiB of compiled code, is timed in [its own section](#waking-an-app-whose-code-is-not-in-the-page-cache).

The comparison ran in four sessions, because the numbers move between sessions. Tables show median p50 / p99 in microseconds over 3 runs (5 in session 4). The last two columns divide workerd's figure by spared's, so "lower" means spared answered faster. Compare within a session, not across sessions.

Session 4, measured before the later server changes described below, 5 rounds with the order of the two servers alternating (`TARGETS=<one server> OUT=vs-workerd-passA-r<round> PARTS="main cold-existing" bench/demo/measure.sh 1`, `bench/results/vs-workerd-passA-r*-{spared,workerd}-*.json`, 0 errors, steal 0.38% to 0.75% per segment).

| | spared p50 / p99 | workerd p50 / p99 | spared against workerd, p50 | spared against workerd, p99 |
|---|---|---|---|---|
| Wake from idle, data on disk, first byte | 890 / 1908 | 1067 / 3713 | 1.20x lower | 1.95x lower |
| First request of a new app, seeds 100 rows | 4131 / 9864 | 3429 / 8324 | 1.20x higher | 1.19x higher |
| Warm GET, 1 connection | 148 / 405 | 388 / 1289 | 2.62x lower | 3.18x lower |
| Warm GET, 16 connections | 2114 / 5448 | 3270 / 16531 | 1.55x lower | 3.03x lower |
| 90% GET 10% POST, 1 connection | 170 / 1977 | 413 / 1935 | 2.43x lower | 0.98x (level) |
| 90% GET 10% POST, 16 connections | 4181 / 9690 | 4352 / 18377 | 1.04x lower | 1.90x lower |

The wake and first-request rows have 100 samples per round, so their p99 is the 99th sample and moves with one slow request: the per-round p99 ratio of the wake row ran from 0.39 to 3.60. Per-round p50 ratios for warm GET at 1 connection ran from 2.45x to 3.08x.

- After the later server changes (request body budget, data-dir lock, per-write `fstat`), a before and after run of 6 rounds each (`bench/results/e-passE-*`, summary in `bench/results/e-passE-summary.md`) read a warm GET p50 of 162.5 us before and 163.0 us after. Warm throughput, SQLite and 1 MiB POST were within noise. File writes cost about 5.6% more server CPU per request (2988 against 3155 us), which the summary also counts as within noise. The session 4 table above was not rerun.

Session 3, after the SQLite change (`TARGETS="spared workerd" OUT=vs-workerd-set3 PARTS="main cold-existing" bench/demo/measure.sh`, `bench/results/vs-workerd-set3-*.json`). This was the noisiest session: both servers were about 1.5 times slower at p50 than in sessions 1 and 2, and workerd's p99 at one connection was 2,586 us against 456 and 536.

| | spared p50 / p99 | workerd p50 / p99 | spared against workerd, p50 | spared against workerd, p99 |
|---|---|---|---|---|
| Wake from idle, data on disk, first byte | 864 / 1960 | 1072 / 2757 | 1.24x lower | 1.41x lower |
| First request of a new app, seeds 100 rows | 4352 / 11450 | 3595 / 12245 | 1.21x higher | 1.07x lower |
| Warm GET, 1 connection | 162 / 558 | 467 / 2586 | 2.88x lower | 4.63x lower |
| Warm GET, 16 connections | 2442 / 5953 | 3428 / 16967 | 1.40x lower | 2.85x lower |
| 90% GET 10% POST, 1 connection | 188 / 2521 | 473 / 2678 | 2.52x lower | 1.06x lower |
| 90% GET 10% POST, 16 connections | 4545 / 10605 | 4643 / 20094 | 1.02x lower | 1.89x lower |

Session 2, before the SQLite change, with celld 0.6.3 added (`COMPARE_CELLD=1 bench/demo/measure.sh`, `bench/results/vs-workerd-set2-*.json`). celld ran as `celld dev` (single node, local object store, every write waits for the object store), so its numbers are for dev mode only and are not like for like.

| | spared p50 / p99 | workerd p50 / p99 | celld (dev mode) p50 / p99 | spared against workerd, p50 | spared against workerd, p99 |
|---|---|---|---|---|---|
| Wake from idle, data on disk, first byte | 701 / 1634 | 886 / 2427 | 14383 / 32650 | 1.26x lower | 1.49x lower |
| First request of a new app, seeds 100 rows | 2938 / 4679 | 2240 / 4899 | 8402 / 24687 | 1.31x higher | 1.05x lower |
| Warm GET, 1 connection | 108 / 177 | 271 / 456 | 301 / 689 | 2.51x lower | 2.58x lower |
| Warm GET, 16 connections | 1518 / 2218 | 2254 / 8993 | 3064 / 5630 | 1.48x lower | 4.05x lower |
| 90% GET 10% POST, 1 connection | 125 / 1153 | 290 / 810 | 354 / 3266 | 2.32x lower | 1.42x higher |
| 90% GET 10% POST, 16 connections | 3103 / 4801 | 2921 / 6008 | 7463 / 15732 | 1.06x higher | 1.25x lower |

Session 1, also before the SQLite change (`bench/results/vs-workerd-*-run*.json` without a `set` in the name):

| | spared p50 / p99 | workerd p50 / p99 | spared against workerd, p50 | spared against workerd, p99 |
|---|---|---|---|---|
| Wake from idle, data on disk, first byte | 1088 / 1771 | 1010 / 2776 | 1.08x higher | 1.57x lower |
| First request of a new app, seeds 100 rows | 3700 / 6205 | 2393 / 5020 | 1.55x higher | 1.24x higher |
| Warm GET, 1 connection | 121 / 189 | 283 / 536 | 2.34x lower | 2.84x lower |
| Warm GET, 16 connections | 1507 / 2245 | 2348 / 10261 | 1.56x lower | 4.57x lower |
| 90% GET 10% POST, 1 connection | 126 / 1117 | 299 / 825 | 2.37x lower | 1.35x higher |
| 90% GET 10% POST, 16 connections | 3188 / 4924 | 3022 / 5582 | 1.05x higher | 1.13x lower |

What the four sessions say:

- Warm requests at one connection: p50 was 2.34x, 2.51x, 2.88x and 2.62x lower on spared (121 against 283 us, 108 against 271 us, 162 against 467 us, 148 against 388 us). The headline is session 4, 148 against 388 us. Per-run ratios inside session 3 ranged from 2.56x to 3.68x, so that session alone cannot tell 2.5x from 2.9x.
- Warm p99 at 16 connections was 4.57x, 4.05x, 2.85x and 3.03x lower (5.45 against 16.53 ms in session 4). At 16 connections one CPU is saturated, so these figures are mostly queueing. workerd has four threads and was pinned to that one CPU with `taskset`, which also holds V8's background threads. Pinning does not inflate it: in a separate run, workerd's p99 at 16 connections was 15.8 ms pinned to one CPU, 15.6 ms on two CPUs and 13.9 ms unpinned, and its p50 did not move (3.13, 3.16 and 3.19 ms).
- Where spared loses or is level:
  - The first request of a new app that seeds 100 rows was slower at p50 in all four sessions (1.55x, 1.31x, 1.21x and 1.20x higher).
  - Waking an idle app was 1.08x slower at p50 in session 1, and 1.26x, 1.24x and 1.20x faster in sessions 2, 3 and 4. It is close to workerd, and we do not claim a wake win.
  - With one connection and 10% writes, spared's p50 was about 2.4x lower but its p99 was worse in sessions 1 and 2 (1,117 against 825 us and 1,153 against 810 us, 1.35x and 1.42x higher). In sessions 3 and 4 it was level (2,521 against 2,678 us, then 1,977 against 1,935 us).
  - With 16 connections and 10% writes, spared's p50 was slightly higher in sessions 1 and 2 and level in sessions 3 and 4.
- The app on spared is Rust and the apps on the other hosts are JavaScript. workerd's page also pays a hop through a Durable Object stub. The empty-request baseline in [workloads](workloads.md) is 69 us on spared against 106 us on workerd (1.5x), and it does not pass through a Durable Object.
- Memory per hot app, 50 hot apps running the same code (`bench/results/vs-workerd-set2-mem-pss-set2-*.json`, before the current build): identical deployments share one compiled copy, giving 435 KiB of PSS growth per hot app against 470 KiB for workerd and 665 KiB for celld (dev mode, not like for like). The 200-app fit below gives 288 KiB for the same case, from the slope of the whole curve rather than the growth in one 50-app run. Before compiled code was shared, each deployed name mapped its own copy and PSS equalled RSS at about 4.0 MiB (4,120 KiB) per hot app (`vs-workerd-mem-pss-before-same-run*.json`). Apps with different code cannot share, and by PSS they cost 4.4 MiB each.
- Memory curve (`PARTS=curve bench/demo/measure.sh`, `bench/results/memory-curve-*.json`, fits in `bench/results/mcurve-passA-summary.json` from `bench/results/passA-scripts/mem-curve-fit-passA.py`, method in [memory-methodology](memory-methodology.md)): total memory for N = 0, 1, 10, 20, 50, 100 and 200 different apps after each served one request, 5 runs per point (`bench/results/mcurve-passA-*-n*-run*.json`), fitted as fixed + N x slope. The curve is from the current build. The lean build was not rerun, so its figures come from the build before the `pread`/`pwrite` change. By PSS, spared is 15.4 MiB + 4,476 KiB per app (lean build 15.8 MiB + 3,295 KiB) and workerd 50.9 MiB + 2,677 KiB. spared is lower below about 20 apps (about 58 lean), and at 200 apps it totals 889 MiB against 574 MiB. By RssAnon, the memory the kernel cannot drop without swap, spared is 1.4 MiB + 546 KiB against workerd 6.5 MiB + 2,607 KiB, lower at every N (108 against 516 MiB at 200).
- How many hot different apps fit in 1 GiB: 230 on spared (313 lean) and 372 on workerd, counted by PSS. That is the capacity figure. The RssAnon fit gives 1,917 against 399, but it is an out-of-memory bound, not a serving capacity: 1,917 different apps would need about 7 GiB of compiled code pages mapped, so in 1 GiB the kernel would be dropping and re-reading code constantly, and latency in that state was not measured. The cgroup, which is what a container limit counts, agrees with PSS. At 200 apps, spared's cgroup usage after the first request was 1,036.0 MiB against 563.4 MiB for workerd (1.84x), because page cache counts. After 100 requests per app the two were level (1,041.8 and 1,081.3 MiB).
- Idle: an evicted different app on spared keeps about 4.1 MiB mapped (compiled code, reclaimable file pages) and 0.35 MiB of anonymous memory. A deployed app that has never been requested costs about 0.67 MiB at server start. Neither is zero. workerd freed nothing when idle. Identical copies of one app keep about 0.09 MiB each.
- Caveats: one small app, loopback, no TLS, one pinned CPU of one VM, warm OS page cache. JavaScript on V8 against Rust on wasmtime. The durable write paths differ (Durable Object SQLite against `synchronous=FULL` per commit, and we did not check when workerd syncs). workerd is restarted with SIGTERM after a 3 s flush. spared apps are separate deployments, while workerd's are Durable Objects in one worker with one shared V8. celld was measured in `celld dev` mode in the comparison tables and, separately, in production mode (see [Not measured](#not-measured)). Memory-curve caveats: one VM, synthetic variants, no memory pressure, latency when code pages are dropped not measured, workerd after 1,000 requests per worker still growing.

## Waking an app whose code is not in the page cache

- Measures: the first request to each of 499 different idle apps when the operating system has lost their compiled code, as after a reboot (cold page cache), and what the read-ahead pass changes. The 0.9 ms wake in the workerd comparison above is an idle app whose code is still in the page cache.
- Rerun: `SUITE=d2 ROUNDS=5 SPEC=bench/passB-scripts/d-spec.txt bash bench/passB-scripts/d-run.sh` (drivers `d-*.sh` and `d-*.py` in the same directory). Raw files `bench/results/d2-passD-*`, steal time in `d2-passD-steal.jsonl`.
- Method: 500 apps, each with its own compiled component of 6.7 MiB (each embeds SQLite), databases seeded, server pinned to one CPU, one request at a time over one connection on loopback, 5 rounds with the order of the scenarios rotated. Cold-cache protocol: `sync`, `echo 3 > /proc/sys/vm/drop_caches`, start the server, serve app 0, then one GET to each of apps 1 to 499. workerd runs the same 500 apps as 500 workers, each with its own Durable Object database. Local virtio disk, one VM.
- Result, median p50 / p99 in microseconds over 5 rounds:

| scenario | spared p50 / p99 | major faults per wake |
|---|---|---|
| page cache dropped before the start, no read-ahead (`--prefetch-mb 0`) | 5394 / 12030 | 52 |
| same, binary from before the read-ahead pass existed | 5728 / 11124 | 52 |
| read-ahead pass at the default 1024 MB, requests after the pass had finished | 3272 / 7093 | 0 |
| read-ahead pass at 4096 MB, which covers all 500 components, requests after the pass had finished | 1760 / 5201 | 0 |
| default 1024 MB, requests starting as soon as the server is up | 3334 / 8428 | 0 |
| page cache warm | 1617 / 3983 | 0 |
| workerd, page cache dropped before the start | 2532 / 6329 | |
| workerd, page cache warm | 2366 / 6023 | |

- A plain process restart does not drop the page cache, so it gives the warm figure (1.6 ms), not the cold ones. The cold rows apply after a reboot or when the cache is otherwise emptied.
- Without the pass, each first wake takes 52 major page faults and reads 3.7 MB. With the page cache dropped, spared was 2.1x workerd at p50 (5.4 against 2.5 ms).
- When the pass covers the whole fleet (`--prefetch-mb 4096`) and has finished, a first wake is 1.8 ms against workerd's 2.5 ms, near the warm figure of 1.6 ms. Reading the 3.4 GB took 1.4 s and 0.35 s of CPU. The default 1024 MB reaches 151 of the 500 components, so the same test gives 3.3 ms, slower than workerd. A fleet whose components add up to more than `--prefetch-mb` needs a larger value, and the pass never reads more than half of the available memory.
- If the cache is dropped after the server has started (memory pressure later), the pass does not help. The check at first wake reads the file ahead and brings the first request to 3.3 ms from 5.8 ms (workerd 2.2 ms), in a run with the cache dropped after app 0 was served (`d2-passD-n500-b5-*`).
- Cost: the server's anonymous memory did not change. The page cache holds 3.3 GiB of components afterwards instead of 0.45 GiB, and the kernel can reclaim it. On a server pinned to one CPU, a hot app saw p50 about 16% higher and p99 about 2x for the 2 s the pass ran (`d-passD-hot-*`). With two CPUs there was no visible effect.
- Caveats: one app size, loopback, one VM with a local virtio disk, one request at a time. A network disk would make the serial reads cost more. A smaller component reads less. workerd reads its worker code when it starts, so its cost after a reboot sits in its start time, which was not measured. Other kernels and macOS were not measured.

## Native Rust process and container baselines

- Measures: what the sandbox costs against running the same app as a plain native Rust process, and against one container per app. Warm request latency, the time from idle to the first response, and memory for N different apps.
- Rerun: `bench/native-baseline.sh [rounds]` (parts `build check latency memory container`, chosen with `PARTS`; `SKIP_DONE=1` resumes an interrupted memory run). Summary: `bench/demo/native-summary.py` writes `bench/results/native-baseline-summary.json`. App: `apps/demo-native` (its `Dockerfile` holds the container image). Load generator: `bench/demo/loadgen` (style `native`, mode `spawn`). Memory harness: `bench/demo/mem-curve.py` (targets `native-distinct`, `native-same`) and `bench/demo/mem-container.py`.
- The app: `apps/demo-native` is `apps/demo-spared` rewritten for the host with hyper 1.12 and tokio 1.54, with the same routes, SQL, seed rows, pragmas (`journal_mode=TRUNCATE`, `synchronous=FULL`) and JSON, and the same bundled SQLite (rusqlite 0.41.1, libsqlite3-sys 0.39.1), release build with LTO. The script's `check` part compares status, content type and body on `GET /`, `GET /api/items`, a missing path, a bad and a good `POST /api/items`, and both reads again: all identical.
- Method: one VM, one session, spared and native alternating which goes first in each of 3 rounds; medians across the 3 runs. Warm: 20,000 requests against one hot app after 200 warm-up requests per connection, server on CPU 0, load generator on CPU 2, each run after the one-minute load average fell under 0.3. Idle to first response: for spared, 100 apps that already hold data, server restarted, first request to each (as in the workerd comparison); for the native process, 20 starts of the binary on a seeded database, timed from the spawn to the first response byte, the load generator connecting as soon as the port listens; for containers, 10 `docker run -d` of a `FROM scratch` image holding the same app built statically, on the default bridge network with a published loopback port and `--cpuset-cpus 0`, timed from the start of `docker run` to the first response byte. Memory: the method of [the memory methodology](memory-methodology.md) at N = 1, 10 and 50, 3 runs each, snapshot after every app served one GET. spared runs N different apps in one server (`bench/demo/gen-distinct.py`); native runs N processes on CPU 0 in one memory cgroup, each with its own database, either N different binaries built from the same generated variants (`gen-distinct.py --native`) or one binary N times. For containers (N = 1, 10, 30), PSS is summed over the app processes, the containerd shim of each container, the docker-proxy of each published port, and dockerd plus containerd.

Warm and cold latency, median of 3 runs, in us (`bench/results/native-warm-*.json`, `native-cold-*.json`, `container-cold-run*.json`):

| | spared | native process | container |
|---|---|---|---|
| Warm GET, 1 connection, p50 / p99 | 161 / 311 | 59 / 131 | not measured |
| Warm GET, 16 connections, p50 / p99 | 2388 / 5678 | 540 / 1748 | not measured |
| Idle to first response, p50 / p99 (max for native and container, which had 20 and 10 samples) | 935 / 2892 (server restarted, first request to each of 100 apps that share one component) | 5052 / 11290 (start the process) | 183,929 / 204,450 (`docker run` with Docker's defaults) |

Memory for N different hot apps, PSS / RssAnon in MiB, median of 3 runs (`bench/results/native-memory-*.json`, `container-memory-*.json`):

| N | 1 | 10 | 50 |
|---|---|---|---|
| spared, one server | 22.6 / 1.9 | 62.3 / 7.0 | 238.1 / 29.1 |
| native, a process and a binary per app | 3.2 / 0.4 | 28.6 / 4.1 | 138.8 / 20.6 |
| native, a process per app, one binary | 3.2 / 0.4 | 7.8 / 4.1 | 25.0 / 20.7 |
| containers, Docker defaults, one image, with dockerd and containerd (N = 1, 10, 30) | 165.0 / 60.1 | 225.4 / 119.2 | 346.6 / 239.0 at N = 30 |
| containers, Docker defaults, without dockerd and containerd | 18.1 / 5.5 | 68.9 / 55.3 | 179.4 / 165.3 at N = 30 |

Fitted fixed + slope over all runs (`bench/results/native-baseline-summary.json`):

| | PSS | RssAnon |
|---|---|---|
| spared, different apps | 18.3 MiB + 4,503 KiB x N | 1.4 MiB + 566 KiB x N |
| native, a binary per app | 0.7 MiB + 2,831 KiB x N | 0.0 MiB + 421 KiB x N |
| native, one binary | 3.1 MiB + 451 KiB x N | 0.0 MiB + 424 KiB x N |
| containers, Docker defaults, one image, everything | 159.4 MiB + 6,425 KiB x N | 54.3 MiB + 6,338 KiB x N |

The container slope splits into 355 KiB for the app process, 3,844 KiB for its containerd shim, 1,491 KiB for its docker-proxy and 734 KiB of growth in dockerd and containerd. Most of that slope is Docker's own helpers, not the app: the shim is Docker's and the proxy exists only because ports are published through Docker's userland proxy. The container's own memory cgroup, which is what `docker stats` reports, showed 0.65 to 0.85 MiB per container; the shim and the proxy sit outside it. spared's slope in this session (4.40 MiB per app) matches the 200-app curve above (4.37 MiB).

- What it shows: a warm request costs about 100 us more inside spared than in the native process (161 against 59 us at p50, 2.7x). At 16 connections, where one CPU is saturated, the native process was 4.4x faster at p50 (2,388 against 540 us), which is about 6,700 requests per second per core on spared against about 29,600 for the native process, and 3.2x at p99 (5.7 against 1.7 ms). A different hot app costs 4.4 MiB on spared against 2.8 MiB as a native process, so 1.6 MiB more per app, plus spared's 18 MiB server; spared uses more memory than native processes at every N measured, by PSS and by RssAnon (0.55 against 0.41 MiB per app). Most of spared's extra memory is compiled code: an app's compiled component is 6.7 MiB on disk with 3.8 MiB resident, against about 2.8 MiB on disk and 2.4 MiB resident for the native binary of the same app. The sandbox's advantage is the idle path: waking an evicted app took 0.9 ms against 5.1 ms to start the native process and 184 ms for `docker run` with Docker's defaults. Docker 29.9.2 with default settings, a container per app costs 6.3 MiB per app (most of it the containerd shim and the docker-proxy) and about 150 MiB of daemons (dockerd and containerd measured 139.5 to 164.3 MiB before any container started). With the daemons charged, that is more than spared at every N measured, even though all containers here share one image's code. The daemons are a sunk cost on a host that already runs Docker. Without them the containers read 18.1 against spared's 22.6 MiB at N = 1 (less) and 68.9 against 62.3 MiB at N = 10 (more). Other runtimes, `docker start` of a created container and `--network host` were not measured.
- Caveats: the three models do different things when idle. A native process or container that stays running pays no start cost and keeps its memory; spared drops an idle app's instance after 30 s but keeps its compiled code mapped (about 4.1 MiB PSS per different app, see the memory section of the workerd comparison), so idle apps are not free. Process and container start were timed without a supervisor that holds the socket, so a real process-per-app host could hide part of the start. The native app uses SQLite's default `unix` VFS with POSIX locks, spared's guest uses `unix-none`. The native processes share libc pages with each other and with the rest of the machine, which PSS splits; the one-binary rows also share the app's code, which is the native counterpart of spared's identical apps (21.2 MiB + 288 KiB per app on the 200-app curve). Each native process ran tokio with one worker thread, because its CPU affinity was one CPU. The containers all ran one image, so the static binary's pages were shared; a container per different app would add about 2.3 MiB each (the gap between the two native rows). Container latency includes Docker's bridge network and docker-proxy; warm requests through a container were not measured. Polling the published port while `docker run` was still creating the container measured about 1.17 s instead of 0.18 s, because a connection that reaches the port before the container's network is up waits for a TCP retransmission (`bench/results/container-cold-pollduringstart-run*.json`); the table uses the method that waits for `docker run` to return. Docker 29.9.2 with default settings, cgroup v1, overlayfs. Same machine caveats as above: one VM, loopback, no TLS, warm OS page cache.

## Other hosts

- Spin 4.3.0 on the same app, measured in a separate run (`bench/results/vs-spin-*.json`): warm GET at 1 connection 456 / 740 first byte (spared 109 / 195 in that run); at 16 connections 6697 / 16463. The Spin app opened its database on every request. Spin's 100 requests after `spin up` read p50 523 us, lower than spared's wake p50; Spin pools instances and has no per-app idle unload, so that is not the same event. Spin runs SQLite on the host with the delete journal, and one process per app (about 71 MiB RSS). Like-for-like differences: the Spin app opens its database on each request (as the Spin SDK expects) while spared caches the connection and prepared statement for the instance's life; Spin's SQLite calls run on blocking threads off the pinned core; Spin uses the default delete journal mode, spared TRUNCATE.

## Agent deploys

- Measures: whether a fresh coding agent, given a prompt, can write and deploy a working app on spared, and how long it takes.
- Protocol: `bench/agent-deploys/RUN.md`; prompts in `bench/agent-deploys/prompts.json`; checker `bench/agent-deploys/check.py`.
- Result (`bench/results/agent-deploys/runs.jsonl`, `run*-check.json`): 9 of 10 fixed prompts ended in a deployed app that passed a behaviour check; median 25.7 s over the nine (25.8 s over all ten), slowest 55.7 s. Run 10 stopped at a TLS error caused by the test network intercepting HTTPS; a diagnostic rerun passed in 22.5 s and is not counted.
- Caveats: the prompts were written by the authors and state the API precisely; spared was already installed, a server was running and the build cache was warm (a cold first build adds about 40 s); agents checked with curl and `spared test`, never a browser.

## Per-app limits

- Measures: whether the accounting behind per-app limits (`spared limits`) costs anything on a warm request. Before deciding to run it, a regression was defined as a warm p50 higher than the build before by more than the spread between rounds.
- Rerun: `BEFORE_BIN=<release build of b568aeb> bench/limits-scripts/warm-latency.sh` after `scripts/build-examples.sh`. Raw files `bench/results/limits-<variant>-run<N>.json`.
- Method: the notes example, `GET /api/notes` with one row, measured by `spared bench` (20,000 warm GETs on one connection after 20 cold cycles). Server pinned with `taskset -c 0`, client with `taskset -c 2`, access log off. Three variants: `before` (the commit before per-app limits), `after` (this build, no limits set) and `after-limited` (this build with a CPU budget, an egress budget and a data limit set high enough never to throttle, so the bucket is checked and charged on every request). 12 rounds in two batches of 6, the order of the variants rotated each round. The one-minute load average was between 0.7 and 2.1 at the start, from earlier work on the VM, not the 0.3 the comparison runs wait for.
- Result, median over 12 rounds of each round's warm p50 and p99, in microseconds:

| variant | p50 (min to max) | p99 (min to max) |
|---|---|---|
| before | 134 (131 to 146) | 412 (310 to 490) |
| after, no limits | 132 (126 to 157) | 341 (192 to 473) |
| after, limits set | 134 (131 to 140) | 416 (319 to 496) |

- No measurable cost: the three medians are within 2 us of each other, inside the spread between rounds of the same variant. The slowest rounds at p50 (146 us before, 157 us after with no limits) came from both builds, which points at the VM, not the code. p99 moves by about 100 us between rounds of one variant and says nothing either way.
- Caveats: one app, one connection, GET only, loopback, one VM. A budget's check and charge are two short mutex sections per request, which the measurement could not separate from noise. The CPU throttling itself and the egress meter were tested for behavior, not for cost.

## Not measured

- A power-loss test, a 1 vCPU / 1 GB cgroup run, large databases, TLS and a real network.
- The celld column in the tables above is `celld dev` mode, not like for like. celld was also run in production mode, as one node and as a two-node fleet on the same VM, against RustFS 1.0.5, a Rust S3-compatible server on loopback with local disk. That is a best case for celld: no network and no real object-store latency. celld's docs do not qualify RustFS as a production bucket, so treat those runs as an upper bound, not a deployment. Real S3, R2, GCS or Azure buckets and a fleet across machines were not measured. The celld runs cannot be reproduced from this repo, because how to obtain and build celld 0.6.3 is not recorded here.
- Spin was measured in its own run and was not rerun alongside workerd and celld. No wasip2 Spin variant was built.
- Latency when the kernel drops compiled code pages of apps that are being served, as opposed to the first request after a reboot.
- Running Spin apps on spared (an optional compatibility shim) was assessed from Spin's interfaces, not built.
- Against the native process and container baselines: warm requests through a container, a container per different image, a supervisor that starts processes or containers on demand behind a held socket, and any of the three under memory pressure.
