Linux 6.18.44-fc-v338 x86_64
CPU(s):                                  4
Model name:                              Intel(R) Xeon(R) Processor @ 2.10GHz
Mem:           16086         573       12550          15        3268       15512
rustc 1.97.0 (2d8144b78 2026-07-07) for the comparison passes A to D; pass E used rustc 1.99.0 (the version rust-toolchain.toml now pins)
wasmtime 50.0.1
wasi-sdk 25.0 (clang 19)
rusqlite 0.41.1 / libsqlite3-sys 0.39.1
spin 4.3.0 (83b512f)

## Same-app comparison against workerd (bench/demo/measure.sh)
Linux 6.18.44-fc-v338 x86_64; 4 vCPU Intel Xeon @ 2.10GHz, 16 GB; server pinned taskset -c 0, load generator taskset -c 2; loopback HTTP/1.1 keep-alive, no TLS.
workerd workerd 2026-11-10 (npm workerd@1.20261110.1), compatibilityDate 2026-11-10, DO SQLite localDisk.
spared: target/release/spared (wasmtime 50.0.1), --idle-secs 600, demo component apps/demo-spared. Load generator: bench/demo/loadgen (hyper 1 + tokio current_thread), rustc 1.97.0 (2d8144b78 2026-07-07).
apps/demo-workerd/worker.js now routes the Durable Object by ?id=K (default "main"), needed for N distinct DOs; check.sh still passes 15/15.
Load average condition: 1-minute average < 0.3 before each run.
