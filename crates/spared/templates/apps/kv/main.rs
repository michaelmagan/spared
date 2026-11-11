// kv: a tiny key-value store over HTTP. PUT a value, GET it back, DELETE it, list every key.
//
//   curl -X PUT -d 'hello' http://<app>.localhost:8080/kv/greeting
//   curl http://<app>.localhost:8080/kv/greeting
//   curl http://<app>.localhost:8080/kv
#[allow(dead_code)]
mod spared;

use serde::Serialize;
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY, value TEXT NOT NULL)";

const MAX_VALUE: usize = 64 * 1024;

#[derive(Serialize)]
struct Entry {
    key: String,
    value: String,
}

// Keys come from the URL path, so keep them to a safe alphabet instead of decoding %XX escapes.
fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 64 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method.clone(), path.as_str()) {
        (Method::GET, "/") => spared::text_response(200, "kv store: PUT/GET/DELETE /kv/<key>, GET /kv lists all keys\n"),
        (Method::GET, "/kv") => {
            let all: Vec<Entry> = spared::with_db(|c| {
                // A TEXT primary key sorts bytewise, so the listing order is stable and predictable.
                let mut s = c.prepare("SELECT key, value FROM kv ORDER BY key").unwrap();
                s.query_map([], |r| Ok(Entry { key: r.get(0)?, value: r.get(1)? })).unwrap().map(Result::unwrap).collect()
            });
            spared::json_response(200, &all)
        }
        (_, p) if p.starts_with("/kv/") => {
            let key = &p["/kv/".len()..];
            if !valid_key(key) {
                return Ok(spared::text_response(400, "key must be 1 to 64 characters of a-z A-Z 0-9 . _ -\n"));
            }
            match method {
                Method::PUT => {
                    let value = match req.body_mut().str_contents().await {
                        Ok(v) => v.to_string(),
                        Err(_) => return Ok(spared::text_response(400, "value must be UTF-8 text\n")),
                    };
                    if value.len() > MAX_VALUE {
                        return Ok(spared::text_response(413, "value is over 64 KiB\n"));
                    }
                    // INSERT ... ON CONFLICT is an upsert: create the key or overwrite it in one statement.
                    spared::with_db(|c| {
                        c.execute("INSERT INTO kv(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2", [key, value.as_str()])
                            .unwrap()
                    });
                    spared::text_response(200, "stored\n")
                }
                Method::GET => {
                    let found: Option<String> =
                        spared::with_db(|c| c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).ok());
                    match found {
                        Some(v) => spared::text_response(200, v),
                        None => spared::text_response(404, "no such key\n"),
                    }
                }
                Method::DELETE => {
                    let n = spared::with_db(|c| c.execute("DELETE FROM kv WHERE key = ?1", [key]).unwrap());
                    if n == 0 { spared::text_response(404, "no such key\n") } else { spared::text_response(200, "deleted\n") }
                }
                _ => spared::text_response(405, "use PUT, GET or DELETE\n"),
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
