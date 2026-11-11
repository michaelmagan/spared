// blog: posts in SQLite, list and single-post pages rendered as HTML, and a protected endpoint to
// publish. Reading is public; writing needs `Authorization: Bearer <TOKEN>`.
//
//   curl -X POST -H 'authorization: Bearer <your TOKEN>' \
//        -d '{"title":"Hello world","body":"First post.\n\nSecond paragraph."}' http://<app>.localhost:8080/api/posts
#[allow(dead_code)]
mod spared;

use serde::Deserialize;
use serde_json::json;
use wstd::http::{Body, Error, Method, Request, Response};

// CHANGE THIS before you publish anything. An app gets no environment variables or secrets, so the
// token lives in the source. Do not publish the source with the real value. While it still holds the
// shipped value, POST /api/posts answers 503 and publishes nothing.
const TOKEN: &str = "change-me-before-you-deploy";
const SHIPPED_TOKEN: &str = "change-me-before-you-deploy";

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS posts(
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
)";

#[derive(Deserialize)]
struct NewPost {
    title: String,
    body: String,
    slug: Option<String>,
}

// Post text is untrusted HTML until escaped; every value goes through `esc` before it is inserted into a page.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(60).collect()
}

fn layout(title: &str, inner: &str) -> Response<Body> {
    spared::html_response(format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>body{{font:18px/1.6 Georgia,serif;max-width:38rem;margin:2rem auto;padding:0 1rem}}small{{color:#777}}</style>{inner}",
        esc(title)
    ))
}

fn bearer_ok(req: &Request<Body>) -> bool {
    req.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some(format!("Bearer {TOKEN}").as_str())
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => {
            let rows: Vec<(String, String, String)> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT slug, title, created_at FROM posts ORDER BY id DESC LIMIT 100").unwrap();
                s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
            });
            // Relative links ("posts/x", not "/posts/x") keep working under the /_/<app>/ path form.
            let items: String = rows
                .iter()
                .map(|(slug, title, at)| format!("<li><a href=\"posts/{}\">{}</a> <small>{}</small></li>", esc(slug), esc(title), esc(at)))
                .collect();
            layout("__NAME__", &format!("<h1>__NAME__</h1><ul>{items}</ul>"))
        }
        (Method::GET, p) if p.starts_with("/posts/") => {
            let slug = &p["/posts/".len()..];
            let post: Option<(String, String, String)> = spared::with_db(|c| {
                c.query_row("SELECT title, body, created_at FROM posts WHERE slug = ?1", [slug], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok()
            });
            match post {
                Some((title, body, at)) => {
                    let paras: String = body.split("\n\n").map(|p| format!("<p>{}</p>", esc(p.trim()))).collect();
                    layout(&title, &format!("<p><a href=\"../\">all posts</a></p><h1>{}</h1><small>{}</small>{paras}", esc(&title), esc(&at)))
                }
                None => spared::text_response(404, "no such post\n"),
            }
        }
        (Method::POST, "/api/posts") => {
            if TOKEN == SHIPPED_TOKEN {
                return Ok(spared::json_response(503, &json!({"error": "publishing is off until you change TOKEN in src/main.rs and redeploy"})));
            }
            if !bearer_ok(&req) {
                return Ok(spared::json_response(401, &json!({"error": "send `authorization: Bearer <TOKEN>`"})));
            }
            let new: NewPost = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            let slug = slugify(new.slug.as_deref().unwrap_or(&new.title));
            if slug.is_empty() || new.title.trim().is_empty() || new.body.trim().is_empty() {
                return Ok(spared::json_response(400, &json!({"error": "title, body and a slug of letters or digits are required"})));
            }
            let inserted = spared::with_db(|c| c.execute("INSERT INTO posts(slug, title, body) VALUES (?1, ?2, ?3)", [slug.as_str(), new.title.trim(), new.body.as_str()]));
            match inserted {
                Ok(_) => spared::json_response(201, &json!({"slug": slug, "path": format!("/posts/{slug}")})),
                Err(_) => spared::json_response(409, &json!({"error": "a post with that slug exists"})),
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
