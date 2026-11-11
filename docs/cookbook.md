# Cookbook

Copy a recipe to get an app that does one common thing. Each one is a complete `src/main.rs`, the `spared.toml` lines it needs, and `curl` commands to try it.

Every snippet was built with `spared test` against a fresh `spared new` scaffold and run on a real `spared serve`. Recipes that need the network say so. The scaffold and helpers may change in this early preview. If a snippet stops building, [AGENTS.md](../AGENTS.md) is the reference. See also [getting started](getting-started.md), [how it works](how-it-works.md) and the [FAQ](faq.md).

## Using a recipe

```sh
spared serve --data-dir ./data     # in another terminal, if it is not running
spared new my-app && cd my-app
# replace src/main.rs with the recipe, and edit spared.toml if the recipe says so
spared test --path <a GET path the recipe serves>
spared deploy
curl http://my-app.localhost:8080/<the same path>
```

Not every recipe answers `/`. Use a GET path from the recipe's own "Try it" block, such as `/todos` for the first one. A path the recipe does not serve gets a 404, which `spared test` counts as a pass.

Notes that apply to every recipe:

- `src/spared.rs` is generated and provides `with_db`, `json_response`, `html_response`, `text_response` and `read_json`. The recipes use it. Every `src/main.rs` starts with `#[allow(dead_code)] mod spared;` and must define `const SCHEMA: &str`. It runs on every database open, so use `CREATE TABLE IF NOT EXISTS`. Recipes that never touch the database set it to an empty string.
- `Cargo.toml` already has `wstd` 0.6.8, `rusqlite` 0.41 (bundled SQLite), `serde` with `derive`, and `serde_json`. No recipe adds a dependency.
- `spared test` only sends GET requests and treats 4xx as a pass. To try POST, PUT and DELETE, deploy and use `curl`.
- Write links and `fetch` calls in pages with relative URLs (`sign`, not `/sign`) so they work with path routing as well as with hostnames.
- The examples use `my-app` as the app name. Use your own.

Recipes:

1. [JSON CRUD with SQLite](#json-crud-with-sqlite)
2. [An HTML form page](#an-html-form-page)
3. [Query parameters and path routing without a framework](#query-parameters-and-path-routing-without-a-framework)
4. [Schema changes with `PRAGMA user_version`](#schema-changes-with-pragma-user_version)
5. [Call an outside API with a fetch grant](#call-an-outside-api-with-a-fetch-grant)
6. [Read and set headers](#read-and-set-headers)
7. [Redirects](#redirects)
8. [File storage under /data](#file-storage-under-data)
9. [A counter with an atomic update](#a-counter-with-an-atomic-update)
10. [Simple pagination](#simple-pagination)
11. [Error handling that returns JSON](#error-handling-that-returns-json)
12. [Logging for `spared logs`](#logging-for-spared-logs)

## JSON CRUD with SQLite

A todo list with create, read, update and delete. `PUT` takes a partial body: fields you leave out stay as they are.

`spared.toml`: the scaffold's defaults are enough (`data = true`).

```rust
#[allow(dead_code)]
mod spared;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS todos(
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0
)";

#[derive(Serialize)]
struct Todo {
    id: i64,
    text: String,
    done: bool,
}

#[derive(Deserialize)]
struct NewTodo {
    text: String,
}

#[derive(Deserialize)]
struct Patch {
    text: Option<String>,
    done: Option<bool>,
}

fn find(id: i64) -> Option<Todo> {
    spared::with_db(|c| {
        c.query_row("SELECT id, text, done FROM todos WHERE id = ?1", [id], |r| {
            Ok(Todo { id: r.get(0)?, text: r.get(1)?, done: r.get::<_, i64>(2)? != 0 })
        })
        .optional()
        .unwrap()
    })
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/todos") => {
            let todos: Vec<Todo> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT id, text, done FROM todos ORDER BY id").unwrap();
                s.query_map([], |r| Ok(Todo { id: r.get(0)?, text: r.get(1)?, done: r.get::<_, i64>(2)? != 0 }))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            });
            spared::json_response(200, &todos)
        }
        (Method::POST, "/todos") => {
            let new: NewTodo = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            let id = spared::with_db(|c| {
                c.execute("INSERT INTO todos(text) VALUES (?1)", [&new.text]).unwrap();
                c.last_insert_rowid()
            });
            spared::json_response(201, &find(id).unwrap())
        }
        (method, p) if p.starts_with("/todos/") => {
            let Ok(id) = p["/todos/".len()..].parse::<i64>() else {
                return Ok(spared::text_response(400, "todo id must be a number\n"));
            };
            match method {
                Method::GET => match find(id) {
                    Some(t) => spared::json_response(200, &t),
                    None => spared::text_response(404, "no such todo\n"),
                },
                Method::PUT => {
                    let patch: Patch = match spared::read_json(&mut req).await {
                        Ok(v) => v,
                        Err(bad_request) => return Ok(bad_request),
                    };
                    spared::with_db(|c| {
                        c.execute(
                            "UPDATE todos SET text = COALESCE(?1, text), done = COALESCE(?2, done) WHERE id = ?3",
                            rusqlite::params![patch.text, patch.done, id],
                        )
                        .unwrap();
                    });
                    match find(id) {
                        Some(t) => spared::json_response(200, &t),
                        None => spared::text_response(404, "no such todo\n"),
                    }
                }
                Method::DELETE => {
                    let n = spared::with_db(|c| c.execute("DELETE FROM todos WHERE id = ?1", [id]).unwrap());
                    if n == 0 { spared::text_response(404, "no such todo\n") } else { spared::text_response(200, "deleted\n") }
                }
                _ => spared::text_response(405, "method not allowed\n"),
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl -X POST -d '{"text":"buy milk"}' http://my-app.localhost:8080/todos
curl -X PUT -d '{"done":true}' http://my-app.localhost:8080/todos/1
curl http://my-app.localhost:8080/todos
curl http://my-app.localhost:8080/todos/1
curl -X DELETE http://my-app.localhost:8080/todos/1
curl -X POST -d 'not json' http://my-app.localhost:8080/todos
```

The last call returns a 400 with `{"error":"invalid JSON body: ..."}` from `read_json`. The recipe uses `unwrap` on database calls for brevity. A failed query panics, the request fails with `app_trap`, and the next request gets a fresh instance. [Error handling that returns JSON](#error-handling-that-returns-json) shows the alternative.

## An HTML form page

A guestbook: a page, a form that posts to `sign`, and a redirect back. It decodes `application/x-www-form-urlencoded` by hand and escapes everything it prints.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS guests(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    message TEXT NOT NULL
)";

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn percent_decode(s: &str) -> String {
    let b = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = if b[i] == b'%' && i + 2 < b.len() {
            std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        } else {
            None
        };
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn page() -> String {
    let rows: String = spared::with_db(|c| {
        let mut s = c.prepare("SELECT name, message FROM guests ORDER BY id DESC LIMIT 50").unwrap();
        s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .map(|(n, m)| format!("<li><b>{}</b>: {}</li>", escape(&n), escape(&m)))
            .collect()
    });
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Guestbook</title>\
         <h1>Guestbook</h1>\
         <form method=\"post\" action=\"sign\">\
         <p><input name=\"name\" placeholder=\"Name\" required>\
         <p><textarea name=\"message\" placeholder=\"Message\" required></textarea>\
         <p><button>Sign</button></form><ul>{rows}</ul>"
    )
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => spared::html_response(page()),
        (Method::POST, "/sign") => {
            let body = req.body_mut().str_contents().await?.to_string();
            let (mut name, mut message) = (String::new(), String::new());
            for (k, v) in pairs(&body) {
                match k.as_str() {
                    "name" => name = v,
                    "message" => message = v,
                    _ => {}
                }
            }
            if name.trim().is_empty() || message.trim().is_empty() {
                return Ok(spared::text_response(400, "name and message are required\n"));
            }
            spared::with_db(|c| {
                c.execute("INSERT INTO guests(name, message) VALUES (?1, ?2)", [&name, &message]).unwrap();
            });
            Response::builder().status(303).header("location", "./").body(Body::empty()).unwrap()
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Open `http://my-app.localhost:8080/` in a browser, or:

```sh
curl -i -X POST -d 'name=Ada&message=Hello+%3Cb%3Eworld%3C%2Fb%3E' http://my-app.localhost:8080/sign
curl http://my-app.localhost:8080/
```

The first returns `303 See Other` with `location: ./`. The page shows the message with the tags escaped. The form `action="sign"` and the redirect target `./` are relative, so the page also works at `/_/my-app/` when path routing is on. Always escape text you put into HTML: anyone who can reach the app can post anything.

## Query parameters and path routing without a framework

No database here. The handler splits the path into segments and matches on the slice, which gives clean routes with captures. A small helper parses the query string.

`spared.toml`: defaults. The app never opens `/data`, so `SCHEMA` is empty.

```rust
#[allow(dead_code)]
mod spared;

use serde_json::json;
use std::collections::HashMap;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "";

fn percent_decode(s: &str) -> String {
    let b = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = if b[i] == b'%' && i + 2 < b.len() {
            std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        } else {
            None
        };
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(req: &Request<Body>) -> HashMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let q = query_params(&req);
    let segments: Vec<&str> = req.uri().path().trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let resp = match (req.method(), segments.as_slice()) {
        (&Method::GET, []) => spared::text_response(200, "try /hello/ada, /add?a=2&b=3 or /users/7/posts/42\n"),
        (&Method::GET, ["hello", name]) => {
            let greeting = q.get("greeting").map(String::as_str).unwrap_or("Hello");
            spared::json_response(200, &json!({"message": format!("{greeting}, {}!", percent_decode(name))}))
        }
        (&Method::GET, ["add"]) => {
            let num = |k: &str| q.get(k).and_then(|v| v.parse::<i64>().ok());
            match (num("a"), num("b")) {
                (Some(a), Some(b)) => spared::json_response(200, &json!({"sum": a + b})),
                _ => spared::json_response(400, &json!({"error": "a and b must be integers"})),
            }
        }
        (&Method::GET, ["users", user, "posts", post]) => {
            spared::json_response(200, &json!({"user": user, "post": post}))
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl 'http://my-app.localhost:8080/hello/ada?greeting=Hi'
curl 'http://my-app.localhost:8080/add?a=2&b=3'
curl 'http://my-app.localhost:8080/add?a=2'
curl http://my-app.localhost:8080/users/7/posts/42
```

Quote URLs that contain `&` so your shell does not split them. A trailing slash is ignored because of `trim_matches('/')`. If you want the opposite, drop that call and match on the raw path.

## Schema changes with `PRAGMA user_version`

`SCHEMA` runs on every open, so it can only hold statements that are safe to repeat. To change a table that already has data, keep an ordered list of migrations and record how many have run in SQLite's `user_version`, a number stored in the database file.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cell::Cell;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS people(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
)";

// Append only. Entry N moves the database from user_version N to N + 1.
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE people ADD COLUMN email TEXT",
    "CREATE INDEX people_name ON people(name)",
];

thread_local! {
    static MIGRATED: Cell<bool> = const { Cell::new(false) };
}

fn migrate() {
    if MIGRATED.with(Cell::get) {
        return;
    }
    spared::with_db(|c| {
        let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = c.unchecked_transaction().unwrap();
            tx.execute_batch(sql).unwrap();
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1)).unwrap();
            tx.commit().unwrap();
        }
    });
    MIGRATED.with(|m| m.set(true));
}

#[derive(Serialize)]
struct Person {
    id: i64,
    name: String,
    email: Option<String>,
}

#[derive(Deserialize)]
struct NewPerson {
    name: String,
    email: Option<String>,
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    migrate();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/version") => {
            let v: i64 = spared::with_db(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap());
            spared::json_response(200, &json!({"user_version": v, "migrations": MIGRATIONS.len()}))
        }
        (Method::GET, "/people") => {
            let people: Vec<Person> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT id, name, email FROM people ORDER BY id").unwrap();
                s.query_map([], |r| Ok(Person { id: r.get(0)?, name: r.get(1)?, email: r.get(2)? }))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            });
            spared::json_response(200, &people)
        }
        (Method::POST, "/people") => {
            let new: NewPerson = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            let id = spared::with_db(|c| {
                c.execute("INSERT INTO people(name, email) VALUES (?1, ?2)", rusqlite::params![new.name, new.email])
                    .unwrap();
                c.last_insert_rowid()
            });
            spared::json_response(201, &json!({"id": id}))
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl http://my-app.localhost:8080/version
curl -X POST -d '{"name":"Ada","email":"ada@example.com"}' http://my-app.localhost:8080/people
curl http://my-app.localhost:8080/people
```

The first call shows `{"migrations":2,"user_version":2}`. To change the schema later, add a line to the end of `MIGRATIONS` and redeploy. A redeploy keeps `data/`, so the new instance finds `user_version` behind the list and runs only the new entries. Checked on a live server: adding a third entry and redeploying moved `user_version` from 2 to 3 and kept the existing rows.

Rules:

- Only append. Never edit or reorder an entry that has shipped.
- Each migration and its version bump run in one transaction, so a failure leaves the database at the old version. A failing migration panics, so the app returns `app_trap` until you fix it and redeploy.
- `ALTER TABLE ... ADD COLUMN` cannot add a column with a non-constant default such as `CURRENT_TIMESTAMP`. Add the column, then fill it with an `UPDATE` in the same migration.
- Take a [backup](getting-started.md#9-back-up-and-restore) before you ship a migration.

## Call an outside API with a fetch grant

Outbound HTTP needs two things: a host in `fetch` under `[grants]`, and the wstd client. This app fetches `https://<host>/` for a host named in the path, which makes it easy to see what is allowed and what is refused.

`spared.toml`:

```toml
name = "my-app"

[grants]
data = true
fetch = ["example.com"]
```

```rust
#[allow(dead_code)]
mod spared;

use serde_json::json;
use wstd::http::{Body, Client, Error, Method, Request, Response};

const SCHEMA: &str = "";

async fn get_text(url: &str) -> Result<(u16, String), Error> {
    let request = Request::get(url).body(Body::empty())?;
    let mut response = Client::new().send(request).await?;
    let status = response.status().as_u16();
    let body = response.body_mut().str_contents().await?.to_string();
    Ok((status, body))
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let path = req.uri().path().to_string();
    let resp = match (req.method(), path.as_str()) {
        (&Method::GET, "/") => spared::text_response(200, "GET /fetch/example.com or /fetch/<any other host>\n"),
        (&Method::GET, p) if p.starts_with("/fetch/") => {
            let host = &p["/fetch/".len()..];
            match get_text(&format!("https://{host}/")).await {
                Ok((status, body)) => {
                    spared::json_response(200, &json!({"upstream_status": status, "bytes": body.len()}))
                }
                Err(e) => spared::json_response(502, &json!({"error": format!("{e}")})),
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Without the `fetch` line, `spared test` and `spared deploy` refuse the component with `ungranted_import`. With it, try:

```sh
curl http://my-app.localhost:8080/fetch/example.com
curl http://my-app.localhost:8080/fetch/unlisted.example.org
curl http://my-app.localhost:8080/fetch/127.0.0.1
```

The first returns something like `{"bytes":577,"upstream_status":200}`. The other two return a 502 with `{"error":"ErrorCode::HttpRequestDenied"}`. `spared test --path /fetch/unlisted.example.org` exits 1, because it treats any 5xx as a failure.

What is refused, and what is not:

- A host that is not in `fetch`. `*.example.com` matches subdomains only, not `example.com` itself.
- Private, loopback, link-local and cloud metadata addresses, even for a host you listed. The host checks the address the name resolves to and connects to that address.
- Raw sockets and DNS lookups. `std::net` builds, but the component is refused at deploy.
- More than `max_outbound` calls in one request (default 10).
- A URL with a user name in front of the host (`user@host`).
- The check is by host, not by port: `fetch = ["example.com"]` also allows `example.com:25`.
- Apps get no secrets or environment variables, so an API key has to live in your code or in the database. Keep that in mind before you commit one.
- If the call fails with `ErrorCode::TlsCertificateError`, your network probably intercepts HTTPS. See the [troubleshooting table](../AGENTS.md#troubleshooting).

The fetch from `spared test` uses your network and your CA bundle. The deployed app uses the server's.

## Read and set headers

Request headers come from `req.headers()`. Response headers are set on the builder. This app echoes request headers, checks a bearer token, and reads the user agent.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use serde_json::{Map, Value, json};
use wstd::http::{Body, Error, Request, Response};

const SCHEMA: &str = "";

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let resp = match req.uri().path() {
        "/headers" => {
            let mut all = Map::new();
            for (name, value) in req.headers() {
                all.insert(name.to_string(), Value::String(String::from_utf8_lossy(value.as_bytes()).into_owned()));
            }
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("cache-control", "no-store")
                .header("x-powered-by", "spared")
                .body(Value::Object(all).to_string().into())
                .unwrap()
        }
        "/secret" => match header("authorization").as_deref() {
            Some("Bearer let-me-in") => spared::json_response(200, &json!({"secret": 42})),
            _ => Response::builder()
                .status(401)
                .header("content-type", "application/json")
                .header("www-authenticate", "Bearer")
                .body(json!({"error": "missing or wrong bearer token"}).to_string().into())
                .unwrap(),
        },
        "/agent" => {
            let agent = header("user-agent").unwrap_or_else(|| "unknown".to_string());
            spared::text_response(200, format!("you are {agent}\n"))
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl -i http://my-app.localhost:8080/headers
curl -i http://my-app.localhost:8080/secret
curl -H 'Authorization: Bearer let-me-in' http://my-app.localhost:8080/secret
curl -A mybot/1.0 http://my-app.localhost:8080/agent
```

`/secret` returns 401 with a `www-authenticate` header until you send the token. The token is in the source here only to keep the recipe short. In a test run, the headers the app saw were `accept` and `user-agent`; the `Host` header was not among them, so route on the path, not on the host name. `spared test` sends no headers at all.

## Redirects

A redirect is a status in the 3xx range and a `location` header. Use relative locations so the redirect also works under path routing: `new` resolves next to the current path, while `/new` would leave `/_/my-app/`.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "";

fn redirect(status: u16, location: &str) -> Response<Body> {
    Response::builder().status(status).header("location", location).body(Body::empty()).unwrap()
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let resp = match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => spared::html_response(
            "<h1>redirects</h1><p><a href=\"old\">old</a> | <a href=\"moved\">moved</a>\
             <form method=\"post\" action=\"submit\"><button>submit</button></form>",
        ),
        (&Method::GET, "/old") => redirect(301, "new"),
        (&Method::GET, "/moved") => redirect(302, "new"),
        (&Method::GET, "/new") => spared::text_response(200, "you made it\n"),
        (&Method::POST, "/submit") => redirect(303, "done"),
        (&Method::GET, "/done") => spared::text_response(200, "submitted\n"),
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl -i http://my-app.localhost:8080/old
curl -L http://my-app.localhost:8080/old
curl -i -X POST http://my-app.localhost:8080/submit
```

Use 301 for a permanent move, 302 for a temporary one, and 303 after a POST so the browser follows with a GET. Do not build a redirect from a user-supplied value without checking it: that makes an open redirect.

## File storage under /data

Besides the SQLite file, an app can keep ordinary files in `/data`. Everything else on the filesystem is invisible. This app stores uploads under `/data/uploads`. They live in the app's `data/` directory, so redeploys keep them and backups include them.

`spared.toml`: `data = true`, which is the default. Without it the app has no filesystem, and a component that uses files is refused with `ungranted_import`.

```rust
#[allow(dead_code)]
mod spared;

use serde_json::json;
use std::fs;
use std::path::PathBuf;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "";
const DIR: &str = "/data/uploads";

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    fs::create_dir_all(DIR).expect("cannot create /data/uploads: is `data = true` set under [grants]?");
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/files") => {
            let mut names: Vec<String> =
                fs::read_dir(DIR).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            spared::json_response(200, &names)
        }
        (method, p) if p.starts_with("/files/") => {
            let name = &p["/files/".len()..];
            if !safe_name(name) {
                return Ok(spared::json_response(400, &json!({"error": "bad file name"})));
            }
            let file = PathBuf::from(DIR).join(name);
            match method {
                Method::PUT => {
                    let bytes = req.body_mut().contents().await?.to_vec();
                    fs::write(&file, &bytes).unwrap();
                    spared::json_response(201, &json!({"name": name, "bytes": bytes.len()}))
                }
                Method::GET => match fs::read(&file) {
                    Ok(bytes) => Response::builder()
                        .status(200)
                        .header("content-type", "application/octet-stream")
                        .body(bytes.into())
                        .unwrap(),
                    Err(_) => spared::text_response(404, "no such file\n"),
                },
                Method::DELETE => match fs::remove_file(&file) {
                    Ok(()) => spared::text_response(200, "deleted\n"),
                    Err(_) => spared::text_response(404, "no such file\n"),
                },
                _ => spared::text_response(405, "method not allowed\n"),
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl -X PUT --data-binary 'hello file' http://my-app.localhost:8080/files/hello.txt
curl http://my-app.localhost:8080/files
curl http://my-app.localhost:8080/files/hello.txt
curl -i -X PUT -d x http://my-app.localhost:8080/files/.hidden
curl -X DELETE http://my-app.localhost:8080/files/hello.txt
```

The name check is the important part: it only accepts letters, digits, dots, dashes and underscores, and refuses a leading dot, so a request cannot name a path like `../x`. The sandbox already confines the app to `/data`, but the check stops one request from overwriting other files in it, such as `app.db`. The server caps each app's data (1 GB by default, `--max-app-data-mb`), so the disk is safe, but anyone who can reach this app can fill that allowance and make its database writes fail. Add a size check before you use this with an app on the internet. Request bodies are capped at 8 MB by default.

## A counter with an atomic update

Do it in one SQL statement. `INSERT ... ON CONFLICT DO UPDATE ... RETURNING` creates the row or increments it and gives back the new value. Do not keep the count in a Rust variable: the next idle unload resets it.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use serde_json::json;
use std::collections::BTreeMap;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS counters(
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
)";

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let path = req.uri().path().to_string();
    let resp = match (req.method(), path.as_str()) {
        (&Method::GET, "/counters") => {
            let all: BTreeMap<String, i64> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT name, value FROM counters").unwrap();
                s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
            });
            spared::json_response(200, &all)
        }
        (&Method::POST, p) if p.starts_with("/hit/") => {
            let name = &p["/hit/".len()..];
            if name.is_empty() || name.len() > 64 {
                return Ok(spared::text_response(400, "counter name must be 1 to 64 characters\n"));
            }
            let value: i64 = spared::with_db(|c| {
                c.query_row(
                    "INSERT INTO counters(name, value) VALUES (?1, 1)
                     ON CONFLICT(name) DO UPDATE SET value = value + 1
                     RETURNING value",
                    [name],
                    |r| r.get(0),
                )
                .unwrap()
            });
            spared::json_response(200, &json!({"name": name, "value": value}))
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl -X POST http://my-app.localhost:8080/hit/home
curl -X POST http://my-app.localhost:8080/hit/home
curl -X POST http://my-app.localhost:8080/hit/about
curl http://my-app.localhost:8080/counters
```

You get `{"name":"home","value":1}`, then `2`, then `{"about":1,"home":2}`. Requests to one app are already serialized, so two requests cannot interleave. A single statement is still the right shape: it stays correct if you add a second statement, and it needs no separate read.

## Simple pagination

Page by key, not by offset. `after` is the last id the client saw, and the query asks for one extra row to learn whether there is more. Offsets get slower as they grow and skip or repeat rows when data changes between requests.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS items(
    id INTEGER PRIMARY KEY,
    label TEXT NOT NULL
)";

#[derive(Serialize)]
struct Item {
    id: i64,
    label: String,
}

fn query(req: &Request<Body>) -> HashMap<&str, &str> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|p| p.split_once('='))
        .collect()
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let q = query(&req);
    let resp = match (req.method(), req.uri().path()) {
        (&Method::POST, "/seed") => {
            spared::with_db(|c| {
                let tx = c.unchecked_transaction().unwrap();
                for i in 1..=100 {
                    tx.execute("INSERT INTO items(label) VALUES (?1)", [format!("item {i}")]).unwrap();
                }
                tx.commit().unwrap();
            });
            spared::json_response(201, &json!({"inserted": 100}))
        }
        (&Method::GET, "/items") => {
            let after: i64 = q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
            let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20).clamp(1, 100);
            let mut items: Vec<Item> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT id, label FROM items WHERE id > ?1 ORDER BY id LIMIT ?2").unwrap();
                s.query_map(rusqlite::params![after, limit as i64 + 1], |r| Ok(Item { id: r.get(0)?, label: r.get(1)? }))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            });
            let has_more = items.len() > limit;
            items.truncate(limit);
            let next_after = if has_more { items.last().map(|i| i.id) } else { None };
            spared::json_response(200, &json!({"items": items, "next_after": next_after}))
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
```

Try it:

```sh
curl -X POST http://my-app.localhost:8080/seed
curl 'http://my-app.localhost:8080/items?limit=3'
curl 'http://my-app.localhost:8080/items?limit=3&after=3'
curl 'http://my-app.localhost:8080/items?limit=3&after=98'
```

Each response has `items` and `next_after`. Pass `next_after` as `after` for the next page; `null` means you reached the end. `limit` is clamped to 1 through 100, which also keeps one response far below the 16 MB cap. The `/seed` route inserts 100 rows in one transaction; remove it from a real app.

## Error handling that returns JSON

If the handler returns `Err`, the client gets no response and you see `no_response`. A panic gives `app_trap`. Neither tells the client much. Instead, convert every failure to a status and a JSON body. One function does the routing and returns `Result`, and `main` turns an error into a response.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use rusqlite::ErrorCode;
use serde::Deserialize;
use serde_json::json;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS users(
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL UNIQUE
)";

struct ApiError {
    status: u16,
    kind: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: u16, kind: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, kind, message: message.into() }
    }

    fn into_response(self) -> Response<Body> {
        spared::json_response(self.status, &json!({"error": {"kind": self.kind, "message": self.message}}))
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        match &e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation => {
                ApiError::new(409, "conflict", "that value already exists")
            }
            _ => {
                eprintln!("database error: {e}");
                ApiError::new(500, "database", "database error")
            }
        }
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError::new(400, "bad_request", format!("{e}"))
    }
}

#[derive(Deserialize)]
struct NewUser {
    email: String,
}

async fn route(req: &mut Request<Body>) -> Result<Response<Body>, ApiError> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    match (method, path.as_str()) {
        (Method::POST, "/users") => {
            let body = req.body_mut().str_contents().await?.to_string();
            let new: NewUser = serde_json::from_str(&body)
                .map_err(|e| ApiError::new(400, "invalid_json", format!("invalid JSON body: {e}")))?;
            if !new.email.contains('@') {
                return Err(ApiError::new(422, "invalid_email", "email must contain @"));
            }
            let id = spared::with_db(|c| -> Result<i64, rusqlite::Error> {
                c.execute("INSERT INTO users(email) VALUES (?1)", [&new.email])?;
                Ok(c.last_insert_rowid())
            })?;
            Ok(spared::json_response(201, &json!({"id": id, "email": new.email})))
        }
        (Method::GET, p) if p.starts_with("/users/") => {
            let id: i64 = p["/users/".len()..]
                .parse()
                .map_err(|_| ApiError::new(400, "bad_id", "user id must be a number"))?;
            let email: Option<String> = spared::with_db(|c| -> Result<Option<String>, rusqlite::Error> {
                match c.query_row("SELECT email FROM users WHERE id = ?1", [id], |r| r.get(0)) {
                    Ok(v) => Ok(Some(v)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e),
                }
            })?;
            match email {
                Some(email) => Ok(spared::json_response(200, &json!({"id": id, "email": email}))),
                None => Err(ApiError::new(404, "not_found", "no such user")),
            }
        }
        _ => Err(ApiError::new(404, "not_found", "no such route")),
    }
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    Ok(match route(&mut req).await {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    })
}
```

Try it:

```sh
curl -X POST -d '{"email":"a@example.org"}' http://my-app.localhost:8080/users
curl -i -X POST -d '{"email":"a@example.org"}' http://my-app.localhost:8080/users
curl -X POST -d '{"email":"nope"}' http://my-app.localhost:8080/users
curl -X POST -d '{bad' http://my-app.localhost:8080/users
curl http://my-app.localhost:8080/users/abc
curl http://my-app.localhost:8080/users/99
```

Responses are `201`, then `409` `conflict` (the `UNIQUE` constraint), `422` `invalid_email`, `400` `invalid_json`, `400` `bad_id` and `404` `not_found`, each as `{"error":{"kind":"...","message":"..."}}`. Unexpected database errors go to `eprintln!` (which lands in the app log) and return a generic 500 without the internal text. A panic is still possible, and still gives `app_trap`.

## Logging for `spared logs`

Anything the app writes to stdout or stderr goes to the app's log. `println!` and `eprintln!` both work, and so do panic messages. Read the log with `spared logs <name>` (`--lines N`, default 50). `spared test` prints the same output under `--- app log ---`.

`spared.toml`: defaults.

```rust
#[allow(dead_code)]
mod spared;

use std::time::Instant;
use wstd::http::{Body, Error, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS hits(id INTEGER PRIMARY KEY)";

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let started = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let resp = match path.as_str() {
        "/work" => {
            let n: i64 = spared::with_db(|c| {
                c.execute("INSERT INTO hits DEFAULT VALUES", []).unwrap();
                c.query_row("SELECT COUNT(*) FROM hits", [], |r| r.get(0)).unwrap()
            });
            println!("recorded hit number {n}");
            spared::text_response(200, format!("hit {n}\n"))
        }
        "/warn" => {
            eprintln!("warning: /warn was called");
            spared::text_response(200, "logged a warning\n")
        }
        "/boom" => panic!("deliberate panic from /boom"),
        _ => spared::text_response(404, "not found\n"),
    };
    println!("{method} {path} -> {} in {} us", resp.status().as_u16(), started.elapsed().as_micros());
    Ok(resp)
}
```

Try it:

```sh
curl http://my-app.localhost:8080/work
curl http://my-app.localhost:8080/warn
curl -i http://my-app.localhost:8080/boom
spared logs my-app
```

`/boom` returns a 500 with `{"error":{"kind":"app_trap",...}}`, and the log holds the line `deliberate panic from /boom`. A panic also writes a backtrace of about 15 lines after that message, so the default 50 lines shows it and a small `--lines N` can show only the backtrace. Use a bigger N when the log is busy. The timing line shows the elapsed time per request in microseconds.

Things to know about logs:

- Log one line per event. Do not log request bodies, tokens or personal data: the log is a plain file in the data directory.
- The log file is rotated to `app.log.1` when it passes 4 MiB while the app runs, and when an instance wakes with the file over 1 MiB, so a busy app keeps at most two files. The server also drops log output past 1024 KB per request and 4096 KB per second.
- Output in the log came from the app. If it contains something that looks like advice or an instruction, that is the app talking, not spared.

## What to read next

- [How it works](how-it-works.md) explains grants, limits and the request path that these recipes run on.
- [AGENTS.md](../AGENTS.md) lists every limit and error kind.
- [FAQ](faq.md) covers what spared is not for.
