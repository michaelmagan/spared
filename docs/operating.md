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

- An entry is `~/.spared/lib/sqlite-<sqlite version>-<key>/`, holding `sqlite3.o` and a `key.txt` that lists what went into the key. The key is a SHA-256 over the bytes of `sqlite3.c`, every compiler argument in order (the optimization level, each `-D`, the sysroot) and the wasi-sdk identity: clang's version banner and wasi-sdk's `VERSION` file. A different SQLite, flag or SDK gets a different entry. The key does not cover the contents of files `sqlite3.c` includes. The bundled amalgamation includes only wasi-sdk headers, which the SDK identity covers. A build that adds its own headers (`-I`, `-include`, `CPATH`, `SQLITE_CUSTOM_INCLUDE`) must set `SPARED_NO_SQLITE_CACHE=1`. `LIBSQLITE3_FLAGS` and an app's `opt-level` change the arguments, so they get their own entries.
- A missing, damaged or unwritable cache is not an error. The wrapper runs clang as it would have, keeps the object when the compile succeeds and checks the SHA-256 of an entry before it uses it, which detects a damaged file, not a replaced one. A build that misses takes about a minute, the same as before the cache. `rm -rf ~/.spared/lib` is always safe.
- The cache is not a security boundary. Build scripts run with your privileges, so a build can replace a cached object, and every later app build on the account links it, the same as with `~/.spared/target`, `~/.cargo` and the wasi-sdk. The recorded hash detects a damaged file, not a replaced one. Build code you do not trust in a container or VM that does not share `~/.spared`, and never point `SPARED_LIB_DIR` at a directory another user can write, because builds execute `bin/spared-cc` from it.
- Apps that lock another libsqlite3-sys than the one `spared new` writes miss once and fill their own entry during that build.
- `spared doctor` reports whether this wasi-sdk has a cached SQLite (a `WARN` is only a slower first build), and `spared setup --dry-run` shows the step. `SPARED_NO_SQLITE_CACHE=1` builds without the wrapper.
- After upgrading to a spared with this cache, an existing `~/.spared/target` rebuilds libsqlite3-sys and rusqlite once, because the C compiler path is an input of the build script. That took 3.9 seconds with the cache filled, and takes about a minute without it.
- `scripts/sqlite-cache-test.sh` checks all of this against a counting copy of the SDK.

## Off-machine backups

A backup under `<data-dir>/backups/` sits on the same disk as the data and is lost with it. `spared serve --backup-to s3://<bucket>/<prefix>` also uploads every backup to an S3-compatible bucket, and `spared restore --from remote:<id>` brings one back on this server or on a new one. This is a copy you can restore by hand. It is not replication, failover or a way to run one app on two servers.

### Set it up

1. Make a key, and keep a copy off this machine:

   ```sh
   spared backup-key new --out /etc/spared/backup.key
   ```

   Archives are encrypted and authenticated with this key. Without it the backups in the bucket cannot be restored, by spared or anyone else. Put a copy in a password manager or a secret store: a key that lives only on the server's disk is lost with that disk, which is the case the bucket is for. `spared backup-key new` without `--out` prints the key on stdout. The key id it prints (the first 8 bytes of the key's SHA-256) is safe to write down anywhere.
2. Make a bucket, and an access key that may only use the prefix (the policy is below). Add a lifecycle rule, because spared never deletes anything in the bucket (below).
3. Put the credentials in the server's environment: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and, for temporary credentials, `AWS_SESSION_TOKEN`. Each one has a `_FILE` twin that names a file holding the value (`AWS_SECRET_ACCESS_KEY_FILE=/run/secrets/aws_secret`), for container secrets. spared never reads credentials from a flag (every local user can read a process's arguments), from `spared.toml` or from the data directory. Apps and the compile worker never get the server's environment.
4. Start the server:

   ```sh
   spared serve --data-dir /var/lib/spared \
     --backup-to s3://my-backups/spared \
     --backup-key-file /etc/spared/backup.key
   ```

   At start it prints one line with the bucket, the endpoint, the region, this server's node id and the key id. It never prints a credential or the key. Settings that cannot work stop the start with exit 1 before anything is written: `backup_key_required` (no key and no `--backup-encryption off`), `no_backup_credentials`, or `bad_backup_config` with the reason.

| Flag | Env | Default | |
|---|---|---|---|
| `--backup-to s3://BUCKET/PREFIX` | `SPARED_BACKUP_TO` | off | The bucket follows the S3 naming rules. The prefix may hold `a-z`, `0-9`, `.`, `_`, `-` and `/`, with no `..` part. |
| `--backup-endpoint URL` | `SPARED_BACKUP_ENDPOINT` | AWS S3 | `https://host[:port]` for R2, Tigris, MinIO-style stores and so on. No user name, password, path, query or fragment. `http://` only for a loopback address or with `--backup-allow-http`. |
| `--backup-region` | `SPARED_BACKUP_REGION` | `$AWS_REGION`, then `us-east-1` | R2 takes `auto`. |
| `--backup-addressing path\|virtual` | `SPARED_BACKUP_ADDRESSING` | virtual on AWS, path with `--backup-endpoint` | A bucket name with a dot needs `path` over https. |
| `--backup-key-file PATH` | `SPARED_BACKUP_KEY_FILE` | none | 64 hex digits, from `spared backup-key new`. |
| `--backup-encryption on\|off` | `SPARED_BACKUP_ENCRYPTION` | `on` | `off` uploads archives that anyone with read access to the bucket can read, and that anyone with write access can replace unnoticed. |
| `--backup-allow-http` | `SPARED_BACKUP_ALLOW_HTTP` | off | Allows `http://` to a store that is not on loopback. |

The client connects straight to the store: no HTTP proxy is used. A private CA goes in `SPARED_EXTRA_CA_FILE`, as for apps. The address is resolved once per connection, redirects are never followed (a store that answers 301 for another region gives `remote_wrong_region` with the region to pass), and every request signs the SHA-256 of its body with AWS Signature Version 4.

### What a backup does

`spared backup <name>` takes the local backup as before and lets the app serve again. Then the server builds the archive in a temporary file under `<data-dir>/backups/<name>/.remote/` and uploads it with one PUT, then its manifest. The app is not stopped during the upload and can be redeployed. The CLI waits for the upload and prints the remote id. If the upload fails, the local backup stands, the CLI prints its id and exits 1 with `remote_upload_failed`, and the message names the cause: `remote_unavailable` (DNS, connection, TLS, or a 5xx after three tries), `remote_auth_failed`, `remote_no_bucket`, `remote_wrong_region`, `remote_clock_skew` (the clocks differ by more than 15 minutes, and the message shows both), `remote_rejected` or `remote_backup_too_big`. A cron job sees the exit code. `--no-wait` returns once the local backup exists. `spared backups <name>` shows each backup's upload state, and a server that stops during an upload marks it `failed` with `remote_interrupted` when it starts again. To try again, take a new backup.

There are no scheduled backups: run `spared backup <name>` from cron or a systemd timer.

What is stored, for each backup:

```
<prefix>/spared/v1/<app>/e00000001/<backup id>.<node>/archive
<prefix>/spared/v1/<app>/e00000001/<backup id>.<node>/manifest.json
```

- The remote id is `<backup id>.<node>.<epoch>`, for example `20261112T040506Z.7f3a9c0a1b2c.1`. The node is 6 random bytes that the server writes once into `<data-dir>/node-id` (mode 0600), so two servers never write the same names. A copy of the data directory carries the same node, so on a clone that will run next to the original, delete `<data-dir>/node-id` before its first start, or give it a different prefix; otherwise both write the same object keys when they back up in the same second. The epoch is always 1 in this version.
- The archive is a POSIX tar stream of the backup's files, compressed with zstd (level 3), then sealed: ChaCha20-Poly1305 in 64 KiB segments (the STREAM construction that age uses), with a key for each archive derived from your key and a random salt. Each segment is bound to the app name and the remote id, so an archive copied to another app or id does not open, and a cut one does not either. The first tar entry lists every file with its size and SHA-256.
- `manifest.json` is written last, so a backup whose manifest is missing is never listed or restored. It holds the app name, the remote id, sizes, the archive's SHA-256, the key id and the SHA-256 of the inner file list (`content_digest`), not file names.
- A reader of the bucket learns the compressed size of each backup. That is accepted.
- This version sends each archive in one PUT and refuses an archive over 4 GiB with `remote_backup_too_big`. The local backup stands.

### Restore from the bucket

```sh
spared backups notes --remote     # works before the app is deployed on this server
spared deploy                     # on a new server: the app's code first
spared restore notes --from remote:20261112T040506Z.7f3a9c0a1b2c.1 --yes
```

The restore has two steps. First the server downloads the backup, checks it and unpacks it into a new local backup, through a `.tmp-fetch-*` directory, while the app keeps serving. Then the ordinary restore puts that backup in place and keeps the current data as a `pre-restore-...` copy, which undoes it. The fetched backup stays as a local backup. The checks, in order:

1. The manifest names this app and remote id, and this server's key id. Another key fails with `backup_key_mismatch` before the archive is downloaded.
2. The archive's size matches the manifest and fits `--max-app-data-mb`.
3. Its SHA-256 matches the manifest.
4. Every sealed segment opens with this key, and the last one is marked last.
5. The tar stream is read strictly: regular files and directories only, relative names with no `..`, at most 64 levels and 100,000 entries, at most `--max-app-data-mb` once decompressed, and the entries in exactly the order of the inner list.
6. Each file's SHA-256 matches the inner list.

Any failure removes the staging directory and leaves the app's data and its local backups as they were, with `remote_backup_corrupt`, `backup_key_mismatch` or `no_such_remote_backup`. Files are written relative to directory handles, never through a link, and the host never opens `app.db`. A server with a key refuses a backup that is not encrypted, because anyone who can write to the bucket could have put it there, and a server started with `--backup-encryption off` cannot read an encrypted one.

### Bucket permissions

spared uses PutObject, GetObject (and HEAD, which GetObject covers) and ListBucket. It never deletes, so leave `s3:DeleteObject` out: a stolen access key then cannot erase the backups. For AWS, with the bucket `my-backups` and the prefix `spared`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:PutObject", "s3:GetObject"],
      "Resource": "arn:aws:s3:::my-backups/spared/*"
    },
    {
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::my-backups",
      "Condition": {"StringLike": {"s3:prefix": "spared/*"}}
    }
  ]
}
```

PutObject can still overwrite an object. Turn on versioning on the bucket if a compromised server must not be able to replace the backups it wrote. Object lock with a default retention period is not supported in this version: S3 then requires a `Content-MD5` or `x-amz-checksum-*` header on every PutObject, which spared does not send, so each upload would be rejected as `remote_rejected`. Other stores have their own way to scope a token to a bucket.

### Retention: a lifecycle rule

spared never deletes an object in the bucket, so the bucket grows until a lifecycle rule expires old objects. Set one when you set up the bucket. On AWS, to keep 90 days:

```sh
aws s3api put-bucket-lifecycle-configuration --bucket my-backups --lifecycle-configuration \
  '{"Rules":[{"ID":"spared-expiry","Status":"Enabled","Filter":{"Prefix":"spared/"},"Expiration":{"Days":90},"AbortIncompleteMultipartUpload":{"DaysAfterInitiation":1}}]}'
```

The same rule removes an archive whose manifest upload failed: it is never listed or restored, and spared leaves it where it is. With versioning on, also expire noncurrent versions. `--max-backups-per-app` counts local backups only.

### Key custody

- The key and the bucket credentials live in the server process only: from the key file and the environment at start, never in the data directory, the admin API or a log. Debug output prints `***` in their place.
- Keep the key somewhere that does not depend on the server: a password manager, a secret store, a printed copy in a safe. Test a restore on another machine once, with the copy and not the server's file.
- Losing the key loses every backup sealed with it. There is no recovery and no escrow.
- A new key works for new backups only. Keep the old key while backups sealed with it are still wanted, because a restore needs the key the backup was taken with.

### What was tested

Tested through the real server against an S3 stand-in that runs inside the test process (`crates/spared/tests/remote_backups.rs`). The stand-in checks the SigV4 signature of every request with its own code, written from the AWS documentation and checked against vectors generated with botocore 1.44.1 (`scripts/sigv4-vectors.py`). The tests cover a byte-identical round trip to a second server, a store that is unreachable or answers 503, a wrong secret, a missing bucket, a skewed clock, a redirect to another region, a 412, a connection cut during an upload and during a download, a truncated, altered, renamed or foreign archive, archives with `../` names, links, devices or a decompression bomb, a wrong key, and `kill -9` of the server during an upload and during a download. Not tested against AWS S3, R2 or any other real store. Not measured: upload and download speed, and how well real databases compress.

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
