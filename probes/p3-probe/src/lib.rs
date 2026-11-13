//! A wasip3 guest for the tests of `runtime = "wasip3-experimental"`. Each route does one thing the
//! host must handle while other requests are in flight on the same instance.

use std::cell::Cell;
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::{wit_bindgen, wit_future, wit_stream};

wasip3::http::service::export!(Probe);

struct Probe;

thread_local! {
    static IN_FLIGHT: Cell<u32> = const { Cell::new(0) };
    static PEAK: Cell<u32> = const { Cell::new(0) };
}

struct Entered;

impl Entered {
    fn new() -> Entered {
        IN_FLIGHT.with(|c| {
            let n = c.get() + 1;
            c.set(n);
            PEAK.with(|p| p.set(p.get().max(n)));
        });
        Entered
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        IN_FLIGHT.with(|c| c.set(c.get() - 1));
    }
}

fn text(status: u16, body: String) -> Response {
    let (mut tx, rx) = wit_stream::new();
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (resp, _done) = Response::new(Fields::new(), Some(rx), trailers_rx);
    drop(trailers_tx);
    resp.set_status_code(status).unwrap();
    wit_bindgen::spawn_local(async move {
        let rest = tx.write_all(body.into_bytes()).await;
        assert!(rest.is_empty());
    });
    resp
}

async fn fetch(authority: &str) -> Result<u16, ErrorCode> {
    let (_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (req, _sent) = Request::new(Fields::new(), None, trailers_rx, None);
    req.set_method(&Method::Get).unwrap();
    req.set_scheme(Some(&Scheme::Http)).unwrap();
    req.set_authority(Some(authority)).unwrap();
    req.set_path_with_query(Some("/")).unwrap();
    let resp = wasip3::http::client::send(req).await?;
    Ok(resp.get_status_code())
}

fn query_u64(path: &str, key: &str, default: u64) -> u64 {
    path.split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl wasip3::exports::http::handler::Guest for Probe {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let _entered = Entered::new();
        let body = if path.starts_with("/sleep") {
            let ms = query_u64(&path, "ms", 200);
            wasip3::clocks::monotonic_clock::wait_for(ms * 1_000_000).await;
            format!("slept {ms}")
        } else if let Some(rest) = path.strip_prefix("/fetch/") {
            let times = query_u64(rest, "times", 1);
            let authority = rest.split('?').next().unwrap_or_default();
            let mut out = vec![];
            for _ in 0..times {
                out.push(match fetch(authority).await {
                    Ok(s) => format!("upstream {s}"),
                    Err(e) => format!("error {e:?}"),
                });
            }
            out.join(", ")
        } else if path.starts_with("/spin") {
            let mut x: u64 = 1;
            let mut i: u64 = 0;
            loop {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
                i += 1;
                if x == 7 {
                    break format!("spun {i}");
                }
            }
        } else if path.starts_with("/trap") {
            wasip3::clocks::monotonic_clock::wait_for(50_000_000).await;
            panic!("p3-probe trapped on purpose");
        } else if path.starts_with("/log") {
            let n = query_u64(&path, "bytes", 0) as usize;
            println!("{}", "x".repeat(n));
            format!("logged {n}")
        } else if path.starts_with("/stats") {
            format!("peak {}", PEAK.with(|p| p.get()))
        } else {
            "hello from wasip3".to_string()
        };
        Ok(text(200, format!("{body}\n")))
    }
}
