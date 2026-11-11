// webhook: receive POSTed JSON of any shape, store it with a timestamp, list the last 50.
//
//   curl -X POST -H 'x-webhook-secret: change-me-before-you-deploy' -d '{"any":"json"}' http://<app>.localhost:8080/hook
//   curl -H 'x-webhook-secret: change-me-before-you-deploy' http://<app>.localhost:8080/events
#[allow(dead_code)]
mod spared;

use serde_json::{Value, json};
use wstd::http::{Body, Error, Method, Request, Response};

// CHANGE THIS before you deploy anywhere other than your own machine. An app gets no environment
// variables or secrets, so a shared secret lives in the source (or in a table you fill yourself).
// Whoever reads this file can send events, so do not publish the source with the real value in it.
const SECRET: &str = "change-me-before-you-deploy";

// The payload is stored as the text it arrived as, so nothing the sender wrote is reshaped.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events(
    id INTEGER PRIMARY KEY,
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    payload TEXT NOT NULL
)";

const MAX_PAYLOAD: usize = 256 * 1024;

// Compare without stopping at the first differing byte, so response time does not leak the secret.
fn authorized(req: &Request<Body>) -> bool {
    let given = req.headers().get("x-webhook-secret").map(|v| v.as_bytes()).unwrap_or(b"");
    let want = SECRET.as_bytes();
    let mut diff = (given.len() ^ want.len()) as u8;
    for (i, b) in want.iter().enumerate() {
        diff |= b ^ given.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn unauthorized() -> Response<Body> {
    spared::json_response(401, &json!({"error": "missing or wrong x-webhook-secret header"}))
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => spared::text_response(200, "webhook receiver: POST /hook, GET /events (both need the x-webhook-secret header)\n"),
        (Method::POST, "/hook") => {
            if !authorized(&req) {
                return Ok(unauthorized());
            }
            // Any JSON value is accepted (object, array, number, string); anything else is a 400.
            let payload: Value = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            let text = payload.to_string();
            if text.len() > MAX_PAYLOAD {
                return Ok(spared::json_response(413, &json!({"error": "payload is over 256 KiB"})));
            }
            let id = spared::with_db(|c| {
                c.execute("INSERT INTO events(payload) VALUES (?1)", [&text]).unwrap();
                c.last_insert_rowid()
            });
            spared::json_response(201, &json!({"id": id}))
        }
        (Method::GET, "/events") => {
            if !authorized(&req) {
                return Ok(unauthorized());
            }
            let events: Vec<Value> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT id, received_at, payload FROM events ORDER BY id DESC LIMIT 50").unwrap();
                s.query_map([], |r| {
                    let payload: String = r.get(2)?;
                    Ok(json!({"id": r.get::<_, i64>(0)?, "received_at": r.get::<_, String>(1)?,
                              "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null)}))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
            });
            spared::json_response(200, &events)
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
