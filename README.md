# spared

**An open-source host for Rust web apps with SQLite.** Each app and its database run in their own Wasm sandbox.

It's like Cloudflare Workers plus Durable Objects, but self-hosted, and with better data isolation. It's just one binary on one server that can host many isolated apps.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/pitch-dark.png">
  <img src="docs/img/pitch-light.png" alt="Bar chart of spared against workerd on the same small app, one 4-vCPU VM, lower is better. Warm request p50: 148 against 388 microseconds, 2.6x faster. p99 at 16 connections: 5.45 against 16.53 ms, 3.0x lower. Waking an idle app: 0.9 against 1.07 ms, about level. Memory that cannot be reclaimed at 200 apps: 108 against 516 MiB, 4.8x less. Total memory at 200 apps: 889 against 574 MiB, where spared uses 1.5x more.">
</picture>

- **Faster warm requests.** 2.6x faster than workerd at the median (148 against 388 microseconds).
- **Rust on spared beats JavaScript on workerd on most backend tasks.** 8 of the 14 tested. JavaScript is ahead on crypto, gzip and JSON, where workerd calls native code.
- **Fast wake.** An idle app answers its next request in about 0.9 ms (workerd 1.07 ms). After a reboot, the first request to each app is slower until its code is back in the page cache ([details](docs/benchmarks.md#waking-an-app-whose-code-is-not-in-the-page-cache)).
- **Data isolation that ships with the app.** SQLite runs inside each app's sandbox, not in the host.

## Try it

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/michaelmagan/spared/main/scripts/install.sh | sh
```

Tested on Linux x86_64 with glibc 2.35 or newer. Upgrades: [docs/upgrading.md](docs/upgrading.md).

Apps are Rust crates, so you need Rust from [rustup](https://rustup.rs). Then:

```sh
export PATH="$HOME/.spared/bin:$PATH"
spared setup                                      # once: adds the wasm32-wasip2 target with rustup and puts wasi-sdk 25 in ~/.spared
spared serve --data-dir ./data &                  # the server: apps on 127.0.0.1:8080, admin on 127.0.0.1:8081
spared new my-app && cd my-app && spared deploy   # scaffold a notes app, build it, deploy it
```

```sh
curl -X POST -d '{"text":"first note"}' http://my-app.localhost:8080/api/notes
curl http://my-app.localhost:8080/api/notes
```

The first build compiles SQLite to Wasm and takes about a minute. Later builds take seconds. The full walk-through is [docs/getting-started.md](docs/getting-started.md).

Prefer to let a coding agent do it? Paste this into Claude Code or Codex:

```text
Read https://github.com/michaelmagan/spared/blob/main/AGENTS.md, install spared, and build me a shared shopping list app. Run `spared test` first, deploy it, then give me the live URL and one curl command that proves it works.
```

No Rust toolchain? Run three example apps from the server image with the [try script](docs/operating.md#run-the-examples-from-the-image).

## How it compares

| | Where SQLite runs | Shape | Apps |
|---|---|---|---|
| **spared** | Inside each app's sandbox | One binary | Rust, as Wasm |
| celld | In the host | Single node or a fleet | JavaScript |
| Spin | In the host | Pooled instances | Wasm components |
| workerd | In the host process | Self-hostable runtime | JavaScript |
| wasmCloud | Host plugins, no SQLite | Kubernetes plus NATS | Wasm components |

The long version, with caveats, is in the [FAQ](docs/faq.md#how-is-it-different-from-spin-celld-workerd-golem-and-wasmcloud).

## The trade-offs

- **Memory.** Every app carries its own SQLite. spared uses less memory than workerd up to about 20 apps and more beyond that (889 against 574 MiB at 200 apps). Memory the kernel cannot reclaim is lower at every count (108 against 516 MiB at 200 apps).
- **CPU.** SQLite in the sandbox costs about 1.85x native SQLite's CPU.
- **One request at a time per app.** Different apps run in parallel.
- **No TLS.** spared serves plain HTTP. Put a proxy in front.

The numbers and how to rerun them are in [docs/benchmarks.md](docs/benchmarks.md).

## Future

Next up:

- Stronger resource limits.
- Point-in-time restore and off-machine backups.
- Per-app deploy tokens and disk quotas in the manifest.

## Early preview

spared is version 0.0.1-preview. Data isolation is what it enforces today. It has had no outside audit, so don't host code that would cause serious harm if the sandbox failed.

Guides: [getting started](docs/getting-started.md), [how it works](docs/how-it-works.md), [cookbook](docs/cookbook.md), [operating spared](docs/operating.md), [FAQ](docs/faq.md). For coding agents: [AGENTS.md](AGENTS.md). Contributing: [CONTRIBUTING.md](CONTRIBUTING.md). License: Apache 2.0.
