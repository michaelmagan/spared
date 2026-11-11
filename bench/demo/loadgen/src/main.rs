use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Spared,
    Workerd,
    Spin,
    Celld,
    Native,
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn target(style: Style, k: usize, path: &str) -> (String, Option<String>) {
    match style {
        Style::Spared => (path.to_string(), Some(format!("demo-{k}.localhost"))),
        Style::Native => (path.to_string(), None),
        Style::Workerd | Style::Spin | Style::Celld => {
            let sep = if path.contains('?') { '&' } else { '?' };
            (format!("{path}{sep}id={k}"), None)
        }
    }
}

struct Sample {
    first_us: u64,
    full_us: u64,
    status: u16,
}

async fn connect(addr: &str) -> hyper::client::conn::http1::SendRequest<Full<Bytes>> {
    let s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    s.set_nodelay(true).ok();
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s)).await.expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
}

async fn one(
    sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    style: Style,
    app: usize,
    post: bool,
    seq: usize,
) -> Sample {
    let (path, host) = target(style, app, if post { "/api/items" } else { "/" });
    let mut b = Request::builder().method(if post { "POST" } else { "GET" }).uri(path);
    let default_host = match style {
        Style::Spin => "127.0.0.1:9191",
        Style::Celld => "127.0.0.1:9292",
        Style::Native => "localhost",
        _ => "127.0.0.1:9090",
    };
    b = b.header("host", host.unwrap_or_else(|| default_host.to_string()));
    let body = if post {
        b = b.header("content-type", "application/json");
        Full::new(Bytes::from(format!("{{\"text\":\"load {seq}\"}}")))
    } else {
        Full::new(Bytes::new())
    };
    let req = b.body(body).unwrap();
    sender.ready().await.ok();
    let t0 = Instant::now();
    match sender.send_request(req).await {
        Ok(res) => {
            let first = t0.elapsed();
            let status = res.status().as_u16();
            let ok = res.into_body().collect().await.is_ok();
            let full = t0.elapsed();
            Sample { first_us: first.as_micros() as u64, full_us: full.as_micros() as u64, status: if ok { status } else { 0 } }
        }
        Err(_) => Sample { first_us: 0, full_us: 0, status: 0 },
    }
}

// Spawn the command after `--` (pinned to --spawn-cpu, default 0), connect to --addr as soon as it accepts, GET /, and time the
// first byte from the spawn. The child is killed afterwards unless --keep is given (for `docker run -d`, which exits by itself).
async fn spawn_first(args: &[String], addr: &str, style: Style) -> Sample {
    use std::os::unix::process::CommandExt;
    let cmd = &args[args.iter().position(|a| a == "--").expect("spawn needs -- CMD ...") + 1..];
    let cpu: usize = arg(args, "--spawn-cpu").map(|v| v.parse().unwrap()).unwrap_or(0);
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe {
        c.pre_exec(move || {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(cpu, &mut set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
            Ok(())
        });
    }
    let t0 = Instant::now();
    let mut child = c.spawn().expect("spawn");
    let stream = loop {
        if let Ok(s) = tokio::net::TcpStream::connect(addr).await {
            break s;
        }
        if t0.elapsed().as_secs() > 30 {
            panic!("no listener on {addr} after 30 s");
        }
        std::thread::sleep(std::time::Duration::from_micros(50));
    };
    stream.set_nodelay(true).ok();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let (path, host) = target(style, 0, "/");
    let req = Request::builder().uri(path).header("host", host.unwrap_or_else(|| "localhost".into())).body(Full::new(Bytes::new())).unwrap();
    let s = match sender.send_request(req).await {
        Ok(res) => {
            let first = t0.elapsed();
            let status = res.status().as_u16();
            let ok = res.into_body().collect().await.is_ok();
            Sample { first_us: first.as_micros() as u64, full_us: t0.elapsed().as_micros() as u64, status: if ok { status } else { 0 } }
        }
        Err(_) => Sample { first_us: 0, full_us: 0, status: 0 },
    };
    if !args.iter().any(|a| a == "--keep") {
        child.kill().ok();
    }
    child.wait().ok();
    s
}

fn stats(mut v: Vec<u64>) -> serde_json::Value {
    v.sort_unstable();
    if v.is_empty() {
        return json!({});
    }
    let q = |p: f64| v[(((v.len() as f64) * p).ceil() as usize).clamp(1, v.len()) - 1];
    json!({"p50": q(0.5), "p99": q(0.99), "max": v[v.len() - 1], "mean": v.iter().sum::<u64>() / v.len() as u64})
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = arg(&args, "--mode").expect("--mode cold|warm|mixed");
    let style = match arg(&args, "--style").expect("--style spared|workerd|spin").as_str() {
        "spared" => Style::Spared,
        "spin" => Style::Spin,
        "celld" => Style::Celld,
        "native" => Style::Native,
        _ => Style::Workerd,
    };
    let addr = arg(&args, "--addr").unwrap_or_else(|| match style {
        Style::Spared => "127.0.0.1:8080".into(),
        Style::Spin => "127.0.0.1:9191".into(),
        Style::Celld => "127.0.0.1:9292".into(),
        Style::Native => "127.0.0.1:8090".into(),
        Style::Workerd => "127.0.0.1:9090".into(),
    });
    let n: usize = arg(&args, "--n").map(|v| v.parse().unwrap()).unwrap_or(100);
    let conc: usize = arg(&args, "--conc").map(|v| v.parse().unwrap()).unwrap_or(1);
    let total: usize = arg(&args, "--requests").map(|v| v.parse().unwrap()).unwrap_or(20000);
    let app: usize = arg(&args, "--app").map(|v| v.parse().unwrap()).unwrap_or(0);
    let out = arg(&args, "--out");

    let mut samples: Vec<Sample> = Vec::new();
    let wall = Instant::now();
    let mut detail = json!(null);
    match mode.as_str() {
        "cold" | "touch" => {
            let mut sender = connect(&addr).await;
            for k in 0..n {
                samples.push(one(&mut sender, style, k, false, k).await);
            }
            if mode == "cold" {
                detail = json!({"per_request_first_us": samples.iter().map(|s| s.first_us).collect::<Vec<_>>(), "per_request_full_us": samples.iter().map(|s| s.full_us).collect::<Vec<_>>()});
            }
        }
        "warm" | "mixed" => {
            let mut senders = Vec::new();
            for _ in 0..conc {
                senders.push(connect(&addr).await);
            }
            for s in senders.iter_mut() {
                for i in 0..200 {
                    one(s, style, app, false, i).await;
                }
            }
            let wall2 = Instant::now();
            let counter = Arc::new(AtomicUsize::new(0));
            let mixed = mode == "mixed";
            let mut tasks = Vec::new();
            for mut sender in senders {
                let counter = counter.clone();
                tasks.push(tokio::spawn(async move {
                    let mut local = Vec::new();
                    loop {
                        let i = counter.fetch_add(1, Ordering::Relaxed);
                        if i >= total {
                            break;
                        }
                        local.push(one(&mut sender, style, app, mixed && i % 10 == 9, i).await);
                    }
                    local
                }));
            }
            for t in tasks {
                samples.extend(t.await.unwrap());
            }
            detail = json!({"measured_wall_s": wall2.elapsed().as_secs_f64()});
        }
        "spawn" => {
            let sample = spawn_first(&args, &addr, style).await;
            detail = json!({"first_byte_from_spawn_us": sample.first_us, "full_body_from_spawn_us": sample.full_us});
            samples.push(sample);
        }
        _ => panic!("bad mode"),
    }
    let wall_s = wall.elapsed().as_secs_f64();
    let errors = samples.iter().filter(|s| !(200..300).contains(&s.status)).count();
    let good: Vec<&Sample> = samples.iter().filter(|s| (200..300).contains(&s.status)).collect();
    let res = json!({
        "mode": mode, "style": match style { Style::Spared => "spared", Style::Spin => "spin", Style::Celld => "celld", Style::Native => "native", Style::Workerd => "workerd" },
        "conc": conc, "requests": samples.len(), "errors": errors, "wall_s": wall_s,
        "first_byte_us": stats(good.iter().map(|s| s.first_us).collect()),
        "full_body_us": stats(good.iter().map(|s| s.full_us).collect()),
        "detail": detail,
    });
    let text = serde_json::to_string(&res).unwrap();
    match out {
        Some(p) => std::fs::write(p, text + "\n").unwrap(),
        None => println!("{text}"),
    }
}
