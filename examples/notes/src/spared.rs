use rusqlite::{Connection, OpenFlags};
use serde::{Serialize, de::DeserializeOwned};
use std::cell::RefCell;
use wstd::http::{Body, Request, Response};

const DB_PATH: &str = "/data/app.db";
const JOURNAL: &str = "TRUNCATE";

thread_local! {
    static DB: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

fn open() -> Connection {
    let conn = Connection::open_with_flags_and_vfs(
        DB_PATH,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        "unix-none",
    )
    .expect("cannot open /data/app.db: is `data = true` set under [grants] in spared.toml?");
    if JOURNAL == "WAL" {
        let _: String = conn.query_row("PRAGMA locking_mode=EXCLUSIVE", [], |r| r.get(0)).unwrap();
    }
    let _: String = conn.query_row(&format!("PRAGMA journal_mode={JOURNAL}"), [], |r| r.get(0)).unwrap();
    conn.pragma_update(None, "synchronous", "FULL").unwrap();
    conn.execute_batch(crate::SCHEMA).expect("SCHEMA failed");
    conn
}

pub fn with_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
    DB.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(open());
        }
        f(slot.as_ref().unwrap())
    })
}

fn build(status: u16, content_type: &str, body: String) -> Response<Body> {
    Response::builder().status(status).header("content-type", content_type).body(body.into()).expect("valid response")
}

pub fn json_response<T: Serialize>(status: u16, value: &T) -> Response<Body> {
    build(status, "application/json", serde_json::to_string(value).expect("serializable"))
}

pub fn html_response(html: impl Into<String>) -> Response<Body> {
    build(200, "text/html; charset=utf-8", html.into())
}

pub fn text_response(status: u16, text: impl Into<String>) -> Response<Body> {
    build(status, "text/plain; charset=utf-8", text.into())
}

pub async fn read_json<T: DeserializeOwned>(req: &mut Request<Body>) -> Result<T, Response<Body>> {
    match req.body_mut().json::<T>().await {
        Ok(v) => Ok(v),
        Err(e) => Err(json_response(400, &serde_json::json!({"error": format!("invalid JSON body: {e}")}))),
    }
}
