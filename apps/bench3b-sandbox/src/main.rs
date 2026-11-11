use rusqlite::{Connection, OpenFlags};
use std::cell::RefCell;
use wstd::http::{Body, Error, Request, Response, StatusCode};

thread_local! {
    static DB: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

fn open() -> Connection {
    let conn = Connection::open_with_flags_and_vfs(
        "/data/app.db",
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        "unix-none",
    )
    .expect("open");
    let _: String = conn.query_row("PRAGMA journal_mode=TRUNCATE", [], |r| r.get(0)).unwrap();
    conn.pragma_update(None, "synchronous", "FULL").unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, text TEXT NOT NULL)", []).unwrap();
    let n: i64 = conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0)).unwrap();
    if n == 0 {
        conn.execute("BEGIN", []).unwrap();
        for i in 1..=100i64 {
            conn.prepare_cached("INSERT INTO items(id, text) VALUES (?1, ?2)")
                .unwrap()
                .execute((i, format!("item {i} ................................")))
                .unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
    }
    conn
}

fn work(n: u64) -> String {
    DB.with(|cell| {
        let mut slot = cell.borrow_mut();
        let conn = slot.get_or_insert_with(open);
        let mut out = String::new();
        for k in 0..10u64 {
            let id = ((n * 7 + k * 13) % 100 + 1) as i64;
            let text: String = conn
                .prepare_cached("SELECT text FROM items WHERE id = ?1")
                .unwrap()
                .query_row([id], |r| r.get(0))
                .unwrap();
            out.push_str(&text);
            out.push('\n');
        }
        conn.prepare_cached("UPDATE items SET text = ?1 WHERE id = ?2")
            .unwrap()
            .execute((format!("v{n} ............................."), (n % 100 + 1) as i64))
            .unwrap();
        out.push_str("ok");
        out
    })
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let n: u64 = req.uri().query().and_then(|q| q.strip_prefix("n=")).and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(Response::builder().status(StatusCode::OK).header("content-type", "text/plain").body(work(n).into())?)
}
