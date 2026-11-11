# Templates

`spared new <name> --template <t>` starts an app from one of nine small, commented examples. Each is one `src/main.rs` and one `spared.toml`, with the same generated `src/spared.rs` helpers and the same `SCHEMA` pattern. `spared templates` lists them (`--json` for scripts). Without `--template` you get `notes`.

```sh
spared templates
spared new my-links --template shortener
cd my-links
spared test --path /           # build and request / in a throwaway sandbox
spared serve --data-dir ./data # in another terminal, once
spared deploy                  # prints http://my-links.localhost:8080/
```

`spared test` sends GET requests only. The routes below that write data (POST, PUT, PATCH, DELETE) need a deployed app and curl. In the examples, `$APP` is `http://<name>.localhost:8080`.

## What every template shares

- `const SCHEMA` holds `CREATE TABLE IF NOT EXISTS` statements. The generated `with_db` runs it each time the app opens its database, so a first request creates the tables and a redeploy keeps the data.
- The database file is `/data/app.db`, opened with the `unix-none` VFS. That is safe because one instance owns the file at a time; the helper in `src/spared.rs` sets it up and you should not open the file any other way.
- `spared.toml` lists the grants the app has: nothing is allowed unless it is listed. Every template needs only `data = true`, except `fetcher`, which adds one outbound host.
- Pages use relative URLs (`api/count`, not `/api/count`), so they work under `/_/<app>/` when a server has path routing on.

## The templates

| Template | What it shows | Routes |
|---|---|---|
| `notes` | The default. Add and delete notes from a page, plus a JSON API. | `GET /`, `GET /api/notes`, `POST /api/notes`, `DELETE /api/notes/<id>` |
| `counter` | An atomic increment with one `UPDATE ... RETURNING`, a button page and a JSON API. | `GET /`, `GET /api/count`, `POST /api/increment` |
| `todo` | Create, read, update and delete with checkboxes. | `GET /`, `GET /api/todos`, `POST /api/todos` with `{"text": "buy milk"}`, `PATCH /api/todos/<id>` with `{"done": true}`, `DELETE /api/todos/<id>` |
| `guestbook` | Visitor text in server-rendered HTML, escaped, newest first. | `GET /`, `POST /api/messages` with `{"name", "message"}` |
| `kv` | A key-value store: PUT, GET, DELETE one key, list all keys sorted. Keys are 1 to 64 characters of `a-z A-Z 0-9 . _ -`; values are text up to 64 KiB. | `GET /` (a one-line help text), `GET /kv`, `PUT /kv/<key>`, `GET /kv/<key>`, `DELETE /kv/<key>` |
| `shortener` | Create a short code for an http(s) URL, answer 302, count the hits. | `GET /`, `POST /api/links` with `{"url"}`, `GET /api/links`, `GET /api/links/<code>`, `GET /<code>` |
| `webhook` | Receive any JSON, store it with a timestamp, list the last 50 (newest first). `POST /hook` and `GET /events` need the `x-webhook-secret` header. | `GET /` (a one-line help text), `POST /hook`, `GET /events` |
| `fetcher` | An outbound HTTPS call to a host you grant, returning its status and size. | `GET /`, `GET /fetch` |
| `blog` | A posts table, list and single-post pages as HTML, and a bearer-token publish endpoint. | `GET /`, `GET /posts/<slug>`, `POST /api/posts` |

### Two constants you must change

`webhook` and `blog` protect their write routes with a secret kept in a constant at the top of `src/main.rs`: `SECRET = "change-me-before-you-deploy"` and `TOKEN = "change-me-before-you-deploy"`. An app gets no environment variables, so a secret can only live in the app's code or database, and the server stores both unencrypted. Change it before you deploy anywhere other than your own machine, and do not publish the source with the real value in it.

`blog` will not publish until you do: while `TOKEN` still holds the shipped value, `POST /api/posts` answers 503 and stores nothing, even with the right header. Edit the constant, then redeploy. `webhook` has no such guard, so it accepts the shipped value. `scripts/templates-test.py` checks both.

```sh
curl -X POST -H 'x-webhook-secret: change-me-before-you-deploy' -d '{"any":"json"}' $APP/hook
curl -X POST -H 'authorization: Bearer <your TOKEN>' \
     -d '{"title":"Hello","body":"First post.\n\nSecond paragraph."}' $APP/api/posts
```

### The `fetcher` grant

`fetcher` calls `https://example.com/`. Two edits point it elsewhere: the `HOST` constant in `src/main.rs` and the `fetch = ["example.com"]` line in `spared.toml`. Keep them equal. A request to a host that is not in `fetch` is refused, and so is a listed host whose name resolves to a private, loopback, link-local or metadata address. `spared test` only requests `/`, which makes no outbound call, so it passes offline. Hit `/fetch` on a deployed app to make the call. If the network rewrites HTTPS certificates, set `SPARED_EXTRA_CA_FILE` for `spared serve` (see the troubleshooting table in AGENTS.md).

## Start from one and adapt it

1. Scaffold with `spared new <name> --template <t>` and run `spared test --path /` to confirm it builds before you change anything.
2. Change `SCHEMA` first, then the handlers. To add a column to a table that already holds data, run an `ALTER TABLE` in `SCHEMA` that is safe to repeat, or add a new table; there is no migration step. To start over, `spared rm <name> --yes` deletes the data.
3. Add a route by adding an arm to the `match (method, path)` in `main`. Return a 4xx response for the caller's mistakes; a 5xx fails `spared test`.
4. Add a grant only when the build needs one. `spared test` and `spared deploy` name the missing grant in an `ungranted_import` error.
5. `spared dev` rebuilds and redeploys on every save once you have the app running, if ports 8080 and 8081 are free.

The behaviour of every template is checked by `scripts/templates-test.py`, which a new template must extend.
