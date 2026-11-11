# Memory: spared against workerd, total per host

This page compares the memory of spared and workerd when one host serves many apps. It gives the method first, then the results and the conclusion.

## Why totals, not per-app growth

Dividing the growth from 1 to 50 apps by 49 (`vs-workerd-mem-distinct-*.json`) gives 4,000 KiB per different app on spared against 508 KiB per different worker on workerd. That leaves out what each system needs before its first app. workerd carries V8, its runtime and one native SQLite for every worker, and pays for them once, in its baseline. spared compiles SQLite into every app, so it pays for SQLite again in every app's slope. A comparison of slopes alone charges spared for its SQLite and lets workerd's SQLite go free. What a host operator pays for is the total.

## What we measure

For a host serving N different apps, total(N) is the physical memory held by the server's process tree once every one of the N apps has served at least one request. We fit

    total(N) = fixed + N x slope

for each system by least squares over all measured points with N from 1 to 200. `fixed` is what the host needs before any app (runtime, compiler, shared libraries, and for workerd its SQLite and V8). `slope` is what each further app adds. We also measure N = 0 directly, to check the fitted intercept against a real empty server.

From the two fits we report the crossover N at which the totals are equal, and how many apps fit in 1 GB and 4 GB: (budget - fixed) / slope.

## Which memory

The headline is PSS (proportional set size) summed over every process in the server's tree, read from `/proc/PID/smaps_rollup`. PSS counts a page shared between k processes as 1/k to each, so libc pages that every process on the machine maps are not charged in full to the server, and pages shared inside one process are counted once. Both servers are a single process here (the harness records the tree, so a child process would be counted).

We also report two components of RSS separately, from `/proc/PID/status`, because a megabyte of each does not cost the same:

- RssAnon: heap, stacks, JIT code, Wasm linear memory, SQLite page caches. The kernel cannot drop these pages without swap, and the hosts we target run without swap. This is the memory that decides when a box runs out.
- RssFile: clean pages mapped from files (the server binary, shared libraries, spared's compiled component files). Under memory pressure the kernel can drop a clean file page and read it back from disk on next use. It is cheap memory while there is room and slow memory when there is not, but it does not by itself cause an out-of-memory kill.

We fit fixed and slope separately for PSS, RssAnon and RssFile (and the PSS split into anonymous and file parts). Crossovers and apps-per-GB are given for PSS and for RssAnon. RssAnon is the more generous accounting for spared, because most of spared's per-app cost is file-backed code; PSS is the stricter one. We give both and say which is which.

## What "hot" means on each system

- spared: an app's instance (its Wasm memory, its open SQLite connection and SQLite page cache) lives while requests arrive and is dropped after the idle window (`--idle-secs`, 30 seconds by default). The compiled component file stays on disk; whether its pages stay mapped after the instance is dropped is part of what we measure. We expected nothing of the app to remain in anonymous memory after eviction; the results show that some does.
- workerd: every configured worker's script is parsed and compiled when the process starts and stays loaded for the life of the process. A Durable Object instance (with its SQLite database handle) is created on first request and may be evicted after inactivity; we measure whether it is.

So "all hot" is not the same state on the two systems, and neither is "idle". We measure both at the same points in time and report each.

## States measured, at every N

1. startup: server started, no request yet. workerd has compiled every worker at this point; spared has compiled nothing beyond reading its app list (compilation to native code happened at deploy, on disk). This state is reported but not used for the comparison, because the two systems have done different amounts of work by then.
2. first: every app has served one GET of its page. On both systems every app now has its code loaded, its database created and seeded with 100 rows, and a live instance. This is the state we compare.
3. steady: every app has served 100 requests in total (90 GET of the page, 10 POST that insert a row), so caches, allocator arenas and JIT tiers have had a chance to grow.
4. idle: after steady, spared's instances are dropped through the admin evict endpoint (`POST /v1/apps/<name>/evict`, the same action the idle timer takes; one run is also checked with the real timer), and both servers are left without requests. Snapshots at 15 seconds and 65 seconds. workerd is left alone: whatever it frees on its own is what we report.

The server is started fresh for each point, so no point inherits allocator state from a larger one.

## Baselines

- spared at N = 0 is the server process with an empty data directory.
- workerd at N = 0 is a workerd process with a config that has no worker (only the disk service). If workerd will not run such a config, a config with one trivial worker (a fetch handler that returns a string, no Durable Object, no SQLite use) is used, and that worker's own cost is estimated as the difference between one and two trivial workers and subtracted.
- The fitted intercept is the number used for crossovers; the measured N = 0 is a check on it. If the two disagree by more than noise, we say so, because it means the curve is not straight near zero.

## What counts and what does not

Counted: every page mapped by the server process tree, in proportion (PSS).

Not counted in the headline:

- Kernel page cache for files the server read but does not have mapped (database files read with `pread`, component files after they are unmapped). It is reclaimable, shared with every other reader, and it survives the process. We report it separately through a per-point memory cgroup (v1): the server runs alone in its own cgroup, the page cache is dropped before it starts, and `memory.stat` gives `rss` (anonymous), `cache` and `mapped_file`, so page cache attributable to the server is visible without being added to the headline.
- The load generator: requests come from a separate Python process outside the server's tree and outside its cgroup.
- tmpfs: neither server's data is on tmpfs (data directories are on the root disk); `/dev/shm` usage is recorded before and after each point to confirm nothing lands there.
- Files on disk (compiled components, databases). Disk is a separate budget.

Possible double counting we check: spared's shared component store keeps one file per distinct component and a hard link under each app; PSS counts a mapped page once however many names point to its file. Shared libraries mapped by both the server and other processes on the machine are split by PSS rather than counted in full. A per-mapping breakdown (server binary, shared libraries, component files, database files, anonymous) is recorded for every point so the sums can be checked by category, and the cgroup's `rss` is compared with the sum of RssAnon.

## Apps

- spared, different apps: generated variants of `apps/demo-spared` (`bench/demo/gen-distinct.py`), each with its own page text and a different number of extra routes with different code, so no two compiled components are equal (sha256 recorded). The demo uses rusqlite with the bundled SQLite as the crate ships it (FTS3, FTS5, R-tree, JSON and others enabled), Rust opt-level 3.
- spared, lean: the same variants and the same handler, built size-optimized (Rust opt-level "s", panic abort, SQLite at -Os with the optional modules the demo does not use turned off: FTS3, FTS5, R-tree, dbstat, JSON, STAT4, soundex, column metadata, deprecated APIs, shared cache). `gen-distinct.py --lean`. This shows how low spared's slope goes with build settings alone, without changing the server. serde and serde_json are kept; dropping them would save a little more.
- spared, identical apps: one component deployed under N names, which the component store shares. This is the other end of the range.
- workerd, different workers: one workerd process with N workers generated from `apps/demo-workerd` (`bench/demo/gen-distinct-workerd.py`), each with its own script text, page and SQLite-backed Durable Object class, matching the spared variants route for route.

N = 0, 1, 10, 20, 50, 100 and 200, five runs at every N, snapshots startup, first and steady. The lean, identical-app and idle rows below come from an earlier run on the build before the `pread`/`pwrite` change at N = 0, 1, 5, 10, 25, 50, 100 and 200 (three runs at N = 0, 1, 10, 50 and 100, one elsewhere, idle snapshots on the first run only). One measurement at a time on one 4 vCPU VM; memory does not need a quiet machine, so builds may run alongside. The harness is `bench/demo/mem-curve.py`, driven by `PARTS=curve bench/demo/measure.sh`; raw files are `bench/results/mcurve-passA-*.json` and, for the earlier run, `bench/results/memory-curve-*.json`.

## Results

Measured on one 4 vCPU, 16 GB VM: spared on the current build (binary sha256 `87d77883…`), workerd 1.20261110.1 (`workerd 2026-11-10`), every request answered with 2xx, raw files `bench/results/mcurve-passA-<target>-n<N>-run<R>.json`, fits from `bench/results/passA-scripts/mem-curve-fit-passA.py` in `bench/results/mcurve-passA-summary.json`. The lean build, the identical-app curve and the idle snapshots were not rerun: their rows, marked as earlier, come from the build before the `pread`/`pwrite` change (binary sha256 `7d06cd9f…`, `bench/results/memory-curve-*.json`, fits from `bench/demo/mem-curve-fit.py` in `memory-curve-summary.json`), which read about 1% lower than the current build at N = 50 in a check at the time (241,813 against 244,208 KiB PSS). Sizes are in KiB and MiB (1 GB below means 1 GiB).

### Totals, PSS / RssAnon in MiB, median of 5 runs, current build

| N different apps | 0 | 1 | 10 | 20 | 50 | 100 | 200 |
|---|---|---|---|---|---|---|---|
| spared, after first request | 10.6 / 1.1 | 19.3 / 1.8 | 58.1 / 6.7 | 102.5 / 12.1 | 234.4 / 28.1 | 453.7 / 54.8 | 889.1 / 108.0 |
| workerd, after first request | 25.2 / 6.4 | 53.6 / 9.3 | 77.2 / 32.6 | 103.2 / 57.4 | 181.0 / 133.3 | 312.7 / 260.5 | 573.7 / 516.2 |
| spared, after 100 requests each | | 19.6 / 2.0 | 59.2 / 7.0 | 104.6 / 12.4 | 239.1 / 28.9 | 462.4 / 56.1 | 903.3 / 110.4 |
| workerd, after 100 requests each | | 60.8 / 12.4 | 108.2 / 59.1 | 160.1 / 110.1 | 315.9 / 264.0 | 578.1 / 521.7 | 1091.0 / 1031.6 |

The N = 0 column is the empty server for spared (10.6 MiB PSS, 9.2 to 10.8 over the five runs) and a workerd with no worker (25.2 MiB); workerd exits when it has no socket, so its empty config serves the disk service on one socket.

### Totals, PSS / RssAnon in MiB, median of runs, earlier build

| N different apps | 1 | 5 | 10 | 25 | 50 | 100 | 200 |
|---|---|---|---|---|---|---|---|
| spared lean, after first request | 20 / 2 | 33 / 4 | 49 / 6 | 95 / 14 | 175 / 27 | 337 / 52 | 662 / 103 |
| spared identical apps, after first request | 22 / 2 | 22 / 3 | 24 / 4 | 28 / 9 | 35 / 16 | 50 / 30 | 77 / 58 |
| spared, 65 s after eviction | 21 / 2 | 38 / 3 | 59 / 5 | 121 / 10 | 224 / 19 | 429 / 36 | 843 / 71 |
| workerd, 65 s without requests | 61 / 12 | 82 / 33 | 109 / 60 | 189 / 137 | 315 / 266 | 578 / 526 | 1080 / 1025 |

### Fitted fixed + slope (N = 1 to 200, all runs)

| | PSS | RssAnon | RssFile |
|---|---|---|---|
| spared, after first request | 15.4 MiB + 4,476 KiB x N | 1.4 MiB + 546 KiB x N | 16.0 MiB + 3,946 KiB x N |
| workerd, after first request | 50.9 MiB + 2,677 KiB x N | 6.5 MiB + 2,607 KiB x N | 46.2 MiB + 70 KiB x N |
| spared, after 100 requests each | 16.1 MiB + 4,550 KiB x N | 1.6 MiB + 557 KiB x N | 16.5 MiB + 4,009 KiB x N |
| workerd, after 100 requests each | 56.9 MiB + 5,301 KiB x N | 7.9 MiB + 5,244 KiB x N | 50.8 MiB + 57 KiB x N |
| spared lean, after first request (earlier build) | 15.8 MiB + 3,295 KiB x N | 1.3 MiB + 519 KiB x N | 17.0 MiB + 2,792 KiB x N |
| spared identical apps, after first request (earlier build) | 21.2 MiB + 288 KiB x N | 1.6 MiB + 287 KiB x N | 21.8 MiB + 136 KiB x N |
| spared, after eviction (earlier build) | 17.4 MiB + 4,223 KiB x N | 1.4 MiB + 358 KiB x N | 18.4 MiB + 3,864 KiB x N |
| workerd, idle (earlier build) | 58.5 MiB + 5,248 KiB x N | 9.4 MiB + 5,218 KiB x N | 51.3 MiB + 29 KiB x N |

The largest residual of any after-first-request fit is 1.8 MiB (PSS, for both servers, and 2.6 MiB for spared lean on the earlier build), so those lines are straight over 1 to 200 apps. They are not straight down to zero: the fitted intercepts are above the measured empty servers (15.4 against 10.6 MiB for spared, 50.9 against 25.2 MiB for workerd), because the first app pulls in code pages of the server binary that an empty server never touches (wasmtime's runtime for spared, V8 for workerd). A workerd with one trivial worker (no Durable Object) already holds 49.9 MiB PSS, so workerd's fixed cost is about 50 MiB as soon as it serves any JavaScript. We use the fitted intercepts.

### Crossover and capacity, after every app has served its first request

- PSS: spared's total is lower than workerd's below about 20 different apps, and higher above (15.4 + 4.37 N against 50.9 + 2.61 N MiB). With the lean build the crossover moves to about 58 apps.
- RssAnon: spared is lower at every N, by a growing margin: 1.4 MiB + 0.53 MiB per app against 6.5 MiB + 2.55 MiB per worker.
- After 100 requests per app, spared is lower at every N on both accountings, because workerd's per-worker heap kept growing (2.6 to 5.1 MiB) while spared's stayed flat.

Hot different apps that fit in a memory budget (fitted, after first request, in brackets after 100 requests each, the lean and identical-app rows from the earlier build). The PSS columns are the capacity figures. The RssAnon columns are an out-of-memory bound and not a serving capacity: at 1,917 different apps in 1 GiB, about 7 GiB of compiled code would have to be paged in from disk, so the kernel would be dropping and re-reading it constantly, and latency in that state was not measured. A container memory limit counts page cache, and in the cgroup at 200 apps spared used 1.84x workerd's memory after the first request.

| | 1 GB, PSS | 4 GB, PSS | 1 GB, RssAnon | 4 GB, RssAnon |
|---|---|---|---|---|
| spared | 230 (226) | 933 (918) | 1,917 (1,878) | 7,679 (7,524) |
| spared lean | 313 (310) | 1,268 (1,257) | 2,019 (2,009) | 8,086 (8,046) |
| spared, identical apps | 3,569 (3,518) | 14,503 (14,299) | 3,647 (3,597) | 14,608 (14,407) |
| workerd | 372 (186) | 1,547 (780) | 399 (198) | 1,606 (798) |

### What happened when apps went idle

- spared: the evict endpoint and the real idle timer (`--idle-secs 10`, one run at N = 50, `memory-curve-spared-distinct-n50-run1-idletimer.json`) gave the same memory within 0.1 MiB. Eviction released the instances' anonymous memory (565 down to 358 KiB per app), but the compiled component stays mapped: about 3.8 MiB of file-backed pages per different app remain, plus about 270 KiB of anonymous memory per different component that is not released (identical apps keep 87 KiB per app). So an evicted different app still costs 4.1 MiB PSS, of which 0.35 MiB cannot be reclaimed without swap.
- spared at startup maps every deployed component before any request: a deployed, never-requested different app costs about 0.67 MiB PSS (0.23 MiB anonymous; fitted from the startup snapshots). Identical apps cost nothing measurable at startup.
- workerd freed nothing in 65 seconds: the Durable Objects and the isolates' heaps stayed (idle equals steady within 3 MiB at N = 200).

### Is 100 requests a steady state?

For spared, yes: at N = 10, 1,000 requests per app gave 63.6 MiB PSS against 63.3 MiB after 100 (`memory-curve-*-n10-run1-steady1000.json`). For workerd, no: 1,000 requests per worker took 10 workers from 109 to 238 MiB PSS (RssAnon 61 to 182 MiB), about 15 MiB per worker, and still growing. V8 sizes each isolate's heap from its allocation rate and collects lazily when the machine has room; we did not find where it stops, and it may give memory back under pressure, which this test did not apply. So the "after 100 requests" rows favour spared by an amount that depends on V8's heuristics, and we do not use them for the headline.

### Checks for double counting and omissions

- Both servers ran as one process with no children in every point (`pids` = 1 in every snapshot); spared used 6 to 7 threads, workerd 4.
- The cgroup's `rss` equals the sum of RssAnon to the KiB in every after-first-request snapshot (for example 110,576 KiB for spared and 528,608 KiB for workerd at N = 200).
- Shared libraries: 2.5 MiB RSS, 0.3 MiB PSS in both servers, so PSS charges them only their share.
- spared's component store: each different component is mapped about three times by one process; RssFile counts those pages for each mapping while PSS counts them once, which is why spared's RssFile slope (3,946 KiB) is a little above its PSS file slope (3,930 KiB) and the identical-apps case shows 136 KiB RssFile but 1 KiB PSS file per app.
- Page cache not mapped by the server is outside the headline. At N = 200 the cgroup held 917 MiB of page cache for spared, of which 769 MiB was mapped (already in PSS). The other 148 MiB (compiled files read but not mapped, databases) is reclaimable and would add about 0.7 MiB per app if counted. workerd held 35 MiB of page cache. Compiled code on disk is 6.7 MiB per different app for spared (3.7 MiB lean) against 19 KiB of JavaScript per worker. Because a container limit counts page cache, spared's cgroup usage at 200 apps after the first request was 1,036.0 MiB against workerd's 563.4 MiB (1.84x). After 100 requests per app the two were level (1,041.8 and 1,081.3 MiB).
- `/dev/shm` was empty in every snapshot. The load generator (this harness, a Python process) is outside the server's process tree and cgroup.
- Binaries: a trial run showed spared's binary at half its PSS because another process on the machine had the same file mapped; the measurement used private copies of both binaries, and the breakdown shows each binary charged in full.

## Conclusion

Measured as total memory for a host, the two systems trade places depending on what you count.

- Few apps: spared uses less in every state and on every accounting. Its server needs about 15 MiB once it serves an app, against about 51 MiB for workerd, whose V8 and runtime are paid once whether there is one worker or many.
- Many different apps, all hot, counted by PSS right after each has served a request: workerd uses less above about 20 apps (58 with a size-optimized guest build), because each spared app maps its own compiled SQLite and Rust code (about 3.8 MiB) while each workerd worker adds about 2.6 MiB of V8 heap. At 200 apps: 889 MiB against 574 MiB.
- Many different apps counted by memory the kernel cannot take back (RssAnon): spared uses less at every N, about a fifth of workerd per app (0.53 against 2.55 MiB). Most of spared's per-app memory is clean file-backed code that the kernel can drop under pressure and read back from disk at the cost of slower next requests; workerd's per-worker memory is heap.
- Many apps, all hot and busy: after 100 requests each, workerd's heaps had doubled and spared was lower at every N on both accountings; with more requests workerd grew further. This depends on V8's collection heuristics on a machine with free memory, so we do not lean on it.
- Many apps, few hot: an idle spared app releases most of its anonymous memory but keeps its compiled code mapped (4.1 MiB PSS, 0.35 MiB anonymous per different app), and a deployed app that has not been requested since the server started costs about 0.67 MiB. workerd keeps every configured worker's isolate from startup and frees nothing when idle. Neither system returns an idle different app to zero; on unreclaimable memory spared's idle apps cost about a fourteenth of workerd's.
- Many instances of the same app: spared is far lower on every accounting (288 KiB PSS per app).

## Limits

One VM, one measurement at a time while unrelated builds ran (memory does not need a quiet machine; the residuals stayed under 3 MiB). The apps are small synthetic variants of one demo (6.7 MiB of compiled code each on spared, 19 KiB of JavaScript each on workerd), generated from one template on each side; real apps with more code would raise both slopes, real JavaScript with larger heaps would raise workerd's. Steady state is 100 requests per app at one connection, with no memory pressure, so neither V8 nor the kernel was pushed to give anything back; we did not measure what happens to latency when the kernel drops spared's file-backed pages. workerd here is the open-source runtime in one process, not Cloudflare's production system, which shares and evicts isolates differently. The lean build changes only build settings (Rust opt-level "s", panic abort, SQLite -Os without optional modules); serde was kept.
