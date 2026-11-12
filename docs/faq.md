# FAQ

Direct answers to what people ask first. Every measurement here comes from one 4 vCPU Linux VM, and anything we did not check is marked "not verified". spared is an early preview, so the CLI and manifest can change.

Other pages: [getting started](getting-started.md), [how it works](how-it-works.md), [isolation](isolation.md), [cookbook](cookbook.md). [AGENTS.md](../AGENTS.md) is the agent-facing reference.

## What is spared for, and what is it not for?

spared hosts many small Rust web apps on one machine. Each app is a Rust crate compiled to a Wasm component with SQLite inside, owns one SQLite file, and can touch only what its manifest grants. It is aimed at code nobody reviewed, for example apps that a coding agent wrote for you.

It is a fit for small apps with modest traffic: a shared list, a form, a small JSON API, a personal tool.

It is not for:

- Code that would cause serious harm if the sandbox failed. There has been no outside audit.
- Large databases or heavy CPU per request. SQLite inside Wasm costs about 1.85 times the CPU of native, and large databases were not tested.
- More than one machine. There is no multi-node mode.
- WebSockets, cron or scheduled jobs. They are not built.
- Apps that need environment variables or secrets handed to them. Apps get none.
- Production traffic that needs a service level. This is a preview with no stability promise.

## How is it different from Spin, celld, workerd, Golem and wasmCloud?

Only what [benchmarks](benchmarks.md) and [design.md](design.md#how-it-differs-from-others) support. The comparisons are not like for like in every case; each benchmark lists its caveats.

- **Spin** (v4.3.0, from its source). SQLite runs in the host process, as a host-side `rusqlite` connection. Instances are pooled and reused, and several instances of one app can run at once with no per-app serialization. spared keeps SQLite inside the sandbox and runs one instance per app. On the same small app, in a separate run, a warm GET at one connection had a p50 of 456 us on Spin against 109 us on spared. Spin was not rerun alongside workerd and celld, and its app opened the database on every request.
- **celld** (v0.6.3, from its source and docs). It uses native SQLite in the node process, and its own security notes say it trusts the application code and that mutually distrusting tenants should not share a fleet. spared pays a speed cost to avoid that. The comparison tables use `celld dev` mode, so their cold and write numbers are not like for like. It was also run in production mode against a local S3-compatible bucket (RustFS, on loopback with local disk), which is a best case with no network, and celld's docs do not qualify that bucket for production.
- **workerd.** Measured against workerd 1.20261110.1 with a JavaScript worker using a SQLite-backed Durable Object, in four sessions. Warm requests at one connection were 2.62x lower at p50 on spared in the latest session (148 against 388 us). The earlier sessions read 2.34x, 2.51x and 2.88x. The p99 was 3.03x lower at 16 connections (5.45 against 16.53 ms) on one saturated CPU, where the numbers are mostly queueing. spared did not win everywhere. With one connection and 10% writes its p99 was higher in two of the four sessions, a first request that seeds the database was slower in all four, and waking an idle app with data on disk and its code in the page cache (not a cold process start) was close (1.08x slower, then 1.26x, 1.24x and 1.20x faster), so there is no wake claim. After a reboot (cold page cache), the first request to each of 500 apps took 5.4 ms with no read-ahead. With `--prefetch-mb 4096`, which covered their compiled code, it took 1.8 ms against workerd's 2.5 ms, and with the 1 GB default it took 3.3 ms ([benchmarks](benchmarks.md#waking-an-app-whose-code-is-not-in-the-page-cache)). One runs JavaScript on V8 and the other Rust on wasmtime, and workerd's page also pays a Durable Object hop. Memory for different apps was measured as totals: spared uses less up to about 20 apps and workerd uses less physical memory beyond that (889 MiB against 574 MiB at 200), while spared uses less memory that cannot be reclaimed at every count (108 MiB against 516 MiB at 200), though in a cgroup, which counts page cache, it used 1.84x workerd's. Identical apps cost 0.28 MiB each on spared.
- **Cloudflare Durable Objects.** spared's one-owner-per-app rule serializes writes the way a Durable Object does. That is a design reference, not a tested equivalence. 'Better data isolation' means one thing: in workerd, the open-source Workers runtime, the SQLite engine behind a Durable Object runs natively in the one process all apps share, while in spared the engine and the database file sit inside each app's own sandbox, so a SQLite bug one app reaches stays in that app. It is a claim about where the database engine runs, not about Cloudflare's hosted service, which was not run.
- **Golem.** Its pages describe a runtime under the BUSL-1.1 license, moving to Apache-2.0. Not verified against the repository's license file. Beyond that, not verified.
- **wasmCloud** (v2.12.1). Its docs describe components as stateless. State goes through host plugins such as wasi:keyvalue and wasmcloud:postgres, which run in the host process outside the component sandbox, and its source has no SQLite support. It runs untrusted components on wasmtime and its threat model treats them as hostile, but it advises separate hosts or nodes for mutually hostile tenants. Production deployment is Kubernetes with NATS, and `wash dev` runs locally with in-memory plugins.

## Can it run untrusted code safely?

Tested:

- A 50-case escape suite: 43 attacks, all denied, and 7 controls that had to succeed and did. It covers reading and writing other apps' files, symlinks, memory and CPU caps, raw sockets, and outbound requests to loopback, private, link-local and metadata addresses in many spellings.
- A server integration test with a deliberately misbehaving app: admin API checks, limit refusals, a panic, oversized responses, a spinning app while other apps keep answering, redeploys under load, and stalled uploads.

Not tested or not covered:

- No outside audit.
- No fuzzing. The suite is black-box tests of one configuration against wasmtime 50.0.1. A bug in wasmtime would defeat the sandbox.
- Side channels, power loss, large databases, TLS and real networks.
- Building an app is not sandboxed. `spared test`, `spared deploy` and `spared dev` run `cargo build` on your machine, so the app's `build.rs`, its dependencies' build scripts and its proc macros run with your privileges, like any cargo build. The CLI keeps `SPARED_TOKEN` and `SPARED_ADMIN_TOKEN` out of the build's environment and reads the token only after the build has finished, but a build script can still read `~/.spared/token` the way you can. Do not build an app you did not write on a machine where that file matters: build it in a container or VM, or build elsewhere and upload the finished component to the admin API. The admin token is one token for the whole server and controls every app; a token that can deploy one app only is planned, not built.
- The per-request outbound call limit (`max_outbound`) has code but no test in the suite. The request deadline (`deadline_ms`) is tested: a request that outlives it is ended with a 504.
- Known gaps: one app can still use more than its share of CPU, disk bandwidth and network across requests, because the limits bound one request or one app's stored data. The data size cap (`--max-app-data-mb`, 1024 MB by default) is an operator setting that applies to every app alike and cannot be set in an app's manifest. The CPU limit counts wall-clock milliseconds of the app's code and of the server's work inside its file calls (outbound HTTP work is bounded by `deadline_ms`, not by `cpu_ms`). An app can reach services on the server's own public addresses through a `fetch` host that resolves there, and any port on a listed host is allowed.

Do not host code that would cause serious harm if the sandbox failed. Nobody outside the project has audited it, and a bug in wasmtime would defeat it. [Isolation, in plain words](isolation.md) says what is guaranteed today, what is not, and where it is going.

## Is it production ready?

No. spared is an early preview (0.0.1-preview): the CLI, the manifest and the host interface can change between releases, and [upgrading](upgrading.md) says what a new release does with your data directory.

Tested on one Linux x86_64 VM (glibc 2.35 or newer): building and running apps, deploy and redeploy, wake and idle unload, restarting a server on its data directory, backups and restore, the escape suite, the server integration test, 100 `kill -9` trials in the shipped journal mode, the install script against a local stand-in for the release server, and the Docker image built locally.

Not tested: macOS, arm64 Linux, the release binaries for those platforms, Railway, Fly, an upgrade between two published releases, power loss, large databases, TLS and real networks. Nobody outside the project has reviewed its security.

Known gaps: there are no per-app deploy tokens, no point-in-time restore and no disk quota set per app in the manifest, and local backups are kept by count only (`--max-backups-per-app`, 20 by default). Off-machine backups and `spared move` were tested against an S3 stand-in, not against AWS S3, R2 or another real store. There is no failover: a move needs both servers up, and a second server serving the same app is detected only when it uploads a backup, not prevented.

Data isolation is what spared enforces today, so do not host code that would cause serious harm if the sandbox failed. [Isolation, in plain words](isolation.md) says what is guaranteed and what is not.

## What languages can I use?

Rust. The scaffold, the helpers and every test are Rust with `wstd` and `rusqlite`. In principle a component built from another language that exports the HTTP handler and imports only granted interfaces would load, but that was not tried. Not verified.

## How many apps can one machine hold?

Measured on one 4 vCPU, 16 GB VM:

- Different apps: about 15 MiB plus 4.4 MiB of physical memory (PSS) per hot app, so about 230 hot apps per GiB (about 310 with a size-optimized build). workerd fits about 372 per GiB in the same test. Most of spared's per-app cost is each app's own compiled code as file-backed pages that the kernel can reclaim, but that code is not free to drop, because dropping it means reading it from disk again, and latency in that state was not measured. We do not publish a memory-only capacity figure for it.
- An idle app is not free. After its instance is dropped, an app with its own code keeps about 4.1 MiB mapped. An app that is deployed but has never been requested since the server started costs about 0.67 MiB. Disk is extra.
- Identical apps share compiled code and cost about 0.28 MiB each when hot, about 0.09 MiB when idle.
- 10,000 apps: an in-process harness, not the server, created 10,000 app directories of one shared component with 500 instances live, with the CPU limit off and no memory cap. The running server was never tested at that count, and it maps every deployed component at start. Read it as a measure of per-app overhead for copies of one app, not as a number of apps a server holds.
- The measurements used synthetic apps on one VM with no memory pressure, and did not measure latency when the kernel drops code pages (`docs/memory-methodology.md`). The memory-curve fits come from the current build, except the size-optimized one, which comes from the build before the `pread`/`pwrite` change.
- CPU matters as much as memory. A request with 10 reads and 1 durable write used about 560 us of CPU against about 280 us with SQLite on the host (2.05x), so one core serves about half as many write-carrying requests.

Those tests used tiny databases and one pinned CPU, not a 1 vCPU, 1 GB cgroup. Treat them as an upper bound.

## How does it compare with running the binary directly or in a container?

The same small app was written as a plain Rust program (same routes, same SQL, same SQLite build and settings, identical responses) and measured against spared on one 4 vCPU VM ([benchmarks](benchmarks.md#native-rust-process-and-container-baselines)):

- Running the binary directly is faster and smaller. A warm request took 59 us at p50 against 161 us on spared, so the sandbox adds about 0.1 ms per request (2.7x). With 16 connections on one saturated CPU the native process was 4.4x faster at p50 (540 against 2,388 us), about 29,600 requests per second per core against about 6,700. Each different app costs about 2.8 MiB as its own process against 4.4 MiB on spared, which also needs about 18 MiB for its server. Native used less memory at every app count measured.
- A container per app with Docker's default settings costs about 6.3 MiB per container, and most of that slope is Docker's own helpers: the containerd shim and the docker-proxy. The app itself is about 0.35 MiB and the container's own cgroup about 0.65 to 0.85 MiB. Add about 150 MiB for dockerd and containerd, which is a sunk cost on a host that already runs Docker. With the daemons charged it used more than spared at every count measured. Without them it used less at one app (18.1 against 22.6 MiB) and more at ten (68.9 against 62.3 MiB). Other runtimes and `--network host` were not measured.
- Starting from idle is where spared is faster: waking an evicted app took 0.9 ms with its code in the page cache (1.8 to 5.4 ms after a reboot with a cold page cache, depending on `--prefetch-mb`, see the [benchmarks](benchmarks.md#waking-an-app-whose-code-is-not-in-the-page-cache)), starting the native process took 5.1 ms, and `docker run` took 184 ms. A process or container that is never stopped pays no start cost but keeps its memory.
- What spared adds for that cost is the sandbox: each app sees only its own data directory and granted hosts, and runs under memory and CPU limits. A plain process has whatever access its user has unless you confine it yourself.

## Is my data safe if the process crashes? If the power fails?

Process crashes: 100 `kill -9` trials in the shipped journal mode (49,307 acknowledged commits) and 100 more in WAL (140,341) lost no committed write and failed no integrity check, and the same again on the current build (50,620 and 165,003). The test kills a probe program that runs the same SQLite settings, not `spared serve`. Killing a process does not drop the operating system's page cache, so the test checks that the journal leaves a consistent file and does not exercise fsync or cover a power loss.

Power loss: not tested. Whether the virtual disk's flushes reach stable storage was not verified either.

## Can I use my own domain?

Yes, one domain for all apps. Run `spared serve --domain example.com` (or set `SPARED_DOMAIN`) and point wildcard DNS for `*.example.com` at the machine. Each app then answers at `<app>.example.com`, and `<app>.localhost` keeps working. The docs describe no setting for a different, unrelated domain per app.

Two warnings:

- spared does not describe TLS on the public listener, and TLS is listed as untested. Check how you will serve HTTPS before you use a real domain.
- Apps on sibling subdomains can set cookies for each other unless the domain is on the Public Suffix List.

Path routing is the other form: `http://<host>:<port>/_/<app>/`. It needs `--allow-path-routing` (or `SPARED_PATH_ROUTING=1`) and is off by default. It puts every app on one browser origin, so one app's pages can read and change other apps' data in a visitor's browser. Use it only for apps you wrote and trust, such as the bundled examples on a host with a single name.

## How do I back up my data?

`spared backup <name>` copies one app's data directory to `<data-dir>/backups/<name>/<UTC timestamp>/` and verifies the copy with SHA-256. `spared backups <name>` lists them, and `spared restore <name> --from <id> --yes` replaces the data, keeping the replaced data as a `pre-restore` copy. A backup holds data only, not code. It sits on the same disk. To survive losing the disk, start the server with `--backup-to s3://<bucket>/<prefix>` and a backup key: every backup is then also uploaded, encrypted, to that S3-compatible bucket, and `spared restore <name> --from remote:<id> --yes` brings one back on any server that has the key ([operating](operating.md#off-machine-backups)). spared never deletes in the bucket, so give the bucket a lifecycle rule. Copying `<data-dir>/backups/` elsewhere also works. Point-in-time restore is not built.

To move an app to another server, start both with the same `--backup-to` and key and run `spared move <name> --to <target admin URL>`. It copies the code, freezes the app on the source, backs it up, restores it on the target and checks it, then tells you to point DNS at the target. Clients that still reach the old server get 503 `app_frozen` with `Retry-After`. Details, and what can still lose writes, are in [operating](operating.md#move-an-app-to-another-server). Commands are in [getting started](getting-started.md#9-back-up-and-restore), and details in [AGENTS.md](../AGENTS.md#backups).

## How do I see what the server is doing?

`spared serve` writes one JSON line per request to stdout: time, listener, app, method, path, status, duration and a request id. It never logs query strings, headers or bodies, and `--access-log off` turns it off. `GET /healthz` on the public port answers 200 with the version and uptime, for a load balancer. `GET /metrics` on the admin port gives Prometheus-format counters (requests, errors by kind, wakes, evictions, apps, memory) and needs the admin token. There is no OpenTelemetry and no tracing. `spared logs <name>` is the app's own output. See [how it works](how-it-works.md#what-the-operator-sees).

## What happens to my data when I upgrade spared?

App data is not touched: the host never opens an app's database. The data directory carries a format stamp, and a release that cannot read the layout refuses to start with `data_dir_too_new` or `data_dir_too_old` and a fix, before it changes anything. After a release that updates wasmtime, the server recompiles the stored components from their stored wasm at start. The CLI, the manifest keys and the admin API can still change between preview releases. Copy the data directory before you upgrade; the steps are in [upgrading](upgrading.md).

## What happens when I deploy?

The server validates the manifest, the component and its imports first, then precompiles it, stores it by hash, swaps it in and drops the old instance. The app's data is kept. If validation fails, the running version is unchanged. Redeploying the same bytes reuses the stored compiled copy. Details and one untested crash window are in [how it works](how-it-works.md#deploy).

## What is the licence?

Apache 2.0. See `LICENSE`. `THIRD_PARTY.md` records the dependency license scan.

## How do I report a security issue?

Use GitHub private vulnerability reporting: open the repository's Security tab, choose "Report a vulnerability", and describe the problem with the version or commit, how you ran it, and steps to reproduce. Do not open a public issue. This is a preview with no service level.

## How do I contribute?

Open an issue before large work, since the API and CLI change. [CONTRIBUTING.md](../CONTRIBUTING.md) has the build steps, the benchmark scripts, and the rules: run `cargo fmt` and `cargo clippy`, update [AGENTS.md](../AGENTS.md) when you change the CLI, templates or an error kind, add a case to the escape suite for sandbox changes, and never add a speed claim without a result file. By contributing you agree to license your work under Apache 2.0.
