// fetcher3: the fetcher template on the EXPERIMENTAL wasip3 runtime.
//
// One instance of this app serves several requests at once: while one request waits for the
// host below, the instance runs others. That needs `runtime = "wasip3-experimental"` in
// spared.toml and a server started with `spared serve --experimental-wasip3`. Read
// docs/wasip3.md before you use it: one crash or timeout fails every request in flight in the
// instance, computing in one request holds up the others, and the app gets no files (data = false).
//
// The handler is `wasip3` 0.10 built for wasm32-wasip2, exported with
// `wasip3::http::service::export!`. Outbound HTTP is a grant, as in the fetcher template: the
// host must be listed under [grants] fetch in spared.toml, and private addresses are refused.

use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::{wit_bindgen, wit_future, wit_stream};

// Change this host, and the `fetch` line in spared.toml, together. Hosts only, no scheme or path.
const HOST: &str = "example.com";

wasip3::http::service::export!(App);

struct App;

/// A response with `status`, a JSON content type and `body`.
fn json(status: u16, body: String) -> Response {
    let headers = Fields::new();
    headers
        .append("content-type", b"application/json")
        .expect("valid header");
    let (mut tx, rx) = wit_stream::new();
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (resp, _done) = Response::new(headers, Some(rx), trailers_rx);
    drop(trailers_tx);
    resp.set_status_code(status).expect("valid status");
    // The body is written after the response is returned, while the host reads it.
    wit_bindgen::spawn_local(async move {
        tx.write_all(body.into_bytes()).await;
    });
    resp
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// GET https://HOST/: its status and how many body bytes came back.
async fn fetch() -> Result<(u16, usize), ErrorCode> {
    let (_tx, trailers) = wit_future::new(|| Ok(None));
    let (req, _sent) = Request::new(Fields::new(), None, trailers, None);
    req.set_method(&Method::Get).expect("valid method");
    req.set_scheme(Some(&Scheme::Https)).expect("valid scheme");
    req.set_authority(Some(HOST)).expect("valid host");
    req.set_path_with_query(Some("/")).expect("valid path");
    let resp = wasip3::http::client::send(req).await?;
    let status = resp.get_status_code();
    let (_done_tx, done) = wit_future::new(|| Ok(()));
    let (body, _trailers) = Response::consume_body(resp, done);
    // Reads the whole body into memory: fine for a page, not for a large download.
    Ok((status, body.collect().await.len()))
}

impl wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let url = format!("https://{HOST}/");
        Ok(match path.as_str() {
            // `/` makes no outbound call, so `spared test` passes even with no network.
            "/" => json(
                200,
                format!(
                    "{{\"about\": \"GET /fetch calls the host below and returns its status and size\", \"host\": {}, \"runtime\": \"wasip3-experimental\"}}",
                    quoted(HOST)
                ),
            ),
            "/fetch" => match fetch().await {
                Ok((status, bytes)) => json(
                    200,
                    format!(
                        "{{\"url\": {}, \"status\": {status}, \"bytes\": {bytes}}}",
                        quoted(&url)
                    ),
                ),
                // A refusal (host not in `fetch`, or a private address) and a network failure both land here.
                Err(e) => json(
                    502,
                    format!(
                        "{{\"url\": {}, \"error\": {}}}",
                        quoted(&url),
                        quoted(&format!("{e:?}"))
                    ),
                ),
            },
            _ => json(404, "{\"error\": \"not found\"}".into()),
        })
    }
}
