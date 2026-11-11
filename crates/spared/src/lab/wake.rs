use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::path::{Path, PathBuf};
use std::time::Instant;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Result, Store};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

pub struct AppState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl AppState {
    pub fn new(wasi: WasiCtx, http: WasiHttpCtx, table: ResourceTable) -> Self {
        Self { wasi, http, table }
    }
}

impl WasiView for AppState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for AppState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

pub fn load(engine: &Engine, path: &Path) -> Result<ProxyPre<AppState>> {
    let comp = if path.extension().is_some_and(|e| e == "cwasm") {
        unsafe { Component::deserialize_file(engine, path)? }
    } else {
        Component::from_file(engine, path)?
    };
    let mut linker: Linker<AppState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    ProxyPre::new(linker.instantiate_pre(&comp)?)
}

pub struct Timings {
    pub instantiate_us: u128,
    pub response_us: u128,
    pub total_us: u128,
    pub status: u16,
    pub body: Bytes,
}

pub async fn request(
    pre: &ProxyPre<AppState>,
    data: &Path,
    method: &str,
    path: &str,
    body: &str,
) -> Result<Timings> {
    let t0 = Instant::now();
    let mut b = WasiCtx::builder();
    b.preopened_dir(data, "/data", FsPerms::ReadWrite)?;
    b.allow_blocking_current_thread(true);
    for k in ["SPARED_TRACE", "SPARED_JOURNAL"] {
        if let Ok(v) = std::env::var(k) {
            b.env(k, &v);
        }
    }
    b.inherit_stderr();
    let mut store = Store::new(
        pre.engine(),
        AppState {
            wasi: b.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        },
    );
    store.set_epoch_deadline(u64::MAX / 2);
    let req = hyper::Request::builder()
        .method(method)
        .uri(format!("http://app.test{path}"))
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(body.to_owned()))
                .map_err(|e: std::convert::Infallible| -> hyper::Error { match e {} }),
        )?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let req = store
        .data_mut()
        .http()
        .new_incoming_request(Scheme::Http, req)?;
    let out = store.data_mut().http().new_response_outparam(tx)?;
    let pre = pre.clone();
    let t_inst = Instant::now();
    let task = tokio::task::spawn(async move {
        let proxy = pre.instantiate_async(&mut store).await?;
        let inst_done = Instant::now();
        proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, req, out)
            .await?;
        Ok::<Instant, wasmtime::Error>(inst_done)
    });
    let resp = rx
        .await
        .map_err(|_| wasmtime::format_err!("no response"))??;
    let t_resp = Instant::now();
    let status = resp.status().as_u16();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| wasmtime::format_err!("{e:?}"))?
        .to_bytes();
    let inst_done = task.await??;
    Ok(Timings {
        instantiate_us: inst_done.duration_since(t_inst).as_micros(),
        response_us: t_resp.duration_since(inst_done).as_micros(),
        total_us: t0
            .elapsed()
            .as_micros()
            .max(t_resp.duration_since(t0).as_micros()),
        status,
        body,
    })
}

pub fn pct(v: &mut [u128], p: f64) -> u128 {
    v.sort_unstable();
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

pub async fn bench(
    engine: Engine,
    comp: PathBuf,
    data: PathBuf,
    iters: usize,
    seed: usize,
) -> Result<()> {
    let pre = load(&engine, &comp)?;
    std::fs::create_dir_all(&data)?;
    for i in 0..seed {
        let t = request(
            &pre,
            &data,
            "POST",
            "/items",
            &format!("{{\"text\":\"item {i}\"}}"),
        )
        .await?;
        assert_eq!(t.status, 201, "{:?}", t.body);
    }
    let mut inst = vec![];
    let mut resp = vec![];
    let mut total = vec![];
    for _ in 0..iters {
        let t = request(&pre, &data, "GET", "/items", "").await?;
        assert_eq!(t.status, 200);
        inst.push(t.instantiate_us);
        resp.push(t.response_us);
        total.push(t.total_us);
    }
    let mut posts = vec![];
    for i in 0..200 {
        let t = request(
            &pre,
            &data,
            "POST",
            "/items",
            &format!("{{\"text\":\"w{i}\"}}"),
        )
        .await?;
        assert_eq!(t.status, 201);
        posts.push(t.total_us);
    }
    let out = serde_json::json!({
        "wake_then_durable_write_us": {"p50": pct(&mut posts, 0.5), "p99": pct(&mut posts, 0.99)},
        "iters": iters, "seeded_rows": seed,
        "instantiate_us": {"p50": pct(&mut inst, 0.5), "p99": pct(&mut inst, 0.99)},
        "first_response_us": {"p50": pct(&mut resp, 0.5), "p99": pct(&mut resp, 0.99)},
        "wake_to_first_query_us": {"p50": pct(&mut total, 0.5), "p99": pct(&mut total, 0.99), "max": *total.last().unwrap()},
    });
    println!("{out}");
    Ok(())
}
