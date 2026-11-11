// shortener: POST a long URL, get a short code; GET /<code> answers 302 and counts the hit.
#[allow(dead_code)]
mod spared;

use serde::{Deserialize, Serialize};
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS links(
    code TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    hits INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
)";

#[derive(Deserialize)]
struct NewLink {
    url: String,
}

#[derive(Serialize)]
struct Link {
    code: String,
    url: String,
    hits: i64,
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__NAME__</title>
<style>body { font: 16px system-ui, sans-serif; max-width: 32rem; margin: 2rem auto; padding: 0 1rem; }
form { display: flex; gap: .5rem; } input { flex: 1; padding: .5rem; }</style>
<h1>__NAME__</h1>
<form id="f"><input id="u" type="url" placeholder="https://example.com/a/long/url" required><button>Shorten</button></form>
<p id="out"></p>
<script>
document.getElementById('f').onsubmit = async (e) => {
  e.preventDefault();
  const r = await fetch('api/links', { method: 'POST', body: JSON.stringify({ url: document.getElementById('u').value }) });
  const j = await r.json();
  document.getElementById('out').textContent = r.ok ? new URL(j.code, location.href).href : j.error;
};
</script>
"#;

// Only web links: redirecting to javascript: or data: URLs would be an open door for abuse.
fn valid_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://")) && u.len() <= 2048 && !u.contains(char::is_whitespace)
}

fn find(code: &str) -> Option<Link> {
    spared::with_db(|c| {
        c.query_row("SELECT code, url, hits FROM links WHERE code = ?1", [code], |r| {
            Ok(Link { code: r.get(0)?, url: r.get(1)?, hits: r.get(2)? })
        })
        .ok()
    })
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => spared::html_response(PAGE),
        (Method::POST, "/api/links") => {
            let new: NewLink = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            if !valid_url(&new.url) {
                return Ok(spared::json_response(400, &serde_json::json!({"error": "url must be http:// or https:// with no spaces"})));
            }
            // The app has no random number source of its own; SQLite's randomblob gives 6 hex characters.
            let code: String = spared::with_db(|c| {
                c.query_row("INSERT INTO links(code, url) VALUES (lower(hex(randomblob(3))), ?1) RETURNING code", [&new.url], |r| r.get(0))
                    .unwrap()
            });
            spared::json_response(201, &find(&code).unwrap())
        }
        (Method::GET, "/api/links") => {
            let links: Vec<Link> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT code, url, hits FROM links ORDER BY hits DESC, code").unwrap();
                s.query_map([], |r| Ok(Link { code: r.get(0)?, url: r.get(1)?, hits: r.get(2)? })).unwrap().map(Result::unwrap).collect()
            });
            spared::json_response(200, &links)
        }
        (Method::GET, p) if p.starts_with("/api/links/") => match find(&p["/api/links/".len()..]) {
            Some(l) => spared::json_response(200, &l),
            None => spared::text_response(404, "no such code\n"),
        },
        // Any other single path segment is a short code. The count goes up before the redirect is sent.
        (Method::GET, p) if p.len() > 1 && !p[1..].contains('/') => {
            let code = &p[1..];
            let url: Option<String> = spared::with_db(|c| {
                c.query_row("UPDATE links SET hits = hits + 1 WHERE code = ?1 RETURNING url", [code], |r| r.get(0)).ok()
            });
            match url {
                Some(u) => Response::builder().status(302).header("location", u).body(Body::empty()).expect("valid response"),
                None => spared::text_response(404, "no such code\n"),
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
