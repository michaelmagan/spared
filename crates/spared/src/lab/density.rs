use super::wake::{AppState, load, pct};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::path::{Path, PathBuf};
use std::time::Instant;
use wasmtime::component::ResourceTable;
use wasmtime::{Engine, Result, Store};
use wasmtime_wasi::{FsPerms, WasiCtx};
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::bindings::{Proxy, ProxyPre};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};

pub struct Hot {
    store: Store<AppState>,
    proxy: Proxy,
}

impl Hot {
    pub async fn wake(pre: &ProxyPre<AppState>, data: &Path) -> Result<Hot> {
        let mut b = WasiCtx::builder();
        b.preopened_dir(data, "/data", FsPerms::ReadWrite)?;
        b.allow_blocking_current_thread(true);
        let mut store = Store::new(
            pre.engine(),
            AppState::new(b.build(), WasiHttpCtx::new(), ResourceTable::new()),
        );
        store.set_epoch_deadline(u64::MAX / 2);
        let proxy = pre.instantiate_async(&mut store).await?;
        Ok(Hot { store, proxy })
    }

    pub async fn call(&mut self, method: &str, path: &str, body: &str) -> Result<u16> {
        let req = hyper::Request::builder()
            .method(method)
            .uri(format!("http://app.test{path}"))
            .header("content-type", "application/json")
            .body(
                Full::new(Bytes::from(body.to_owned()))
                    .map_err(|e: std::convert::Infallible| -> hyper::Error { match e {} }),
            )?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = self
            .store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, req)?;
        let out = self.store.data_mut().http().new_response_outparam(tx)?;
        let call = self
            .proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut self.store, req, out);
        let (c, resp) = tokio::join!(call, rx);
        c?;
        let resp = resp.map_err(|_| wasmtime::format_err!("no response"))??;
        let status = resp.status().as_u16();
        let _ = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| wasmtime::format_err!("{e:?}"))?;
        Ok(status)
    }
}

fn status_kb(key: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn dir_size(p: &Path) -> u64 {
    let mut t = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let m = e.metadata().unwrap();
            t += if m.is_dir() {
                dir_size(&e.path())
            } else {
                m.len()
            };
        }
    }
    t
}

pub async fn run(
    engine: Engine,
    comp: PathBuf,
    root: PathBuf,
    deployed: usize,
    hot: usize,
    rows: usize,
) -> Result<()> {
    let pre = load(&engine, &comp)?;
    std::fs::create_dir_all(&root)?;
    let dir = |i: usize| root.join(format!("app{i:05}"));
    let rss0 = status_kb("VmRSS:");

    let t = Instant::now();
    for i in 0..deployed {
        std::fs::create_dir_all(dir(i))?;
        let mut h = Hot::wake(&pre, &dir(i)).await?;
        for r in 0..rows {
            assert_eq!(
                h.call("POST", "/items", &format!("{{\"text\":\"item {r}\"}}"))
                    .await?,
                201
            );
        }
    }
    let deploy_s = t.elapsed().as_secs_f64();
    let rss_idle = status_kb("VmRSS:");
    let disk = dir_size(&root);

    let mut apps = Vec::with_capacity(hot);
    let mut wake = vec![];
    for i in 0..hot {
        let t = Instant::now();
        let mut h = Hot::wake(&pre, &dir(i * (deployed / hot.max(1)).max(1) % deployed)).await?;
        assert_eq!(h.call("GET", "/items", "").await?, 200);
        wake.push(t.elapsed().as_micros());
        apps.push(h);
    }
    let rss_hot = status_kb("VmRSS:");
    let hwm = status_kb("VmHWM:");

    let mut warm = vec![];
    for round in 0..20 {
        for (i, h) in apps.iter_mut().enumerate() {
            let t = Instant::now();
            let s = if (round + i) % 10 == 0 {
                h.call("POST", "/items", "{\"text\":\"x\"}").await?
            } else {
                h.call("GET", "/items", "").await?
            };
            assert!(s == 200 || s == 201);
            warm.push(t.elapsed().as_micros());
        }
    }
    let rss_after = status_kb("VmRSS:");
    println!(
        "{}",
        serde_json::json!({
            "deployed": deployed, "hot": hot, "rows_per_app": rows,
            "deploy_seconds": deploy_s,
            "rss_kb_baseline": rss0, "rss_kb_after_deploy_idle": rss_idle,
            "idle_rss_growth_kb": rss_idle as i64 - rss0 as i64,
            "disk_bytes_total": disk, "disk_bytes_per_app": disk / deployed as u64,
            "rss_kb_with_hot": rss_hot, "rss_kb_after_requests": rss_after, "rss_kb_peak": hwm,
            "hot_kb_per_instance": (rss_hot as i64 - rss_idle as i64) / hot as i64,
            "hot_kb_per_instance_after_requests": (rss_after as i64 - rss_idle as i64) / hot as i64,
            "wake_us": {"p50": pct(&mut wake, 0.5), "p99": pct(&mut wake, 0.99)},
            "warm_request_us": {"p50": pct(&mut warm, 0.5), "p99": pct(&mut warm, 0.99), "n": warm.len()},
        })
    );
    Ok(())
}
