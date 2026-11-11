# Workloads: Rust on spared against JavaScript on workerd

## Purpose

The same-app comparison against workerd covered one small app. This page tests more cases. If a backend request does real work (sorting, parsing, hashing, rendering, a database scan), how long does it take as an idiomatic Rust app on spared compared with an idiomatic JavaScript worker on workerd? We ran 14 tasks and one empty-request baseline. The table lists every one, including those JavaScript wins.

The rules we held to:

- The JavaScript is what a JavaScript developer would write by default: built-ins (`Array.prototype.sort` with a comparator, `JSON.parse`, `RegExp`, `Map`, typed arrays, `crypto.subtle`, `CompressionStream`) and the popular library where one is normal (`marked` for markdown). No WebAssembly. Each side uses its default idiom, with one exception on the Rust side that is named in the sort bullet below.
- The Rust is plain too: the common crates with default settings (`serde_json` with typed structs, `regex`, `sha2`, `pulldown-cmark`, `flate2`) and the standard `HashMap`. The one choice that is not the default is `sort_unstable` in the sort workload.
- Both sides generate their inputs in code with the same 32-bit LCG and the same seeds, so they process identical bytes. `bench/workloads/verify.py` calls every route on both servers and compares the JSON. All 15 outputs match. Markdown matches byte for byte (the HTML from `marked` and `pulldown-cmark` has the same SHA-256 for this document). For gzip the compressed bytes differ by design (different deflate implementations); the script decompresses both and checks they equal each other (`bench/results/workloads-verify.json`).
- Each route returns a small JSON result, so response size is not what is measured. Inputs are generated once per instance and cached; every request does the full task.

Code: `apps/workloads-spared/src/main.rs` and `apps/workloads-workerd/worker.js`, written as twins, one function per workload.

## Results

The suite was run on both servers in one sitting with the current build, where spared's SQLite is built with `pread`/`pwrite`: `OUT=workloads-passA bench/workloads/run.sh 5`. Each figure is the median of five runs of that run's p50 and p99 (full response, one connection). Raw files are `bench/results/workloads-passA-{spared,workerd}-<workload>-run{1,2,3,4,5}.json`, summary `workloads-passA-summary.json`, and the outputs were verified equal again (`workloads-passA-verify.json`, 15 of 15 MATCH). Every one of the 150 timed runs started with the load average below 0.3. The CPU columns are server CPU time per request (all server threads, from `/proc/<pid>/task/*/schedstat`). Ratio is workerd time divided by spared time, so above 1 spared answered faster. "Level" means within 25% either way. The last column is the ratio from the earlier build, shown in the next section.

| workload | spared p50 ms | workerd p50 ms | ratio (p50) | spared p99 ms | workerd p99 ms | spared CPU ms | workerd CPU ms | ratio, earlier build |
|---|---|---|---|---|---|---|---|---|
| sort | 14.48 | 87.48 | 6.04 | 17.29 | 97.61 | 14.25 | 87.19 | 6.17 |
| markdown | 0.83 | 4.19 | 5.05 | 1.29 | 12.43 | 0.77 | 4.48 | 4.59 |
| SQLite read and aggregate | 15.68 | 61.27 | 3.91 | 22.74 | 83.49 | 15.54 | 61.07 | 4.24 |
| SQLite inserts | 10.69 | 37.98 | 3.55 | 19.15 | 102.80 | 9.05 | 38.83 | 2.81 |
| pixels | 2.51 | 7.11 | 2.84 | 3.29 | 10.75 | 2.39 | 6.21 | 2.89 |
| word frequency | 20.99 | 45.16 | 2.15 | 30.64 | 61.52 | 20.81 | 43.67 | 1.81 |
| HTML table | 2.62 | 4.94 | 1.89 | 4.01 | 7.83 | 2.53 | 4.82 | 1.94 |
| regex | 14.04 | 22.25 | 1.58 | 21.18 | 29.55 | 13.74 | 22.03 | 1.56 |
| empty request (baseline) | 0.07 | 0.11 | 1.54 | 0.16 | 0.43 | 0.05 | 0.11 | 1.84 |
| n-body | 14.54 | 18.03 | 1.24 | 19.12 | 25.91 | 14.43 | 17.95 | 1.21 |
| sieve | 10.94 | 8.81 | 0.81 | 14.12 | 15.60 | 10.81 | 8.78 | 0.82 |
| JSON | 1.97 | 1.42 | 0.72 | 3.96 | 4.22 | 1.89 | 1.44 | 0.79 |
| gzip | 33.92 | 21.09 | 0.62 | 43.73 | 29.32 | 33.81 | 20.89 | 0.67 |
| PBKDF2 | 18.30 | 6.60 | 0.36 | 25.62 | 9.52 | 18.17 | 6.43 | 0.36 |
| SHA-256 | 13.73 | 3.43 | 0.25 | 17.33 | 5.83 | 13.47 | 3.34 | 0.28 |

What moved against the earlier build: SQLite inserts, the workload that writes pages, got faster on spared (p50 15.8 to 10.7 ms, CPU 12.0 to 9.1 ms, ratio 2.8x to 3.6x). The SQLite read, which runs from SQLite's page cache, did not get faster. It read 15.7 ms against 15.7 ms before (CPU 15.5 against 15.2 ms), ratio 4.2x to 3.9x. Everything else moved by less than 20%. The groups are unchanged: spared faster on 8 of 14 (1.6x to 6.0x), level on 2, JavaScript faster on 4.

## Before the SQLite change

The same suite on the earlier build, before spared's SQLite was built with `pread`/`pwrite`. Same columns. Raw files: `bench/results/workloads-{spared,workerd}-<workload>-run{1,2,3}.json`, summary `bench/results/workloads-summary.json`.

| workload | spared p50 ms | workerd p50 ms | ratio (p50) | spared p99 ms | workerd p99 ms | spared CPU ms | workerd CPU ms | reading |
|---|---|---|---|---|---|---|---|---|
| sort 200,000 integers and 50,000 strings | 15.40 | 95.02 | 6.17 | 27.46 | 132.60 | 14.36 | 90.10 | spared faster |
| markdown to HTML, 50 KB | 0.99 | 4.55 | 4.59 | 4.50 | 13.12 | 0.89 | 4.67 | spared faster |
| SQLite: read 50,000 rows, aggregate in app | 15.66 | 66.32 | 4.24 | 24.53 | 106.99 | 15.17 | 63.05 | spared faster |
| grayscale and 5x5 box blur, 512x512 RGBA | 2.67 | 7.71 | 2.89 | 7.55 | 19.35 | 2.55 | 6.48 | spared faster |
| SQLite: 10,000 inserts in one transaction | 15.82 | 44.52 | 2.81 | 32.37 | 138.93 | 11.95 | 42.06 | spared faster |
| HTML table, 10,000 rows, string building | 2.83 | 5.50 | 1.94 | 7.70 | 11.43 | 2.72 | 5.33 | spared faster |
| empty request (baseline) | 0.08 | 0.14 | 1.84 | 0.27 | 0.95 | 0.06 | 0.15 | baseline |
| word frequency, 2 MB of text | 25.20 | 45.69 | 1.81 | 41.60 | 70.54 | 22.17 | 44.46 | spared faster |
| 4 regex patterns over 2 MB, count matches | 15.40 | 24.07 | 1.56 | 25.54 | 35.28 | 14.28 | 22.79 | spared faster |
| n-body, 200,000 steps | 15.48 | 18.69 | 1.21 | 30.61 | 35.50 | 15.20 | 18.30 | level |
| sieve of Eratosthenes to 2,000,000 | 11.97 | 9.77 | 0.82 | 22.09 | 17.51 | 11.44 | 9.15 | level (JavaScript 1.2x faster) |
| JSON: parse 280 KB, filter, group, stringify | 1.94 | 1.54 | 0.79 | 4.17 | 5.82 | 1.85 | 1.49 | JavaScript faster (1.3x) |
| gzip 1 MB | 36.84 | 24.78 | 0.67 | 61.32 | 38.23 | 34.31 | 22.27 | JavaScript faster (1.5x) |
| PBKDF2-SHA256, 4 keys x 10,000 iterations | 20.07 | 7.24 | 0.36 | 35.66 | 14.05 | 18.57 | 6.87 | JavaScript faster (2.8x) |
| SHA-256 of 256 KB, 16 times | 13.60 | 3.84 | 0.28 | 21.83 | 10.03 | 13.43 | 3.50 | JavaScript faster (3.5x) |

The three runs agree closely: per-run p50s are in the summary file, and the largest spread is gzip on workerd (22.9 to 29.2 ms). CPU time per request tracks the p50 on both sides, so the latencies are compute, not waiting. Run 1, the only run taken on a fully quiet machine, gives the same ordering and the same win, level and loss groups (its ratios: sort 6.1, markdown 4.3, SQLite read 4.2, pixels 2.9, SQLite insert 2.8, word frequency 2.2, HTML 1.9, regex 1.65, n-body 1.14, sieve 0.85, gzip 0.86, JSON 0.79, PBKDF2 0.41, SHA-256 0.29). gzip is the one task whose reading depends on the run: level in run 1, a JavaScript win in the median.

## What each side ran, and why the result is what it is

The "why" sentences are our reading, marked likely where we did not isolate the cause.

- **sort.** JS: `Array.prototype.sort((a, b) => a - b)` on 200,000 numbers and the default `sort()` on 50,000 strings, after `slice()`. Rust: `sort_unstable` on a cloned `Vec<u32>` and on a `Vec<&str>`. Likely: V8 calls the JavaScript comparator for each of millions of comparisons, and the 32-bit values above 2^31 are stored as doubles, while Rust's pattern-defeating quicksort compares inline. Both sides made a choice here. `sort_unstable` is a faster Rust sort than the default `sort()`, and the JavaScript is a stable sort with a comparator on a plain array. A `Uint32Array` sort in JavaScript and Rust's default `sort()` were not run, so the 6.0x is the figure for these two choices and may be smaller for others.
- **markdown.** JS: `marked` 18.2.0, default options. Rust: `pulldown-cmark` 0.13.6, default options. Both produce identical HTML for this document. Likely: `marked` tokenizes with many regular expressions per block, while pulldown-cmark is a single pass over bytes.
- **SQLite read and aggregate.** JS: a SQLite-backed Durable Object, `for (const row of ctx.storage.sql.exec(...))`, a `Set` for distinct labels. Rust: `rusqlite` 0.41.1 with bundled SQLite compiled into the component, `query_map` into `(i64, i64, String)`, a `HashSet`. spared's SQLite runs inside the sandbox at about 1.85x native cost on the current build (3.2x on the earlier one; SQLite microbenchmark), and still wins. Likely: workerd builds one JavaScript object per row with named properties and crosses from C++ to JavaScript for every row.
- **SQLite inserts.** 10,000 inserts in one transaction after deleting the previous batch. JS: `ctx.storage.transactionSync` around `sql.exec` per row. Rust: one transaction, a cached prepared statement, `synchronous=FULL` and a TRUNCATE journal (the spared default). Likely the same per-statement cost as above: workerd prepares or looks up each statement from a JavaScript string per call, while Rust reuses one prepared statement. Durability settings differ (see caveats).
- **pixels.** JS: `Uint8ClampedArray` loops. Rust: `Vec<u8>` loops with `clamp`. Likely: the Rust is compiled ahead of time with bounds checks hoisted, while V8's typed-array loops keep more checks and clamped stores.
- **HTML table.** JS: `map` with a template literal per row, `join`, a `replace` chain for escaping (the same escape as the same-app comparison's demo). Rust: `String` with `push_str` and `write!`. Likely: per-row temporary strings and five regex replaces per name in JavaScript.
- **empty request.** Fixed per-request cost: routing, the HTTP stack and JSON. spared 69 us, workerd 106 us. This is the warm result of the same-app comparison again in another form; it is a baseline, not a task.
- **word frequency.** JS: `text.split(/\s+/)` and a `Map`. Rust: `split_whitespace` and the standard `HashMap` (SipHash, the default). Likely: JavaScript allocates 263,000 substrings and hashes each string, while Rust borrows slices.
- **regex.** JS: four `RegExp` literals with `text.match(re).length`. Rust: `regex` 1.13.1, default (Unicode) mode, `find_iter().count()`. Patterns are identical strings and the match counts are equal. Likely: the email pattern makes V8's backtracking engine retry at every word start, which the `regex` crate's automata avoid; `match` also allocates an array of every match.
- **n-body.** The standard benchmark-game n-body, identical floating-point order on both sides, same energy to 9 decimals. Level: V8's optimizing compiler handles this tight floating-point loop about as well as Cranelift does.
- **sieve.** JS: `Uint8Array`. Rust: `Vec<bool>`. JavaScript is 1.2x faster, within our "level" band. Likely: V8 removes the array bounds checks in this simple loop, and Wasm keeps its memory checks.
- **JSON.** JS: `JSON.parse`, `filter`, a `Map`, `JSON.stringify`. Rust: `serde_json` 1.0.154 into typed structs and back. JavaScript is 1.4x faster: `JSON.parse` and `JSON.stringify` are native C++ in V8 with years of tuning, while serde_json runs as Wasm.
- **gzip.** JS: `CompressionStream("gzip")`, which in workerd is native zlib. Rust: `flate2` 1.1.11 with the pure-Rust `miniz_oxide` 0.9.1 backend, level 6 (default) on both. JavaScript is 1.6x faster: native C against Wasm. The two compressors produce different bytes (423,534 bytes from spared, 415,698 from workerd).
- **PBKDF2.** JS: `crypto.subtle.deriveBits`. Rust: `pbkdf2` 0.12.2 with `hmac` 0.12.1 and `sha2` 0.10.9. JavaScript is 2.8x faster: WebCrypto in workerd is native BoringSSL, likely using the CPU's SHA instructions, which Wasm cannot reach.
- **SHA-256.** JS: `crypto.subtle.digest`. Rust: `sha2` 0.10.9, the portable software implementation. JavaScript is 4.0x faster for the same reason as PBKDF2.

## Summary

- Rust on spared is faster on 8 of the 14 tasks, by 1.6x to 6.0x at p50 on the current build: sorting (6.0x, see the sort bullet), markdown rendering (5.1x), reading and aggregating 50,000 SQLite rows (3.9x), bulk SQLite inserts (3.6x), pixel work (2.8x), word counting (2.2x), HTML string building (1.9x) and regex scanning (1.6x). The pattern: work that runs in JavaScript code (comparators, per-row objects, per-token strings, library code written in JavaScript) is where compiled Rust pulls ahead.
- Level on 2: n-body (spared 1.2x faster) and the sieve (JavaScript 1.2x faster). Tight numeric loops are where V8's optimizing compiler is close to compiled code.
- JavaScript is faster on 4: SHA-256 (4.0x), PBKDF2 (2.8x), gzip (1.6x) and JSON (1.4x). In each, workerd hands the work to native C or C++ (BoringSSL, zlib, V8's JSON code), and the Rust side runs the equivalent as Wasm without hardware SHA instructions.
- Example sentences the data supports: "Sorting 200,000 numbers in a request took 14.5 ms on spared and 87.5 ms on workerd." "Rendering a 50 KB markdown page took 0.83 ms on spared and 4.2 ms with `marked` on workerd." "Hashing 4 MB with SHA-256 took 3.4 ms on workerd and 13.7 ms on spared."

## Caveats

- One virtual machine (4 vCPU Xeon at 2.10 GHz), server pinned to one CPU, load generator on another, loopback HTTP/1.1, one connection, no TLS. Other tenants of the host were not measured.
- Quiet machine: every run of the current-build table started with the one-minute load average below 0.3. In the earlier table, run 1 was taken that way (28 of its 30 measurements; regex and PBKDF2 on workerd started at 0.30 and 0.35), and runs 2 and 3 started at load 0.4 to 1.15, most of it from the benchmark itself. Those files carry `"quiet": false`. Run 1 alone gives the same groups.
- Warm only. Each server got 20 warmup requests per workload before 200 timed ones (2,000 for the empty request), so V8's JIT is warm and the inputs are cached. Cold starts and first requests are not measured here (the same-app comparison covers cold start).
- Generated data. Inputs are synthetic (random words, random numbers, a noise image). Real data could change some results, such as regex and markdown, where the input's shape matters.
- Small apps, single requests at concurrency 1. A real application mixes these tasks with I/O and waiting; for most requests the fixed cost (69 us against 106 us here) and database calls matter more than any one of these loops. The speedups apply to the share of a request spent in that kind of work.
- Database workloads: spared runs SQLite inside the Wasm sandbox, about 1.85x native CPU on the current build and 3.2x before the `pread`/`pwrite` change (SQLite microbenchmark), and about 0.33 ms more on a small write-carrying request (request-cost benchmark). workerd's SQLite is native in its process. spared wins the two SQLite workloads here despite that, because the per-row JavaScript cost on workerd is larger. Durability differs: spared commits with `synchronous=FULL` and a TRUNCATE journal; we did not check when workerd's Durable Object storage syncs to disk. The workerd SQLite routes also pass through one Durable Object stub hop.
- CPU limits on: spared ran with epoch-based CPU checks as shipped, which cost 12 to 34% on SQLite-heavy work on the current build (SQLite microbenchmark, about 21% as a geometric mean). The app's `spared.toml` asks for `memory_mb = 256`, `cpu_ms = 5000`, `deadline_ms = 30000`, all within the server's default caps, so no `--max-*` flags were needed.
- Build: the Rust app uses the same release profile as `apps/demo-spared` (`opt-level = 3`, `lto = true`, `codegen-units = 1`), and SQLite is compiled with wasi-sdk 25 at `-O3` with the flags in `scripts/guest-cflags.txt` (read by `bench/env.sh`); the earlier table predates `HAVE_PREAD`/`HAVE_PWRITE`, the current-build table has them. A size-optimized build (`opt-level = "s"`) would likely be slower.
- Engines: JavaScript on V8 inside workerd 1.20261110.1 (compatibility date 2026-11-10) against Rust compiled to Wasm and run by wasmtime 50.0.1 inside spared. The comparison is of the two platforms as a developer would use them, not of the languages in isolation; native Rust would be faster than both on most of these tasks.
- Libraries and versions: `bench/results/workloads-env.json` lists them.

## Rerun

```sh
cargo build --release -p spared
mkdir -p bench/work/workerd && (cd bench/work/workerd && npm init -y && npm i workerd@1.20261110.1 marked@18.2.0)
(cd apps/workloads-spared && source ../../bench/env.sh && cargo build --release --target wasm32-wasip2 -j2)
bench/workloads/run.sh 5       # builds what is missing, verifies, measures, summarizes
```

`run.sh` starts a fresh `spared serve` on 127.0.0.1:8180 (admin 8181) with a temporary data directory and `HOME`, deploys the component through the admin API with the token from that data directory, and starts workerd from `apps/workloads-workerd/config.capnp` on 127.0.0.1:9095. Environment: `WORKLOADS` (subset), `REQS` (200), `WARMUP` (20), `LOAD_MAX` (0.3), `QUIET_BUDGET_S` (5400), `SKIP_VERIFY=1`. `python3 bench/workloads/verify.py` checks outputs against two running servers; `python3 bench/workloads/summarize.py` rebuilds the table from the raw files.
