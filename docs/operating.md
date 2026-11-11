# Operating spared

This page covers what the server prints, logs and limits, and how to try the bundled examples from the server image without a Rust toolchain. [How it works](how-it-works.md#what-the-operator-sees) and [design](design.md#observability) have the long versions.

## What you see

- Version: `spared --version` prints the version and the commit it was built from. `spared serve` prints the version, the data directory and both listen addresses when it starts. It never prints the admin token.
- Request log: one JSON line per request on stdout, with the time, listener, app, method, path, status, duration and a request id (also sent back as the `x-spared-request-id` response header). Query strings, headers and bodies are never logged. `--access-log off` turns it off.
- Health: `GET /healthz` on the public listener answers 200 with the version and uptime, for a load balancer or a platform health check. It says nothing about the apps.
- Metrics: `GET /metrics` on the admin listener, in the Prometheus text format: requests by status class, errors by kind, wakes, evictions, apps deployed and hot, bytes of components read ahead, and process memory. It needs the admin token, like the rest of the admin API. It is a metrics endpoint, not OpenTelemetry, and there are no traces.

## Limits

The server caps every app's limits. The defaults are `--max-memory-mb` 256, `--max-cpu-ms` 5000, `--max-deadline-ms` 30000 and `--max-outbound` 1000, with 8 MB requests and 16 MB responses. An app's `spared.toml` can only ask for less. [AGENTS.md](../AGENTS.md) lists every flag.

Request bodies are read in full before a request takes its place in the app's queue, so the server holds them in memory while clients are still sending. `--max-body-buffer-mb` (env `SPARED_MAX_BODY_BUFFER_MB`, default 64) caps what all of them may hold together, over every app and connection. A request that declares a content length reserves that many bytes, up to `--max-request-mb`, before its body is read; a body without a length reserves as it grows. When the budget is used up the request gets `503 server_busy` with `Retry-After: 1`, and the reservation comes back when the request ends or the client goes away. The server raises the figure to `--max-request-mb` when it is lower, so one request of that size can always proceed on its own. Requests without a body (GET) take nothing. The budget is shared: an app whose requests wait on a slow instance holds its bodies, and uploads to other apps can get the 503 until they finish. It bounds memory, not fairness.

## One server per data directory

`spared serve` and `spared dev` take an exclusive lock on `<data-dir>/.lock` at start and hold it until they exit. A second server on the same directory exits 1 with `data_dir_in_use`, names the process that holds it, and changes nothing (no token, no stamp). Two servers would each run an instance of every app on the same SQLite file. The kernel drops the lock when the process ends, including after `kill -9`, so a crashed server never needs a cleanup; the file stays, holds the last pid, and is safe to leave. Do not delete it while a server runs: the next server would lock a new file and the two would no longer exclude each other.

The ports are bound before the directory is touched: a taken port exits 1 with `address_in_use` and the address, without writing a token file. `spared dev` uses 127.0.0.1:8080 and 127.0.0.1:8081, so it fails the same way next to a `spared serve`.

In the container image the lock is `/data/.lock`. A platform that starts the new container before it stops the old one on a shared volume makes the new one exit with `data_dir_in_use` until the old one is gone; let it restart (a restart policy does). The lock needs a file system that supports `flock`; on one that does not, the server prints a warning and runs without the guarantee. A backup (`spared backup`) copies an app's `data/` and never the lock. If you copy a whole data directory by hand, stop the server first. A data directory owned by another user than the server's is not locked, and the server says so in a warning at start. A copied `.lock` is harmless: the lock is held by the running process, not by the file, and the next server rewrites it.

## Reading components ahead

A compiled component is mapped from its file under `<data-dir>/components`, and the server reads only its headers when it loads it. The code pages are read from disk on the first requests, one page fault at a time. When the file is not in the operating system's page cache, for example after a reboot, the first request to each app waits for that. A plain process restart keeps the page cache and does not.

So after it starts, `spared serve` reads the stored components into the page cache on a background thread. The thread never delays the listeners. It runs at the lowest CPU and I/O priority, takes the newest deploys and the most recently written apps first, and stops at the next 1 MiB read when the server is asked to stop. The pages stay ordinary page cache: they are not locked, they do not count in the server's resident memory, and the kernel can reclaim them. When it finishes the server prints one line on stderr with the components read, the megabytes, the time and the CPU time.

- `--prefetch-mb` (env `SPARED_PREFETCH_MB`) is how many megabytes of component files the pass may read. The default is 1024. `0` turns the pass off, and the check at wake described below as well. The pass also never reads more than half of the memory the kernel reports as available, or of what is left under a cgroup v2 memory limit. A component that does not fit in what is left is skipped and smaller ones still get read.
- The first time an app of a given component wakes in a process, the server samples a few spots of the mapped file with `mincore`. If most of the pages are missing, it asks the kernel to read the file ahead while the wake goes on.
- `spared_prefetch_bytes_total` on `/metrics` counts the bytes read by the pass and asked for at wake.

Measured on one 4 vCPU VM with a local virtio disk and 7 MB components (each embeds SQLite), the server on one CPU, 500 different apps, one request at a time (`bench/results/d2-passD-*`, five rounds): with the page cache dropped just before the start, as after a reboot, the first request to each of 499 apps took 5.4 ms at p50 without the pass. With `--prefetch-mb 4096`, which covers all 500 components, and the request run after the pass had finished, it took 1.8 ms. With the default 1024, which covers about a third of these components, it took 3.3 ms. With the page cache warm it takes 1.6 ms. Reading the 3.4 GB took 1.4 s and 0.35 s of CPU, and the server's anonymous memory did not change. If the cache is dropped after the server has started, the pass does not help and the check at wake brings the first request to 3.3 ms from 5.8 ms. Components larger or smaller than these 7 MB, other disks and network storage were not measured. A fleet whose components add up to more than the default needs a larger `--prefetch-mb`.

## The SQLite build cache

Every app links SQLite as C code, and compiling it with wasi-sdk is most of a first build: 49 to 54 of 56 to 68 seconds on a 4-core VM. `spared setup` compiles it once and keeps the object file in `~/.spared/lib` (`$SPARED_LIB_DIR` moves it). The CLI points the build's C compiler at `~/.spared/lib/bin/spared-cc`, a symlink to the `spared` binary that forwards every command to wasi-sdk clang, except a compile of `sqlite3.c`, which it answers from the cache when it can. The libsqlite3-sys build script still runs as before, so the flags, the VFS and the bindings are the ones a build without the cache uses, and nothing in an app's `Cargo.toml` or `Cargo.lock` changes.

- An entry is `~/.spared/lib/sqlite-<sqlite version>-<key>/`, holding `sqlite3.o` and a `key.txt` that lists what went into the key. The key is a SHA-256 over the bytes of `sqlite3.c`, every compiler argument in order (the optimization level, each `-D`, the sysroot) and the wasi-sdk identity: clang's version banner and wasi-sdk's `VERSION` file. A different SQLite, flag or SDK gets a different entry, so an entry cannot be stale. `LIBSQLITE3_FLAGS` and an app's `opt-level` change the arguments, so they get their own entries.
- A missing, damaged or unwritable cache is not an error. The wrapper runs clang as it would have, keeps the object when the compile succeeds and checks the SHA-256 of an entry before it uses it. A build that misses takes about a minute, the same as before the cache. `rm -rf ~/.spared/lib` is always safe.
- Apps that lock another libsqlite3-sys than the one `spared new` writes miss once and fill their own entry during that build.
- `spared doctor` reports whether this wasi-sdk has a cached SQLite (a `WARN` is only a slower first build), and `spared setup --dry-run` shows the step. `SPARED_NO_SQLITE_CACHE=1` builds without the wrapper.
- After upgrading to a spared with this cache, an existing `~/.spared/target` rebuilds libsqlite3-sys and rusqlite once, because the C compiler path is an input of the build script. That took 3.9 seconds with the cache filled, and takes about a minute without it.
- `scripts/sqlite-cache-test.sh` checks all of this against a counting copy of the SDK.

## The admin API

The admin API deploys code. It needs a bearer token. The CLI reads it from `$SPARED_TOKEN` or `~/.spared/token`, which `spared serve` writes on first start. Keep the admin listener on loopback.

## Upgrades

The data directory carries a format stamp. A release that cannot read it refuses to start and names the fix. App data is never rewritten by an upgrade. Back up first: [upgrading](upgrading.md).

## Run the examples from the image

The server image holds the server and three example apps (`notes`, `list`, `demo`, see [examples](../examples/README.md)). No Rust toolchain is needed. This script detects Docker, Fly and Railway, lets you choose (or prints the commands for any VM with Docker), and prints the URLs:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/michaelmagan/spared/main/scripts/try.sh | sh
```

`sh scripts/try.sh --dry-run` prints every command without running any. `--target docker|fly|railway|vm` and `--yes` skip the menu. With Docker it runs:

```sh
docker run -d --name spared --read-only --tmpfs /tmp:rw,noexec,nosuid,size=64m --user 10001:10001 \
  --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=256 --memory=512m \
  -p 127.0.0.1:8080:8080 -v spared-data:/data ghcr.io/michaelmagan/spared:0.0.1-preview
```

That is a read-only root, no capabilities, no new privileges, and bounded processes and memory. Locally the apps are at `http://notes.localhost:8080/` and so on. On a host with one hostname, such as Fly or Railway, use `https://<host>/_/notes/`. That path form exists because the image turns on path routing (`--allow-path-routing`, env `SPARED_PATH_ROUTING`) for its bundled examples. It serves every app from one browser origin, so apps are not isolated from each other in a visitor's browser. `try.sh` prints this warning too. It is off by default in `spared serve`. Use one host per app (`--domain`) for anything you did not write.

Tested on one Linux VM: the image builds and runs under Docker, and the Docker path of `try.sh` runs against a locally built image. The Fly and Railway steps follow those vendors' references and print their plan under `--dry-run`, but we have not run them, and we have not run the arm64 image.
