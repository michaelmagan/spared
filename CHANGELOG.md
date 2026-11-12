# Changelog

Releases are published with generated notes on the repository's Releases page. This file records what an operator or app author needs to know when moving between versions. [docs/upgrading.md](docs/upgrading.md) has the upgrade steps and the data directory formats.

## Unreleased

- Off-machine backups. `spared serve --backup-to s3://<bucket>/<prefix>` uploads every backup to an S3-compatible bucket (`--backup-endpoint` for a store other than AWS S3, `--backup-region`, `--backup-addressing`), as one archive (tar, zstd, then ChaCha20-Poly1305 with the operator's key from `--backup-key-file`) and a manifest written last. Credentials come only from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN` or their `_FILE` twins. The server refuses to start with `--backup-to` and no key unless `--backup-encryption off` is given, and `spared backup-key new` makes a key. `spared backup` waits for the upload and exits 1 with `remote_upload_failed` when it fails, keeping the local backup (`--no-wait` returns at once). `spared backups <name> --remote` lists the bucket, and `spared restore <name> --from remote:<id> --yes` downloads, verifies and unpacks a backup into a new local backup, then restores it. spared never deletes in the bucket: set a lifecycle rule. No new crates. Tested against an S3 stand-in in the test process, not against AWS S3, R2 or another real store. See [docs/operating.md](docs/operating.md#off-machine-backups).

- `spared setup` compiles SQLite for Wasm once and keeps the object in `~/.spared/lib` (`$SPARED_LIB_DIR` moves it, `SPARED_NO_SQLITE_CACHE=1` turns it off). The first build of an app reuses it: on a 4-core Linux VM the first `spared test` of a new notes app took 12 seconds instead of 57. A missing or damaged cache is not an error: the build compiles SQLite as before and keeps the object. The key covers the SQLite source, every compiler flag and the wasi-sdk version, and apps, `Cargo.toml` and `Cargo.lock` are unchanged. `spared doctor` reports the cache. An existing `~/.spared/target` rebuilds libsqlite3-sys and rusqlite once after the upgrade (3.9 seconds with the cache filled). See [docs/operating.md](docs/operating.md#the-sqlite-build-cache).

## 0.0.1-preview - 2026-11-11

The first public preview. Data directory format 1.

- `spared` is one binary: the server (`spared serve`) and the CLI (`setup`, `doctor`, `new`, `templates`, `test`, `deploy`, `dev`, `ps`, `logs`, `rm`, `backup`, `backups`, `restore`, `evict`, `bench`).
- Apps are Rust crates compiled to `wasm32-wasip2` components with SQLite inside; each runs in its own sandbox with one instance, its own data directory, and limits and grants from its `spared.toml`.
- `spared --version`; the server prints its version at start and reports it at `GET /healthz`.
- A request log on stdout (one JSON line per request, no query strings, headers or bodies; `--access-log off`), `GET /healthz` on the public listener, and `GET /metrics` (Prometheus text format) on the admin listener.
- The data directory carries a format stamp (`format.json`); a server that cannot read the format refuses to start with `data_dir_too_new` or `data_dir_too_old`.
- Per-app backups and restores with SHA-256 verification.
- Release binaries are built for Linux x86_64 and arm64 and macOS arm64 and x86_64 and published with `SHA256SUMS`, which the install script checks. Only Linux x86_64 is tested.
- Per-app resource caps so one app cannot take the server down: data size (`--max-app-data-mb`, 1024), entries under `/data` (`--max-app-entries`, 50000), open files (`--max-app-fds`, 64), four directory listings at once, log bytes per request and per second (`--max-log-kb-per-request`, `--max-log-kb-per-sec`; the log also rotates at 4 MiB), and requests running or waiting (`--max-app-queue`, 16; more get `503 app_busy` with `Retry-After`). An app that goes over one sees an ordinary error from the call.
- Backups keep holes in sparse files and refuse a data tree whose files add up to more than `--max-app-data-mb`.
- A server-wide budget for request bodies held in memory (`--max-body-buffer-mb`, 64); past it a request gets `503 server_busy` with `Retry-After`.
- After start the server reads stored components into the page cache in the background (`--prefetch-mb`, default 1024, `0` turns it off).
- One server per data directory: `spared serve` and `spared dev` lock `<data-dir>/.lock` and a second one exits with `data_dir_in_use`. A taken port exits with `address_in_use` before anything is written.
- Admin commands wait up to 2 s for a server that is still starting, so `spared serve &` followed at once by `spared deploy` works.
- A `fetch` entry with a scheme, path, port or user name, a bare `*`, or a private or loopback IP literal is refused as `bad_manifest`. A server that loads an app stored with such an entry skips that app and says why.
- Plain CLI errors read `error[kind]: message`. `spared dev` prints the `export SPARED_TOKEN=...` line for its own token. The blog template refuses to publish until its `TOKEN` constant is changed.
