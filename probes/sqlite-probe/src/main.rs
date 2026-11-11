use rusqlite::Connection;
use std::io::Write;
use std::time::Instant;

// Native comparator built with SQLITE_THREADSAFE=0 (what the wasm guest gets). rusqlite refuses a
// single-threaded SQLite on non-wasm targets, so that build renames SQLite's own check
// (-Dsqlite3_threadsafe=sqlite3_threadsafe_real) and this module answers the two questions
// rusqlite asks. The probe is single-threaded. Enabled by RUSTFLAGS="--cfg sqlite_nomutex".
#[cfg(sqlite_nomutex)]
mod nomutex {
    use std::ffi::{c_int, c_void};
    #[unsafe(no_mangle)]
    pub extern "C" fn sqlite3_threadsafe() -> c_int {
        1
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn sqlite3_mutex_alloc(_: c_int) -> *mut c_void {
        16 as *mut c_void
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn sqlite3_mutex_free(_: *mut c_void) {}
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("writes");
    let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let journal = args.get(3).map(String::as_str).unwrap_or("WAL").to_string();
    let path = std::env::var("DB").unwrap_or_else(|_| "/data/app.db".into());
    let conn = Connection::open_with_flags_and_vfs(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        "unix-none",
    ).expect("open");
    let _: String = conn.query_row("PRAGMA locking_mode=EXCLUSIVE", [], |r| r.get(0)).unwrap();
    let _: String = conn.query_row(&format!("PRAGMA journal_mode={journal}"), [], |r| r.get(0)).unwrap();
    conn.pragma_update(None, "synchronous", "FULL").unwrap();
    // Optional extra setup SQL (tuning experiments), e.g. "PRAGMA cache_size=-8000".
    if let Some(extra) = args.get(4) {
        conn.execute_batch(extra).unwrap();
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS t(id INTEGER PRIMARY KEY, v TEXT NOT NULL)").unwrap();
    match mode {
        "writes" => {
            let out = std::io::stdout();
            for i in 0..n {
                let big = format!("{i}-{}", "x".repeat(200));
                conn.execute("INSERT INTO t(id, v) VALUES (?1, ?2)", (i as i64, big)).unwrap();
                let mut o = out.lock();
                writeln!(o, "C {i}").unwrap();
                o.flush().unwrap();
            }
        }
        "verify" => {
            let ic: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
            let cnt: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
            let max: i64 = conn.query_row("SELECT coalesce(max(id),-1) FROM t", [], |r| r.get(0)).unwrap();
            println!("V {ic} {cnt} {max}");
        }
        "bench" => {
            conn.execute_batch("DROP TABLE IF EXISTS b; CREATE TABLE b(id INTEGER PRIMARY KEY, k INTEGER, v TEXT)").unwrap();
            let t = Instant::now();
            conn.execute_batch("BEGIN").unwrap();
            { let mut s = conn.prepare("INSERT INTO b(k,v) VALUES (?1,?2)").unwrap();
              for i in 0..n { s.execute((i as i64 % 1000, format!("row{i}"))).unwrap(); } }
            conn.execute_batch("COMMIT").unwrap();
            println!("B insert_bulk {}", t.elapsed().as_micros());
            conn.execute_batch("CREATE INDEX bk ON b(k)").unwrap();
            let t = Instant::now();
            { let mut s = conn.prepare("SELECT id,v FROM b WHERE id=?1").unwrap();
              for i in 0..n { let _: String = s.query_row([(i as i64 % n as i64) + 1], |r| r.get(1)).unwrap(); } }
            println!("B point_select {}", t.elapsed().as_micros());
            let t = Instant::now();
            { let mut s = conn.prepare("SELECT count(*), sum(id) FROM b WHERE k=?1").unwrap();
              for i in 0..200 { let _: (i64, Option<i64>) = s.query_row([i as i64], |r| Ok((r.get(0)?, r.get(1)?))).unwrap(); } }
            println!("B index_agg {}", t.elapsed().as_micros());
            let t = Instant::now();
            let _: i64 = conn.query_row("SELECT sum(length(v)) FROM b", [], |r| r.get(0)).unwrap();
            println!("B scan {}", t.elapsed().as_micros());
            let t = Instant::now();
            { let mut s = conn.prepare("UPDATE b SET v=?2 WHERE id=?1").unwrap();
              conn.execute_batch("BEGIN").unwrap();
              for i in 0..n { s.execute((((i % n) + 1) as i64, "upd")).unwrap(); }
              conn.execute_batch("COMMIT").unwrap(); }
            println!("B update_bulk {}", t.elapsed().as_micros());
            let t = Instant::now();
            for i in 0..200 { conn.execute("INSERT INTO b(k,v) VALUES (?1,'d')", [i as i64]).unwrap(); }
            println!("B durable_insert_200 {}", t.elapsed().as_micros());
        }
        _ => panic!("mode"),
    }
}
