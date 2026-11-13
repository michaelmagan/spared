//! Test guest for scripts/server-test.py: routes that misbehave on purpose, and one that fetches.
use std::cell::RefCell;
use std::fs::{File, ReadDir};
use std::hint::black_box;
use std::io::Write;
use wstd::http::{Body, Error, Request, Response};

thread_local! {
    static STREAMS: RefCell<Vec<wasip2::io::streams::OutputStream>> = const { RefCell::new(Vec::new()) };
    static READERS: RefCell<Vec<wasip2::io::streams::InputStream>> = const { RefCell::new(Vec::new()) };
    static FILES: RefCell<Vec<File>> = const { RefCell::new(Vec::new()) };
    static DIRS: RefCell<Vec<ReadDir>> = const { RefCell::new(Vec::new()) };
}

fn param(req: &Request<Body>, key: &str) -> Option<usize> {
    req.uri()
        .query()?
        .split('&')
        .find_map(|p| p.strip_prefix(key)?.strip_prefix('='))?
        .parse()
        .ok()
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let body = match req.uri().path() {
        "/spin" => {
            let mut x = 0u64;
            loop {
                x = black_box(x.wrapping_add(1));
            }
        }
        "/panic" => panic!("hostile-app: panic on purpose"),
        "/big" => vec![b'x'; param(&req, "mb").unwrap_or(1) << 20],
        "/alloc" => {
            let mut v = vec![0u8; param(&req, "mb").unwrap_or(1) << 20];
            for i in (0..v.len()).step_by(4096) {
                v[i] = black_box(1);
            }
            format!("allocated {} bytes\n", black_box(v).len()).into_bytes()
        }
        "/mkfiles" => {
            let _ = std::fs::create_dir_all("/data/d");
            let (start, n) = (param(&req, "start").unwrap_or(0), param(&req, "n").unwrap_or(1));
            let mut made = 0;
            let mut err = String::new();
            for i in start..start + n {
                match File::create(format!("/data/d/{i:08}")) {
                    Ok(_) => made += 1,
                    Err(e) => {
                        err = e.to_string();
                        break;
                    }
                }
            }
            format!("made {made} err={err}\n").into_bytes()
        }
        "/readdirs" => {
            let n = param(&req, "n").unwrap_or(1);
            let mut opened = 0;
            let mut err = String::new();
            for _ in 0..n {
                match std::fs::read_dir("/data/d") {
                    Ok(d) => {
                        DIRS.with(|v| v.borrow_mut().push(d));
                        opened += 1;
                    }
                    Err(e) => {
                        err = e.to_string();
                        break;
                    }
                }
            }
            format!("listed {opened} err={err}\n").into_bytes()
        }
        "/hostwork" => {
            let ms = param(&req, "ms").unwrap_or(1000) as u128;
            let start = std::time::Instant::now();
            let mut calls = 0;
            while start.elapsed().as_millis() < ms {
                drop(black_box(std::fs::read_dir("/data/d")));
                calls += 1;
            }
            format!("listed {calls} times in {} ms\n", start.elapsed().as_millis()).into_bytes()
        }
        "/listcount" => match std::fs::read_dir("/data/d") {
            Ok(d) => format!("entries {}\n", d.count()).into_bytes(),
            Err(e) => format!("err={e}\n").into_bytes(),
        },
        "/fds" => {
            let n = param(&req, "n").unwrap_or(1);
            let mut err = String::new();
            for i in 0..n {
                match File::create(format!("/data/fd-{i}")) {
                    Ok(f) => FILES.with(|v| v.borrow_mut().push(f)),
                    Err(e) => {
                        err = e.to_string();
                        break;
                    }
                }
            }
            let held = FILES.with(|v| v.borrow().len());
            format!("held {held} err={err}\n").into_bytes()
        }
        // n GETs of http://example.com/ (granted in the test manifest), stopping at the first error
        "/fetch" => {
            let n = param(&req, "n").unwrap_or(1);
            let (mut ok, mut bytes, mut err) = (0, 0, String::new());
            for _ in 0..n {
                let out = Request::get("http://example.com/").body(Body::empty()).expect("valid request");
                match wstd::http::Client::new().send(out).await {
                    Ok(mut resp) => match resp.body_mut().contents().await {
                        Ok(b) => {
                            ok += 1;
                            bytes += b.len();
                        }
                        Err(e) => {
                            err = format!("body: {e:?}");
                            break;
                        }
                    },
                    Err(e) => {
                        err = format!("send: {e:?}");
                        break;
                    }
                }
            }
            format!("fetched {ok} bytes={bytes} err={err}\n").into_bytes()
        }
        "/sleep" => {
            wstd::task::sleep(std::time::Duration::from_millis(param(&req, "ms").unwrap_or(100) as u64).into()).await;
            b"slept\n".to_vec()
        }
        "/log" => {
            let n = param(&req, "n").unwrap_or(1);
            let chunk = vec![b'L'; 1 << 20];
            let t = std::time::Instant::now();
            let mut out = std::io::stdout();
            for _ in 0..n {
                let _ = out.write_all(&chunk);
            }
            format!("logged {n} MiB in {} ms\n", t.elapsed().as_millis()).into_bytes()
        }
        "/sparse" => match File::create("/data/sparse.bin").and_then(|f| f.set_len((param(&req, "n").unwrap_or(1) as u64) << 20)) {
            Ok(()) => b"sparse ok\n".to_vec(),
            Err(e) => format!("sparse err={e}\n").into_bytes(),
        },
        "/fill" => {
            let n = param(&req, "n").unwrap_or(1);
            let chunk = vec![7u8; 1 << 20];
            let mut written = 0;
            let mut err = String::new();
            match std::fs::OpenOptions::new().create(true).append(true).open("/data/fill.bin") {
                Ok(mut f) => {
                    for _ in 0..n {
                        if let Err(e) = f.write_all(&chunk) {
                            err = e.to_string();
                            break;
                        }
                        written += 1;
                    }
                }
                Err(e) => err = e.to_string(),
            }
            format!("wrote {written} MiB err={err}\n").into_bytes()
        }
        // n files of mb MB each; with trick=1 each is renamed onto itself after it is written
        "/selfrename" => {
            let (n, mb, trick) = (param(&req, "n").unwrap_or(1), param(&req, "mb").unwrap_or(1), param(&req, "trick") == Some(1));
            let chunk = vec![0x5au8; 1 << 20];
            let mut made = 0;
            let mut err = String::new();
            'files: for i in 0..n {
                let p = format!("/data/f{i:06}");
                let mut f = match File::create(&p) {
                    Ok(f) => f,
                    Err(e) => {
                        err = format!("create: {e}");
                        break;
                    }
                };
                for _ in 0..mb {
                    if let Err(e) = f.write_all(&chunk) {
                        err = format!("write: {e}");
                        break 'files;
                    }
                }
                drop(f);
                if trick && let Err(e) = std::fs::rename(&p, &p) {
                    err = format!("rename: {e}");
                    break;
                }
                made += 1;
            }
            format!("made {made} err={err}\n").into_bytes()
        }
        // n empty files; with trick=1 each is renamed onto itself after it is created
        "/selfrename-entries" => {
            let (n, trick) = (param(&req, "n").unwrap_or(1), param(&req, "trick") == Some(1));
            let _ = std::fs::create_dir_all("/data/e");
            let mut made = 0;
            let mut err = String::new();
            for i in 0..n {
                let p = format!("/data/e/{i:08}");
                if let Err(e) = File::create(&p) {
                    err = format!("create: {e}");
                    break;
                }
                if trick && let Err(e) = std::fs::rename(&p, &p) {
                    err = format!("rename: {e}");
                    break;
                }
                made += 1;
            }
            format!("made {made} err={err}\n").into_bytes()
        }
        // n times: hard-link /data/f000000 to a second name, then unlink that name
        "/linkcycle" => {
            let mut done = 0;
            let mut err = String::new();
            for _ in 0..param(&req, "n").unwrap_or(1) {
                if let Err(e) = std::fs::hard_link("/data/f000000", "/data/alias") {
                    err = format!("link: {e}");
                    break;
                }
                if let Err(e) = std::fs::remove_file("/data/alias") {
                    err = format!("unlink: {e}");
                    break;
                }
                done += 1;
            }
            format!("link cycles {done} err={err}\n").into_bytes()
        }
        // n files: open, take a write stream, drop the descriptor, keep the stream
        "/streams" => {
            use wasip2::filesystem::types::{DescriptorFlags, OpenFlags, PathFlags};
            let (root, _) = wasip2::filesystem::preopens::get_directories().into_iter().next().unwrap();
            let mut held = 0;
            let mut err = String::new();
            for i in 0..param(&req, "n").unwrap_or(1) {
                let fd = match root.open_at(PathFlags::empty(), &format!("s{i:05}"), OpenFlags::CREATE, DescriptorFlags::READ | DescriptorFlags::WRITE) {
                    Ok(fd) => fd,
                    Err(e) => {
                        err = format!("open: {e:?}");
                        break;
                    }
                };
                let kept = if param(&req, "read") == Some(1) {
                    fd.read_via_stream(0).map(|s| READERS.with(|v| v.borrow_mut().push(s)))
                } else {
                    fd.write_via_stream(0).map(|s| STREAMS.with(|v| v.borrow_mut().push(s)))
                };
                match kept {
                    Ok(()) => held += 1,
                    Err(e) => {
                        err = format!("stream: {e:?}");
                        break;
                    }
                }
            }
            format!("streams held {held} err={err}\n").into_bytes()
        }
        // n times: open a file, take a write stream, drop both again
        "/streamcycle" => {
            use wasip2::filesystem::types::{DescriptorFlags, OpenFlags, PathFlags};
            let (root, _) = wasip2::filesystem::preopens::get_directories().into_iter().next().unwrap();
            let mut done = 0;
            let mut err = String::new();
            for _ in 0..param(&req, "n").unwrap_or(1) {
                let fd = match root.open_at(PathFlags::empty(), "cycle", OpenFlags::CREATE, DescriptorFlags::READ | DescriptorFlags::WRITE) {
                    Ok(fd) => fd,
                    Err(e) => {
                        err = format!("open: {e:?}");
                        break;
                    }
                };
                match fd.write_via_stream(0) {
                    Ok(s) => drop(s),
                    Err(e) => {
                        err = format!("stream: {e:?}");
                        break;
                    }
                }
                done += 1;
            }
            format!("stream cycles {done} err={err}\n").into_bytes()
        }
        // per file: grow it sparse to mb MB, take a stream at 0, truncate the file, then write mb
        // MB of real data through the stream
        "/stale" => {
            use wasip2::filesystem::types::{DescriptorFlags, OpenFlags, PathFlags};
            let (root, _) = wasip2::filesystem::preopens::get_directories().into_iter().next().unwrap();
            let (n, mb) = (param(&req, "n").unwrap_or(1), param(&req, "mb").unwrap_or(1));
            let chunk = vec![0x5au8; 4096];
            let mut made = 0;
            let mut err = String::new();
            'files: for i in 0..n {
                let fd = match root.open_at(PathFlags::empty(), &format!("t{i:05}"), OpenFlags::CREATE, DescriptorFlags::READ | DescriptorFlags::WRITE) {
                    Ok(fd) => fd,
                    Err(e) => {
                        err = format!("open: {e:?}");
                        break;
                    }
                };
                if let Err(e) = fd.set_size((mb as u64) << 20) {
                    err = format!("grow: {e:?}");
                    break;
                }
                let st = match fd.write_via_stream(0) {
                    Ok(s) => s,
                    Err(e) => {
                        err = format!("stream: {e:?}");
                        break;
                    }
                };
                if let Err(e) = fd.set_size(0) {
                    err = format!("shrink: {e:?}");
                    break;
                }
                for _ in 0..(mb << 20) / 4096 {
                    if let Err(e) = st.blocking_write_and_flush(&chunk) {
                        err = format!("write: {e:?}");
                        break 'files;
                    }
                }
                made += 1;
            }
            format!("stale-stream files {made} err={err}\n").into_bytes()
        }
        _ => b"ok\n".to_vec(),
    };
    Ok(Response::new(body.into()))
}
