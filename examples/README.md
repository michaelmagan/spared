# Examples

Three small apps that the server image deploys on first start. Each is a normal spared app: a Rust crate built to a `wasm32-wasip2` component, with a `spared.toml` that grants only what it uses (`data = true` for its SQLite file, no `fetch`, no outbound calls).

| App | Source | What it is | Try |
|---|---|---|---|
| `notes` | `examples/notes` (the `spared new` template) | A notes page with add and delete, and a JSON API | `/`, `/api/notes` |
| `list` | `apps/shared-list` | A shared list as a JSON API | `/items` (GET, or POST `{"text":"milk"}`) |
| `demo` | `apps/demo-spared` | A page that lists items and takes new ones | `/`, `/api/items` |

Each `examples/<name>/spared.toml` holds the manifest. `examples/notes` also holds the source; `list` and `demo` are built from the `apps/` crates the benchmarks use, so there is one copy of each.

## Build

```sh
scripts/build-examples.sh      # writes examples/dist/<name>/app.wasm and spared.toml (gitignored)
```

It needs the `wasm32-wasip2` Rust target and wasi-sdk 25 (`spared setup` installs both). The Dockerfile runs it.

## URL forms

With the server on port 8080:

- `http://notes.localhost:8080/`: the app name as a subdomain of `localhost`. Most systems resolve `*.localhost` to loopback.
- `http://localhost:8080/_/notes/` (or any host or IP): the path form for hosts without wildcard DNS, such as one Railway or Fly hostname. It needs `spared serve --allow-path-routing` (env `SPARED_PATH_ROUTING=1`), which the server image turns on. It puts every app on one browser origin, so use it for these bundled examples only. The server strips `/_/notes`, so the app sees `/`; `/_/notes` redirects to `/_/notes/`. A request whose host already names an app goes to that app unchanged.
- `http://notes.example.com/` after `spared serve --domain example.com` (or `SPARED_DOMAIN=example.com`) with wildcard DNS pointing at the server.

The pages use relative URLs so they work under all three.

## Deploy them yourself

`spared serve --examples-dir examples/dist` deploys any subdirectory holding `app.wasm` and `spared.toml` that is not already deployed, and leaves existing apps and their data alone.
