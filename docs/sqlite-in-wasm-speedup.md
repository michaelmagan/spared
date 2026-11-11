# SQLite in Wasm: the pread/pwrite speedup and other tuning levers

Measured 2026-11-10 on the benchmark machine (4 vCPU Xeon 2.1 GHz, wasmtime 50.0.1, wasi-sdk 25, rustc 1.97, rusqlite 0.41.1 / SQLite 3.54.1). Question: how close can SQLite compiled to wasm32-wasip2 get to native SQLite with configuration alone, and what does each lever do to memory per app.

The numbers on this page come from the tuning harness and its own sessions. The published figures are in [benchmarks](benchmarks.md), and they differ because they come from other sessions and, for the SQLite microbenchmark, an unpinned harness. Compare within a session only.

## Result

| | before | after (current build) |
|---|---|---|
| SQLite microbenchmark ratio, geometric mean, run with the original harness (not pinned, WAL) | 3.2 originally; 3.4 to 4.0 remeasured | 1.8 |
| Same, pinned to one CPU, WAL | 2.2 to 2.3 | 1.8 |
| Same, pinned, TRUNCATE (the shipped journal mode) | 2.1 to 2.2 | 1.8 |
| Worst query | scan 5.1 | point_select and index_agg, 2.3 to 2.4 |
| durable_insert_200 (dominated by fsync noise, benchmarks.md reads 1.07) | 2.7 | 0.95 to 1.1 |
| Request-cost benchmark, request (10 reads + 1 durable write): p50, tuning session | 1440 us | 1125 us (host-side SQLite: 789 us) |
| Request-cost benchmark, CPU per request | 882 us | 595 us (host-side SQLite: 300 us) |
| Memory per hot app, 50 different apps (PSS growth after the first request in this harness, not the fitted slope) | 4.0 to 4.3 MiB | 3.8 MiB (unchanged: the code is the same size); 3.0 MiB with `-Os` |

Parity is not reachable with configuration. What is left after this change is Cranelift code against native C (about 1.6x) and epoch interruption (about 12 to 20% more in this pinned harness, against 12 to 34% by query shape and 21% on average in the unpinned microbenchmark in [benchmarks](benchmarks.md)). Both are engine work, see the last section.

Ratios in the "after" column are against the native comparator as the SQLite microbenchmark defined it (libsqlite3-sys defaults). Against a native build with the same SQLite options the mean is 1.70 to 1.76; against one that also has `SQLITE_THREADSAFE=0`, like the guest, 1.83 to 2.15.

## What was wrong: every page went through a thread hop

SQLite turns `pread`/`pwrite` on by itself only for Linux and macOS (`sqlite3.c`, "Use pread() and pwrite() if they are available"). On WASI it falls back to `lseek` plus `read`/`write`. The wasi-libc that Rust links for wasm32-wasip2 implements `read`/`write` on a file with WASI streams (`read-via-stream`, `input-stream.read`, drop; for writes `write-via-stream`, `check-write`, `write`, `blocking-flush`, drop). In wasmtime-wasi 50, `FileInputStream::read` and `FileOutputStream::write` always call `spawn_blocking`, whatever `allow_blocking_current_thread` says (`src/p2/filesystem.rs`). So each SQLite page read or write crossed to a tokio blocking-pool thread and back, in the SQLite microbenchmark harness and in the shipped server.

Evidence, one `bench 100000` run under `strace -f -c`: 4,285 `pwrite64`, 1,868 `pread64` and 216 `fsync` on both sides, plus 47,006 `futex` calls for the wasm guest with stream I/O against 41 once the guest is built with `-DHAVE_PREAD=1 -DHAVE_PWRITE=1`. `pread`/`pwrite` in wasi-libc map to one `descriptor.read`/`descriptor.write` host call that runs on the calling thread. The hop is cheap when the process is pinned to one CPU (a context switch) and expensive when it is not (waking another core), which is why the original, unpinned scan ratio was 5.1 and the pinned one is 2.5. The server is not pinned.

The file syscalls are identical to native; the futex traffic was the difference, so a syscall count alone does not show the gap.

## Method

`bench/tuning/bench.py`: the SQLite microbenchmark workload (`probes/sqlite-probe`, `bench 100000`), variants interleaved and the order reversed every repetition, median of 7 or 9 runs, `loadavg < 0.3` checked before every run. Pinned runs use CPU 3: CPU 0 was in use by another benchmark's pinned server and a first batch there had outliers of 10x (`final-core-wal-cpu0-contended` entries in the result files; not used). Unpinned runs use `taskset -c 0-3`. Guests are built by `bench/tuning/build.sh`. Each SQLite compile option is applied to the wasm guest and to its own native comparator (`LIBSQLITE3_FLAGS`); C compiler flags and engine settings are wasm-only.

`run-probe` (a `--features lab` command) now uses the server's host path (async WASI linker, `allow_blocking_current_thread`). `SPARED_PROBE_LEGACY_IO=1` gives the original harness (sync linker on a plain thread); it appears below as "original harness". Engine settings were tried through an environment knob that is not shipped: `bench/tuning/engine-knobs.patch`.

Noise: `-fno-math-errno` produces a byte-identical guest and still measured 2.27 against 2.08 for the base in the same batch. Differences in the geometric mean under about 8% are not resolved by this method. Verdicts below say "noise" where that applies.

Raw data: `bench/results/sqlite-tuning-<lever>.json` (every run, wasm and native medians, ratios, sizes).

## Levers

Ratios are wasm / native per query; "gm" is the geometric mean, with the base measured in the same batch in brackets. WAL, pinned, unless stated.

### Kept

| lever | scope | gm (base) | insert | point_select | index_agg | scan | update | durable | why kept |
|---|---|---|---|---|---|---|---|---|---|
| `-DHAVE_PREAD=1 -DHAVE_PWRITE=1` | wasm-only; native already has it | 1.86 (2.24) | 1.90 | 2.74 | 2.40 | 1.84 | 1.71 | 1.05 | One host call per page on the calling thread. Unpinned: 1.90 against 3.58 (WAL), 1.79 against 3.39 (TRUNCATE) |
| `SQLITE_DEFAULT_MEMSTATUS=0`, `SQLITE_OMIT_SHARED_CACHE`, `SQLITE_USE_ALLOCA` | both | see below | | | | | | | No change in SQL behaviour. Ratio unchanged because native gains too; the guest's absolute time for insert and point_select fell 7 to 13% with the recommended set |
| `run-probe` on the server's host path | harness | 2.24 (2.24 legacy, pinned); 3.4 to 3.6 (3.5 to 4.0 legacy, unpinned) | | | | | | | The harness should measure what the server does. It makes little difference by itself, because streams hop on both paths |

Final configuration, all four runs (median of 9 pinned, 7 unpinned):

| run | original harness, old flags | server path, old flags | final vs default native | final vs native with same options | final vs same options and THREADSAFE=0 | final, epoch off (vs last) |
|---|---|---|---|---|---|---|
| WAL, pinned | 2.27 | 2.33 | 1.82 | 1.76 | 2.01 | 1.62 |
| TRUNCATE, pinned | 2.14 | 2.18 | 1.80 | 1.74 | 1.83 | 1.59 |
| WAL, not pinned | 3.48 | 3.42 | 1.81 | 1.70 | 2.15 | 1.79 |
| TRUNCATE, not pinned | 3.23 | 3.17 | 1.75 | 1.76 | 1.98 | 1.66 |

Per query, final vs default native: WAL pinned 1.81 / 2.36 / 2.36 / 1.99 / 1.71 / 1.06 (insert / point_select / index_agg / scan / update / durable); TRUNCATE pinned 2.01 / 1.96 / 2.27 / 1.97 / 1.78 / 1.09; WAL unpinned 1.87 / 2.43 / 2.15 / 1.97 / 1.90 / 0.95; TRUNCATE unpinned 1.96 / 1.85 / 2.06 / 1.87 / 1.82 / 1.12.

Durability check on the final build: the `bench/crash-test.sh` procedure, 100 `kill -9` trials each for TRUNCATE and WAL, 0 integrity failures, 0 lost acknowledged commits (`sqlite-tuning-crash-*.json`). `synchronous=FULL`, the journal mode and the `fsync` count per run (216) are unchanged.

### Rejected or left out

| lever | scope | gm (base) | insert | point_select | index_agg | scan | update | durable | verdict |
|---|---|---|---|---|---|---|---|---|---|
| `SQLITE_DEFAULT_MEMSTATUS=0` alone | both | 2.23 (2.17) | 2.36 | 2.46 | 2.66 | 2.71 | 1.93 | 1.52 | noise on the ratio; kept as part of the set above |
| `SQLITE_OMIT_SHARED_CACHE` alone | both | 2.37 (2.17) | 2.32 | 2.81 | 2.85 | 2.90 | 2.13 | 1.56 | noise; kept as part of the set |
| `SQLITE_USE_ALLOCA` alone | both | 2.31 (2.17) | 1.99 | 2.45 | 2.60 | 2.73 | 2.08 | 2.10 | noise; kept as part of the set |
| `LIKE_DOESNT_MATCH_BLOBS`, `DQS=0`, `OMIT_DEPRECATED`, `MAX_EXPR_DEPTH=0`, `OMIT_PROGRESS_CALLBACK` | both | 2.15 (2.17) | 1.85 | 2.84 | 2.67 | 2.67 | 1.77 | 1.50 | noise. `DQS=0` and the LIKE option change SQL behaviour for apps; `OMIT_PROGRESS_CALLBACK` breaks rusqlite's `hooks` feature. Left out |
| All eight recommended options | both | 2.25 (2.17) | 2.02 | 2.37 | 2.63 | 2.65 | 2.06 | 1.90 | ratio flat, absolute guest time better; only the three safe ones ship |
| `SQLITE_OMIT_AUTOINIT` | both | not measurable | | | | | | | traps in `openDatabase` ("uninitialized element"): rusqlite does not call `sqlite3_initialize` on wasm. Never use |
| `SQLITE_THREADSAFE=0` for native | native only | 2.32 (2.17) | 2.35 | 3.04 | 2.92 | 2.24 | 2.35 | 1.43 | makes native about 7% faster, so the fair ratio is that much worse. Needs a shim (`--cfg sqlite_nomutex` in the probe) because rusqlite refuses a single-threaded SQLite off wasm |
| `HAVE_FDATASYNC` | both | 2.03 (2.17); 1.91 (1.87) on the combined build | 1.75 | 2.15 | 2.63 | 2.64 | 1.86 | 1.42 | no gain on this disk; left out, it touches the durability path |
| `SQLITE_DEFAULT_WAL_SYNCHRONOUS` | | not measured | | | | | | | no effect: the helper sets `synchronous=FULL` explicitly |
| Drop FTS3, RTREE, DBSTAT, SOUNDEX | both | 2.15 (2.17) | 1.85 | 2.51 | 2.32 | 2.64 | 2.09 | 1.64 | speed noise; wasm 1.98 to 1.72 MB. Removes SQL features |
| Drop those plus FTS5 and STAT4 | both | 2.28 (2.17) | 1.93 | 2.61 | 2.68 | 2.74 | 2.20 | 1.73 | speed noise; wasm 1.41 MB. Removes FTS5 |
| `-O2` | wasm | 2.10 (2.08); 1.85 (1.87) combined | 2.18 | 2.29 | 2.48 | 2.13 | 2.12 | 1.53 | same speed, 5% smaller; not worth a change |
| `-Os` | wasm | 2.21 (2.08); 2.00 (1.87) and 1.99 (2.01) combined | 2.26 | 2.71 | 2.68 | 2.30 | 2.06 | 1.48 | 0 to 7% slower (at the edge of what this method resolves), wasm 35% smaller, compiled code 27% smaller, 0.8 MiB less memory per app. Trades speed for memory: not applied, see memory section |
| `-Oz` | wasm | 2.34 (2.08); 2.10 (1.87) | 2.60 | 3.11 | 2.86 | 2.32 | 2.17 | 1.42 | 12% slower; rejected |
| `-flto` on the C | wasm | 2.23 (2.08) | 2.07 | 2.00 | 4.28 | 2.27 | 2.07 | 1.48 | SQLite is one translation unit; no gain |
| `-fno-math-errno -fno-trapping-math` | wasm | 2.27 (2.08) | | | | | | | byte-identical output; this row is the noise floor |
| no `-mbulk-memory` | wasm | 2.17 (2.08) | 2.29 | 2.64 | 2.61 | 2.23 | 2.04 | 1.45 | noise; flags stay |
| no `-msimd128` | wasm | 2.02 (2.08) | 2.00 | 2.48 | 2.41 | 2.03 | 1.86 | 1.52 | noise. SIMD changes nothing because the hot libc routines are not SIMD (below) |
| no wasm feature flags | wasm | 2.17 (2.08) | 2.18 | 2.60 | 2.55 | 2.42 | 2.15 | 1.40 | noise |
| `-mllvm -inline-threshold=1000` | wasm | 1.98 (2.01) | 2.48 | 2.80 | 2.32 | 2.01 | 1.91 | 0.97 | no gain, wasm 65% larger; rejected |
| Word-at-a-time `memcmp` forced into sqlite3.c (`bench/tuning/fastmem.h`) | wasm | 2.31 (2.24) WAL; 1.93 (2.01) TRUNCATE | 2.20 | 2.37 | 2.84 | 2.93 | 2.17 | 1.59 | point_select 15% faster in WAL mode only (the WAL header check); nothing in TRUNCATE. Needs a header shipped with the CLI. Left out |
| `wasm-opt -O3` (binaryen 124) | wasm | 2.15 (2.08); 1.79 (1.87) combined; 1.94 (2.00) on `-Os` | 2.21 | 2.48 | 2.81 | 2.29 | 1.99 | 1.40 | about 3% faster and 11% smaller wasm, inside the noise. Cannot run on a component: build the core module (`-C link-arg=--skip-wit-component`), run wasm-opt, then `wasm-tools component new`. Two more tools in the app build; left out |
| Rust profile of the template (opt 2, no LTO, 16 units) | wasm | 2.18 (2.08) | | | | | | | noise: SQLite's time is in C. Template profile stays (fast builds) |
| Rust `opt-level=s`, `panic=abort` | wasm | 2.29 (2.08) | | | | | | | noise on speed, 2% smaller app |
| `Config::table_lazy_init(false)` | engine | 2.14 (2.08); 1.84 (1.87) | | | | | | | noise |
| `Config::compiler_inlining(Yes)` | engine | 2.24 (2.08); 1.87 (1.87); 1.97 (2.00) on `-Os` | | | | | | | no gain; compiled code 70% larger. Rejected |
| Cranelift `SpeedAndSize` | engine | 2.12 (2.08); 1.89 (1.87) | | | | | | | no change in speed or size (identical cwasm size) |
| Epoch interruption off | engine | 1.62 (2.01) WAL, 1.59 (1.83) TRUNCATE | 1.64 | 1.95 | 2.03 | 1.74 | 1.57 | 1.04 | 12 to 20%. Not shipped: the CPU cap needs it |
| `PRAGMA cache_size=-8000` | both | 1.99 (2.08); with pread 1.87 | 2.12 | 2.47 | 2.29 | 1.79 | 2.01 | 1.44 | guest scan 41% faster in absolute time (the 3 MB table no longer misses the 2 MB default cache); native gains too. Costs up to 8 MB per hot app. An app can set it; no default change |
| `PRAGMA temp_store=MEMORY` | both | 2.14 (2.08) | | | | | | | no effect on this workload |
| `mmap_size` | | | | | | | | | unavailable: WASI has no mmap |
| Page size | | not measured | | | | | | | changes the file format of existing databases |
| Winch instead of Cranelift | engine | wasmtime CLI, epoch off | | | | | | | 1.8 to 2.3x slower than Cranelift on insert, point_select and scan. Data point only |
| memory64, relaxed SIMD, tail calls, other proposals | | not measured | | | | | | | none helps: memory64 would add explicit bounds checks; SQLite has no code that the others speed up |

Checked and already right: linear memory is reserved with guard pages, so compiled loads carry no bounds check (disassembly of `memcmp` and `sqlite3VdbeExec` in the `.cwasm`: plain `movzbq (%r15,%rax,1)`); Cranelift opt level is `Speed`; the regalloc alternative (`single_pass`) only trades code quality for compile time; signals-based traps are on. Epoch checks sit at every function entry and loop back-edge; wasmtime exposes no coarser placement.

libc: Rust links its own wasi-libc (built by clang 22, not the wasi-sdk 25 sysroot, which only supplies headers). Its `memcmp` and `strlen` are byte loops; `memcpy` and `memset` switch to `memory.copy`/`memory.fill` above 32 bytes. `-msimd128` on SQLite does not change them.

## Memory per app

Units in this section: KB and MB for memory are the kB that `/proc` reports, so they are KiB and MiB. MB for the size of a wasm or compiled file is decimal.

Harness one (`bench/tuning/mem.py`): the `PARTS=distinct` procedure of `bench/demo/measure.sh` on its own ports. Levers are screened with 16 copies of `apps/demo-spared` made distinct by a unique custom section (different sha256, so each is compiled and mapped separately). Check of that shortcut with 50 apps: 50 generated variants (`gen-distinct.py`) 4,334 KB per app, 50 copies of variant 0 3,911 KB.

Harness two (`bench/demo/mem-curve.py`, page cache dropped, cgroup): 50 generated variants built with the shipped flags and again with `-Os`.

What the number is made of: after one GET, 84 to 100% of an app's compiled code section is resident (4,352 or 5,120 KB of 5,184 KB for the demo), because the kernel maps file pages around each fault in 64 KB steps and SQLite's hot functions are spread over the whole section. Another 0.2 to 0.8 MB of metadata sections is resident from load. Anonymous memory is 0.3 MB. So memory per different app is about the size of its compiled code, and only smaller code moves it. Compiled code by origin for the probe: SQLite core 63%, FTS5 14%, FTS3 10%, Rust 7%, JSON 3%, RTREE 2%. The pages are clean page cache, which the kernel can reclaim under pressure; PSS still counts them.

The same binary measured 4,091, 4,893 and 4,878 KB in three runs: the code section is mapped in large steps. Treat differences under about 0.8 MB between single runs as unresolved.

| lever (demo app) | wasm MB | cwasm MB | PSS growth per hot app, KB (16 distinct copies, each run) | speed gm (base) | verdict |
|---|---|---|---|---|---|
| old flags, `-O3` | 2.19 | 6.94 | 4,091 / 4,893 / 4,878 | 2.08 to 2.33 | baseline |
| shipped flags (`-O3`, pread, three options) | 2.18 | 6.92 | 4,738 / 4,735 | 1.8 to 2.0 | kept: speed up, memory unchanged |
| `-Os` (old flags) | 1.49 | 5.09 | 3,393 / 3,454 / 3,457 | 2.21 (2.08) | trades 0 to 7% speed for about 1 MB |
| shipped flags with `-Os` | 1.49 | 5.08 | 3,509 / 3,517 | 1.99 (2.01), 1.86 (1.83) | same trade; not applied |
| `-Oz` | 1.35 | 4.74 | 3,186 | 2.34 (2.08) | 12% slower |
| `-O2` | 2.07 | 6.63 | 4,810 | 2.10 (2.08) | no gain |
| drop FTS3, RTREE, DBSTAT, SOUNDEX | 1.93 | 6.10 | 4,831; shipped flags 4,802 / 4,809 | 2.15 (2.17) | not resolved at `-O3`; removes features |
| drop those plus FTS5, STAT4 | 1.63 | 5.12 | 3,837 | 2.28 (2.17) | removes FTS5 |
| `-Os` and drop FTS3 group | 1.35 | 4.55 | 3,353; shipped flags 3,394 / 3,397 | 1.99 (2.01) | 0.1 MB beyond `-Os` |
| `-Os` and drop all optional modules | 1.17 | 3.93 | 3,172 | not measured | smallest: about 1.5 MB under baseline |
| Rust `opt-level=s`, `panic=abort` | 2.14 | 6.85 | 4,819 | 2.29 (2.08) | no gain: Rust is 7% of the code |
| Rust template profile | 2.29 | 7.33 | 4,820 | 2.18 (2.08) | no gain |
| Cranelift `SpeedAndSize` | 2.19 | 6.94 | 4,335 | 2.12 (2.08) | no change in code size |
| `native_unwind_info(false)` and `generate_address_map(false)` | 2.19 | 5.96 | growth 5,749 / 5,769; 490 KB less per deployed app before any request; total with 16 hot apps 113 MB against 94 to 107 MB | not measured (metadata only) | cwasm 14% smaller and less resident at load, but more of the code section ends up mapped, so the total did not fall. Not applied. Old compiled components still load under the changed settings |
| `wasm-opt -O3` | 11% smaller wasm, 2% smaller cwasm (probe) | | not measured | 1.79 (1.87) | too small to matter for memory |

Memory curve harness, 50 generated variants, one run each (`sqlite-tuning-memcurve-*.json`):

| build | cwasm total | PSS at startup | after first request | growth per app | steady (100 requests per app) | cgroup usage, steady |
|---|---|---|---|---|---|---|
| shipped flags, `-O3` | 348.6 MB | 48.3 MB | 243.1 MB | 3,896 KB | 247.5 MB (4,951 KB per app all-in) | 272.8 MB |
| shipped flags, `-Os` | 256.2 MB | 46.8 MB | 201.6 MB | 3,096 KB | 206.1 MB (4,121 KB per app all-in) | 233.7 MB |

`-Os` is the one lever that clearly helps memory (0.8 MB, 20% of the growth per app) and it costs speed that this method can only bound (0 to 7%). Nothing found helps both. To switch: replace `-O3` with `-Os` in `scripts/guest-cflags.txt`. Making a different app much cheaper than that needs the engine work below.

## Best combined configuration

Shipped: `-O3 -mbulk-memory -msimd128 -mnontrapping-fptoint -mmultivalue -mreference-types -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION=1 -DHAVE_PREAD=1 -DHAVE_PWRITE=1 -DSQLITE_DEFAULT_MEMSTATUS=0 -DSQLITE_OMIT_SHARED_CACHE -DSQLITE_USE_ALLOCA` plus the WASI emulation defines; engine settings unchanged. Ratio 1.75 to 1.82 against default native (1.70 to 1.76 against native with the same options), worst query 2.4; 3.8 MiB per different hot app (growth after the first request).

If memory per different app matters more than the last few percent of speed: the same with `-Os`. Ratio 1.86 to 2.06 against the THREADSAFE=0 native in the four runs (the `-O3` build measured 1.83 to 2.15 against the same comparator), 3.0 MiB per app.

Existing apps keep their old compiled component until they are redeployed; the gain comes with the next `spared deploy`.

## What it would take to reach parity

Not configuration. In order of size:

1. Code generation, about 1.6x with epochs off. Cranelift does no loop optimisation and its register allocation suffers in SQLite's large functions (`sqlite3VdbeExec` compiles to 145 KB of machine code with about 10,000 stack-slot accesses). An LLVM-class optimising backend for Wasm typically lands at 1.1 to 1.4x native; wasmtime has none, so this means another engine or upstream Cranelift work. Not measured here.
2. Epoch interruption, 12 to 20% in this harness. The check runs at every function entry and loop back-edge. Cheaper would be checks on back-edges and non-leaf entries only, or interruption by signal; wasmtime offers neither. The CPU cap stays.
3. The WASI file path. A page read is now one host call, with a buffer allocated in the guest (`cabi_realloc`) and two copies. A host-provided VFS interface that reads straight into guest memory would remove that; it showed as roughly 15% of the scan samples after tuning. Upstream alternative: make wasmtime-wasi file streams honour `allow_blocking_current_thread`, which would fix apps that do not use this build's flags.
4. SIMD libc routines (`memcmp`, `strlen`) in the libc Rust links.

For memory per different app: every app carries its own compiled SQLite (about 4 MB of the 5.2 MB code section). Sharing it needs the engine to compile and map SQLite once and link it into each app (content-addressed compiled core modules); wasmtime compiles and stores a component as one unit today. A linker-level alternative is ordering hot functions together so that one request maps a fraction of the code; no tool in this toolchain does that for Wasm.
