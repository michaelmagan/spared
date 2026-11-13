use crate::grants::{Grants, Limits, Manifest, Runtime, ungranted_imports};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use wasmtime::component::Component;
use wasmtime::{Engine, Result};
use wasmtime_wasi::p2::bindings::Command;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

pub struct Outcome {
    pub stdout: String,
    pub error: Option<String>,
    pub ms: u128,
}

pub async fn run_cmd(
    engine: &Engine,
    wasm: &Path,
    m: &Manifest,
    data: &Path,
    args: &[&str],
) -> Outcome {
    run_cmd_with(
        engine,
        wasm,
        m,
        data,
        args,
        &crate::fsguard::AppCaps::default(),
        None,
    )
    .await
}

async fn run_cmd_with(
    engine: &Engine,
    wasm: &Path,
    m: &Manifest,
    data: &Path,
    args: &[&str],
    caps: &crate::fsguard::AppCaps,
    quota: Option<Arc<crate::quota::AppQuota>>,
) -> Outcome {
    let t = Instant::now();
    let fail = |e: String| Outcome {
        stdout: String::new(),
        error: Some(e),
        ms: t.elapsed().as_millis(),
    };
    let comp = match Component::from_file(engine, wasm) {
        Ok(c) => c,
        Err(e) => return fail(format!("load: {e:#}")),
    };
    let bad = ungranted_imports(engine, &comp, &m.grants, Runtime::Wasip2);
    if !bad.is_empty() {
        return fail(format!("refused: ungranted imports {bad:?}"));
    }
    let linker = match crate::sandbox::linker(engine) {
        Ok(l) => l,
        Err(e) => return fail(format!("link: {e:#}")),
    };
    let out = MemoryOutputPipe::new(1 << 16);
    let mut b = match crate::sandbox::wasi_builder(m, data, &out) {
        Ok(b) => b,
        Err(e) => return fail(format!("preopen: {e:#}")),
    };
    b.args(
        &std::iter::once("app")
            .chain(args.iter().copied())
            .collect::<Vec<_>>(),
    );
    let mut store = crate::sandbox::store_for(engine, m, data, b.build(), caps, quota).await;
    let run = async {
        let cmd = Command::instantiate_async(&mut store, &comp, &linker).await?;
        cmd.wasi_cli_run().call_run(&mut store).await
    };
    let res = tokio::time::timeout(Duration::from_millis(m.limits.deadline_ms), run).await;
    let stdout = String::from_utf8_lossy(&out.contents()).to_string();
    let error = match res {
        Err(_) => Some("deadline exceeded".to_string()),
        Ok(Err(e)) => Some(format!("{e:#}")),
        Ok(Ok(Err(()))) => Some("guest exited with error".to_string()),
        Ok(Ok(Ok(()))) => None,
    };
    Outcome {
        stdout,
        error,
        ms: t.elapsed().as_millis(),
    }
}

struct Case {
    name: &'static str,
    denied: bool,
    control: bool,
    detail: String,
}

fn manifest(name: &str, fetch: &[&str], memory_mb: usize, cpu_ms: u64) -> Manifest {
    Manifest {
        name: name.into(),
        runtime: Runtime::Wasip2,
        grants: Grants {
            data: true,
            fetch: fetch.iter().map(|s| s.to_string()).collect(),
        },
        limits: Limits {
            memory_mb,
            cpu_ms,
            deadline_ms: 5000,
            max_outbound: 50,
            concurrency: None,
        },
    }
}

const ROOT_MARKER: &str = ".spared-escape-root";

/// Make `root` an empty directory the suite owns. A directory that has anything in it and was not
/// made by a previous run (it has no marker file) is refused rather than emptied.
fn prepare_root(root: &Path) -> Result<()> {
    match std::fs::read_dir(root) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                if !root.join(ROOT_MARKER).is_file() {
                    return Err(wasmtime::format_err!(
                        "{} is not empty and was not created by escape-test; pass an empty or new directory",
                        root.display()
                    ));
                }
                std::fs::remove_dir_all(root)?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::fs::create_dir_all(root)?;
    std::fs::write(root.join(ROOT_MARKER), "")?;
    Ok(())
}

pub async fn run(probes: PathBuf, root: PathBuf) -> Result<()> {
    let engine = super::engine()?;
    crate::sandbox::start_epoch_ticker(&engine);

    prepare_root(&root)?;
    let root = root.canonicalize()?;
    let (a, b) = (root.join("appA"), root.join("appB"));
    std::fs::create_dir_all(&a)?;
    std::fs::create_dir_all(&b)?;
    std::fs::write(a.join("secret.txt"), "TOP SECRET OF APP A")?;
    std::fs::write(b.join("own.txt"), "mine")?;
    std::os::unix::fs::symlink("../appA", b.join("link"))?;
    std::os::unix::fs::symlink(&a, b.join("abslink"))?;
    let fsp = probes.join("fs-probe.wasm");
    let netp = probes.join("net-probe.wasm");
    let httpp = probes.join("http-probe.wasm");
    let abs_secret = a.join("secret.txt").to_string_lossy().to_string();
    let mut cases: Vec<Case> = vec![];

    let base = manifest("appB", &[], 32, 500);
    let o = run_cmd(&engine, &fsp, &base, &b, &["read", "/data/own.txt"]).await;
    cases.push(Case {
        name: "control: own data file is readable",
        denied: o.stdout.starts_with("READ 4"),
        control: true,
        detail: o.stdout.trim().into(),
    });

    for (name, path) in [
        (
            "another app's file via ../ traversal",
            "/data/../appA/secret.txt".to_string(),
        ),
        (
            "another app's file via deeper ../../",
            "/data/../../appA/secret.txt".to_string(),
        ),
        (
            "another app's file by absolute host path",
            abs_secret.clone(),
        ),
        (
            "another app's file via relative symlink in own dir",
            "/data/link/secret.txt".to_string(),
        ),
        (
            "another app's file via absolute symlink in own dir",
            "/data/abslink/secret.txt".to_string(),
        ),
        ("/etc/passwd", "/etc/passwd".to_string()),
        ("/proc/self/environ", "/proc/self/environ".to_string()),
    ] {
        let o = run_cmd(&engine, &fsp, &base, &b, &["read", &path]).await;
        cases.push(Case {
            name: Box::leak(format!("read {name}").into_boxed_str()),
            denied: o.stdout.starts_with("ERR"),
            control: false,
            detail: o.stdout.trim().to_string(),
        });
    }
    for (name, path) in [
        (
            "outside the data dir (/escape.txt)",
            "/escape.txt".to_string(),
        ),
        ("another app via ../", "/data/../appA/pwn.txt".to_string()),
    ] {
        let o = run_cmd(&engine, &fsp, &base, &b, &["write", &path]).await;
        let exists = a.join("pwn.txt").exists() || Path::new("/escape.txt").exists();
        cases.push(Case {
            name: Box::leak(format!("write {name}").into_boxed_str()),
            denied: o.stdout.starts_with("ERR") && !exists,
            control: false,
            detail: o.stdout.trim().to_string(),
        });
    }

    let m32 = manifest("appB", &[], 32, 500);
    let o = run_cmd(&engine, &fsp, &m32, &b, &["alloc", "4"]).await;
    cases.push(Case {
        name: "control: 4 MB allocation under a 32 MB cap works",
        denied: o.stdout.starts_with("ALLOC_OK"),
        control: true,
        detail: o.stdout.trim().into(),
    });
    let o = run_cmd(&engine, &fsp, &m32, &b, &["alloc", "512"]).await;
    cases.push(Case {
        name: "512 MB allocation under a 32 MB memory cap",
        denied: o.stdout.contains("ALLOC_FAIL"),
        control: false,
        detail: format!("{} {:?}", o.stdout.trim(), o.error),
    });

    let o = run_cmd(
        &engine,
        &fsp,
        &manifest("appB", &[], 32, 200),
        &b,
        &["spin"],
    )
    .await;
    let stopped = o.error.as_deref().is_some_and(|e| e.contains("interrupt")) && o.ms < 1500;
    cases.push(Case {
        name: "infinite loop under a 200 ms CPU cap",
        denied: stopped,
        control: false,
        detail: format!("stopped after {} ms: {:?}", o.ms, o.error),
    });

    let quotap = probes.join("quota-probe.wasm");
    let caps = crate::fsguard::AppCaps {
        max_data_bytes: 4 << 20,
        max_entries: 60,
        max_open: 4,
        ..Default::default()
    };
    let mq = manifest("appQ", &[], 32, 4000);
    let mut quota_case =
        async |name: &'static str, args: &[&str], within: fn(&str, u64, u64) -> bool| {
            let dir = root.join(format!("quota{}", cases.len()));
            std::fs::create_dir_all(&dir).unwrap();
            let o = run_cmd_with(&engine, &quotap, &mq, &dir, args, &caps, None).await;
            let u = crate::fsguard::measure(&dir, u64::MAX, u64::MAX);
            cases.push(Case {
                name,
                denied: within(&o.stdout, u.bytes, u.entries),
                control: false,
                detail: format!(
                    "{} ({} bytes, {} entries) {:?}",
                    o.stdout.trim(),
                    u.bytes,
                    u.entries,
                    o.error
                ),
            });
        };
    quota_case(
        "data cap: renaming each file onto itself does not credit its size back",
        &["selfrename-bytes", "20", "1", "1"],
        |_, bytes, _| bytes <= 4 << 20,
    )
    .await;
    quota_case(
        "entry cap: renaming each file onto itself does not credit its entry back",
        &["selfrename-entries", "300"],
        |_, _, entries| entries <= 60,
    )
    .await;
    quota_case(
        "data cap: unlinking a hard link does not credit the file's size back",
        &["linkcycle", "30", "4", "6"],
        |_, bytes, _| bytes <= 4 << 20,
    )
    .await;
    quota_case(
        "data cap: a stream opened before a truncate is charged for what it writes",
        &["stale-stream", "20", "1"],
        |_, bytes, _| bytes <= 4 << 20,
    )
    .await;
    quota_case(
        "descriptor cap: write streams kept after their descriptor is dropped are counted",
        &["hold-streams", "40", "0"],
        |out, _, _| {
            out.starts_with("HELD ")
                && out
                    .split_whitespace()
                    .nth(1)
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|n| n <= 4)
        },
    )
    .await;
    quota_case(
        "descriptor cap: so are read streams",
        &["hold-streams", "40", "1"],
        |out, _, _| {
            out.starts_with("HELD ")
                && out
                    .split_whitespace()
                    .nth(1)
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|n| n <= 4)
        },
    )
    .await;
    let dir = root.join("quota-control");
    std::fs::create_dir_all(&dir)?;
    let o = run_cmd_with(
        &engine,
        &quotap,
        &mq,
        &dir,
        &["selfrename-bytes", "2", "1", "1"],
        &caps,
        None,
    )
    .await;
    cases.push(Case {
        name: "control: files within the caps are written and renamed",
        denied: o.stdout.starts_with("DONE 2 "),
        control: true,
        detail: o.stdout.trim().into(),
    });

    // The debug text of a failed stream, asked for by the guest, in a child process started with
    // RUST_BACKTRACE=1. With SPARED_ALLOW_HOST_BACKTRACE=1 the child captures host backtraces, and
    // the guest's text must still hold none. Without it the child captures none at all.
    let allowed = debug_text_in_child(&root, &probes, true)?;
    cases.push(Case {
        name: "control: with SPARED_ALLOW_HOST_BACKTRACE=1 and RUST_BACKTRACE=1 the host captures backtraces",
        denied: allowed.starts_with("BACKTRACES on"),
        control: true,
        detail: allowed.lines().next().unwrap_or("").into(),
    });
    cases.push(Case {
        name: "debug text of a failed stream holds no host backtrace, paths or frames (host backtraces on)",
        denied: debug_text_clean(&allowed),
        control: false,
        detail: allowed.lines().nth(1).unwrap_or("").chars().take(300).collect(),
    });
    let default = debug_text_in_child(&root, &probes, false)?;
    cases.push(Case {
        name: "RUST_BACKTRACE=1 without SPARED_ALLOW_HOST_BACKTRACE: the host captures no backtrace",
        denied: default.starts_with("BACKTRACES off") && debug_text_clean(&default),
        control: false,
        detail: default.replace('\n', " | ").chars().take(300).collect(),
    });

    let o = run_cmd(&engine, &netp, &base, &b, &["127.0.0.1:80"]).await;
    cases.push(Case {
        name: "raw TCP socket without a grant (import refused at load)",
        denied: o
            .error
            .as_deref()
            .is_some_and(|e| e.contains("refused") && e.contains("wasi:sockets")),
        control: false,
        detail: format!("{:?}", o.error),
    });
    let o = run_cmd(
        &engine,
        &netp,
        &manifest("appB", &["127.0.0.1"], 32, 500),
        &b,
        &["127.0.0.1:80"],
    )
    .await;
    cases.push(Case {
        name: "raw TCP socket even with fetch granted (sockets never grantable)",
        denied: o
            .error
            .as_deref()
            .is_some_and(|e| e.contains("refused") && e.contains("wasi:sockets")),
        control: false,
        detail: format!("{:?}", o.error),
    });
    let o = run_cmd(&engine, &httpp, &base, &b, &["http://example.com/"]).await;
    cases.push(Case {
        name: "outbound fetch without a fetch grant (import refused at load)",
        denied: o
            .error
            .as_deref()
            .is_some_and(|e| e.contains("refused") && e.contains("outgoing-handler")),
        control: false,
        detail: format!("{:?}", o.error),
    });

    let o = run_cmd(
        &engine,
        &httpp,
        &manifest("appB", &["example.com"], 64, 3000),
        &b,
        &["http://example.com/"],
    )
    .await;
    cases.push(Case {
        name: "control: fetch to a granted public host succeeds",
        denied: o.stdout.contains("STATUS 200"),
        control: true,
        detail: o.stdout.trim().to_string(),
    });

    let o = run_cmd(
        &engine,
        &httpp,
        &manifest("appB", &["example.com"], 64, 3000),
        &b,
        &["https://example.com/"],
    )
    .await;
    cases.push(Case {
        name: "control: HTTPS fetch to a granted public host verifies TLS and succeeds",
        denied: o.stdout.contains("STATUS 200"),
        control: true,
        detail: o.stdout.trim().to_string(),
    });

    // The operator gave this app an egress budget and it is spent: the same granted fetch is
    // refused before it connects, whatever the guest does.
    let spent = Arc::new(crate::quota::AppQuota::new(Some(crate::quota::Override {
        egress_bytes_per_hour: Some(1),
        ..Default::default()
    })));
    spent.egress_charge(2);
    let o = run_cmd_with(
        &engine,
        &httpp,
        &manifest("appB", &["example.com"], 64, 3000),
        &b,
        &["http://example.com/"],
        &crate::fsguard::AppCaps::default(),
        Some(spent),
    )
    .await;
    cases.push(Case {
        name: "fetch to a granted host after the app's egress budget is spent",
        denied: o.stdout.contains(" ERR ")
            && o.stdout.contains("egress_budget_exhausted")
            && !o.stdout.contains("STATUS"),
        control: false,
        detail: o.stdout.trim().to_string(),
    });

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h2 = hits.clone();
    std::thread::spawn(move || {
        loop {
            if listener.accept().is_ok() {
                h2.fetch_add(1, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    let targets: Vec<(&str, String, &str)> = vec![
        (
            "cloud metadata 169.254.169.254",
            "http://169.254.169.254/latest/meta-data/".into(),
            "169.254.169.254",
        ),
        (
            "loopback 127.0.0.1 on a live local listener",
            format!("http://127.0.0.1:{port}/"),
            "127.0.0.1",
        ),
        (
            "localhost by name",
            format!("http://localhost:{port}/"),
            "localhost",
        ),
        ("private 10.0.0.1", "http://10.0.0.1/".into(), "10.0.0.1"),
        (
            "private 192.168.1.1",
            "http://192.168.1.1/".into(),
            "192.168.1.1",
        ),
        (
            "private 172.16.0.1",
            "http://172.16.0.1/".into(),
            "172.16.0.1",
        ),
        (
            "carrier-grade NAT 100.64.0.1",
            "http://100.64.0.1/".into(),
            "100.64.0.1",
        ),
        (
            "IPv6 loopback [::1]",
            format!("http://[::1]:{port}/"),
            "[::1]",
        ),
        (
            "IPv4-mapped IPv6 metadata [::ffff:169.254.169.254]",
            "http://[::ffff:169.254.169.254]/".into(),
            "[::ffff:169.254.169.254]",
        ),
        (
            "Azure platform endpoint 168.63.129.16",
            "http://168.63.129.16/".into(),
            "168.63.129.16",
        ),
        (
            "metadata as a decimal integer 2852039166",
            "http://2852039166/".into(),
            "2852039166",
        ),
        (
            "metadata as hex 0xa9fea9fe",
            "http://0xa9fea9fe/".into(),
            "0xa9fea9fe",
        ),
        (
            "host not on the allowlist (example.com)",
            "http://example.com/".into(),
            "allowed.test",
        ),
        (
            "loopback in short form 127.1 on the live listener's port",
            format!("http://127.1:{port}/"),
            "127.1",
        ),
        (
            "0.0.0.0 on the live listener's port",
            format!("http://0.0.0.0:{port}/"),
            "0.0.0.0",
        ),
        (
            "localhost with a trailing dot",
            format!("http://localhost.:{port}/"),
            "localhost",
        ),
        (
            "LOCALHOST in upper case",
            format!("http://LOCALHOST:{port}/"),
            "localhost",
        ),
        (
            "IPv4-compatible IPv6 loopback [::127.0.0.1]",
            format!("http://[::127.0.0.1]:{port}/"),
            "[::127.0.0.1]",
        ),
        (
            "IPv4-mapped IPv6 loopback in hex [::ffff:7f00:1]",
            format!("http://[::ffff:7f00:1]:{port}/"),
            "[::ffff:7f00:1]",
        ),
        (
            "NAT64 form of the metadata address [64:ff9b::a9fe:a9fe]",
            "http://[64:ff9b::a9fe:a9fe]/".into(),
            "[64:ff9b::a9fe:a9fe]",
        ),
        (
            "userinfo naming an unlisted host before an allowed one",
            "http://evil.test:x@example.com/".into(),
            "example.com",
        ),
        (
            "userinfo in front of loopback",
            format!("http://example.com@127.0.0.1:{port}/"),
            "127.0.0.1",
        ),
    ];
    for (name, url, allow) in targets {
        let o = run_cmd(
            &engine,
            &httpp,
            &manifest("appB", &[allow], 64, 2000),
            &b,
            &[&url],
        )
        .await;
        let refused = o.stdout.contains(" ERR ") && !o.stdout.contains("STATUS");
        cases.push(Case {
            name: Box::leak(format!("fetch {name}").into_boxed_str()),
            denied: refused,
            control: false,
            detail: o.stdout.trim().to_string(),
        });
    }
    for (name, url) in [
        (
            "upper-case host matches a lower-case grant",
            "http://EXAMPLE.com/",
        ),
        (
            "trailing-dot host matches the grant",
            "http://example.com./",
        ),
    ] {
        let o = run_cmd(
            &engine,
            &httpp,
            &manifest("appB", &["example.com"], 64, 3000),
            &b,
            &[url],
        )
        .await;
        cases.push(Case {
            name: Box::leak(format!("control: {name}").into_boxed_str()),
            denied: o.stdout.contains("STATUS"),
            control: true,
            detail: o.stdout.trim().to_string(),
        });
    }
    std::thread::sleep(Duration::from_millis(50));
    let n = hits.load(Ordering::SeqCst);
    cases.push(Case {
        name: "no connection ever reached the local listener",
        denied: n == 0,
        control: false,
        detail: format!("{n} connections"),
    });

    let mut failed = 0;
    let mut rows = vec![];
    for c in &cases {
        println!(
            "{} {} :: {}",
            match (c.control, c.denied) {
                (true, true) => "CONTROL-OK  ",
                (true, false) => "CONTROL-FAIL",
                (false, true) => "DENIED      ",
                (false, false) => "HOLE/UNPROVEN",
            },
            c.name,
            c.detail
        );
        if !c.denied {
            failed += 1;
        }
        rows.push(serde_json::json!({"test": c.name, "control": c.control, "pass": c.denied, "detail": c.detail}));
    }
    let report = serde_json::json!({"total": cases.len(), "failures": failed, "cases": rows});
    std::fs::write(
        "bench/results/escape.json",
        serde_json::to_string_pretty(&report)?,
    )?;
    println!("{} cases, {} failures", cases.len(), failed);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Run `debug-text-probe` in a child of this binary with `RUST_BACKTRACE=1` and
/// `RUST_LIB_BACKTRACE=1`, and with `SPARED_ALLOW_HOST_BACKTRACE=1` when `allow`. Its stdout.
fn debug_text_in_child(root: &Path, probes: &Path, allow: bool) -> Result<String> {
    let data = root.join(format!("debug-text-{}", if allow { "on" } else { "off" }));
    std::fs::create_dir_all(&data)?;
    let mut c = std::process::Command::new(std::env::current_exe()?);
    c.arg("debug-text-probe")
        .arg("--probes")
        .arg(probes)
        .arg("--data")
        .arg(&data)
        .env("RUST_BACKTRACE", "1")
        .env("RUST_LIB_BACKTRACE", "1")
        .env_remove("SPARED_ALLOW_HOST_BACKTRACE");
    if allow {
        c.env("SPARED_ALLOW_HOST_BACKTRACE", "1");
    }
    let out = c.output()?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The guest got a failed stream and its debug text names nothing of the host.
fn debug_text_clean(out: &str) -> bool {
    let Some(text) = out.lines().find_map(|l| l.strip_prefix("DEBUG ")) else {
        return false;
    };
    let t = text.to_ascii_lowercase();
    ![
        "/home",
        "/root",
        "/rustc",
        ".rs:",
        "wasmtime",
        "backtrace",
        "cargo",
    ]
    .iter()
    .any(|bad| t.contains(bad))
}

/// The child side of [`debug_text_in_child`]: whether this process captures host backtraces, then
/// what the guest printed after its write stream hit the data cap.
pub async fn debug_text_child(probes: PathBuf, data: PathBuf) -> Result<()> {
    let engine = super::engine()?;
    crate::sandbox::start_epoch_ticker(&engine);
    let caps = crate::fsguard::AppCaps {
        max_data_bytes: 1 << 20,
        ..Default::default()
    };
    let o = run_cmd_with(
        &engine,
        &probes.join("quota-probe.wasm"),
        &manifest("appD", &[], 32, 4000),
        &data,
        &["debug-text", "4"],
        &caps,
        None,
    )
    .await;
    let on =
        std::backtrace::Backtrace::capture().status() == std::backtrace::BacktraceStatus::Captured;
    println!("BACKTRACES {}", if on { "on" } else { "off" });
    println!("{} {:?}", o.stdout.trim(), o.error);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spared-escape-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_directory_with_other_files_is_refused_and_left_alone() {
        let d = scratch("foreign");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("precious.txt"), "keep").unwrap();
        assert!(prepare_root(&d).is_err());
        assert_eq!(
            std::fs::read_to_string(d.join("precious.txt")).unwrap(),
            "keep"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_new_or_empty_directory_is_taken_and_a_previous_run_is_replaced() {
        let d = scratch("own");
        prepare_root(&d).unwrap();
        std::fs::write(d.join("left-over"), "x").unwrap();
        prepare_root(&d).unwrap();
        assert!(!d.join("left-over").exists());
        assert!(d.join(ROOT_MARKER).is_file());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
