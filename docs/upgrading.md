# Upgrading

This page shows how to move a spared server to a new release and how to go back. The promise is simple. A release either opens your data directory or refuses to start and tells you why. It never guesses.

Other pages: [getting started](getting-started.md), [how it works](how-it-works.md#where-data-lives), [design](design.md#versions-and-the-data-directory-format).

## Which version do I have?

```sh
spared --version                      # spared 0.0.1-preview (1a2b3c4d5e6f): version, then the commit it was built from
curl http://127.0.0.1:8080/healthz    # {"status":"ok","uptime_seconds":12,"version":"0.0.1-preview"} from a running server
```

`spared serve` also prints its version as the first line of its startup output. The CLI and the server are the same binary.

## Back up first

An upgrade does not rewrite app data, but take a copy before you change the binary. It is your way back.

The complete copy: stop the server, then copy the whole data directory. It holds everything: the apps' databases, their code, their backups and the admin token.

```sh
# stop `spared serve` (Ctrl-C, or stop the service or container), then:
cp -a ./data ./data-before-upgrade
```

With the server image, the data directory is the `spared-data` volume. Stop the container and copy the volume, for example:

```sh
docker stop spared
docker run --rm -v spared-data:/data:ro -v "$PWD":/out debian:bookworm-slim tar -C /data -czf /out/spared-data.tar.gz .
```

Copy a stopped server's directory. A copy taken while apps are writing can catch a database in the middle of a write.

The lighter option, with the server running: `spared backup <name>` for each app. It copies that app's data (not its code or manifest) to `<data-dir>/backups/<name>/`, on the same disk, while the app's instance is stopped. It protects the data against a bad deploy, not against losing the disk or the directory, unless the server also uploads backups to a bucket with `--backup-to` ([operating](operating.md#off-machine-backups)). See [the backup section of getting started](getting-started.md#9-back-up-and-restore).

## Upgrade

1. Note the version you run: `spared --version`.
2. Stop the server and copy the data directory (above).
3. Install the new binary. Run the install script again, build from source, or pull the new image tag. The install script puts one binary, `spared`, in `~/.spared/bin`, replacing the old one, and checks it against the release's `SHA256SUMS`. It prints the `PATH` line to add, offers to run `spared setup` and changes nothing else. Add `SPARED_VERSION=<version>` to pick a release. Linux x86_64 is the tested platform.
4. Start the server on the same data directory. Read its startup lines: the version, the data directory with its format, and how many apps it loaded.
5. Check it: `spared ps` lists your apps, `curl http://127.0.0.1:8080/healthz` reports the new version, and one request to each app you care about answers.

If the server refuses to start, it has changed nothing. The error has a kind and a `fix:` line, and the kinds are in the table below.

## What survives an upgrade

| What | Where | What a new release does with it |
|---|---|---|
| App data | `apps/<name>/data/` (the app's `app.db` and anything else it wrote) | Nothing. The host never opens an app's database; only the app's own code, inside its sandbox, reads it. |
| Deployed code | `components/<sha256>.wasm`, linked as `apps/<name>/component.wasm` | Kept as deployed. |
| Compiled code | `components/<sha256>.cwasm` | Kept while it loads. A compiled file loads only into the wasmtime version and settings that made it, so after a release that updates wasmtime the server recompiles each stored component from its stored wasm, once, at start, and logs `recompiled from the stored wasm`. That start takes longer. The wasm is checked against its SHA-256 first. |
| Manifest, deploy counter, app log | `apps/<name>/manifest.toml`, `version`, `app.log` | Kept. Limits above the server's maximums are clamped at load, as on any start. |
| Backups | `backups/<name>/` | Kept. `spared backups` and `spared restore` work on backups taken by an earlier release of the same data format. |
| Remote backup state | `node-id`, `backups/<name>/.remote/` | Kept. `node-id` names this server in the bucket, so keep it with the data directory: a server with a new one still restores the old backups, and uploads under the new name. On a clone that will run next to the original, delete `<data-dir>/node-id` before its first start, or use a different prefix: two servers with the same node id, bucket, prefix, key and app name write the same object keys. An older release ignores both. |
| Frozen apps | `apps/<name>/frozen` | Kept: a frozen app stays frozen. An older release does not know the marker and serves the app. Do not go back to an older release on a server whose apps were moved away with `spared move`: remove them first with `spared rm <name> --yes`. |
| Epochs | `backups/<name>/.epoch` | Kept. A missing file means epoch 1, which is what every backup from before epochs uses, so they list and restore as before. An older release ignores the file and uploads at epoch 1. |
| Admin token | `admin-token` | Kept, so the CLI's saved token keeps working. |
| Lock file | `.lock` | Kept. The running server holds a lock on it, and the next server rewrites its pid. A copied one is harmless. |
| Format stamp | `format.json` | Read first, then updated with the version that opened the directory. |

Apps do not need a rebuild or a redeploy for a host upgrade: the stored component keeps running.

## The format stamp

The data directory holds `format.json`:

```json
{"created_by":"0.0.1-preview","format":1,"last_opened_by":"0.0.1-preview"}
```

`format` is the number of the on-disk layout: what files an app directory holds, how the component store and the backups are laid out. It is not the release version, and most releases will not change it. At start the server reads the stamp before it reads or writes anything else:

| What it finds | What happens |
|---|---|
| No `format.json` (a new directory) | Opened as format 1 and stamped. |
| The format this release writes | Opened. `last_opened_by` is updated. |
| An older format this release still reads | Upgraded in place at start, then stamped with the new format. From then on, older releases refuse the directory. |
| A newer format than this release reads | Refused: `data_dir_too_new`. Nothing is changed. Run the newer release, or use another `--data-dir`. |
| A format older than this release reads | Refused: `data_dir_too_old`. Nothing is changed. Start it once with the newest release that still reads that format, then with this one. |
| A file that is not a stamp | Refused: `data_dir_stamp_unreadable`. Restore the file from your copy; if you are sure which release last ran on the directory and it is this one, delete the file and the server writes a new one. |

Without `--json`, the refusal is an `error[<kind>]:` line and a `fix:` line, as for every other command, and the exit code is 1. With `spared --json serve ...` it is `{"error":{"kind","message","fix"}}` on stdout.

Data formats so far:

| Format | Written by | Read by |
|---|---|---|
| 1 | 0.0.1-preview and later | 0.0.1-preview and later |

## The compatibility promise

For the 0.0.x preview releases:

- A release opens your data directory, upgrading the layout in place if it has to, or it refuses to start with one of the errors above before changing anything.
- A release that changes the layout raises the format number, says so in its release notes, and adds a row to the table above.
- App data is never read or rewritten by the host, in any release.
- Going back to an older release works while the format is unchanged. Once a newer release has upgraded the directory, the older one refuses it, and the way back is the copy you took.

Not promised yet, and still changing between preview releases: CLI flags and output, `spared.toml` keys, the admin API's routes, the names of metrics and access-log fields, and the generated `src/spared.rs` helper. A release that would stop a stored manifest from loading counts as a layout change and raises the format number.

## Going back

Stop the server, install the older release (`SPARED_VERSION=<version>` with the install script), and start it on the same data directory. If it answers `data_dir_too_new`, the newer release upgraded the layout: put back the copy of the data directory you took before the upgrade, and start the older release on that.

## What has been tested

- A data directory without a stamp opens and is stamped; the apps in it serve with their data.
- A directory stamped with a newer format, an older one, or an unreadable stamp is refused with its error kind, and no file in it changes.
- A compiled component marked as coming from another wasmtime version is recompiled from the stored wasm and the app serves.
- A server restarted on its data directory loads its apps and their data.

These run in `scripts/e2e.sh` and `scripts/server-test.py`. Not tested: an upgrade between two published releases, because there has only been one, and an upgrade interrupted by a crash or a power loss.
