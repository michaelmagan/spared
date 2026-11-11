// guestbook: visitors leave messages; the server renders them as HTML, newest first.
//
// Anything a visitor typed is untrusted. Putting it into HTML without escaping lets one visitor run
// script in every other visitor's browser, so every value goes through `esc` first.
#[allow(dead_code)]
mod spared;

use serde::Deserialize;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS messages(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
)";

#[derive(Deserialize)]
struct NewMessage {
    name: String,
    message: String,
}

// Keep rows small: the request body limit is 8 MB, but a guestbook line is not a file upload.
const MAX_NAME: usize = 80;
const MAX_MESSAGE: usize = 2000;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn page() -> String {
    let rows: Vec<(String, String, String)> = spared::with_db(|c| {
        // id DESC is newest first, and breaks ties between messages in the same second.
        let mut s = c.prepare("SELECT name, message, created_at FROM messages ORDER BY id DESC LIMIT 200").unwrap();
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
    });
    let items: String = rows
        .iter()
        .map(|(name, msg, at)| format!("<li><b>{}</b> <small>{}</small><br>{}</li>", esc(name), esc(at), esc(msg)))
        .collect();
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__NAME__</title>
<style>
  body {{ font: 16px system-ui, sans-serif; max-width: 32rem; margin: 2rem auto; padding: 0 1rem; }}
  form {{ display: grid; gap: .5rem; }}
  li {{ margin: 1rem 0; list-style: none; }}
</style>
<h1>__NAME__</h1>
<form id="f">
  <input id="name" placeholder="Your name" maxlength="{MAX_NAME}" required>
  <textarea id="msg" placeholder="Say hello" maxlength="{MAX_MESSAGE}" required></textarea>
  <button>Sign</button>
</form>
<ul>{items}</ul>
<script>
document.getElementById('f').onsubmit = async (e) => {{
  e.preventDefault();
  await fetch('api/messages', {{ method: 'POST', body: JSON.stringify({{
    name: document.getElementById('name').value, message: document.getElementById('msg').value }}) }});
  location.reload();
}};
</script>
"#
    )
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => spared::html_response(page()),
        (Method::POST, "/api/messages") => {
            let new: NewMessage = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            let (name, message) = (new.name.trim(), new.message.trim());
            if name.is_empty() || message.is_empty() || name.chars().count() > MAX_NAME || message.chars().count() > MAX_MESSAGE {
                return Ok(spared::text_response(400, "name and message are required and must be short\n"));
            }
            spared::with_db(|c| c.execute("INSERT INTO messages(name, message) VALUES (?1, ?2)", [name, message]).unwrap());
            spared::json_response(201, &serde_json::json!({"ok": true}))
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
