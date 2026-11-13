# wasip3-experimental: several requests in one instance (EXPERIMENTAL)

**Status: EXPERIMENTAL.** It is off unless the operator turns it on, it is only for apps without data, and its limits and behavior may change in any release. The wasmtime crates spared links (50.0.1) say of their WASI 0.3 support: "Experimental, unstable and incomplete" and "not ready for production use". Do not run apps on it that you cannot afford to have fail.

A wasip2 app runs one request at a time in its one instance. A request that waits 200 ms for an upstream holds the instance for those 200 ms, and the next request waits behind it. An app built for WASI 0.3 (`wasi:http/handler@0.3`) can instead have several requests in flight in one instance: while one awaits, the instance runs another.

## Turning it on

The operator starts the server with the flag:

```sh
spared serve --data-dir ./data --experimental-wasip3
```

| Flag | Env | Default | What it does |
|---|---|---|---|
| `--experimental-wasip3` | `SPARED_EXPERIMENTAL_WASIP3` | off | serve apps whose `spared.toml` says `runtime = "wasip3-experimental"` |
| `--max-app-concurrency` | `SPARED_MAX_APP_CONCURRENCY` | 4 | requests one wasip3 instance may have in flight, 1 to 64 |

Without the flag, a deploy of such an app is refused with 422 `experimental_runtime_off`, whose fix names the flag. An app deployed while the flag was on and loaded by a server started without it is not served: the server prints `skipping app <name>` with the reason, and the app answers 404 until the server runs with the flag again. `spared dev` turns the runtime on by itself when the app in the current directory asks for it.

## Writing an app

```sh
spared new my-app --template fetcher3
```

The `fetcher3` template is a library crate (`crate-type = ["cdylib"]`) that uses the `wasip3` crate, version 0.10, and builds for `wasm32-wasip2` as that crate's README says. It exports the handler with `wasip3::http::service::export!` and makes outbound calls with `wasip3::http::client::send`. `spared test`, `spared deploy` and `spared dev` build it like any other app, and `spared test` runs it in a wasip3 instance. The wstd crate the other templates use has no WASI 0.3 release, so it cannot be used here.

The manifest:

```toml
name = "my-app"
runtime = "wasip3-experimental"

[grants]
data = false
fetch = ["example.com"]

[limits]
concurrency = 4
```

- `runtime` is `wasip2` (the default when the line is absent) or `wasip3-experimental`.
- `data` must be `false`. The default is `true`, so the line is required. With `data = true` the deploy is refused with 400 `wasip3_data_refused`. Requests in flight in one instance would share one SQLite connection, and a transaction held across an await would let one request see, or roll back, another request's writes.
- `[limits] concurrency` asks for fewer requests in flight than the server's `--max-app-concurrency`, never more (`limit_too_high`). Left out, the app gets the server's value. It is refused in a wasip2 manifest.

## What a wasip3 app may import

The import check matches each import with its version, for every app. A wasip3 app may import:

- `wasi:cli`, `wasi:clocks`, `wasi:random` and `wasi:io` at 0.2, which Rust's std uses (`println!` goes to the app log through `wasi:cli/stdout@0.2`),
- `wasi:clocks` and `wasi:random` at 0.3, and `wasi:http/types@0.3`,
- `wasi:http/client@0.3` when `fetch` lists a host.

`wasi:filesystem`, `wasi:sockets` and `wasi:cli` at 0.3 are never linked, and an import of any of them is refused with `ungranted_import`. `wasi:filesystem@0.2` is refused too, since these apps have no data. The outbound policy is the one wasip2 apps have: hosts listed in `fetch` only, private, loopback, link-local and metadata addresses always refused.

## What the limits mean here

| Limit | wasip2 | wasip3-experimental |
|---|---|---|
| `memory_mb` | the instance's memory, which serves one request | one budget for the instance, shared by the requests in flight. Memory a request grew is not given back while the instance lives |
| `cpu_ms` | compute per request | compute per request, counted in 1 ms epoch ticks charged to the request whose code runs (wasmtime's task-group hook). Going over traps, which stops every request in flight |
| `max_outbound` | outbound calls per request | outbound calls per request, kept the same way. A call made while no request's code runs is refused |
| per-request log allowance | per request | per request, for output through `wasi:cli@0.2` (std's `println!`). Output written while no request's code runs shares one allowance for the instance |
| `deadline_ms` | the request is stopped and the instance dropped | the client gets 504, and the request keeps running inside the instance (below) |
| operator CPU budget (`spared limits`) | each request's poll time | the instance's poll time, all requests together |

`cpu_ms` counts the guest's own code only. The host's work for a request (an outbound call's DNS, connect and TLS) is charged to the app's CPU budget over time, and each outbound call costs the flat 1 ms it costs a wasip2 request, but host work is not counted toward one request's `cpu_ms`.

## What it gives up

These come from what wasmtime 50.0.1 can do, and each has a test in `crates/spared/src/server/wasip3_tests.rs`.

- **No database apps.** `data = true` is refused.
- **Compute blocks the other requests.** Guest code that computes holds the instance. While one request computes, the others in the same instance wait, even the ones whose awaited result is ready. The wait is bounded by `cpu_ms`, since the computing request is stopped there.
- **One trap fails every request in flight.** A panic, an abort, a request over `cpu_ms` or over `memory_mb` stops the whole instance, so every request in flight in it gets the same error (`app_trap`, `cpu_limit` or `memory_limit`, the message says that it stopped every request in flight). The instance is dropped and the next request wakes a new one. `--max-app-concurrency` bounds how many requests one trap can take down.
- **A deadline leaves a task running.** wasmtime cannot cancel one request inside an instance. At `deadline_ms` the client gets 504 `deadline`, the instance takes no more requests, and the next request wakes a new instance. The request that ran out keeps running in the old instance, with its memory and its outbound calls, until the old instance's other requests finish or for at most another `deadline_ms`, and then the instance is dropped. Every request the old instance still holds started before the one that ran out, so by then each has passed its own deadline too.
- **Not production ready upstream.** wasmtime marks its WASI 0.3 code "not ready for production use" and says "Bug and security fixes limited to wasip3 will not be given patch releases". spared stays on wasmtime 50.0.1. The label EXPERIMENTAL stays until wasmtime removes that notice and can cancel one task.

## Draining

A redeploy, `spared rm`, `spared evict`, `spared freeze`, a backup or restore, a change to the app's limits, the idle drop and shutdown all wait for the requests in flight before they drop a wasip3 instance, for at most `deadline_ms` (shutdown for at most its grace period). New requests wait meanwhile, as they wait for a wasip2 request in flight. The idle drop skips an instance with requests in flight.

## Measured

In one local run on a 4 vCPU Linux VM (release build), 8 requests to one wasip3 instance, each awaiting a loopback upstream that answers after 200 ms, all finished in 204 ms. Run one at a time they would take at least 1,600 ms. The test that measured it, `eight_requests_awaiting_a_200_ms_upstream_overlap_in_one_instance`, asserts under 600 ms, and `crates/spared/tests/wasip3.rs` asserts the same bound for 8 requests that sleep 200 ms through a real `spared serve`.

Not measured: memory with many requests in flight, request bodies streamed in, more than 8 requests at once, and the release build under load.

## Where it lives

- `crates/spared/src/p3.rs`: the instance (one `run_concurrent` loop per instance, fed by a channel) and the per-request budgets.
- `crates/spared/src/sandbox.rs`: the separate engine with component-model async on (wasip2 apps keep an engine with it off) and the linker.
- `crates/spared/src/grants.rs`: the manifest keys and the versioned import check.
- `probes/p3-probe`: the test guest. The tests build it for `wasm32-wasip2`, so `cargo test` needs that target (`rustup target add wasm32-wasip2`).
