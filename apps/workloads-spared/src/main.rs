//! Workload comparison app, Rust side. Twin of apps/workloads-workerd/worker.js: same routes, same generated data
//! (same LCG and seeds), same JSON results. Inputs are generated once per instance and cached; each request does the work.
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::LazyLock;
use wstd::http::{Body, Error, Request, Response, StatusCode};

// ---------- shared deterministic generator (same constants as the JS twin) ----------

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        self.0
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next() >> 8) % n
    }
    fn word(&mut self) -> String {
        let len = 3 + self.below(8);
        (0..len).map(|_| (b'a' + self.below(26) as u8) as char).collect()
    }
}

fn gen_bytes(n: usize, seed: u32) -> Vec<u8> {
    let mut r = Lcg(seed);
    (0..n).map(|_| (r.next() >> 24) as u8).collect()
}

const CATS: [&str; 8] = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"];
const TAGS: [&str; 6] = ["new", "sale", "hot", "eco", "gift", "bulk"];

static VOCAB: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut r = Lcg(7);
    (0..4000).map(|_| r.word()).collect()
});

static TEXT: LazyLock<String> = LazyLock::new(|| {
    let v = &*VOCAB;
    let mut r = Lcg(42);
    let mut s = String::with_capacity(2_000_100);
    while s.len() < 2_000_000 {
        match r.below(100) {
            0 => {
                let (y, m, d) = (1990 + r.below(40), 1 + r.below(12), 1 + r.below(28));
                write!(s, "{y}-{m:02}-{d:02}").unwrap();
            }
            1 => {
                let a = r.below(4000) as usize;
                let b = r.below(4000) as usize;
                write!(s, "{}@{}.com", v[a], v[b]).unwrap();
            }
            2 => {
                for _ in 0..3 {
                    s.push((b'a' + r.below(26) as u8) as char);
                }
                write!(s, "{}", r.below(100000)).unwrap();
            }
            _ => {
                let a = r.below(4000);
                let b = r.below(4000);
                s.push_str(&v[(a * b / 4000) as usize]);
            }
        }
        s.push_str(if r.below(12) == 0 { "\n" } else { " " });
    }
    s.truncate(2_000_000);
    s
});

// ---------- workloads ----------

static SHA_PAYLOAD: LazyLock<Vec<u8>> = LazyLock::new(|| gen_bytes(256 * 1024, 71));
const SHA_ROUNDS: usize = 16;

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        write!(s, "{x:02x}").unwrap();
    }
    s
}

fn w_sha256() -> Value {
    let mut buf = SHA_PAYLOAD.clone();
    let mut last = [0u8; 32];
    for _ in 0..SHA_ROUNDS {
        let d = Sha256::digest(&buf);
        buf[..32].copy_from_slice(&d);
        last.copy_from_slice(&d);
    }
    json!({"bytes": buf.len(), "rounds": SHA_ROUNDS, "digest": hex(&last)})
}

#[derive(Serialize, Deserialize)]
struct Rec {
    id: u32,
    name: String,
    category: String,
    price_cents: u64,
    qty: u64,
    active: bool,
    tags: Vec<String>,
}

static RECORDS_JSON: LazyLock<String> = LazyLock::new(|| {
    let v = &*VOCAB;
    let mut r = Lcg(11);
    let recs: Vec<Rec> = (0..2000)
        .map(|i| {
            let a = r.below(4000) as usize;
            let b = r.below(4000) as usize;
            let category = CATS[r.below(8) as usize].to_string();
            let price_cents = 100 + r.below(100000) as u64;
            let qty = r.below(20) as u64;
            let active = r.below(4) != 0;
            let n = r.below(4);
            let tags = (0..n).map(|_| TAGS[r.below(6) as usize].to_string()).collect();
            Rec { id: i, name: format!("{} {}", v[a], v[b]), category, price_cents, qty, active, tags }
        })
        .collect();
    serde_json::to_string(&recs).unwrap()
});

#[derive(Serialize)]
struct Group {
    category: String,
    count: u64,
    units: u64,
    revenue_cents: u64,
    on_sale: u64,
}

#[derive(Serialize)]
struct OutItem {
    id: u32,
    name: String,
    total_cents: u64,
    tags: String,
}

#[derive(Serialize)]
struct OutDoc {
    groups: Vec<Group>,
    items: Vec<OutItem>,
}

fn w_json(verify: bool) -> Value {
    let input = &*RECORDS_JSON;
    let recs: Vec<Rec> = serde_json::from_str(input).unwrap();
    let kept: Vec<Rec> = recs.into_iter().filter(|r| r.active && r.qty > 0).collect();
    let mut groups: HashMap<String, Group> = HashMap::new();
    for r in &kept {
        let g = groups.entry(r.category.clone()).or_insert_with(|| Group {
            category: r.category.clone(),
            count: 0,
            units: 0,
            revenue_cents: 0,
            on_sale: 0,
        });
        g.count += 1;
        g.units += r.qty;
        g.revenue_cents += r.price_cents * r.qty;
        if r.tags.iter().any(|t| t == "sale") {
            g.on_sale += 1;
        }
    }
    let mut groups: Vec<Group> = groups.into_values().collect();
    groups.sort_by(|a, b| a.category.cmp(&b.category));
    let items = kept
        .iter()
        .map(|r| OutItem { id: r.id, name: r.name.to_uppercase(), total_cents: r.price_cents * r.qty, tags: r.tags.join(",") })
        .collect();
    let doc = OutDoc { groups, items };
    let out = serde_json::to_string(&doc).unwrap();
    let mut res = json!({"in_len": input.len(), "kept": kept.len(), "groups": doc.groups, "out_len": out.len()});
    if verify {
        res["in_sha256"] = json!(hex(&Sha256::digest(input.as_bytes())));
        res["out_sha256"] = json!(hex(&Sha256::digest(out.as_bytes())));
    }
    res
}

// SQLite: same open path and pragmas as apps/demo-spared.
thread_local! {
    static DB: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

fn with_db<T>(f: impl FnOnce(&mut Connection) -> T) -> T {
    DB.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let conn = Connection::open_with_flags_and_vfs(
                "/data/app.db",
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
                "unix-none",
            )
            .expect("open");
            let _: String = conn.query_row("PRAGMA journal_mode=TRUNCATE", [], |r| r.get(0)).unwrap();
            conn.pragma_update(None, "synchronous", "FULL").unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS agg(id INTEGER PRIMARY KEY, cat INTEGER NOT NULL, amount INTEGER NOT NULL, label TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS w(id INTEGER PRIMARY KEY, cat INTEGER NOT NULL, amount INTEGER NOT NULL, label TEXT NOT NULL);",
            )
            .unwrap();
            *slot = Some(conn);
        }
        f(slot.as_mut().unwrap())
    })
}

const AGG_ROWS: u32 = 50_000;

fn w_sql_seed() -> Value {
    with_db(|c| {
        let n: i64 = c.query_row("SELECT count(*) FROM agg", [], |r| r.get(0)).unwrap();
        if n != AGG_ROWS as i64 {
            let v = &*VOCAB;
            let mut r = Lcg(21);
            let tx = c.transaction().unwrap();
            tx.execute("DELETE FROM agg", []).unwrap();
            {
                let mut st = tx.prepare("INSERT INTO agg(id, cat, amount, label) VALUES (?1, ?2, ?3, ?4)").unwrap();
                for i in 0..AGG_ROWS {
                    let cat = r.below(10);
                    let amount = r.below(1_000_000);
                    let label = &v[r.below(4000) as usize];
                    st.execute(rusqlite::params![i + 1, cat, amount, label]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let n: i64 = c.query_row("SELECT count(*) FROM agg", [], |r| r.get(0)).unwrap();
        json!({"rows": n})
    })
}

fn w_sql_agg() -> Value {
    with_db(|c| {
        let mut st = c.prepare_cached("SELECT cat, amount, label FROM agg").unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))
            .unwrap();
        let (mut count, mut sum, mut min, mut max) = (0i64, 0i64, i64::MAX, i64::MIN);
        let mut cat_count = [0i64; 10];
        let mut cat_sum = [0i64; 10];
        let mut labels: HashSet<String> = HashSet::new();
        for row in rows {
            let (cat, amount, label) = row.unwrap();
            count += 1;
            sum += amount;
            min = min.min(amount);
            max = max.max(amount);
            cat_count[cat as usize] += 1;
            cat_sum[cat as usize] += amount;
            labels.insert(label);
        }
        json!({"count": count, "sum": sum, "min": min, "max": max, "mean": sum / count,
               "cat_count": cat_count, "cat_sum": cat_sum, "distinct_labels": labels.len()})
    })
}

const WRITE_ROWS: u32 = 10_000;

fn w_sql_write() -> Value {
    with_db(|c| {
        let v = &*VOCAB;
        let mut r = Lcg(31);
        let tx = c.transaction().unwrap();
        tx.execute("DELETE FROM w", []).unwrap();
        {
            let mut st = tx.prepare_cached("INSERT INTO w(id, cat, amount, label) VALUES (?1, ?2, ?3, ?4)").unwrap();
            for i in 0..WRITE_ROWS {
                let cat = r.below(10);
                let amount = r.below(1_000_000);
                let label = &v[r.below(4000) as usize];
                st.execute(rusqlite::params![i + 1, cat, amount, label]).unwrap();
            }
        }
        tx.commit().unwrap();
        let (n, s): (i64, i64) = c.query_row("SELECT count(*), sum(amount) FROM w", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        json!({"rows": n, "sum": s})
    })
}

static MARKDOWN: LazyLock<String> = LazyLock::new(|| {
    let v = &*VOCAB;
    let mut r = Lcg(51);
    let mut s = String::new();
    let mut sec = 0;
    let w = |r: &mut Lcg| v[r.below(4000) as usize].as_str();
    while s.len() < 50_000 {
        sec += 1;
        let h = w(&mut r);
        writeln!(s, "## Section {sec} {h}\n").unwrap();
        for _ in 0..1 + r.below(3) {
            let n = 30 + r.below(30);
            for k in 0..n {
                if k > 0 {
                    s.push(' ');
                }
                let kind = r.below(20);
                let t = w(&mut r);
                match kind {
                    0 => write!(s, "*{t}*").unwrap(),
                    1 => write!(s, "**{t}**").unwrap(),
                    2 => write!(s, "`{t}`").unwrap(),
                    3 => write!(s, "[{t}](https://example.com/{t})").unwrap(),
                    _ => s.push_str(t),
                }
            }
            s.push_str(".\n\n");
        }
        if r.below(2) == 0 {
            for _ in 0..3 + r.below(5) {
                s.push_str("-");
                for _ in 0..4 + r.below(7) {
                    s.push(' ');
                    s.push_str(w(&mut r));
                }
                s.push('\n');
            }
            s.push('\n');
        }
        if r.below(3) == 0 {
            for i in 0..3 + r.below(4) {
                write!(s, "{}.", i + 1).unwrap();
                for _ in 0..4 + r.below(7) {
                    s.push(' ');
                    s.push_str(w(&mut r));
                }
                s.push('\n');
            }
            s.push('\n');
        }
        if r.below(4) == 0 {
            s.push_str("```\n");
            for _ in 0..2 + r.below(4) {
                let (a, b) = (w(&mut r), w(&mut r));
                writeln!(s, "let {a} = {b}({});", r.below(1000)).unwrap();
            }
            s.push_str("```\n\n");
        }
    }
    s
});

fn w_markdown(verify: bool) -> Value {
    let md = &*MARKDOWN;
    let parser = pulldown_cmark::Parser::new(md);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    let mut res = json!({"md_len": md.len(), "html_len": html.len()});
    if verify {
        let tags = ["<h2>", "<p>", "<ul>", "<ol>", "<li>", "<em>", "<strong>", "<code>", "<pre>", "<a "];
        let counts: serde_json::Map<String, Value> = tags.iter().map(|t| (t.to_string(), json!(html.matches(t).count()))).collect();
        res["md_sha256"] = json!(hex(&Sha256::digest(md.as_bytes())));
        res["tags"] = Value::Object(counts);
        res["html_sha256"] = json!(hex(&Sha256::digest(html.as_bytes())));
    }
    res
}

const SIEVE_N: usize = 2_000_000;

fn w_sieve() -> Value {
    let n = SIEVE_N;
    let mut composite = vec![false; n + 1];
    let (mut count, mut sum) = (0u64, 0u64);
    for i in 2..=n {
        if !composite[i] {
            count += 1;
            sum += i as u64;
            if (i as u64) * (i as u64) <= n as u64 {
                let mut j = i * i;
                while j <= n {
                    composite[j] = true;
                    j += i;
                }
            }
        }
    }
    json!({"n": n, "primes": count, "sum": sum})
}

#[derive(Clone, Copy)]
struct Body3 {
    x: f64,
    y: f64,
    z: f64,
    vx: f64,
    vy: f64,
    vz: f64,
    mass: f64,
}

const NBODY_STEPS: u32 = 200_000;

fn nbody_bodies() -> Vec<Body3> {
    let pi = std::f64::consts::PI;
    let solar_mass = 4.0 * pi * pi;
    let days = 365.24;
    let b = |x, y, z, vx: f64, vy: f64, vz: f64, m: f64| Body3 { x, y, z, vx: vx * days, vy: vy * days, vz: vz * days, mass: m * solar_mass };
    let mut v = vec![
        Body3 { x: 0.0, y: 0.0, z: 0.0, vx: 0.0, vy: 0.0, vz: 0.0, mass: solar_mass },
        b(4.84143144246472090e+00, -1.16032004402742839e+00, -1.03622044471123109e-01, 1.66007664274403694e-03, 7.69901118419740425e-03, -6.90460016972063023e-05, 9.54791938424326609e-04),
        b(8.34336671824457987e+00, 4.12479856412430479e+00, -4.03523417114321381e-01, -2.76742510726862411e-03, 4.99852801234917238e-03, 2.30417297573763929e-05, 2.85885980666130812e-04),
        b(1.28943695621391310e+01, -1.51111514016986312e+01, -2.23307578892655734e-01, 2.96460137564761618e-03, 2.37847173959480950e-03, -2.96589568540237556e-05, 4.36624404335156298e-05),
        b(1.53796971148509165e+01, -2.59193146099879641e+01, 1.79258772950371181e-01, 2.68067772490389322e-03, 1.62824170038242295e-03, -9.51592254519715870e-05, 5.15138902046611451e-05),
    ];
    let (mut px, mut py, mut pz) = (0.0, 0.0, 0.0);
    for b in &v {
        px += b.vx * b.mass;
        py += b.vy * b.mass;
        pz += b.vz * b.mass;
    }
    v[0].vx = -px / solar_mass;
    v[0].vy = -py / solar_mass;
    v[0].vz = -pz / solar_mass;
    v
}

fn energy(v: &[Body3]) -> f64 {
    let mut e = 0.0;
    for i in 0..v.len() {
        let b = v[i];
        e += 0.5 * b.mass * (b.vx * b.vx + b.vy * b.vy + b.vz * b.vz);
        for c in &v[i + 1..] {
            let (dx, dy, dz) = (b.x - c.x, b.y - c.y, b.z - c.z);
            e -= b.mass * c.mass / (dx * dx + dy * dy + dz * dz).sqrt();
        }
    }
    e
}

fn advance(v: &mut [Body3], dt: f64) {
    let n = v.len();
    for i in 0..n {
        for j in i + 1..n {
            let dx = v[i].x - v[j].x;
            let dy = v[i].y - v[j].y;
            let dz = v[i].z - v[j].z;
            let d2 = dx * dx + dy * dy + dz * dz;
            let mag = dt / (d2 * d2.sqrt());
            let mj = v[j].mass;
            let mi = v[i].mass;
            v[i].vx -= dx * mj * mag;
            v[i].vy -= dy * mj * mag;
            v[i].vz -= dz * mj * mag;
            v[j].vx += dx * mi * mag;
            v[j].vy += dy * mi * mag;
            v[j].vz += dz * mi * mag;
        }
    }
    for b in v.iter_mut() {
        b.x += dt * b.vx;
        b.y += dt * b.vy;
        b.z += dt * b.vz;
    }
}

fn w_nbody() -> Value {
    let mut v = nbody_bodies();
    let e0 = energy(&v);
    for _ in 0..NBODY_STEPS {
        advance(&mut v, 0.01);
    }
    json!({"steps": NBODY_STEPS, "energy_before": format!("{e0:.9}"), "energy_after": format!("{:.9}", energy(&v))})
}

const IMG: usize = 512;
static IMAGE: LazyLock<Vec<u8>> = LazyLock::new(|| gen_bytes(IMG * IMG * 4, 61));

fn w_pixels(verify: bool) -> Value {
    let src = &*IMAGE;
    let (w, h) = (IMG, IMG);
    let mut gray = vec![0u8; w * h];
    for i in 0..w * h {
        let (r, g, b) = (src[4 * i] as u32, src[4 * i + 1] as u32, src[4 * i + 2] as u32);
        gray[i] = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
    }
    const R: isize = 2;
    let mut tmp = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s = 0u32;
            for k in -R..=R {
                let xx = (x as isize + k).clamp(0, w as isize - 1) as usize;
                s += gray[y * w + xx] as u32;
            }
            tmp[y * w + x] = (s / 5) as u8;
        }
    }
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let mut s = 0u32;
            for k in -R..=R {
                let yy = (y as isize + k).clamp(0, h as isize - 1) as usize;
                s += tmp[yy * w + x] as u32;
            }
            let v = (s / 5) as u8;
            let i = 4 * (y * w + x);
            out[i] = v;
            out[i + 1] = v;
            out[i + 2] = v;
            out[i + 3] = 255;
        }
    }
    let mut res = json!({"bytes": out.len(), "sample": [out[0], out[4 * 1000], out[4 * 131_071], out[out.len() - 4]]});
    if verify {
        res["out_sha256"] = json!(hex(&Sha256::digest(&out)));
    }
    res
}

static SORT_INTS: LazyLock<Vec<u32>> = LazyLock::new(|| {
    let mut r = Lcg(91);
    (0..200_000).map(|_| r.next()).collect()
});
static SORT_STRS: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut r = Lcg(92);
    (0..50_000)
        .map(|_| {
            let len = 6 + r.below(9);
            (0..len).map(|_| (b'a' + r.below(26) as u8) as char).collect()
        })
        .collect()
});

fn w_sort(verify: bool) -> Value {
    let mut a = SORT_INTS.clone();
    a.sort_unstable();
    let mut s: Vec<&str> = SORT_STRS.iter().map(String::as_str).collect();
    s.sort_unstable();
    let mut res = json!({"ints": [a[0], a[100_000], a[a.len() - 1]], "strs": [s[0], s[25_000], s[s.len() - 1]]});
    if verify {
        let ints: Vec<String> = a.iter().map(u32::to_string).collect();
        res["ints_sha256"] = json!(hex(&Sha256::digest(ints.join(",").as_bytes())));
        res["strs_sha256"] = json!(hex(&Sha256::digest(s.join(",").as_bytes())));
    }
    res
}

fn w_wordfreq() -> Value {
    let text = &*TEXT;
    let mut counts: HashMap<&str, u32> = HashMap::new();
    let mut tokens = 0u32;
    for w in text.split_whitespace() {
        tokens += 1;
        *counts.entry(w).or_insert(0) += 1;
    }
    let mut v: Vec<(&str, u32)> = counts.iter().map(|(k, c)| (*k, *c)).collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let top: Vec<Value> = v.iter().take(10).map(|(w, c)| json!([w, c])).collect();
    json!({"text_len": text.len(), "tokens": tokens, "distinct": counts.len(), "top": top})
}

struct Row {
    id: u32,
    name: String,
    category: &'static str,
    cents: u32,
}

static TABLE_ROWS: LazyLock<Vec<Row>> = LazyLock::new(|| {
    let v = &*VOCAB;
    let mut r = Lcg(81);
    (0..10_000)
        .map(|i| {
            let a = &v[r.below(4000) as usize];
            let sep = match r.below(10) {
                0 => " & ",
                1 => " <b> ",
                _ => " ",
            };
            let b = &v[r.below(4000) as usize];
            let category = CATS[r.below(8) as usize];
            let cents = r.below(1_000_000);
            Row { id: i + 1, name: format!("{a}{sep}{b}"), category, cents }
        })
        .collect()
});

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

fn w_html(verify: bool) -> Value {
    let mut html = String::new();
    html.push_str("<table>\n<thead><tr><th>ID</th><th>Name</th><th>Category</th><th>Amount</th></tr></thead>\n<tbody>\n");
    for (i, r) in TABLE_ROWS.iter().enumerate() {
        if i > 0 {
            html.push('\n');
        }
        write!(html, "<tr><td>{}</td><td>", r.id).unwrap();
        escape(&r.name, &mut html);
        write!(html, "</td><td>{}</td><td class=\"num\">${}.{:02}</td></tr>", r.category, r.cents / 100, r.cents % 100).unwrap();
    }
    html.push_str("\n</tbody>\n</table>\n");
    let mid = html.len() / 2;
    let mut res = json!({"html_len": html.len(), "sample": &html[mid..mid + 40]});
    if verify {
        res["html_sha256"] = json!(hex(&Sha256::digest(html.as_bytes())));
    }
    res
}

const GZIP_LEN: usize = 1_000_000;

fn w_gzip(verify: bool) -> Value {
    let input = &TEXT.as_bytes()[..GZIP_LEN];
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(input).unwrap();
    let out = enc.finish().unwrap();
    let mut res = json!({"in_len": input.len(), "out_len": out.len()});
    if verify {
        use base64::Engine as _;
        res["gzip_b64"] = json!(base64::engine::general_purpose::STANDARD.encode(&out));
    }
    res
}

static PATTERNS: [&str; 4] = [
    r"\b\d{4}-\d{2}-\d{2}\b",
    r"[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}",
    r"\b[a-z]{3}\d+\b",
    r"\b\w*(?:qu|xz|zz)\w*\b",
];
static REGEXES: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| PATTERNS.iter().map(|p| regex::Regex::new(p).unwrap()).collect());

fn w_regex() -> Value {
    let text = &*TEXT;
    let counts: Vec<usize> = REGEXES.iter().map(|re| re.find_iter(text).count()).collect();
    json!({"text_len": text.len(), "counts": counts})
}

const PBKDF2_ITERS: u32 = 10_000;

fn w_pbkdf2() -> Value {
    let salt = b"spared-workloads-salt";
    let keys: Vec<String> = (0..4)
        .map(|i| {
            let pw = format!("correct horse battery staple {i}");
            let mut out = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(pw.as_bytes(), salt, PBKDF2_ITERS, &mut out);
            hex(&out)
        })
        .collect();
    json!({"iterations": PBKDF2_ITERS, "keys": keys})
}

// ---------- routing ----------

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let path = req.uri().path().to_string();
    let verify = req.uri().query().is_some_and(|q| q.split('&').any(|p| p == "verify=1"));
    let v = match path.as_str() {
        "/w/noop" => json!({"ok": true}),
        "/w/sha256" => w_sha256(),
        "/w/json" => w_json(verify),
        "/w/sql-seed" => w_sql_seed(),
        "/w/sql-agg" => w_sql_agg(),
        "/w/sql-write" => w_sql_write(),
        "/w/markdown" => w_markdown(verify),
        "/w/sieve" => w_sieve(),
        "/w/nbody" => w_nbody(),
        "/w/pixels" => w_pixels(verify),
        "/w/sort" => w_sort(verify),
        "/w/wordfreq" => w_wordfreq(),
        "/w/html" => w_html(verify),
        "/w/gzip" => w_gzip(verify),
        "/w/regex" => w_regex(),
        "/w/pbkdf2" => w_pbkdf2(),
        _ => return Ok(Response::builder().status(StatusCode::NOT_FOUND).body("not found\n".to_owned().into())?),
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from_json(&v)?)?)
}
