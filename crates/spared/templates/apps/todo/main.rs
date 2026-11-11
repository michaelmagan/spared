// todo: create, read, update and delete rows, with a checkbox page on top of a JSON API.
#[allow(dead_code)]
mod spared;

use serde::{Deserialize, Serialize};
use wstd::http::{Body, Error, Method, Request, Response};

// SCHEMA runs on every open; IF NOT EXISTS makes that harmless. To add a column later, add an
// `ALTER TABLE` guarded the same way, or start a new table: spared never migrates for you.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS todos(
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0
)";

#[derive(Serialize)]
struct Todo {
    id: i64,
    text: String,
    done: bool,
}

#[derive(Deserialize)]
struct NewTodo {
    text: String,
}

#[derive(Deserialize)]
struct Patch {
    done: bool,
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__NAME__</title>
<style>
  body { font: 16px system-ui, sans-serif; max-width: 32rem; margin: 2rem auto; padding: 0 1rem; }
  form { display: flex; gap: .5rem; }
  input[type=text] { flex: 1; padding: .5rem; }
  li { margin: .4rem 0; list-style: none; }
  .done span { text-decoration: line-through; color: #888; }
</style>
<h1>__NAME__</h1>
<form id="f"><input type="text" id="t" placeholder="Add a todo" required><button>Add</button></form>
<ul id="list"></ul>
<script>
async function load() {
  const todos = await (await fetch('api/todos')).json();
  document.getElementById('list').replaceChildren(...todos.map(t => {
    const li = document.createElement('li');
    li.className = t.done ? 'done' : '';
    const box = document.createElement('input');
    box.type = 'checkbox';
    box.checked = t.done;
    box.onchange = async () => {
      await fetch('api/todos/' + t.id, { method: 'PATCH', body: JSON.stringify({ done: box.checked }) });
      load();
    };
    const label = document.createElement('span');
    label.textContent = ' ' + t.text + ' ';
    const del = document.createElement('button');
    del.textContent = 'delete';
    del.onclick = async () => { await fetch('api/todos/' + t.id, { method: 'DELETE' }); load(); };
    li.append(box, label, del);
    return li;
  }));
}
document.getElementById('f').onsubmit = async (e) => {
  e.preventDefault();
  const t = document.getElementById('t');
  await fetch('api/todos', { method: 'POST', body: JSON.stringify({ text: t.value }) });
  t.value = '';
  load();
};
load();
</script>
"#;

fn get(id: i64) -> Option<Todo> {
    spared::with_db(|c| {
        c.query_row("SELECT id, text, done FROM todos WHERE id = ?1", [id], |r| {
            Ok(Todo { id: r.get(0)?, text: r.get(1)?, done: r.get::<_, i64>(2)? != 0 })
        })
        .ok()
    })
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/") => spared::html_response(PAGE),
        (Method::GET, "/api/todos") => {
            let todos: Vec<Todo> = spared::with_db(|c| {
                let mut s = c.prepare("SELECT id, text, done FROM todos ORDER BY id").unwrap();
                s.query_map([], |r| Ok(Todo { id: r.get(0)?, text: r.get(1)?, done: r.get::<_, i64>(2)? != 0 }))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            });
            spared::json_response(200, &todos)
        }
        (Method::POST, "/api/todos") => {
            let new: NewTodo = match spared::read_json(&mut req).await {
                Ok(v) => v,
                Err(bad_request) => return Ok(bad_request),
            };
            let id = spared::with_db(|c| {
                c.execute("INSERT INTO todos(text) VALUES (?1)", [&new.text]).unwrap();
                c.last_insert_rowid()
            });
            spared::json_response(201, &get(id).unwrap())
        }
        // PATCH and DELETE share the /api/todos/<id> shape; a non-numeric id is the caller's mistake (400).
        (m, p) if p.starts_with("/api/todos/") && (m == Method::PATCH || m == Method::DELETE) => {
            let Ok(id) = p["/api/todos/".len()..].parse::<i64>() else {
                return Ok(spared::text_response(400, "todo id must be a number\n"));
            };
            if m == Method::DELETE {
                let n = spared::with_db(|c| c.execute("DELETE FROM todos WHERE id = ?1", [id]).unwrap());
                if n == 0 { spared::text_response(404, "no such todo\n") } else { spared::text_response(200, "deleted\n") }
            } else {
                let patch: Patch = match spared::read_json(&mut req).await {
                    Ok(v) => v,
                    Err(bad_request) => return Ok(bad_request),
                };
                spared::with_db(|c| c.execute("UPDATE todos SET done = ?1 WHERE id = ?2", (patch.done as i64, id)).unwrap());
                match get(id) {
                    Some(t) => spared::json_response(200, &t),
                    None => spared::text_response(404, "no such todo\n"),
                }
            }
        }
        _ => spared::text_response(404, "not found\n"),
    };
    Ok(resp)
}
