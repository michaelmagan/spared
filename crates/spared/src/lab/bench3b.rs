use super::wake::pct;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::path::{Path, PathBuf};
use std::time::Instant;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Result, Store};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

wasmtime::component::bindgen!({
    path: "../../apps/bench3b-host/wit",
    world: "bench-host",
});

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Variant {
    Sandbox,
    Host,
}

pub struct BState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    conn: Option<rusqlite::Connection>,
}

impl WasiView for BState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for BState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

fn open_native(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    let _: String = conn.query_row("PRAGMA journal_mode=TRUNCATE", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(conn)
}

fn bind(params: &str) -> std::result::Result<Vec<rusqlite::types::Value>, String> {
    use rusqlite::types::Value as V;
    let arr: Vec<serde_json::Value> = serde_json::from_str(params).map_err(|e| e.to_string())?;
    arr.into_iter()
        .map(|v| match v {
            serde_json::Value::Null => Ok(V::Null),
            serde_json::Value::Bool(b) => Ok(V::Integer(b as i64)),
            serde_json::Value::Number(n) => Ok(n
                .as_i64()
                .map(V::Integer)
                .unwrap_or_else(|| V::Real(n.as_f64().unwrap_or(0.0)))),
            serde_json::Value::String(s) => Ok(V::Text(s)),
            other => Err(format!("unsupported param {other}")),
        })
        .collect()
}

impl spared::bench::db::Host for BState {
    fn exec(&mut self, sql: String, params_json: String) -> std::result::Result<u64, String> {
        let p = bind(&params_json)?;
        let conn = self.conn.as_ref().ok_or("no db")?;
        let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        stmt.execute(rusqlite::params_from_iter(p))
            .map(|n| n as u64)
            .map_err(|e| e.to_string())
    }

    fn query(&mut self, sql: String, params_json: String) -> std::result::Result<String, String> {
        use rusqlite::types::ValueRef as R;
        let p = bind(&params_json)?;
        let conn = self.conn.as_ref().ok_or("no db")?;
        let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        let cols = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(p))
            .map_err(|e| e.to_string())?;
        let mut out: Vec<Vec<serde_json::Value>> = vec![];
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut r = Vec::with_capacity(cols);
            for i in 0..cols {
                r.push(match row.get_ref(i).map_err(|e| e.to_string())? {
                    R::Null => serde_json::Value::Null,
                    R::Integer(n) => n.into(),
                    R::Real(f) => f.into(),
                    R::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
                    R::Blob(_) => return Err("blob unsupported".into()),
                });
            }
            out.push(r);
        }
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }
}

fn rusage_us() -> (u64, u64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    (us(ru.ru_utime), us(ru.ru_stime))
}

pub async fn run(
    engine: Engine,
    comp: PathBuf,
    variant: Variant,
    data: PathBuf,
    requests: usize,
    warmup: usize,
    run_no: u32,
    out: Option<PathBuf>,
) -> Result<()> {
    let component = if comp.extension().is_some_and(|e| e == "cwasm") {
        unsafe { Component::deserialize_file(&engine, &comp)? }
    } else {
        Component::from_file(&engine, &comp)?
    };
    let mut linker: Linker<BState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    if variant == Variant::Host {
        spared::bench::db::add_to_linker::<BState, HasSelf<BState>>(&mut linker, |s| s)?;
    }
    let pre = ProxyPre::new(linker.instantiate_pre(&component)?)?;

    std::fs::create_dir_all(&data)?;
    let mut b = WasiCtx::builder();
    b.allow_blocking_current_thread(true);
    let conn = match variant {
        Variant::Sandbox => {
            b.preopened_dir(&data, "/data", FsPerms::ReadWrite)?;
            None
        }
        Variant::Host => Some(open_native(&data.join("app.db"))?),
    };
    let mut store = Store::new(
        &engine,
        BState {
            wasi: b.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            conn,
        },
    );
    store.set_epoch_deadline(u64::MAX / 2);
    let proxy = pre.instantiate_async(&mut store).await?;

    let mut lat: Vec<u128> = Vec::with_capacity(requests);
    let mut last_body = Bytes::new();
    let mut cpu0 = (0, 0);
    let mut t_start = Instant::now();
    for i in 0..(warmup + requests) {
        if i == warmup {
            cpu0 = rusage_us();
            t_start = Instant::now();
        }
        let req = hyper::Request::builder()
            .method("GET")
            .uri(format!("http://app.test/work?n={i}"))
            .body(
                Full::new(Bytes::new())
                    .map_err(|e: std::convert::Infallible| -> hyper::Error { match e {} }),
            )?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, req)?;
        let outparam = store.data_mut().http().new_response_outparam(tx)?;
        let t0 = Instant::now();
        let call = proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, req, outparam);
        let (c, resp) = tokio::join!(call, rx);
        c?;
        let resp = resp.map_err(|_| wasmtime::format_err!("no response"))??;
        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| wasmtime::format_err!("{e:?}"))?
            .to_bytes();
        let ns = t0.elapsed().as_nanos();
        assert_eq!(status, 200, "{body:?}");
        if i >= warmup {
            lat.push(ns);
        }
        last_body = body;
    }
    let wall = t_start.elapsed().as_secs_f64();
    let cpu1 = rusage_us();
    assert!(last_body.ends_with(b"ok"), "unexpected body {last_body:?}");
    let user = cpu1.0 - cpu0.0;
    let sys = cpu1.1 - cpu0.1;
    let mean = lat.iter().sum::<u128>() as f64 / lat.len() as f64 / 1000.0;
    let us = |ns: u128| ns as f64 / 1000.0;
    let (p50, p99) = (us(pct(&mut lat, 0.5)), us(pct(&mut lat, 0.99)));
    let max = us(*lat.last().unwrap());
    let result = serde_json::json!({
        "benchmark": "request-cost",
        "variant": match variant { Variant::Sandbox => "sandbox", Variant::Host => "host" },
        "run": run_no,
        "requests": requests,
        "warmup": warmup,
        "latency_us": {"p50": p50, "p99": p99, "max": max, "mean": mean},
        "cpu_us_per_request": (user + sys) as f64 / requests as f64,
        "cpu_user_us_per_request": user as f64 / requests as f64,
        "cpu_sys_us_per_request": sys as f64 / requests as f64,
        "wall_s": wall,
        "response_bytes": last_body.len(),
    });
    println!("{result}");
    if let Some(p) = out {
        std::fs::write(p, serde_json::to_string_pretty(&result)? + "\n")?;
    }
    Ok(())
}
