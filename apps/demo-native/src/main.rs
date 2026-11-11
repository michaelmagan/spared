use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

type Error = hyper::http::Error;

const PAGE: &str = include_str!("../../demo-shared/page.html");

#[derive(Serialize)]
struct Item {
    id: i64,
    text: String,
    created_at: i64,
}

#[derive(Deserialize)]
struct NewItem {
    text: String,
}

static DB_PATH: OnceLock<String> = OnceLock::new();
static DB: Mutex<Option<Connection>> = Mutex::new(None);

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn with_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
    let mut slot = DB.lock().unwrap();
    if slot.is_none() {
        let conn = Connection::open_with_flags(
            DB_PATH.get().expect("db path"),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .expect("open");
        let _: String = conn
            .query_row("PRAGMA journal_mode=TRUNCATE", [], |r| r.get(0))
            .unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, text TEXT NOT NULL, created_at INTEGER)")
            .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
            .unwrap();
        if n == 0 {
            conn.execute_batch("BEGIN").unwrap();
            for i in 1..=100 {
                conn.execute(
                    "INSERT INTO items(text, created_at) VALUES (?1, ?2)",
                    rusqlite::params![format!("Seed item {i}"), 1_700_000_000 + i],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        }
        *slot = Some(conn);
    }
    f(slot.as_ref().unwrap())
}

fn latest(c: &Connection) -> Vec<Item> {
    let mut s = c
        .prepare_cached("SELECT id, text, created_at FROM items ORDER BY id DESC LIMIT 20")
        .unwrap();
    s.query_map([], |r| {
        Ok(Item {
            id: r.get(0)?,
            text: r.get(1)?,
            created_at: r.get(2)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn escape(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

fn render(items: &[Item]) -> String {
    let mut list = String::new();
    for it in items {
        list.push_str("<li><span>");
        escape(&it.text, &mut list);
        list.push_str("</span><time>");
        list.push_str(&it.created_at.to_string());
        list.push_str("</time></li>\n");
    }
    PAGE.replacen("<!--ITEMS-->\n", &list, 1)
}

async fn handle(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Error> {
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/") => {
            let html = with_db(|c| render(&latest(c)));
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/html; charset=utf-8")
                .body(html.into())
        }
        (Method::GET, "/api/items") => json(StatusCode::OK, &with_db(latest)),
        (Method::POST, "/api/items") => {
            let new: NewItem = match req
                .into_body()
                .collect()
                .await
                .map(|b| serde_json::from_slice(&b.to_bytes()))
            {
                Ok(Ok(n)) => n,
                _ => {
                    return Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body("bad json\n".to_owned().into());
                }
            };
            let created_at = now_secs();
            let id = with_db(|c| {
                c.execute(
                    "INSERT INTO items(text, created_at) VALUES (?1, ?2)",
                    rusqlite::params![&new.text, created_at],
                )
                .unwrap();
                c.last_insert_rowid()
            });
            json(
                StatusCode::CREATED,
                &Item {
                    id,
                    text: new.text,
                    created_at,
                },
            )
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("not found\n".to_owned().into())?),
    }
}

fn json<T: Serialize>(status: StatusCode, v: &T) -> Result<Response<Full<Bytes>>, Error> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(v).expect("serializable").into())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let listen = arg(&args, "--listen").unwrap_or_else(|| "127.0.0.1:8090".into());
    DB_PATH
        .set(arg(&args, "--db").unwrap_or_else(|| "app.db".into()))
        .unwrap();
    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind");
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        stream.set_nodelay(true).ok();
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service_fn(handle))
                .await;
        });
    }
}
