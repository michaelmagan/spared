use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use wstd::http::{Body, Error, Method, Request, Response, StatusCode};

#[derive(Serialize)]
struct Item {
    id: i64,
    text: String,
}

#[derive(Deserialize)]
struct NewItem {
    text: String,
}

thread_local! {
    static DB: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

fn with_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
    DB.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let trace = std::env::var_os("SPARED_TRACE").is_some();
            let journal = std::env::var("SPARED_JOURNAL").unwrap_or_else(|_| "WAL".into());
            let t = std::time::Instant::now();
            let conn = Connection::open_with_flags_and_vfs(
                "/data/app.db",
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
                "unix-none",
            )
            .expect("open");
            let t_open = t.elapsed().as_micros();
            if journal == "WAL" {
                let _: String = conn.query_row("PRAGMA locking_mode=EXCLUSIVE", [], |r| r.get(0)).unwrap();
            }
            let _: String = conn.query_row(&format!("PRAGMA journal_mode={journal}"), [], |r| r.get(0)).unwrap();
            conn.pragma_update(None, "synchronous", "FULL").unwrap();
            let t_pragma = t.elapsed().as_micros();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, text TEXT NOT NULL)").unwrap();
            if trace {
                eprintln!("open={t_open} pragmas_done={t_pragma} schema_done={}", t.elapsed().as_micros());
            }
            *slot = Some(conn);
        }
        f(slot.as_ref().unwrap())
    })
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/items") => {
            let items: Vec<Item> = with_db(|c| {
                let mut s = c.prepare("SELECT id, text FROM items ORDER BY id").unwrap();
                s.query_map([], |r| Ok(Item { id: r.get(0)?, text: r.get(1)? }))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            });
            json(StatusCode::OK, &items)
        }
        (Method::POST, "/items") => {
            let new: NewItem = req.body_mut().json().await?;
            let id = with_db(|c| {
                c.execute("INSERT INTO items(text) VALUES (?1)", [&new.text]).unwrap();
                c.last_insert_rowid()
            });
            json(StatusCode::CREATED, &Item { id, text: new.text })
        }
        _ => Ok(Response::builder().status(StatusCode::NOT_FOUND).body("not found\n".to_owned().into())?),
    }
}

fn json<T: Serialize>(status: StatusCode, v: &T) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from_json(v)?)?)
}
