// counter: one number in SQLite, an HTML page with a button, and a JSON API.
//
// `mod spared` is the generated helper file next to this one: with_db, json_response,
// html_response, text_response, read_json. It is yours to edit.
#[allow(dead_code)]
mod spared;

use serde_json::json;
use wstd::http::{Body, Error, Method, Request, Response};

// SCHEMA runs every time the app opens its database, so it must be safe to repeat:
// CREATE TABLE IF NOT EXISTS, INSERT OR IGNORE. That is how a fresh database gets its tables, and
// how a redeploy keeps the data it already has. The CHECK pins the table to a single row.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS counter(id INTEGER PRIMARY KEY CHECK (id = 1), n INTEGER NOT NULL);
INSERT OR IGNORE INTO counter(id, n) VALUES (1, 0);
";

// Links and fetch() calls use relative URLs ("api/count", not "/api/count") so the page also works
// under the path form /_/<app>/ when a server turns that on.
const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__NAME__</title>
<style>
  body { font: 16px system-ui, sans-serif; max-width: 24rem; margin: 3rem auto; text-align: center; }
  #n { font-size: 4rem; margin: 1rem 0; }
  button { font-size: 1.2rem; padding: .5rem 1.5rem; }
</style>
<h1>__NAME__</h1>
<div id="n">...</div>
<button id="b">+1</button>
<script>
const show = (c) => { document.getElementById('n').textContent = c.count; };
fetch('api/count').then(r => r.json()).then(show);
document.getElementById('b').onclick = async () => {
  show(await (await fetch('api/increment', { method: 'POST' })).json());
};
</script>
"#;

fn count() -> i64 {
    spared::with_db(|c| c.query_row("SELECT n FROM counter WHERE id = 1", [], |r| r.get(0)).unwrap())
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => spared::html_response(PAGE),
        (Method::GET, "/api/count") => spared::json_response(200, &json!({"count": count()})),
        // One UPDATE ... RETURNING is atomic: two visitors can never both read 7 and both write 8.
        (Method::POST, "/api/increment") => {
            let n: i64 = spared::with_db(|c| {
                c.query_row("UPDATE counter SET n = n + 1 WHERE id = 1 RETURNING n", [], |r| r.get(0)).unwrap()
            });
            spared::json_response(200, &json!({"count": n}))
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
