use serde_json::{Value, json};
use std::cell::Cell;
use wstd::http::{Body, Error, Request, Response, StatusCode};

wit_bindgen::generate!({ path: "wit", world: "bench-host" });

use spared::bench::db;

thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
}

fn exec(sql: &str, params: Value) -> u64 {
    db::exec(sql, &params.to_string()).expect("exec")
}

fn query(sql: &str, params: Value) -> Vec<Vec<Value>> {
    serde_json::from_str(&db::query(sql, &params.to_string()).expect("query")).expect("json")
}

fn init() {
    if READY.get() {
        return;
    }
    exec("CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, text TEXT NOT NULL)", json!([]));
    let n = query("SELECT count(*) FROM items", json!([]))[0][0].as_i64().unwrap();
    if n == 0 {
        exec("BEGIN", json!([]));
        for i in 1..=100 {
            exec("INSERT INTO items(id, text) VALUES (?1, ?2)", json!([i, format!("item {i} ................................")]));
        }
        exec("COMMIT", json!([]));
    }
    READY.set(true);
}

fn work(n: u64) -> String {
    init();
    let mut out = String::new();
    for k in 0..10u64 {
        let id = (n * 7 + k * 13) % 100 + 1;
        let rows = query("SELECT text FROM items WHERE id = ?1", json!([id]));
        out.push_str(rows[0][0].as_str().unwrap());
        out.push('\n');
    }
    exec("UPDATE items SET text = ?1 WHERE id = ?2", json!([format!("v{n} ............................."), n % 100 + 1]));
    out.push_str("ok");
    out
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let n: u64 = req.uri().query().and_then(|q| q.strip_prefix("n=")).and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(Response::builder().status(StatusCode::OK).header("content-type", "text/plain").body(work(n).into())?)
}
