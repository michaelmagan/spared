# Contributing

Thanks for helping. Open an issue before starting large work so it does not collide with changes in progress. spared is an early preview, so the API, CLI, manifest and host interface can change.

Report security problems through private vulnerability reporting, not issues. Open the repository's Security tab and choose "Report a vulnerability".

## Build

The host is one Rust crate in a Cargo workspace (`crates/spared`). Guest apps under `apps/` and `probes/` are separate workspaces that build for `wasm32-wasip2`.

```sh
cargo build --release -p spared        # host and CLI: target/release/spared
cargo test -p spared                   # unit tests in the host crate
```

The release binary is the product: it has `serve` and the CLI and nothing else. The commands that reproduce the published numbers and run the sandbox escape suite (`run-probe`, `compile`, `bench-wake`, `bench-density`, `bench-3b`, `escape-test`), the native SQLite they use, and the `SPARED_NO_EPOCH` and `SPARED_PROBE_LEGACY_IO` switches exist only in a lab build, `--features lab`. The `bench/` scripts build it on first use into its own directory so it never replaces the release binary:

```sh
cargo build --release -p spared --features lab --target-dir target/lab   # target/lab/release/spared
cargo test -p spared --features lab
cargo clippy -p spared --all-targets --features lab -- -D warnings
```

Do not serve apps from a lab build. CI builds and tests both, and a test (`crates/spared/tests/product_binary.rs`) fails if the default binary lists a lab command or contains a lab switch.

Building guests needs the `wasm32-wasip2` target and wasi-sdk 25, because SQLite is C compiled into each component. `spared setup` installs both into `~/.spared` (`--dry-run` prints the plan). The benchmark scripts instead read the SDK from `bench/env.sh`, which defaults to the SDK that `spared setup` installed (`~/.spared/wasi-sdk`) and can be overridden with `$WASI`; without either it stops and says so.

```sh
source bench/env.sh
cd probes/sqlite-probe && cargo build --release --target wasm32-wasip2
```

Without the cache that `spared setup` fills, the first guest build compiles SQLite and takes about a minute (AGENTS.md describes the cache, and `scripts/sqlite-cache-test.sh` proves it).

The C flags for that SQLite live in one file, `scripts/guest-cflags.txt`: `spared deploy`, `test` and `dev` embed it, and `bench/env.sh` and `scripts/build-examples.sh` read it. Change flags there and nowhere else. `-DHAVE_PREAD=1 -DHAVE_PWRITE=1` must stay: without them every SQLite page goes through a WASI stream that wasmtime-wasi runs on another thread, and SQLite in Wasm is about 3x native instead of 1.9x (see `docs/benchmarks.md`). A unit test (`guest_sqlite_uses_pread_and_pwrite`) and `scripts/e2e.sh` (the built component must import `descriptor.read` and `descriptor.write`) fail if they go.

## Try a change by hand

```sh
export PATH="$PWD/target/release:$PATH"
target/release/spared serve --data-dir ./data      # leave running
target/release/spared new my-app && cd my-app
spared test --path /
spared deploy
```

`scripts/templates-test.py` scaffolds every `spared new --template`, runs `spared test`, deploys each to a scratch server (ports 19300-19399, temp data dir and HOME) and checks its behaviour. Run it after you change a template or `cli.rs`: `scripts/templates-test.py` (or name templates to run only those). A new template needs an entry in the table in `cli.rs` and a check in that script; the script fails a template it has no check for.

`scripts/e2e.sh` is the end-to-end test CI runs (it ends with `scripts/server-test.py`, the server integration test). `sh scripts/test-install.sh` tests the install script against a local web server that stands in for the release host, and `sh scripts/test-try.sh` dry-runs every target of `scripts/try.sh`; run them after changing either script, a file under `deploy/`, or the asset names in `.github/workflows/release.yml`.

`AGENTS.md` is the full guide to writing and deploying an app. If you change the CLI, the generated templates in `crates/spared/templates/` or an error kind, update `AGENTS.md` in the same change.

## The benchmark scripts

Each benchmark has a script in `bench/` and writes raw JSON to `bench/results/`. `docs/benchmarks.md` lists them with their caveats. `bench/MACHINE.md` records the machine the committed results came from.

| Script | What it runs | Needs |
|---|---|---|
| `bench/crash-test.sh` | crash durability: kill -9 durability, 100 trials. `JOURNAL=TRUNCATE` for the shipped mode | lab host, sqlite-probe guest |
| `bench/sqlite-overhead.sh` | the SQLite microbenchmark: SQLite in Wasm against native. `N` and `REPS` override size | lab host, wasm and native sqlite-probe |
| `bench/request-cost.sh` | cost per realistic request (latency and CPU) | lab host, `apps/bench3b-*` |
| `bench/wake.sh` | wake to first query | lab host, `apps/shared-list` |
| `spared bench-density` | density: 10,000 deployed, 500 hot (`--deployed`, `--hot`, `--rows`) | lab host, a component |
| `bench/escape-tests.sh` | the sandbox escape suite (50 cases) | lab host, `probes/escape-probe` |
| `bench/demo/measure.sh` | same-app comparison: spared against workerd | workerd from npm, `bench/demo/loadgen`, `taskset` |

Rules for benchmark changes:

- Decide what a benchmark measures and what result would count as a regression before running it. If you change that after seeing a result, say so in `docs/benchmarks.md`.
- Run on a quiet machine. `measure.sh` waits for a one-minute load average under 0.3.
- Keep raw output in `bench/results/`. A number in prose must name the file it came from.
- Do not edit committed results to make a claim read better. Record results that look bad as well as good ones.

## Style

- Rust: edition 2024. Run `cargo fmt` and `cargo clippy` before sending a change. Defaults apply: there is no rustfmt config, and clippy runs with `-D warnings`.
- Prose: short sentences, plain words, no hype, no filler, no em dashes. Say what was measured, how, and what it does not show.
- Errors that agents will read have a kind, a message and a fix line (`{"error":{"kind","message","fix"}}` with `--json`). New errors need all three.
- No new dependency without a license check. `THIRD_PARTY.md` records the current scan: permissive licenses only.
- Changing `Cargo.lock`, the guest apps' dependencies or `scripts/notices/` means regenerating `THIRD-PARTY-NOTICES`: `python3 -I scripts/notices.py > THIRD-PARTY-NOTICES` (add `CARGO_NET_OFFLINE=true` when the crates are already in `~/.cargo/registry`). CI runs `scripts/notices.py --check` and fails on a stale file. The script lists the crates in the binary (default features, the four release targets), the guest crates of the example apps, and the SQLite and wasi-libc notices; the wasi-libc license texts live in `scripts/notices/` (taken from the revision wasi-sdk 25 ships). Release tarballs and the Docker image carry `LICENSE`, `NOTICE` and `THIRD-PARTY-NOTICES`.
- `crates/spared/templates/spared.rs` starts with a `// spared-helper: N` line. Raise `N` when you change that file; `spared doctor` and `spared deploy` use it to warn an app whose copy is older.
- Do not add anything that makes a speed claim without a file in `bench/results/`.

## Pull requests

Describe what changed and why. Say how you tested it. If the change touches the sandbox, grants or the outbound HTTP path, add or update a case in `crates/spared/src/lab/escape.rs`, and run `bench/escape-tests.sh`. If it touches `server.rs` (routing, the admin API, limits, request handling), add a check to `scripts/server-test.py` and run it (`python3 -I scripts/server-test.py`; `scripts/e2e.sh` runs it too).

By contributing you agree your work is licensed under Apache 2.0, the project license.
