// fetcher: the app calls out to another website and reports what came back.
//
// Outbound HTTP is a grant. The host below must also be listed in spared.toml under
// [grants] fetch = [...]; a request to any host that is not listed is refused. Even a listed host
// is refused when its name resolves to a private, loopback, link-local or metadata address
// (10.x, 192.168.x, 127.0.0.1, 169.254.x), so an app cannot be used to probe the machine or
// network it runs on. `spared test` runs on your machine and uses your network and trust store;
// a deployed app uses the server's.
#[allow(dead_code)]
mod spared;

use serde_json::json;
use wstd::http::{Body, Client, Error, Method, Request, Response};

// Change this host, and the `fetch` line in spared.toml, together. Hosts only, no scheme or path.
const HOST: &str = "example.com";

// This app stores nothing, but `src/spared.rs` needs a SCHEMA to open its database.
const SCHEMA: &str = "SELECT 1";

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        // `/` makes no outbound call, so `spared test` passes even with no network.
        (Method::GET, "/") => spared::json_response(
            200,
            &json!({"about": "GET /fetch calls the host below and returns its status and size", "host": HOST}),
        ),
        (Method::GET, "/fetch") => {
            let url = format!("https://{HOST}/");
            let request = Request::get(url.as_str()).body(Body::empty()).expect("valid request");
            match Client::new().send(request).await {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    // Reads the whole body into memory: fine for a page, not for a large download.
                    let size = response.body_mut().contents().await.map(|b| b.len()).unwrap_or(0);
                    spared::json_response(200, &json!({"url": url, "status": status, "bytes": size}))
                }
                // A refusal (host not in `fetch`, or a private address) and a network failure both land here.
                Err(e) => spared::json_response(502, &json!({"url": url, "error": e.to_string()})),
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
