#[allow(dead_code)]
mod spared;

use serde::{Deserialize, Serialize};
use wstd::http::{Body, Error, Method, Request, Response};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS notes(
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
)";

#[derive(Serialize)]
struct Note {
    id: i64,
    text: String,
    created_at: String,
}

#[derive(Deserialize)]
struct NewNote {
    text: String,
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>notes</title>
<style>
  body { font: 16px system-ui, sans-serif; max-width: 32rem; margin: 2rem auto; padding: 0 1rem; }
  form { display: flex; gap: .5rem; }
  input { flex: 1; padding: .5rem; }
  li { margin: .4rem 0; }
</style>
<h1>notes</h1>
<form id="f"><input id="t" placeholder="Add a note" required><button>Add</button></form>
<ul id="list"></ul>
<script>
async function load() {
  const notes = await (await fetch('api/notes')).json();
  const ul = document.getElementById('list');
  ul.replaceChildren(...notes.map(n => {
    const li = document.createElement('li');
    li.textContent = n.text + ' ';
    const b = document.createElement('button');
    b.textContent = 'delete';
    b.onclick = async () => { await fetch('api/notes/' + n.id, { method: 'DELETE' }); load(); };
    li.append(b);
    return li;
  }));
}
document.getElementById('f').onsubmit = async (e) => {
  e.preventDefault();
  const t = document.getElementById('t');
  await fetch('api/notes', { method: 'POST', body: JSON.stringify({ text: t.value }) });
  t.value = '';
  load();
};
load();
</script>
"#;

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => spared::html_response(PAGE),
        (Method::GET, "/api/notes") => {
            let notes: Vec<Note> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT id, text, created_at FROM notes ORDER BY id").unwrap();
                s.query_map([], |r| Ok(Note { id: r.get(0)?, text: r.get(1)?, created_at: r.get(2)? }))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            });
            spared::json_response(200, &notes)
        }
        (Method::POST, "/api/notes") => {
            let new: NewNote = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            let note = spared::with_db(|c| {
                c.execute("INSERT INTO notes(text) VALUES (?1)", [&new.text]).unwrap();
                let id = c.last_insert_rowid();
                c.query_row("SELECT id, text, created_at FROM notes WHERE id = ?1", [id], |r| {
                    Ok(Note { id: r.get(0)?, text: r.get(1)?, created_at: r.get(2)? })
                })
                .unwrap()
            });
            spared::json_response(201, &note)
        }
        (Method::DELETE, p) if p.starts_with("/api/notes/") => match p["/api/notes/".len()..].parse::<i64>() {
            Ok(id) => {
                let n = spared::with_db(|c| c.execute("DELETE FROM notes WHERE id = ?1", [id]).unwrap());
                if n == 0 { spared::text_response(404, "no such note\n") } else { spared::text_response(200, "deleted\n") }
            }
            Err(_) => spared::text_response(400, "note id must be a number\n"),
        },
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
