# Operating spared

This page covers what the server prints, logs and limits, and how to try the bundled examples from the server image without a Rust toolchain. [How it works](how-it-works.md#what-the-operator-sees) and [design](design.md#observability) have the long versions.

## What you see

- Version: `spared --version` prints the version and the commit it was built from. `spared serve` prints the version, the data directory and both listen addresses when it starts. It never prints the admin token.
- Request log: one JSON line per request on stdout, with the time, listener, app, method, path, status, duration and a request id (also sent back as the `x-spared-request-id` response header). Query strings, headers and bodies are never logged. `--access-log off` turns it off.
- Health: `GET /healthz` on the public listener answers 200 with the version and uptime, for a load balancer or a platform health check. It says nothing about the apps.
- Host backtraces: off. `spared` sets `RUST_BACKTRACE=0` and `RUST_LIB_BACKTRACE=0` for itself when it starts, so a host error never carries a stack trace that could reach an app through an error's text. To debug the host, start it with `SPARED_ALLOW_HOST_BACKTRACE=1`, and the two variables then work as Rust documents. Apps never get host error details either way: a failed host call gives them a fixed text (see [isolation](isolation.md#what-you-get-today)). The setting is passed on to what `spared` starts, so a `spared deploy` build also runs without backtraces unless the variable is set.
- Metrics: `GET /metrics` on the admin listener, in the Prometheus text format: requests by status class, errors by kind, wakes, evictions, apps deployed and hot, bytes of components read ahead, and process memory. It needs the admin token, like the rest of the admin API. It is a metrics endpoint, not OpenTelemetry, and there are no traces.

## Limits

The server caps every app's limits. The defaults are `--max-memory-mb` 256, `--max-cpu-ms` 5000, `--max-deadline-ms` 30000 and `--max-outbound` 1000, with 8 MB requests and 16 MB responses. An app's `spared.toml` can only ask for less. [AGENTS.md](../AGENTS.md) lists every flag. To give one app less disk than the others, or a share of CPU and outbound HTTP over time, see [Limit each app](#limit-each-app).

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
<prefix>/spared/v1/<app>/e<epoch, 8 digits>/<backup id>.<node>/archive
<prefix>/spared/v1/<app>/e<epoch, 8 digits>/<backup id>.<node>/manifest.json
```

- The remote id is `<backup id>.<node>.<epoch>`, for example `20261112T040506Z.7f3a9c0a1b2c.1`. The node is 6 random bytes that the server writes once into `<data-dir>/node-id` (mode 0600), so two servers never write the same names. A copy of the data directory carries the same node, so on a clone that will run next to the original, delete `<data-dir>/node-id` before its first start, or give it a different prefix; otherwise both write the same object keys when they back up in the same second. The epoch starts at 1 and goes up when the app is restored from the bucket ([epochs](#epochs-and-a-stale-server)).
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
- Keep the key somewhere that does not depend on the server: a password manager, a secret store, a printed copy in a safe. Test a restore on another machine once (a restore drill), with the copy and not the server's file, and never run `spared backup` for that app there: see [Epochs and a stale server](#epochs-and-a-stale-server).
- Losing the key loses every backup sealed with it. There is no recovery and no escrow.
- A new key works for new backups only. Keep the old key while backups sealed with it are still wanted, because a restore needs the key the backup was taken with.

### What was tested

Tested through the real server against an S3 stand-in that runs inside the test process (`crates/spared/tests/remote_backups.rs`). The stand-in checks the SigV4 signature of every request with its own code, written from the AWS documentation and checked against vectors generated with botocore 1.44.1 (`scripts/sigv4-vectors.py`). The tests cover a byte-identical round trip to a second server, a store that is unreachable or answers 503, a wrong secret, a missing bucket, a skewed clock, a redirect to another region, a 412, a connection cut during an upload and during a download, a truncated, altered, renamed or foreign archive, archives with `../` names, links, devices or a decompression bomb, a wrong key, and `kill -9` of the server during an upload and during a download. Not tested against AWS S3, R2 or any other real store. Not measured: upload and download speed, and how well real databases compress.

## Move an app to another server

`spared move <app> --to <admin URL>` moves an app from the server it runs on (the source) to another spared server (the target), through the bucket both back up to. It is a planned move that an operator runs while both servers are up. It is not failover, it never changes DNS, and it never deletes the app on the source.

### What it needs

- Both servers run a spared with `spared move`. An older one answers `move_unsupported`.
- Both back up to the same bucket and prefix with the same key: the same `--backup-to`, and a copy of the same `--backup-key-file` (or both `--backup-encryption off`). The move reads each server's bucket, prefix and key id from `GET /v1/server` (never a credential or the key) and refuses with `move_remote_mismatch` when they differ, or `remote_backups_off` when a server has no `--backup-to`. The endpoints may differ, for example a private address on one side.
- Different node ids. A server started on a copy of the other's data directory has the same one, and the move refuses with `move_same_server` (delete `<data-dir>/node-id` on the copy before its first start).
- One machine that reaches both admin APIs. The admin listener is on loopback by default and refuses a `Host` that is not an IP address or `localhost`, so forward each one over ssh and use the forwarded port:

```sh
ssh -N -L 9081:127.0.0.1:8081 old-host &    # the source's admin API, here on 127.0.0.1:9081
ssh -N -L 9082:127.0.0.1:8081 new-host &    # the target's, on 127.0.0.1:9082
ssh -N -L 9080:127.0.0.1:8080 new-host &    # the target's public listener, for --smoke-url
(umask 077; ssh old-host cat /var/lib/spared/admin-token > ~/.spared/source-token)
(umask 077; ssh new-host cat /var/lib/spared/admin-token > ~/.spared/target-token)
export SPARED_TOKEN=$(cat ~/.spared/source-token)

spared move notes --server http://127.0.0.1:9081 --to http://127.0.0.1:9082 \
  --to-token-file ~/.spared/target-token --dry-run
spared move notes --server http://127.0.0.1:9081 --to http://127.0.0.1:9082 \
  --to-token-file ~/.spared/target-token --smoke-url http://127.0.0.1:9080
```

The source's token comes from `$SPARED_TOKEN` or `~/.spared/token`, as for every command. The target's comes from `--to-token-file` or `$SPARED_TARGET_TOKEN`, never from an argument, which every local user can read. Neither is printed, written to the data directories, or put in an error. A URL with a user name or password is refused.

### What it does

`--dry-run` runs the checks below and prints the plan without changing anything. Without it:

1. Checks, which change nothing: both servers answer, share the bucket and the key, have different node ids, can list the bucket, the source has the app, and the target either has no app of that name or has it frozen. A target that serves the app already is refused with `move_target_in_use`, because the restore would replace its data.
2. Copies the code. A backup holds data only, so the move reads the app's deployed wasm and its `spared.toml` from the source (`GET /v1/apps/<app>/code`) and deploys them on the target, which compiles the wasm itself. The compiled file never travels. The app is created frozen on the target, so it never serves there before its data arrives.
3. Freezes the app on the source. From here no request runs there.
4. Backs it up on the source and waits for the upload.
5. Fetches that backup on the target, which checks it as any remote restore does, and compares the content digest the target unpacked with the one the source uploaded (`move_digest_mismatch` when they differ).
6. Restores it on the target. The target's epoch for the app becomes one above the highest in the bucket.
7. Checks that the target holds that backup, thaws the app there, and with `--smoke-url` sends `GET /` with `Host: <app>.localhost` to that address. A 5xx or no answer within 30 seconds fails the move.

It prints what to do next. Point DNS (or your proxy) at the target. Keep the source frozen until the old records have expired, then remove the app there with `spared rm <app> --yes`. Its backups stay. Then take a backup on the target, so the bucket holds the new epoch (below).

To undo before any client has written to the target: `spared freeze <app>` on the target, then `spared thaw <app>` on the source. Once the target has uploaded a backup, the source's uploads are refused as stale, so move the app back with `spared move` instead.

### When a step fails

- A check fails: nothing changed, and the error has the check's own kind.
- A step before the restore on the target completed fails: `move_aborted`. Until then the source holds the truth, so the move thaws it again, if this run froze it. A freeze that was already there stays, whoever made it (`spared freeze`, a stale upload, an earlier move), and the error says so: a source an earlier move froze and that has since moved on must not serve again next to the target. The target keeps a frozen copy that does not serve and whose data was not replaced, unless the restore call itself timed out: `spared ps --json` on the target shows `restored_from_remote` when it went through. The error's `state` says what each side holds, and `cause` has the step's own error.
- A step after the restore failed (the check of the target's copy, the thaw, the smoke request): `move_incomplete`. The source stays frozen, because the target now holds the newer copy. A failed smoke request freezes the target again. The error names each side's state and the command to run next.
- A run that was cut off (Ctrl-C, a lost connection) leaves the state it reached.

Run the same command again in every case. It works out from both servers where an earlier run stopped: it starts over when the target's data was not replaced, checks and thaws the target when the source's last backup was restored there, and reports a move that already completed without changing anything. Run it with `--dry-run` first to see which.

### Frozen apps

`spared freeze <app>` stops an app on one server and keeps it stopped: it takes the app's instance lock, so a request in flight finishes first, drops the instance and writes `<data-dir>/apps/<app>/frozen`, which a restart reads. Public requests get 503 `app_frozen` with `Retry-After: 30`, and the response names no reason and no other server. Admin commands keep working: `backup` (with the upload), `backups`, `restore`, `evict`, `logs`, `ps` (which shows the state `frozen` and, with `--json`, why), `rm` and `thaw`. A deploy is refused with 409 `app_frozen` until `spared thaw <app>`, so a frozen copy cannot be brought back to life by a stray deploy. `spared move` deploys with `?frozen=move`, which keeps or makes the app frozen. Freezing an app that is frozen keeps the first freeze, so a move never takes over a freeze someone else made, and so never thaws one.

### Epochs and a stale server

Every remote backup carries an epoch, in its id and its object keys. An app starts at epoch 1 on every server. A restore from the bucket sets this server's epoch for the app to one above the highest epoch in the bucket (kept in `<data-dir>/backups/<app>/.epoch`, which `spared rm` leaves with the backups). A restore from a local backup keeps the epoch. `spared ps --json` shows each app's `epoch`, and `restored_from_remote` while the app's data is the one a restore from the bucket put there: `spared rm` and a later restore from a local backup drop that record and keep the epoch.

Before each upload a server looks for a higher epoch of the app in the bucket. If there is one, the app was restored on another server, so the copy here is stale: the upload is refused with `remote_superseded` (in `remote.error`, under `remote_upload_failed`), the local backup stands, and the server freezes the app. Listing orders backups by epoch, then by id.

This detects a second writer after the fact. It does not prevent one: the look and the upload are separate requests, a stale server is only noticed when it backs up, and only once the other server has uploaded under its higher epoch. A fencing record written with conditional PUTs would prevent it. That is not built.

- Any server on the same bucket and prefix that restores an app from the bucket, and later uploads one backup of it, makes every other server stale for that app: the original server's next `spared backup` is refused, the app freezes there (503 to every client), and every later upload is refused. So on a restore drill, or on a staging server refreshed from production backups, never run `spared backup` for that app there. Restore, look, and `spared rm`.
- The check reads object names only, so anyone who can write under the prefix, a key like `<prefix>/spared/v1/<app>/e<higher>/...`, can freeze that app. That is an availability effect: no data is read or changed.
- To recover, delete the higher-epoch objects from the bucket by hand (every key under `<prefix>/spared/v1/<app>/e<epoch>/` above the one the original server holds), then `spared thaw <app>` on the original server.

### What can still lose writes or leave two writers

- Clients that still resolve the old address reach the frozen source and get 503 `app_frozen` until their DNS cache expires. Those writes are refused, not lost, unless a client treats a 503 as success.
- Thawing the source after the move, or bringing back a VM snapshot of the source taken before the freeze, gives two servers that take writes. The source's next upload is refused and freezes it, as above, but writes made there before that are only on the source. There is no merge: copy them over by hand or drop them.
- Anything that writes to the source's data directory without spared, such as another process or files copied in.
- A server whose epoch file was lost (its backups directory deleted) is back at epoch 1, and its own older uploads make it look stale. Restore the app from the bucket to take the next epoch.
- A target that fails after the move, before it took a backup, loses the writes it took since. The source still holds the data as of its freeze, and `spared thaw` there brings that back.

### What was tested

Tested through real servers and the S3 stand-in (`crates/spared/tests/move_app.rs`): a move with the data byte for byte the same and the app answering on the target, a dry run that changes nothing, a frozen app that answers 503 across a restart while every admin command works, an upload refused by the bucket, a cut download, the target going away during the upload and a digest mismatch (each thaws the source, and a rerun completes), a failed smoke request (the source stays frozen, and a rerun finishes without a new backup), a stale server refused and frozen, epochs across a move and a move back, epoch 1 backups, the refusals before a move, and no token or key in any output, log or data directory. Not tested: a move between two real hosts over ssh, or against a real store.

## Give each app its own token

The admin token controls every app on the server. When you run a platform that hosts apps your users or their coding agents write, give each app's build pipeline an app token instead: a token that works for that one app only, within the scopes you pick.

```sh
spared token create alice-notes --expires 90d --name ci-alice --out alice-notes.token
spared token list alice-notes          # ids, scopes, status, created, expires, last used, never a token
spared token revoke 3f2a9c0a1b2c       # stops it at once, also after a restart
```

The token (`spt_` and 64 hex digits) is shown once. `--out` writes it to a new file with mode 0600 and prints nothing but the path; an existing file is never written over. Without `--out` it is printed alone on stdout, and a redirect gives the file the shell's umask, so use `(umask 077; spared token create alice-notes > alice-notes.token)`. Keep the file out of version control (`spared new` ignores `*.token`). The server stores its SHA-256 in `<data-dir>/tokens/<id>.json` (mode 0600) and nothing else, so a lost token cannot be shown again: revoke it and make a new one. The pipeline then deploys with `SPARED_TOKEN=$(cat alice-notes.token) spared deploy`, from a machine that reaches the admin listener.

What an app token can do, by scope (the default is `deploy,read,evict`):

| Scope | What it allows on its own app |
|---|---|
| `deploy` | deploy new code. The manifest's `name` must be the token's app |
| `read` | `spared ps` (which then lists only that app) and `spared logs` |
| `evict` | `spared evict` |
| `backup` | `spared backup` and `spared backups`, including deleting one of its backups |
| `restore` | `spared restore`, also from the bucket |
| `freeze` | `spared freeze` and `spared thaw` |
| `rm` | `spared rm`, which deletes the app and its data |

What it cannot do, whatever its scopes: touch or list another app, mint, list or revoke tokens, read `/metrics` or `/v1/server`, export an app's code, or take part in `spared move`. Each of those answers 403 `wrong_scope`, and for another app's route the answer is the same whether that app exists or not, so a token cannot find out which apps the server holds. Deploys by an app token go through the same limit, grant and compile checks as every deploy, so it cannot raise an app's limits above your `--max-*` caps. Within those checks the token holder writes the manifest: it picks the app's per-request limits up to your caps and the hosts its app may `fetch` (private, loopback and metadata addresses are refused for every app). The per-app limits you set with `spared limits` are out of its reach: no app token can read or change them.

A token made while no app has its name may create that app with its first deploy, once. A token made for an app that exists only redeploys it. `spared rm <app>` revokes every token of that app, so a removed app cannot come back through an old token: to start an app over from an empty database, remove it and make a new token. An app's tokens are not part of its backups and do not move with `spared move`: make new ones on the target.

Operational advice:

- Give each app, and each pipeline that deploys it, its own token, with the fewest scopes it needs. Leave out `rm`, `restore` and `backup` unless the pipeline needs them.
- Set `--expires` and rotate: make the new token, switch the pipeline to it, then revoke the old one. `last_used_at` in `spared token list` shows whether an old token is still in use.
- Keep tokens in the pipeline's secret store. Never put one in an app's code, its `spared.toml` or its database: an app can read its own code and data, and so can anyone it serves them to.
- Revoke at once when a token may have leaked. Revocation takes effect on the next request.

What an app token does not change: building an app still runs its build scripts and proc macros with the privileges of whoever builds it, outside any sandbox. A pipeline that builds code a user wrote must build it in a container or VM of its own, as before. The token only limits what the upload can touch on the server.

Security notes. The server compares the hash of what a request sends against every stored hash, without an early exit, and the admin token in constant time. After 30 wrong tokens in a minute from one address, every app token and unknown token from it, a valid app token included, gets 429 `auth_rate_limited` until the minute ends. The admin token is checked before that limit and always works, so the operator is never locked out. With 256 bits per token this only stops a noisy client. Requests without a token are not counted. No token reaches the access log (which records the `token_id` of an app token's requests), the server's other output, `/metrics`, `spared ps --json`, an error body or the data directory. That holds for a token sent where an id belongs, as in `DELETE /v1/tokens/spt_...`: the log shows `/v1/tokens/<redacted>`, and the 404 does not repeat the value.

Tested through a real server (`crates/spared/tests/app_tokens.rs`): create, list and revoke, also across a restart. Every admin route with the admin token, an app token for the app, an app token for another app that exists and one that does not (the same answer), an app token without the route's scope, and revoked, expired, malformed and missing tokens. Deploys by an app token, with a mismatched manifest name or another app's name refused and nothing written, first-deploy creation once only, and limits still capped. `spared rm` revoking the app's tokens, the failed-auth limit, and no token in any output, log or file under the server's directories. `scripts/e2e.sh` deploys with an app token through the CLI.

## Limit each app

The `--max-*` caps apply to every app alike. When you run a platform that hosts apps your users or their coding agents write, you may want app A to keep 100 MB and app B 5 GB, and no app to hold a core or the network for long. `spared limits` sets that per app. Only the admin token reaches it: the app's own token, whatever its scopes, gets 403 `wrong_scope`, so a tenant can neither read nor raise its limits.

```sh
# A new tenant app: its token and its limits, before its first deploy.
spared token create alice-notes --expires 90d --name ci-alice --out alice-notes.token
spared limits set alice-notes --data-mb 100 --entries 5000 --cpu-percent 10 --egress-mb-per-hour 50

spared limits show alice-notes            # the limits in force and what it used (--json too)
spared limits set alice-notes --data-mb 500   # a larger plan: more disk, the rest unchanged
spared limits clear alice-notes          # back to the server's caps
```

What each one bounds:

| Flag | Bounds | When it is reached |
|---|---|---|
| `--data-mb N` | apparent bytes under the app's `/data`, as `--max-app-data-mb` does for every app | writes fail as on a full disk, and SQLite says `SQLITE_FULL` |
| `--entries N` | files, directories and links under `/data`, as `--max-app-entries` | creating one more fails with a quota error |
| `--cpu-percent P` or `--cpu-ms-per-min N` | the compute time `cpu_ms` measures, summed over requests: P% of one core, or N ms a minute | new public requests get 503 `cpu_budget_exhausted` with `Retry-After` until the budget refills |
| `--egress-mb-per-hour N` | bytes of the app's outbound HTTP calls: request line and headers, response status line and headers, body and trailers (not TLS handshake bytes or unread socket buffers) | the next call fails in the app with `egress_budget_exhausted`, and a body in flight is cut |

How they work:

- The limit in force is the smaller of yours and the server's cap. A number above the cap is kept and shown as `clamped`, and it takes effect up to the new cap if you raise the cap later. A manifest has no key for any of these.
- The data and entry limits feed the same books as the server's caps. The server counts the disk again before it refuses a write and never trusts the app's own word that it freed something. A change to them drops the app's instance, so the next request wakes with them. A restore can bring back more data than the limit: the app can then read it and must delete some before it writes again.
- The CPU budget is a bucket that refills at the rate you set and holds 10 seconds of it, so a quiet app can take a short burst. A request is let in while the bucket holds anything and charged what it used when it ends. It still stops at its own `cpu_ms`, so one request can overdraw the bucket by that much. This is a host-side time budget, not a real-time guarantee: the time charged is the time the server spent running the app's code and its file calls, as for `cpu_ms`. The time to wake an instance is charged too (its start function runs then), and each outbound call that passed the host policy costs a flat 1 ms for its DNS, TCP and TLS setup. The server's other work on outbound HTTP is not charged.
- The egress budget holds one hour of the rate and refills continuously, so the first hour can see up to twice the figure and later hours average to it. Calls the host policy refuses (a host not in `fetch`, a private address) cost nothing. Responses the app sends to its clients and request bodies it receives are not counted.
- The counters belong to the app's name, not to an instance: idle eviction, a trap, `spared evict` and a redeploy keep them, and an app has one instance at a time. They are in memory only and start over when the server restarts. A restart is therefore a way to refill every budget at once. `spared rm` keeps them while the app has an override, so removing the app and deploying it again under the same name does not refill its budgets. `spared limits clear` drops the budgets and what was used, and a later `set` starts full.
- Changing a budget with `set` keeps what the app used, up to the new size. Lowered below that, the budget starts empty and refills at the new rate, so the app runs again within one window (10 s for CPU, an hour for egress), never longer. Raised, the app keeps what it used and gets the difference.
- The accounting costs a few atomic operations per request when the app has no budget, and two short, uncontended mutex locks when it has one. No measurable warm-path cost was seen ([benchmarks](benchmarks.md#per-app-limits)).

Where they live: `<data-dir>/limits/<app>.json` (directory 0700, file 0600, written to a temporary name, synced and renamed), outside the app's directory, so the app never sees the file. You can set limits for a name before its app exists, which is how a platform makes sure an app never serves without them. They survive restarts and redeploys. `spared rm <app>` keeps them, says `limits_kept`, and they apply again if the name is deployed again. Clear them when a name goes to someone else. `spared move` copies an app's limits to a target that has none of its own for that name, before the app serves there, and refuses a target too old to enforce them. A limits file that cannot be read at start keeps its app from being served (it is not loaded) until `spared limits set` or `spared limits clear` replaces it. If the `limits` directory exists but cannot be read, the server refuses to start with `error[limits_unreadable]` and changes nothing in the data dir, rather than serving apps without their limits.

`spared ps --json` shows each app's `quota` to the admin token: the limits in force, which ones you set, which the caps lowered, and its use (`cpu_ms_total`, `cpu_ms_in_window`, `cpu_ms_left`, `cpu_refused`, `egress_bytes_total`, `egress_bytes_in_window`, `egress_bytes_left`, `egress_refused`). `/metrics` carries no app name, so per-app use is only here.

The format stamp: while any app has limits, the data directory is stamped format 2, so a release from before per-app limits refuses to start on it instead of serving those apps without them ([upgrading](upgrading.md#the-format-stamp)). Clearing the last one stamps it back to format 1.

Not bounded per app, even with limits set: memory beyond the manifest's `memory_mb` under `--max-memory-mb` (you cannot set a smaller memory cap for one app), disk read and write rates, the bandwidth of requests and responses on the public listener, and builds, which run wherever the app is built with the builder's privileges. A request-rate limit per app is not built.

Tested through a real server (`crates/spared/tests/app_limits.rs`, `scripts/server-test.py`): an override below a cap is enforced and one above it is clamped, an app at its data limit gets the full-disk error while another app keeps writing to the server's cap, the existing attacks on the books (a file renamed onto itself, a hard link unlinked, a stale stream) hold under per-app numbers, a spinning app at 10% ran 16 requests of 100 ms in 6 seconds (unthrottled it would run about 55) while a neighbor's median latency stayed the same, the budget refills after `Retry-After`, an in-flight request still stops at its `cpu_ms`, limits persist across a restart, a redeploy and `rm`, every limits route answers 403 to an app token for any app, the format stamp goes to 2 and back. The egress meter is tested against a local server in `src/sandbox.rs` (calls run up to the budget, then fail, a body is cut, a neighbor is untouched) and against example.com in `scripts/server-test.py` when it is reachable.

## The admin API

The admin API deploys code, and with `GET /v1/apps/<app>/code` it hands it out. It needs a bearer token: the admin token for everything, or an app token for one app ([above](#give-each-app-its-own-token)). The CLI reads it from `$SPARED_TOKEN` or `~/.spared/token`, which `spared serve` writes on first start. Keep the admin listener on loopback.

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
