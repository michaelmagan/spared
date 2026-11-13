//! Guest for the escape suite's filesystem quota cases. Each command tries to get more out of
//! /data than the server's per-app caps allow, by telling the host things that are not true, and
//! prints what it reached.
use std::fs::File;
use std::io::Write;
use wasip2::filesystem::preopens::get_directories;
use wasip2::filesystem::types::{Descriptor, DescriptorFlags, OpenFlags, PathFlags};
use wasip2::io::streams::{InputStream, OutputStream, StreamError};

fn arg(i: usize) -> usize {
    std::env::args().nth(i).and_then(|v| v.parse().ok()).unwrap_or(1)
}

fn root() -> Descriptor {
    get_directories().into_iter().next().unwrap().0
}

fn open(root: &Descriptor, name: &str) -> Result<Descriptor, String> {
    root.open_at(
        PathFlags::empty(),
        name,
        OpenFlags::CREATE,
        DescriptorFlags::READ | DescriptorFlags::WRITE,
    )
    .map_err(|e| format!("open: {e:?}"))
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let mut err = String::new();
    let mut count = 0;
    match cmd.as_str() {
        // n files of mb MB; each renamed onto itself when the third argument is 1
        "selfrename-bytes" => {
            let (n, mb, trick) = (arg(2), arg(3), arg(4) == 1);
            let chunk = vec![0x5au8; 1 << 20];
            'files: for i in 0..n {
                let p = format!("/data/f{i:04}");
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
                count += 1;
            }
        }
        "selfrename-entries" => {
            let _ = std::fs::create_dir_all("/data/e");
            for i in 0..arg(2) {
                let p = format!("/data/e/{i:06}");
                if let Err(e) = File::create(&p) {
                    err = format!("create: {e}");
                    break;
                }
                if let Err(e) = std::fs::rename(&p, &p) {
                    err = format!("rename: {e}");
                    break;
                }
                count += 1;
            }
        }
        // rounds of: fill, then n cycles of hard-linking /data/f0000 and unlinking the link
        "linkcycle" => {
            let chunk = vec![0x5au8; 1 << 20];
            let _ = std::fs::write("/data/f0000", &chunk);
            for round in 0..arg(3) {
                if let Ok(mut f) = File::options().create(true).append(true).open(format!("/data/fill{round}")) {
                    for _ in 0..arg(4) {
                        if f.write_all(&chunk).is_err() {
                            break;
                        }
                    }
                }
                for _ in 0..arg(2) {
                    let _ = std::fs::hard_link("/data/f0000", "/data/alias");
                    let _ = std::fs::remove_file("/data/alias");
                    count += 1;
                }
            }
        }
        // per file: grow it sparse to mb MB, take a stream, truncate, then write mb MB through it
        "stale-stream" => {
            let (n, mb) = (arg(2), arg(3));
            let root = root();
            let chunk = vec![0x5au8; 4096];
            'files: for i in 0..n {
                let fd = match open(&root, &format!("t{i:04}")) {
                    Ok(fd) => fd,
                    Err(e) => {
                        err = e;
                        break;
                    }
                };
                if fd.set_size((mb as u64) << 20).is_err() {
                    err = "grow".into();
                    break;
                }
                let Ok(st) = fd.write_via_stream(0) else {
                    err = "stream".into();
                    break;
                };
                let _ = fd.set_size(0);
                for _ in 0..(mb << 20) / 4096 {
                    if let Err(e) = st.blocking_write_and_flush(&chunk) {
                        err = format!("write: {e:?}");
                        break 'files;
                    }
                }
                count += 1;
            }
        }
        // n files: open, take a write (or, with read=1, a read) stream, drop the descriptor
        "hold-streams" => {
            let (n, read) = (arg(2), arg(3) == 1);
            let root = root();
            let mut w: Vec<OutputStream> = vec![];
            let mut r: Vec<InputStream> = vec![];
            for i in 0..n {
                let fd = match open(&root, &format!("s{i:04}")) {
                    Ok(fd) => fd,
                    Err(e) => {
                        err = e;
                        break;
                    }
                };
                let kept = if read {
                    fd.read_via_stream(0).map(|s| r.push(s))
                } else {
                    fd.write_via_stream(0).map(|s| w.push(s))
                };
                match kept {
                    Ok(()) => count += 1,
                    Err(e) => {
                        err = format!("stream: {e:?}");
                        break;
                    }
                }
            }
            println!("HELD {count} {err}");
            return;
        }
        // write mb MB through one stream until the data cap refuses it, then print the debug
        // text the host gives for that error, on one line
        "debug-text" => {
            let root = root();
            let fd = match open(&root, "dbg") {
                Ok(fd) => fd,
                Err(e) => {
                    println!("NOERR {e}");
                    return;
                }
            };
            let Ok(st) = fd.write_via_stream(0) else {
                println!("NOERR stream");
                return;
            };
            let chunk = vec![0x5au8; 4096];
            for _ in 0..(arg(2) << 20) / 4096 {
                match st.blocking_write_and_flush(&chunk) {
                    Ok(()) => count += 1,
                    Err(StreamError::LastOperationFailed(e)) => {
                        let text = e.to_debug_string();
                        println!("DEBUG {}", text.replace('\n', "\\n"));
                        return;
                    }
                    Err(StreamError::Closed) => {
                        println!("NOERR closed");
                        return;
                    }
                }
            }
            println!("NOFAIL {count}");
            return;
        }
        _ => {
            println!("usage");
            return;
        }
    }
    println!("DONE {count} {err}");
}
