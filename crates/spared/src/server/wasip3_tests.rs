//! EXPERIMENTAL `runtime = "wasip3-experimental"` through a real `Server` and the wasip3 probe in
//! `probes/p3-probe`: requests that interleave in one instance, the in-flight cap, per-request
//! budgets, a trap, a deadline, and draining on redeploy. Requests enter at `run_request`, as
//! `serve_app` hands them over once their body is read.

use super::lifecycle_tests::Fixture;
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[path = "../../tests/support/p3_probe.rs"]
mod p3_probe;

/// The probe imports `wasi:http/client`, so it needs a `fetch` grant; `extra` may bring its own.
fn p3_manifest(name: &str, extra: &str) -> String {
    let fetch = if extra.starts_with("fetch") {
        ""
    } else {
        "fetch = [\"example.com\"]\n"
    };
    format!(
        "name = \"{name}\"\nruntime = \"wasip3-experimental\"\n[grants]\ndata = false\n{fetch}{extra}"
    )
}

async fn deploy(fx: &Fixture, name: &str, extra: &str) -> serde_json::Value {
    match fx
        .server
        .install(
            name,
            &p3_manifest(name, extra),
            &p3_probe::p3_probe(),
            false,
            &Creator::Admin,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => panic!("deploy {name}: {} {}: {}", e.status, e.kind, e.message),
    }
}

/// One GET to `name`, the way `serve_app` runs it: status, error kind and body.
async fn get(server: &Arc<Server>, name: &str, path: &str) -> (u16, String, String) {
    let app = server.apps.read().unwrap().get(name).cloned().unwrap();
    let (app, guard) = server.lock_live(app).await.unwrap();
    let req = http::Request::get(format!("http://{name}.localhost{path}"))
        .body(Bytes::new())
        .unwrap();
    let r = server.run_request(app, guard, req, false).await;
    let status = r.status().as_u16();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();
    let kind = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["error"]["kind"].as_str().map(String::from))
        .unwrap_or_default();
    (status, kind, text)
}

/// `n` GETs to `path` at once: their results in order, and the time until the last finished.
async fn many(
    server: &Arc<Server>,
    name: &str,
    paths: Vec<String>,
) -> (Vec<(u16, String, String)>, Duration) {
    let started = Instant::now();
    let tasks: Vec<_> = paths
        .into_iter()
        .map(|p| {
            let (s, n) = (server.clone(), name.to_string());
            tokio::spawn(async move { get(&s, &n, &p).await })
        })
        .collect();
    let mut out = vec![];
    for t in tasks {
        out.push(t.await.unwrap());
    }
    (out, started.elapsed())
}

/// A loopback HTTP server that answers every request with 200 after `delay`.
async fn slow_upstream(delay: Duration) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                tokio::time::sleep(delay).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    port
}

fn live_instance(fx: &Fixture, name: &str) -> Option<Arc<crate::p3::Instance>> {
    let app = fx.app(name)?;
    let g = app.live.try_lock().ok()?;
    match g.as_ref() {
        Some(Live::P3(i)) => Some(i.clone()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn eight_requests_awaiting_a_200_ms_upstream_overlap_in_one_instance() {
    let fx = Fixture::with_loopback();
    let port = slow_upstream(Duration::from_millis(200)).await;
    deploy(
        &fx,
        "fetch3",
        "fetch = [\"localhost\"]\n[limits]\nconcurrency = 8\n",
    )
    .await;
    // Wake the instance first, so the timing below is the requests alone.
    assert_eq!(get(&fx.server, "fetch3", "/").await.0, 200);
    let paths = vec![format!("/fetch/localhost:{port}"); 8];
    let (out, took) = many(&fx.server, "fetch3", paths).await;
    for (st, _, body) in &out {
        assert_eq!(*st, 200, "{body}");
        assert_eq!(body.trim(), "upstream 200", "{body}");
    }
    eprintln!("8 requests awaiting a 200 ms upstream: {took:?}");
    assert!(
        took < Duration::from_millis(600),
        "8 requests of 200 ms each took {took:?}: they did not overlap"
    );
    let (_, _, peak) = get(&fx.server, "fetch3", "/stats").await;
    assert_eq!(peak.trim(), "peak 8", "one instance held all eight at once");
    assert_eq!(fx.app("fetch3").unwrap().wakes.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_in_flight_cap_holds_the_rest_until_a_place_frees() {
    let fx = Fixture::new();
    deploy(&fx, "capped", "[limits]\nconcurrency = 2\n").await;
    assert_eq!(get(&fx.server, "capped", "/").await.0, 200);
    let (out, took) = many(&fx.server, "capped", vec!["/sleep?ms=200".into(); 4]).await;
    assert!(out.iter().all(|r| r.0 == 200), "{out:?}");
    assert!(
        took >= Duration::from_millis(390),
        "4 requests of 200 ms through 2 places took {took:?}"
    );
    assert!(took < Duration::from_millis(790), "{took:?}");
    let (_, _, peak) = get(&fx.server, "capped", "/stats").await;
    assert_eq!(peak.trim(), "peak 2");
}

#[tokio::test(flavor = "multi_thread")]
async fn max_outbound_is_counted_per_request_not_per_instance() {
    let fx = Fixture::with_loopback();
    let port = slow_upstream(Duration::from_millis(50)).await;
    deploy(
        &fx,
        "out3",
        "fetch = [\"localhost\"]\n[limits]\nmax_outbound = 1\n",
    )
    .await;
    let two = format!("/fetch/localhost:{port}?times=2");
    let (out, _) = many(&fx.server, "out3", vec![two.clone(), two]).await;
    for (st, _, body) in out {
        assert_eq!(st, 200, "{body}");
        let calls: Vec<&str> = body.trim().split(", ").collect();
        assert_eq!(
            calls[0], "upstream 200",
            "each request has its own first call: {body}"
        );
        assert!(
            calls[1].contains("HttpRequestDenied"),
            "the second call of a request is over max_outbound: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trap_fails_every_request_in_flight_and_the_next_request_gets_a_fresh_instance() {
    let fx = Fixture::new();
    deploy(&fx, "trap3", "").await;
    assert_eq!(get(&fx.server, "trap3", "/").await.0, 200);
    let mut paths = vec!["/sleep?ms=300".to_string(); 3];
    paths.push("/trap".into());
    let (out, _) = many(&fx.server, "trap3", paths).await;
    for (st, kind, body) in &out {
        assert_eq!(*st, 500, "{body}");
        assert_eq!(kind, "app_trap", "{body}");
        assert!(body.contains("every request in flight"), "{body}");
    }
    let (st, _, body) = get(&fx.server, "trap3", "/").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(fx.app("trap3").unwrap().wakes.load(Ordering::Relaxed), 2);
    let log = std::fs::read_to_string(fx.app("trap3").unwrap().dir.join("app.log")).unwrap();
    assert!(log.contains("p3-probe trapped on purpose"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn compute_past_cpu_ms_stops_the_instance_and_the_next_request_works() {
    let fx = Fixture::new();
    deploy(&fx, "spin3", "[limits]\ncpu_ms = 100\n").await;
    let (st, kind, body) = get(&fx.server, "spin3", "/spin").await;
    assert_eq!((st, kind.as_str()), (508, "cpu_limit"), "{body}");
    assert_eq!(get(&fx.server, "spin3", "/").await.0, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deadline_answers_504_closes_the_instance_and_a_fresh_one_serves_next() {
    let fx = Fixture::new();
    deploy(&fx, "slow3", "[limits]\ndeadline_ms = 300\n").await;
    assert_eq!(get(&fx.server, "slow3", "/").await.0, 200);
    let first = live_instance(&fx, "slow3").unwrap();
    let started = Instant::now();
    let (st, kind, _) = get(&fx.server, "slow3", "/sleep?ms=2000").await;
    assert_eq!((st, kind.as_str()), (504, "deadline"));
    assert!(started.elapsed() < Duration::from_millis(1000));
    assert!(!first.is_open(), "the instance takes no more requests");
    let (st, _, body) = get(&fx.server, "slow3", "/").await;
    assert_eq!(st, 200, "{body}");
    let second = live_instance(&fx, "slow3").unwrap();
    assert!(!Arc::ptr_eq(&first, &second), "a fresh instance served it");
    // The abandoned task holds the old store for at most another deadline_ms.
    let until = Instant::now() + Duration::from_secs(3);
    while Arc::strong_count(&first) > 1 || first.in_flight() > 0 {
        assert!(Instant::now() < until, "the old instance was never dropped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redeploy_and_an_evict_wait_for_the_requests_in_flight() {
    let fx = Fixture::new();
    deploy(&fx, "drain3", "").await;
    assert_eq!(get(&fx.server, "drain3", "/").await.0, 200);
    let s = fx.server.clone();
    let in_flight =
        tokio::spawn(async move { many(&s, "drain3", vec!["/sleep?ms=400".into(); 3]).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    deploy(&fx, "drain3", "[limits]\ncpu_ms = 999\n").await;
    let waited = started.elapsed();
    let (out, _) = in_flight.await.unwrap();
    assert!(out.iter().all(|r| r.0 == 200), "{out:?}");
    assert!(
        waited >= Duration::from_millis(200),
        "the deploy returned after {waited:?}, before the requests in flight finished"
    );
    assert_eq!(get(&fx.server, "drain3", "/").await.0, 200);

    let s = fx.server.clone();
    let in_flight =
        tokio::spawn(async move { many(&s, "drain3", vec!["/sleep?ms=300".into(); 2]).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = fx.server.evict("drain3").await;
    assert_eq!(r.status(), StatusCode::OK);
    let (out, _) = in_flight.await.unwrap();
    assert!(out.iter().all(|r| r.0 == 200), "{out:?}");
    assert!(live_instance(&fx, "drain3").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn wasip3_refusals_at_deploy() {
    let fx = Fixture::new();
    let wasm = p3_probe::p3_probe();
    let data = "name = \"d\"\nruntime = \"wasip3-experimental\"\n";
    let e = fx
        .server
        .install("d", data, &wasm, false, &Creator::Admin)
        .await
        .unwrap_err();
    assert_eq!(e.kind, "wasip3_data_refused", "{}", e.message);
    let e = fx
        .server
        .install(
            "d",
            &p3_manifest("d", "[limits]\nconcurrency = 9\n"),
            &wasm,
            false,
            &Creator::Admin,
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind, "limit_too_high", "{}", e.message);
    // The wasip2 engine has component-model async off, so it cannot even compile the probe.
    let e = fx
        .server
        .install(
            "d",
            "name = \"d\"\n[grants]\ndata = false\nfetch = [\"example.com\"]\n",
            &wasm,
            false,
            &Creator::Admin,
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind, "invalid_component", "{}", e.message);
    assert!(fx.app("d").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn each_request_in_flight_has_its_own_log_allowance() {
    let fx = Fixture::new();
    deploy(&fx, "log3", "").await;
    let per_request = fx.server.cfg.caps.log.per_request;
    let big = format!("/log?bytes={}", per_request + 100_000);
    let (out, _) = many(&fx.server, "log3", vec![big.clone(), big]).await;
    assert!(out.iter().all(|r| r.0 == 200), "{out:?}");
    let log = std::fs::read_to_string(fx.app("log3").unwrap().dir.join("app.log")).unwrap();
    let kept = log.bytes().filter(|b| *b == b'x').count() as u64;
    assert_eq!(kept, 2 * per_request, "each request kept its own allowance");
    assert_eq!(
        log.matches("this request wrote more than the per-request log limit")
            .count(),
        2,
        "{}",
        log.lines()
            .filter(|l| l.starts_with("spared:"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
