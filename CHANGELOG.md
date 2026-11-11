# Changelog

Releases are published with generated notes on the repository's Releases page. This file records what an operator or app author needs to know when moving between versions. [docs/upgrading.md](docs/upgrading.md) has the upgrade steps and the data directory formats.

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
