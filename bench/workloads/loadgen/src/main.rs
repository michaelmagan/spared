//! Sequential (concurrency 1) GET load generator (POST with a body of `--post-bytes` bytes when given) for bench/workloads/run.sh. One keep-alive connection, `--warmup` requests
//! discarded, then `--requests` timed. Server CPU time over the timed phase comes from /proc/<pid>/task/*/schedstat
//! (nanoseconds on CPU, all threads) for every pid in `--pids`.
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::time::Instant;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn cpu_ns(pids: &[u32]) -> u64 {
    let mut total = 0;
    for pid in pids {
        let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for t in rd.flatten() {
            if let Ok(s) = std::fs::read_to_string(t.path().join("schedstat")) {
                total += s.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            }
        }
    }
    total
}

fn stats(mut v: Vec<u64>) -> serde_json::Value {
    v.sort_unstable();
    if v.is_empty() {
        return json!({});
    }
    let q = |p: f64| v[(((v.len() as f64) * p).ceil() as usize).clamp(1, v.len()) - 1];
    json!({"p50": q(0.5), "p90": q(0.9), "p99": q(0.99), "min": v[0], "max": v[v.len() - 1], "mean": v.iter().sum::<u64>() / v.len() as u64})
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = arg(&args, "--addr").expect("--addr host:port");
    let host = arg(&args, "--host").unwrap_or_else(|| addr.clone());
    let path = arg(&args, "--path").expect("--path");
    let warmup: usize = arg(&args, "--warmup").map(|v| v.parse().unwrap()).unwrap_or(20);
    let total: usize = arg(&args, "--requests").map(|v| v.parse().unwrap()).unwrap_or(200);
    let pids: Vec<u32> = arg(&args, "--pids").map(|v| v.split(',').filter(|s| !s.is_empty()).map(|s| s.parse().unwrap()).collect()).unwrap_or_default();
    let out = arg(&args, "--out");
    let post_bytes: Option<usize> = arg(&args, "--post-bytes").map(|v| v.parse().unwrap());
    let payload = Bytes::from(vec![b'x'; post_bytes.unwrap_or(0)]);

    let s = tokio::net::TcpStream::connect(&addr).await.expect("connect");
    s.set_nodelay(true).ok();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s)).await.expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut body_len = None;
    let mut errors = 0usize;
    let mut changed_len = 0usize;
    let mut full = Vec::with_capacity(total);
    let mut first = Vec::with_capacity(total);
    let mut cpu0 = 0;
    let mut wall0 = Instant::now();
    for i in 0..warmup + total {
        if i == warmup {
            cpu0 = cpu_ns(&pids);
            wall0 = Instant::now();
        }
        let method = if post_bytes.is_some() { "POST" } else { "GET" };
        let req = Request::builder().method(method).uri(&path).header("host", &host).body(Full::new(payload.clone())).unwrap();
        sender.ready().await.expect("ready");
        let t0 = Instant::now();
        let res = sender.send_request(req).await.expect("send");
        let fb = t0.elapsed();
        let ok = res.status().is_success();
        let body = res.into_body().collect().await.expect("body").to_bytes();
        let fu = t0.elapsed();
        if !ok {
            errors += 1;
        }
        match body_len {
            None => body_len = Some(body.len()),
            Some(l) if l != body.len() => changed_len += 1,
            _ => {}
        }
        if i >= warmup {
            first.push(fb.as_micros() as u64);
            full.push(fu.as_micros() as u64);
        }
    }
    let wall = wall0.elapsed().as_secs_f64();
    let cpu = cpu_ns(&pids).saturating_sub(cpu0);
    let res = json!({
        "path": path, "host": host, "warmup": warmup, "requests": total, "errors": errors,
        "body_len": body_len, "body_len_changed": changed_len, "wall_s": wall,
        "full_body_us": stats(full), "first_byte_us": stats(first),
        "server_cpu_us_per_request": if pids.is_empty() { serde_json::Value::Null } else { json!(cpu / 1000 / total as u64) },
        "server_pids": pids,
    });
    let text = serde_json::to_string(&res).unwrap();
    match out {
        Some(p) => std::fs::write(p, text + "\n").unwrap(),
        None => println!("{text}"),
    }
}
