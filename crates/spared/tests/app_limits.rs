//! Per-app limits through the real server: `spared limits set`, `show` and `clear`, who may use
//! them, how they combine with the server's caps, the format stamp they raise, that they last
//! across a restart, a redeploy and `spared rm`, and the CPU budget over time against a guest that
//! spins while a neighbor answers. Each test starts its own server on scratch ports and temp
//! directories. The disk caps against a guest that writes files are in `scripts/server-test.py`
//! (it needs a Rust guest), and the egress meter is tested against a local HTTP server in
//! `src/sandbox.rs`, since a deployed app may never reach a loopback address.

#[path = "support/fake_s3.rs"]
mod fake_s3;
#[path = "support/servers.rs"]
mod servers;

use base64::Engine as _;
use serde_json::{Value, json};
use servers::*;
use std::time::{Duration, Instant};

fn call(n: &Node, token: &str, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
    let auth = format!("Bearer {token}");
    let (st, raw) = http(
        n.admin,
        method,
        path,
        &format!("127.0.0.1:{}", n.admin),
        &[("authorization", &auth)],
        body,
    );
    (st, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

fn kind(v: &Value) -> &str {
    v["error"]["kind"].as_str().unwrap_or("")
}

/// Deploy `wat` as `name` with the manifest `extra` appended (a `[limits]` table, say).
fn deploy_with(n: &Node, name: &str, wat: &str, extra: &str) {
    let wasm = wat::parse_str(wat).unwrap();
    let manifest = format!("name = \"{name}\"\n[grants]\ndata = true\n{extra}");
    let m = base64::engine::general_purpose::STANDARD.encode(manifest);
    let auth = format!("Bearer {}", n.token);
    let (st, b) = http(
        n.admin,
        "PUT",
        &format!("/v1/apps/{name}"),
        &format!("127.0.0.1:{}", n.admin),
        &[("authorization", &auth), ("x-spared-manifest", &m)],
        &wasm,
    );
    assert_eq!(st, 200, "deploy: {}", String::from_utf8_lossy(&b));
}

fn put_limits(n: &Node, name: &str, body: Value) -> (u16, Value) {
    call(
        n,
        &n.token,
        "PUT",
        &format!("/v1/apps/{name}/limits"),
        body.to_string().as_bytes(),
    )
}

fn limits(n: &Node, name: &str) -> Value {
    let (st, v) = call(n, &n.token, "GET", &format!("/v1/apps/{name}/limits"), b"");
    assert_eq!(st, 200, "{v}");
    v
}

fn format_of(n: &Node) -> u64 {
    let s: Value =
        serde_json::from_slice(&std::fs::read(n.data.join("format.json")).unwrap()).unwrap();
    s["format"].as_u64().unwrap()
}

/// A component whose handler loops until the CPU limit stops it.
fn spinner() -> String {
    HANDLER.replace(
        "(func (export \"handle\") (param i32 i32))",
        "(func (export \"handle\") (param i32 i32) (loop $l (br $l)))",
    )
}

fn public(n: &Node, app: &str) -> (u16, String, Vec<u8>) {
    http_full(
        n.public,
        "GET",
        "/",
        &format!("{app}.localhost:{}", n.public),
        &[],
        b"",
    )
}

/// User plus system CPU time of the server process, from /proc.
fn cpu_time(n: &Node) -> Duration {
    let pid = n.child.as_ref().unwrap().id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks: u64 = f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap();
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    Duration::from_millis(ticks * 1000 / hz)
}

#[test]
fn limits_are_the_operators_and_persist_until_cleared() {
    let s = scratch("lim-admin");
    let mut n = Node::start(
        &s.0,
        "n",
        vec![
            "--max-app-data-mb".into(),
            "64".into(),
            "--max-app-entries".into(),
            "1000".into(),
        ],
        vec![],
    );
    deploy_with(&n, "a", RESPONDER, "");
    deploy_with(&n, "b", RESPONDER, "");
    assert_eq!(format_of(&n), 1, "no limits yet: format 1");

    // No limits: the server's caps, and nothing used.
    let v = limits(&n, "a");
    assert_eq!(v["override"], Value::Null, "{v}");
    assert_eq!(v["effective"]["data_mb"], 64);
    assert_eq!(v["effective"]["entries"], 1000);
    assert_eq!(v["effective"]["cpu_ms_per_min"], Value::Null);
    assert_eq!(v["app_exists"], true);

    // An override lower than a cap is in force; one higher is lowered to the cap and said so.
    let (st, v) = put_limits(
        &n,
        "a",
        json!({"data_mb": 5, "entries": 99999, "cpu_ms_per_min": 600, "egress_bytes_per_hour": 4096}),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["effective"]["data_mb"], 5);
    assert_eq!(v["effective"]["entries"], 1000, "{v}");
    assert_eq!(v["clamped"], json!(["entries"]));
    assert_eq!(v["effective"]["cpu_ms_per_min"], 600);
    assert_eq!(v["effective"]["egress_bytes_per_hour"], 4096);
    assert_eq!(format_of(&n), 2, "an override raises the format stamp");
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&n.data.join("limits")), 0o700);
    assert_eq!(mode(&n.data.join("limits/a.json")), 0o600);
    assert!(
        !n.app_data("a").join("limits.json").exists(),
        "the override is outside the app's data"
    );

    // The neighbor keeps the server's caps.
    assert_eq!(limits(&n, "b")["effective"]["data_mb"], 64);

    // ps shows the limits in force and the use to the operator only.
    let (rc, ps) = n.json(&["ps"]);
    assert_eq!(rc, 0, "{ps}");
    let a = ps["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "a")
        .unwrap()
        .clone();
    assert_eq!(a["quota"]["effective"]["data_mb"], 5, "{a}");
    assert_eq!(
        a["quota"]["overridden"],
        json!([
            "data_mb",
            "entries",
            "cpu_ms_per_min",
            "egress_bytes_per_hour"
        ])
    );
    assert!(a["quota"]["usage"]["cpu_ms_total"].is_u64(), "{a}");
    assert!(a["data_entries"].is_u64(), "{a}");

    // Bad bodies change nothing.
    for body in [
        json!({}),
        json!({"data_mb": 0}),
        json!({"data_mb": -3}),
        json!({"memory_mb": 10}),
        json!({"cpu_ms_per_min": "lots"}),
    ] {
        let (st, v) = put_limits(&n, "a", body.clone());
        assert_eq!((st, kind(&v)), (400, "bad_limits"), "{body}: {v}");
    }
    let (st, v) = call(&n, &n.token, "PUT", "/v1/apps/a/limits", b"not json");
    assert_eq!((st, kind(&v)), (400, "bad_limits"), "{v}");
    let (st, v) = call(&n, &n.token, "GET", "/v1/apps/Bad_Name/limits", b"");
    assert_eq!(st, 400, "{v}");
    assert_eq!(limits(&n, "a")["effective"]["data_mb"], 5);

    // The CLI merges what it is given into the stored override.
    let (rc, v) = n.json(&["limits", "set", "a", "--cpu-percent", "10"]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(v["override"]["cpu_ms_per_min"], 6000);
    assert_eq!(v["override"]["data_mb"], 5, "{v}");
    let (rc, v) = n.json(&["limits", "set", "a", "--egress-mb-per-hour", "0.5"]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(v["override"]["egress_bytes_per_hour"], 524288);
    for bad in [
        &["limits", "set", "a"][..],
        &["limits", "set", "a", "--cpu-percent", "101"],
        &["limits", "set", "a", "--data-mb", "0"],
        &["limits", "set", "a", "--egress-mb-per-hour", "0"],
    ] {
        let (rc, v) = n.json(bad);
        assert_eq!((rc, kind(&v)), (1, "bad_limits"), "{bad:?}: {v}");
    }
    let text = String::from_utf8(n.cli(&["limits", "show", "a"]).stdout).unwrap();
    assert!(
        text.contains("10.0% of a core") && text.contains("data      5 MB"),
        "{text}"
    );

    // A restart and a redeploy keep the override.
    n.restart();
    assert_eq!(format_of(&n), 2);
    assert_eq!(limits(&n, "a")["effective"]["data_mb"], 5);
    deploy_with(&n, "a", RESPONDER, "");
    assert_eq!(limits(&n, "a")["effective"]["cpu_ms_per_min"], 6000);

    // Removing the app keeps the operator's override for the name, and says so.
    let (rc, v) = n.json(&["rm", "a", "--yes"]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(v["limits_kept"], true, "{v}");
    let v = limits(&n, "a");
    assert_eq!(
        (v["app_exists"].clone(), v["effective"]["data_mb"].clone()),
        (json!(false), json!(5))
    );
    deploy_with(&n, "a", RESPONDER, "");
    assert_eq!(limits(&n, "a")["effective"]["data_mb"], 5);

    // Limits for a name with no app yet, so a platform sets them before the first deploy.
    let (st, _) = put_limits(&n, "later", json!({"data_mb": 1}));
    assert_eq!(st, 200);
    assert_eq!(limits(&n, "later")["app_exists"], false);

    // Clearing restores the server's caps, and the last clear puts the stamp back to 1.
    let (rc, v) = n.json(&["limits", "clear", "a"]);
    assert_eq!((rc, v["cleared"].clone()), (0, json!(true)), "{v}");
    assert_eq!(v["effective"]["data_mb"], 64);
    assert_eq!(v["effective"]["cpu_ms_per_min"], Value::Null);
    assert_eq!(format_of(&n), 2, "another app still has limits");
    let (rc, v) = n.json(&["limits", "clear", "later"]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(format_of(&n), 1);
    let (rc, v) = n.json(&["limits", "clear", "later"]);
    assert_eq!((rc, v["cleared"].clone()), (0, json!(false)));
    assert!(!n.data.join("limits/a.json").exists());

    // A stamp left at 2 with no override (a crash between the two writes) goes back to 1.
    std::fs::write(
        n.data.join("format.json"),
        r#"{"format":2,"created_by":"0.0.1-preview","last_opened_by":"0.0.1-preview"}"#,
    )
    .unwrap();
    n.restart();
    assert_eq!(format_of(&n), 1);
}

#[test]
fn no_app_token_reaches_the_limits_of_any_app() {
    let s = scratch("lim-tokens");
    let n = Node::start(&s.0, "n", vec![], vec![]);
    deploy_with(&n, "a", RESPONDER, "");
    deploy_with(&n, "b", RESPONDER, "");
    let (st, _) = put_limits(&n, "a", json!({"data_mb": 7}));
    assert_eq!(st, 200);
    let all = "deploy,read,evict,backup,restore,freeze,rm";
    let (st, minted) = call(
        &n,
        &n.token,
        "POST",
        "/v1/tokens",
        json!({"app": "a", "scopes": all.split(',').collect::<Vec<_>>()})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(st, 201, "{minted}");
    let tok = minted["token"].as_str().unwrap().to_string();
    let before = std::fs::read(n.data.join("limits/a.json")).unwrap();
    for app in ["a", "b", "nothing"] {
        for (m, body) in [
            ("GET", &b""[..]),
            ("PUT", br#"{"data_mb": 999}"#),
            ("DELETE", b""),
            ("POST", b"{}"),
        ] {
            let (st, v) = call(&n, &tok, m, &format!("/v1/apps/{app}/limits"), body);
            assert_eq!(
                (st, kind(&v)),
                (403, "wrong_scope"),
                "{m} limits of {app}: {v}"
            );
        }
    }
    assert_eq!(
        std::fs::read(n.data.join("limits/a.json")).unwrap(),
        before,
        "nothing changed"
    );
    // The token's own app listing never carries the limits, nor does its deploy answer.
    let (st, v) = call(&n, &tok, "GET", "/v1/apps/a", b"");
    assert_eq!(st, 200);
    assert!(v.get("quota").is_none(), "{v}");
    let (st, v) = call(&n, &tok, "GET", "/v1/apps", b"");
    assert_eq!(st, 200);
    assert!(v["apps"][0].get("quota").is_none(), "{v}");
    let (st, v) = call(&n, &n.token, "GET", "/v1/apps/a", b"");
    assert_eq!(st, 200);
    assert_eq!(v["quota"]["effective"]["data_mb"], 7, "{v}");
    // The CLI says the admin token is needed.
    let o = n
        .cli_cmd(&["--json", "limits", "show", "a"])
        .env("SPARED_TOKEN", &tok)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(kind(&v), "wrong_scope", "{v}");
    assert!(
        v["error"]["fix"].as_str().unwrap().contains("admin token"),
        "{v}"
    );
    // A deploy by the token cannot shed or raise the limits: there is no manifest key for them.
    let wasm = wat::parse_str(RESPONDER).unwrap();
    let m = base64::engine::general_purpose::STANDARD
        .encode("name = \"a\"\n[grants]\ndata = true\n[limits]\ndata_mb = 5000\n");
    let auth = format!("Bearer {tok}");
    let (st, _) = http(
        n.admin,
        "PUT",
        "/v1/apps/a",
        &format!("127.0.0.1:{}", n.admin),
        &[("authorization", &auth), ("x-spared-manifest", &m)],
        &wasm,
    );
    assert_eq!(st, 400, "an unknown [limits] key is refused");
    // rm by the token removes the app, and the operator's limits stay.
    let (st, v) = call(&n, &tok, "DELETE", "/v1/apps/a", b"");
    assert_eq!(st, 200, "{v}");
    assert_eq!(limits(&n, "a")["effective"]["data_mb"], 7);
}

#[test]
fn a_cpu_budget_throttles_a_spinning_app_and_not_its_neighbor() {
    let s = scratch("lim-cpu");
    let n = Node::start(&s.0, "n", vec!["--access-log".into(), "off".into()], vec![]);
    deploy_with(
        &n,
        "spin",
        &spinner(),
        "[limits]\ncpu_ms = 100\ndeadline_ms = 5000\n",
    );
    deploy_with(&n, "calm", RESPONDER, "");

    // Without a budget every request runs until its own cpu_ms stops it.
    for _ in 0..3 {
        let (st, _, body) = public(&n, "spin");
        assert_eq!(st, 508, "{}", String::from_utf8_lossy(&body));
    }

    // 10% of a core: 100 ms a second, and a bucket of 10 s of that (1000 ms).
    let (rc, v) = n.json(&["limits", "set", "spin", "--cpu-percent", "10"]);
    assert_eq!(rc, 0, "{v}");

    let calm = |stop: std::sync::Arc<std::sync::atomic::AtomicBool>| {
        let port = n.public;
        std::thread::spawn(move || {
            let mut lat = vec![];
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let t = Instant::now();
                let (st, _) = http(
                    port,
                    "GET",
                    "/",
                    &format!("calm.localhost:{port}"),
                    &[],
                    b"",
                );
                assert_eq!(st, 200);
                lat.push(t.elapsed());
                std::thread::sleep(Duration::from_millis(20));
            }
            lat.sort();
            lat
        })
    };
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let quiet = calm(stop.clone());
    std::thread::sleep(Duration::from_millis(1500));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let quiet = quiet.join().unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let busy = calm(stop.clone());
    let cpu0 = cpu_time(&n);
    let window = Duration::from_secs(6);
    let t0 = Instant::now();
    let (mut ran, mut refused, mut retry_after) = (0u32, 0u32, 0u64);
    while t0.elapsed() < window {
        let (st, head, body) = public(&n, "spin");
        let text = String::from_utf8_lossy(&body).to_string();
        match st {
            508 => ran += 1,
            503 => {
                assert!(text.contains("\"kind\":\"cpu_budget_exhausted\""), "{text}");
                assert!(
                    !text.contains("10") && !text.contains("6000"),
                    "no figure: {text}"
                );
                let ra = head
                    .lines()
                    .find_map(|l| l.strip_prefix("retry-after: "))
                    .expect("Retry-After")
                    .trim()
                    .parse::<u64>()
                    .unwrap();
                assert!((1..=11).contains(&ra), "{ra}");
                retry_after = ra;
                refused += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            other => panic!("{other}: {text}"),
        }
    }
    let used = cpu_time(&n) - cpu0;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let busy = busy.join().unwrap();

    // Unthrottled, 6 s of back-to-back 100 ms requests is about 55 of them and 6 s of CPU. With
    // the budget it is the 1000 ms bucket plus 100 ms a second: about 16, and well under 3 s.
    eprintln!(
        "ran {ran}, refused {refused}, server cpu {used:?}, calm p50 {:?} -> {:?}, max {:?}",
        quiet[quiet.len() / 2],
        busy[busy.len() / 2],
        busy.last().unwrap()
    );
    assert!(
        (8..=28).contains(&ran),
        "ran {ran} spinning requests in 6 s"
    );
    assert!(refused > 0);
    assert!(
        used < Duration::from_millis(3500),
        "the server used {used:?} of CPU in 6 s"
    );
    let (p50_quiet, p50_busy) = (quiet[quiet.len() / 2], busy[busy.len() / 2]);
    assert!(
        p50_busy < p50_quiet + Duration::from_millis(25),
        "the neighbor's p50 went from {p50_quiet:?} to {p50_busy:?}"
    );

    let v = limits(&n, "spin");
    let u = &v["usage"];
    assert!(u["cpu_refused"].as_u64().unwrap() >= refused as u64, "{v}");
    assert!(
        u["cpu_ms_total"].as_u64().unwrap() >= 100 * (ran as u64 + 3) - 50,
        "each request ran to its cpu_ms: {v}"
    );
    assert_eq!(u["cpu_ms_left"], 0, "{v}");

    // It refills: after the Retry-After it was given, a request runs again (to its own cpu_ms).
    std::thread::sleep(Duration::from_secs(retry_after) + Duration::from_millis(300));
    let (st, _, body) = public(&n, "spin");
    assert_eq!(st, 508, "{}", String::from_utf8_lossy(&body));

    // A restart starts the counters over; the budget stays.
    let mut n = n;
    n.restart();
    let (st, _, _) = public(&n, "spin");
    assert_eq!(st, 508);
    assert_eq!(limits(&n, "spin")["effective"]["cpu_ms_per_min"], 6000);

    // Clearing the budget lets every request in again.
    for _ in 0..12 {
        public(&n, "spin");
    }
    let (rc, _) = n.json(&["limits", "clear", "spin"]);
    assert_eq!(rc, 0);
    for _ in 0..3 {
        let (st, _, _) = public(&n, "spin");
        assert_eq!(st, 508);
    }
}

#[test]
fn a_damaged_limits_file_keeps_its_app_out_until_the_operator_fixes_it() {
    let s = scratch("lim-damaged");
    let mut n = Node::start(&s.0, "n", vec![], vec![]);
    deploy_with(&n, "a", RESPONDER, "");
    deploy_with(&n, "b", RESPONDER, "");
    let (st, _) = put_limits(&n, "a", json!({"data_mb": 3}));
    assert_eq!(st, 200);
    n.kill9();
    std::fs::write(n.data.join("limits/a.json"), "{not json").unwrap();
    n.spawn();
    assert!(
        n.log_text().contains("its limits file cannot be read"),
        "{}",
        n.log_text()
    );
    // Fail closed: the app is not served without the limits its operator set. Its neighbor is.
    assert_eq!(public(&n, "a").0, 404);
    assert_eq!(public(&n, "b").0, 200);
    let v = limits(&n, "a");
    assert!(v["damaged"].is_string(), "{v}");
    // Setting the limits again loads it.
    let (st, v) = put_limits(&n, "a", json!({"data_mb": 4}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(public(&n, "a").0, 200);
    assert_eq!(limits(&n, "a")["effective"]["data_mb"], 4);
}
