//! EXPERIMENTAL `runtime = "wasip3-experimental"` through the real `spared serve`: the operator
//! flag, the refusal of data apps, requests that overlap in one instance, and what `spared ps`
//! says. The budgets, a trap, a deadline and draining are tested inside the server
//! (`src/server/wasip3_tests.rs`), where a test may reach a loopback upstream.

#[path = "support/fake_s3.rs"]
mod fake_s3;
#[path = "support/p3_probe.rs"]
mod p3_probe;
#[path = "support/servers.rs"]
mod servers;

use base64::Engine as _;
use serde_json::Value;
use servers::*;
use std::time::{Duration, Instant};

fn deploy(n: &Node, name: &str, manifest: &str) -> (u16, Value) {
    let m = base64::engine::general_purpose::STANDARD.encode(manifest);
    let auth = format!("Bearer {}", n.token);
    let (st, b) = http(
        n.admin,
        "PUT",
        &format!("/v1/apps/{name}"),
        &format!("127.0.0.1:{}", n.admin),
        &[("authorization", &auth), ("x-spared-manifest", &m)],
        &p3_probe::p3_probe(),
    );
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn p3_manifest(name: &str, data: bool) -> String {
    format!(
        "name = \"{name}\"\nruntime = \"wasip3-experimental\"\n[grants]\ndata = {data}\nfetch = [\"example.com\"]\n"
    )
}

fn get(n: &Node, app: &str, path: &str) -> (u16, String) {
    let (st, b) = http(
        n.public,
        "GET",
        path,
        &format!("{app}.localhost:{}", n.public),
        &[],
        b"",
    );
    (st, String::from_utf8_lossy(&b).to_string())
}

#[test]
fn without_the_operator_flag_a_wasip3_app_is_refused_with_the_flag_named() {
    let s = scratch("wasip3-off");
    let n = Node::start(&s.0, "a", vec![], vec![]);
    let (st, v) = deploy(&n, "p3", &p3_manifest("p3", false));
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"]["kind"], "experimental_runtime_off", "{v}");
    assert!(
        v["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("--experimental-wasip3"),
        "{v}"
    );
    assert_eq!(n.admin("GET", "/v1/apps/p3", b"").0, 404);
}

#[test]
fn a_wasip3_app_with_data_is_refused() {
    let s = scratch("wasip3-data");
    let n = Node::start(&s.0, "a", vec!["--experimental-wasip3".into()], vec![]);
    let (st, v) = deploy(&n, "p3", &p3_manifest("p3", true));
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["error"]["kind"], "wasip3_data_refused", "{v}");
}

#[test]
fn requests_overlap_in_one_instance_and_ps_says_experimental() {
    let s = scratch("wasip3-on");
    let n = Node::start(
        &s.0,
        "a",
        vec![
            "--experimental-wasip3".into(),
            "--max-app-concurrency".into(),
            "8".into(),
        ],
        vec![],
    );
    let (st, v) = deploy(
        &n,
        "p3",
        &format!("{}[limits]\nconcurrency = 8\n", p3_manifest("p3", false)),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["runtime"], "wasip3-experimental", "{v}");
    assert_eq!(v["experimental"], true, "{v}");
    assert_eq!(v["limits"]["concurrency"], 8, "{v}");
    assert_eq!(get(&n, "p3", "/").0, 200);
    let started = Instant::now();
    let port = n.public;
    let workers: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                http(
                    port,
                    "GET",
                    "/sleep?ms=200",
                    &format!("p3.localhost:{port}"),
                    &[],
                    b"",
                )
            })
        })
        .collect();
    for w in workers {
        let (st, b) = w.join().unwrap();
        assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
    }
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(600),
        "8 requests of 200 ms took {took:?}"
    );
    assert_eq!(get(&n, "p3", "/stats").1.trim(), "peak 8");

    let out = n.cli(&["ps"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    let row = text.lines().find(|l| l.starts_with("p3 ")).unwrap();
    assert!(row.contains("EXPERIMENTAL wasip3"), "{text}");
    assert!(
        text.contains("one crash or timeout fails every request"),
        "{text}"
    );
}

#[test]
fn a_restart_without_the_flag_does_not_serve_a_wasip3_app() {
    let s = scratch("wasip3-restart");
    let mut n = Node::start(&s.0, "a", vec!["--experimental-wasip3".into()], vec![]);
    assert_eq!(deploy(&n, "p3", &p3_manifest("p3", false)).0, 200);
    assert_eq!(get(&n, "p3", "/").0, 200);
    n.kill9();
    n.args.clear();
    n.spawn();
    assert_eq!(get(&n, "p3", "/").0, 404);
    assert!(
        n.log_text().contains("--experimental-wasip3"),
        "{}",
        n.log_text()
    );
}
