use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use spin_sdk::http::body::IncomingBodyExt;
use spin_sdk::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use spin_sdk::http_service;
use spin_sdk::sqlite::{Connection, Value};

const PAGE: &str = include_str!("../../demo-shared/page.html");

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, text TEXT NOT NULL, created_at INTEGER)";
const SEED: &str = "INSERT INTO items(text, created_at) \
    WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i < 100) \
    SELECT 'Seed item ' || i, 1700000000 + i FROM c WHERE (SELECT count(*) FROM items) = 0";
const LATEST: &str = "SELECT id, text, created_at FROM items ORDER BY id DESC LIMIT 20";

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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn init(db: &Connection) -> Result<()> {
    db.execute(SCHEMA, []).await.map_err(|e| anyhow!("{e:?}"))?.result().await.map_err(|e| anyhow!("{e:?}"))?;
    db.execute(SEED, []).await.map_err(|e| anyhow!("{e:?}"))?.result().await.map_err(|e| anyhow!("{e:?}"))?;
    Ok(())
}

async fn try_latest(db: &Connection) -> Result<Vec<Item>> {
    let mut q = db.execute(LATEST, []).await.map_err(|e| anyhow!("{e:?}"))?;
    let mut out = Vec::new();
    while let Some(row) = q.next().await {
        out.push(Item {
            id: row.get(0).ok_or_else(|| anyhow!("id"))?,
            text: row.get::<&str>(1).ok_or_else(|| anyhow!("text"))?.to_string(),
            created_at: row.get(2).ok_or_else(|| anyhow!("created_at"))?,
        });
    }
    q.result().await.map_err(|e| anyhow!("{e:?}"))?;
    Ok(out)
}

async fn latest(db: &Connection) -> Result<Vec<Item>> {
    match try_latest(db).await {
        Ok(items) if !items.is_empty() => Ok(items),
        _ => {
            init(db).await?;
            try_latest(db).await
        }
    }
}

async fn insert(db: &Connection, text: &str, created_at: i64) -> Result<i64> {
    let params = || [Value::Text(text.to_string()), Value::Integer(created_at)];
    const SQL: &str = "INSERT INTO items(text, created_at) VALUES (?, ?)";
    let first = match db.execute(SQL, params()).await {
        Ok(q) => q.result().await,
        Err(e) => Err(e),
    };
    if first.is_err() {
        init(db).await?;
        db.execute(SQL, params()).await.map_err(|e| anyhow!("{e:?}"))?.result().await.map_err(|e| anyhow!("{e:?}"))?;
    }
    Ok(db.last_insert_rowid().await)
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

type Reply = (StatusCode, HeaderMap, String);

fn reply(status: StatusCode, content_type: &'static str, body: String) -> Result<Reply> {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static(content_type));
    headers.insert("content-length", HeaderValue::from(body.len()));
    Ok((status, headers, body))
}

#[http_service]
async fn handle(req: Request) -> Result<Reply> {
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/") => {
            let db = Connection::open_default().await.map_err(|e| anyhow!("{e:?}"))?;
            reply(StatusCode::OK, "text/html; charset=utf-8", render(&latest(&db).await?))
        }
        (Method::GET, "/api/items") => {
            let db = Connection::open_default().await.map_err(|e| anyhow!("{e:?}"))?;
            reply(StatusCode::OK, "application/json", serde_json::to_string(&latest(&db).await?)?)
        }
        (Method::POST, "/api/items") => {
            let bytes = req.into_body().bytes().await.map_err(|e| anyhow!("{e:?}"))?;
            let new: NewItem = match serde_json::from_slice(&bytes) {
                Ok(n) => n,
                Err(_) => return reply(StatusCode::BAD_REQUEST, "text/plain", "bad json\n".into()),
            };
            let created_at = now_secs();
            let db = Connection::open_default().await.map_err(|e| anyhow!("{e:?}"))?;
            let id = insert(&db, &new.text, created_at).await?;
            reply(StatusCode::CREATED, "application/json", serde_json::to_string(&Item { id, text: new.text, created_at })?)
        }
        _ => reply(StatusCode::NOT_FOUND, "text/plain", "not found\n".into()),
    }
}
