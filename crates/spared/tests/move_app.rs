//! `spared freeze`, `spared thaw`, advisory epochs and `spared move`, through real servers that
//! back up to a fake S3 running in this test process (`support/fake_s3.rs`). Each test starts its
//! own fake and servers on scratch ports and temp directories, with its own `$HOME`.

#[path = "support/fake_s3.rs"]
mod fake_s3;
#[path = "support/servers.rs"]
mod servers;

use fake_s3::Fake;
use serde_json::{Value, json};
use servers::*;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

/// A token file only its owner can read, holding `n`'s admin token.
fn token_file(s: &Scratch, n: &Node, tag: &str) -> PathBuf {
    use std::os::unix::fs::OpenOptionsExt;
    let p = s.0.join(format!("{tag}-token"));
    let _ = std::fs::remove_file(&p);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&p)
        .unwrap();
    use std::io::Write;
    writeln!(f, "{}", n.token).unwrap();
    p
}

fn move_args<'a>(dst: &Node, tf: &'a Path, extra: &[&'a str]) -> Vec<String> {
    let mut a = vec![
        "--to".to_string(),
        format!("http://127.0.0.1:{}", dst.admin),
        "--to-token-file".into(),
        tf.display().to_string(),
    ];
    a.extend(extra.iter().map(|s| s.to_string()));
    a
}

/// `spared --json move <app>` from `src` to `dst`: exit code and JSON.
fn mv(src: &Node, dst: &Node, tf: &Path, app: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["move".to_string(), app.to_string()];
    args.extend(move_args(dst, tf, extra));
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    src.json(&a)
}

/// The same in text mode: the whole output.
fn mv_text(src: &Node, dst: &Node, tf: &Path, app: &str, extra: &[&str]) -> Output {
    let mut args = vec!["move".to_string(), app.to_string()];
    args.extend(move_args(dst, tf, extra));
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    src.cli(&a)
}

/// A public request to `app` on `n`: status and response head.
fn public(n: &Node, app: &str) -> (u16, String) {
    let (st, head, _) = http_full(n.public, "GET", "/", &format!("{app}.localhost"), &[], b"");
    (st, head)
}

fn app_info(n: &Node, app: &str) -> Value {
    let (st, v) = n.admin("GET", &format!("/v1/apps/{app}"), b"");
    assert_eq!(st, 200, "{v}");
    v
}

/// Data with a few shapes in it: a database-sized file, a nested file, an empty one.
fn write_data(data: &Path, seed: u8) {
    std::fs::create_dir_all(data.join("sub")).unwrap();
    let db: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ seed)
        .collect();
    std::fs::write(data.join("app.db"), db).unwrap();
    std::fs::write(
        data.join("sub/notes.txt"),
        format!("written by seed {seed}"),
    )
    .unwrap();
    std::fs::write(data.join("empty"), b"").unwrap();
}

/// Two servers on one bucket and one key.
fn pair(s: &Scratch, fake: &Fake, key: Option<&Path>) -> (Node, Node) {
    let a = Node::start(&s.0, "a", remote_args(fake, key), creds());
    let b = Node::start(&s.0, "b", remote_args(fake, key), creds());
    (a, b)
}

/// Everything under both data directories but the request logs, which grow with every request.
fn snapshot(nodes: &[&Node]) -> Vec<(String, Option<Vec<u8>>)> {
    nodes
        .iter()
        .flat_map(|n| {
            tree(&n.data)
                .into_iter()
                .filter(|(p, _)| !p.ends_with("app.log") && !p.ends_with(".lock"))
                .map(|(p, b)| (format!("{}/{p}", n.data.display()), b))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn the_responder_component_answers_200() {
    let s = scratch("move-responder");
    let n = Node::start(&s.0, "n", vec![], vec![]);
    n.deploy_wat("hello", RESPONDER);
    assert_eq!(public(&n, "hello").0, 200, "{}", n.log_text());
}

#[test]
fn a_move_carries_the_data_byte_for_byte_and_the_app_serves_on_the_target() {
    let s = scratch("move-full");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, b) = pair(&s, &fake, Some(&key));
    a.deploy_wat("notes", RESPONDER);
    write_data(&a.app_data("notes"), 1);
    let original = tree(&a.app_data("notes"));
    assert_eq!(public(&a, "notes").0, 200);
    let tf = token_file(&s, &b, "b");

    // A dry run checks both servers, prints the plan and changes nothing anywhere.
    let before = snapshot(&[&a, &b]);
    let keys = fake.keys();
    let (rc, plan) = mv(&a, &b, &tf, "notes", &["--dry-run"]);
    assert_eq!(rc, 0, "{plan}");
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["plan"].as_array().unwrap().len(), 6, "{plan}");
    let o = mv_text(&a, &b, &tf, "notes", &["--dry-run"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("plan:") && out.contains("dry run: nothing was changed"),
        "{out}"
    );
    assert_eq!(snapshot(&[&a, &b]), before, "a dry run changed a data dir");
    assert_eq!(fake.keys(), keys, "a dry run wrote to the bucket");
    assert_eq!(public(&a, "notes").0, 200);

    // The move.
    let smoke = format!("http://127.0.0.1:{}", b.public);
    let o = mv_text(&a, &b, &tf, "notes", &["--smoke-url", &smoke]);
    let (out, errs) = (
        String::from_utf8_lossy(&o.stdout).to_string(),
        String::from_utf8_lossy(&o.stderr).to_string(),
    );
    assert_eq!(o.status.code(), Some(0), "{out}\n{errs}\n{}", b.log_text());
    for want in [
        "move: copied the code of notes",
        "move: froze notes on the source",
        "move: restored notes on the target",
        "move: thawed notes on the target",
        "with Host notes.localhost: 200",
        "next: point DNS",
        "spared rm notes --yes --server",
        "undo: before any client writes to the target",
    ] {
        assert!(out.contains(want), "wanted {want:?} in:\n{out}");
    }

    // The data is the same, byte for byte, and the app serves on the target.
    assert_eq!(
        tree(&b.app_data("notes")),
        original,
        "the moved data differs"
    );
    assert_eq!(public(&b, "notes").0, 200);
    let t = app_info(&b, "notes");
    assert_eq!(
        (t["frozen"].as_bool(), t["epoch"].as_u64()),
        (Some(false), Some(2)),
        "{t}"
    );
    assert!(
        t["restored_from_remote"]["remote_id"]
            .as_str()
            .unwrap()
            .ends_with(".1"),
        "{t}"
    );
    // The source is frozen, says so, and keeps its app and data.
    let (st, head) = public(&a, "notes");
    assert_eq!(st, 503, "{head}");
    assert!(head.contains("retry-after: 30"), "{head}");
    let src = app_info(&a, "notes");
    assert_eq!(src["state"], "frozen", "{src}");
    assert_eq!(src["freeze"]["kind"], "move", "{src}");
    assert_eq!(tree(&a.app_data("notes")), original);
    let o = a.cli(&["ps"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("frozen"));

    // Run again: it sees the move was done and changes nothing.
    let before = snapshot(&[&a, &b]);
    let (rc, v) = mv(&a, &b, &tf, "notes", &[]);
    assert_eq!((rc, v["already_moved"].as_bool()), (0, Some(true)), "{v}");
    assert_eq!(snapshot(&[&a, &b]), before);

    // No secret shows up in the output, the server logs, or either data directory (other than
    // each server's own token file).
    let key_hex = std::fs::read_to_string(&key).unwrap().trim().to_string();
    let (_, json_out) = mv(&a, &b, &tf, "notes", &["--dry-run"]);
    let all = format!("{out}{errs}{v}{json_out}{}{}", a.log_text(), b.log_text());
    let secrets = [
        a.token.as_str(),
        b.token.as_str(),
        fake_s3::SECRET_KEY,
        fake_s3::SESSION_TOKEN,
        key_hex.as_str(),
    ];
    for secret in secrets {
        assert!(
            !all.contains(secret),
            "a secret appears in the output or a log"
        );
    }
    for n in [&a, &b] {
        for p in walk_files(&n.data) {
            if p.file_name().is_some_and(|f| f == "admin-token") {
                continue;
            }
            let bytes = std::fs::read(&p).unwrap_or_default();
            for secret in secrets {
                assert!(
                    !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                    "a secret is stored in {}",
                    p.display()
                );
            }
        }
    }
}

#[test]
fn a_frozen_app_answers_503_across_a_restart_and_admin_commands_still_work() {
    let s = scratch("move-frozen");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let mut n = Node::start(&s.0, "n", remote_args(&fake, Some(&key)), creds());
    n.deploy_wat("notes", RESPONDER);
    write_data(&n.app_data("notes"), 3);
    assert_eq!(public(&n, "notes").0, 200);
    let (rc, v) = n.json(&["backup", "notes"]);
    assert_eq!(rc, 0, "{v}");
    let first = v["backup_id"].as_str().unwrap().to_string();

    let (rc, v) = n.json(&["freeze", "notes", "--reason", "maintenance"]);
    assert_eq!((rc, v["frozen"].as_bool()), (0, Some(true)), "{v}");
    let check_frozen = |n: &Node| {
        let (st, head, body) = http_full(n.public, "POST", "/items", "notes.localhost", &[], b"{}");
        assert_eq!(st, 503, "{head}");
        assert!(head.contains("retry-after: 30"), "{head}");
        let e: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(e["error"]["kind"], "app_frozen", "{e}");
        assert!(
            !e.to_string().contains("maintenance"),
            "the reason is for the operator only"
        );
    };
    check_frozen(&n);
    assert!(n.data.join("apps/notes/frozen").is_file());
    n.restart();
    check_frozen(&n);
    let v = app_info(&n, "notes");
    assert_eq!(
        (v["state"].as_str(), v["freeze"]["reason"].as_str()),
        (Some("frozen"), Some("maintenance")),
        "{v}"
    );

    // Admin commands keep working on a frozen app.
    let (rc, v) = n.json(&["backup", "notes"]);
    assert_eq!(
        (rc, v["remote"]["status"].as_str()),
        (0, Some("uploaded")),
        "{v}"
    );
    let (rc, v) = n.json(&["backups", "notes"]);
    assert_eq!(rc, 0, "{v}");
    let (rc, v) = n.json(&["backups", "notes", "--remote"]);
    assert_eq!(rc, 0, "{v}");
    std::fs::write(n.app_data("notes").join("app.db"), b"changed while frozen").unwrap();
    let (rc, v) = n.json(&["restore", "notes", "--from", &first, "--yes"]);
    assert_eq!(rc, 0, "{v}");
    assert_ne!(
        std::fs::read(n.app_data("notes").join("app.db")).unwrap(),
        b"changed while frozen"
    );
    let (rc, v) = n.json(&["evict", "notes"]);
    assert_eq!(rc, 0, "{v}");
    let (rc, v) = n.json(&["logs", "notes"]);
    assert_eq!(rc, 0, "{v}");
    let (rc, v) = n.json(&["ps"]);
    assert_eq!(
        (rc, v["apps"][0]["state"].as_str()),
        (0, Some("frozen")),
        "{v}"
    );
    check_frozen(&n);

    // A deploy is refused while the app is frozen, and nothing changes.
    let version = app_info(&n, "notes")["version"].clone();
    let wasm = wat::parse_str(RESPONDER).unwrap();
    use base64::Engine as _;
    let m = base64::engine::general_purpose::STANDARD.encode("name = \"notes\"\n");
    let auth = format!("Bearer {}", n.token);
    let (st, body) = http(
        n.admin,
        "PUT",
        "/v1/apps/notes",
        "",
        &[("authorization", &auth), ("x-spared-manifest", &m)],
        &wasm,
    );
    let e: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        (st, e["error"]["kind"].as_str()),
        (409, Some("app_frozen")),
        "{e}"
    );
    assert!(
        e["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("spared thaw notes"),
        "{e}"
    );
    assert_eq!(app_info(&n, "notes")["version"], version);

    // Thawed, it serves, and a deploy works again.
    let (rc, v) = n.json(&["thaw", "notes"]);
    assert_eq!((rc, v["was_frozen"].as_bool()), (0, Some(true)), "{v}");
    assert_eq!(public(&n, "notes").0, 200);
    assert!(!n.data.join("apps/notes/frozen").exists());
    n.deploy_wat("notes", RESPONDER);
    // Freezing twice keeps the first freeze; rm removes a frozen app (its backups stay).
    n.json(&["freeze", "notes"]);
    let (_, v) = n.json(&["freeze", "notes", "--reason", "again"]);
    assert_eq!(v["already_frozen"], true, "{v}");
    let (rc, v) = n.json(&["rm", "notes", "--yes"]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(public(&n, "notes").0, 404);
}

#[test]
fn failures_before_the_restore_thaw_the_source_and_a_rerun_completes() {
    let s = scratch("move-before");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, mut b) = pair(&s, &fake, Some(&key));
    let mut tf = token_file(&s, &b, "b");
    for app in ["upload", "fetch", "gone"] {
        a.deploy_wat(app, RESPONDER);
        write_data(&a.app_data(app), 5);
    }
    let aborted = |v: &Value, cause: &str| {
        assert_eq!(v["error"]["kind"], "move_aborted", "{v}");
        assert_eq!(v["cause"]["kind"], cause, "{v}");
        assert!(
            v["state"]["source"]
                .as_str()
                .unwrap()
                .contains("thawed again by this move"),
            "{v}"
        );
        assert!(
            v["error"]["fix"]
                .as_str()
                .unwrap()
                .contains("run the same `spared move` again"),
            "{v}"
        );
    };

    // The bucket refuses the upload of the final backup.
    fake.set(|f| f.fail_put_5xx = true);
    let (rc, v) = mv(&a, &b, &tf, "upload", &[]);
    assert_eq!(rc, 1);
    aborted(&v, "remote_unavailable");
    assert_eq!(public(&a, "upload").0, 200, "the source serves again");
    let t = app_info(&b, "upload");
    assert_eq!(t["frozen"], true, "the target's copy does not serve: {t}");
    assert!(t["restored_from_remote"].is_null(), "{t}");
    assert_eq!(public(&b, "upload").0, 503);
    fake.clear_faults();
    let (rc, v) = mv(&a, &b, &tf, "upload", &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(tree(&b.app_data("upload")), tree(&a.app_data("upload")));
    assert_eq!(public(&b, "upload").0, 200);

    // The target's download is cut off, so its restore never starts.
    fake.set(|f| f.truncate_get = true);
    let (rc, v) = mv(&a, &b, &tf, "fetch", &[]);
    assert_eq!(rc, 1);
    aborted(&v, "remote_unavailable");
    assert_eq!(public(&a, "fetch").0, 200);
    fake.clear_faults();
    let (rc, v) = mv(&a, &b, &tf, "fetch", &[]);
    assert_eq!(rc, 0, "{v}");

    // The target goes away during the upload.
    write_data(&a.app_data("gone"), 6);
    std::fs::write(a.app_data("gone").join("big"), vec![7u8; 1 << 20]).unwrap();
    fake.set(|f| f.slow_put_ms = 200);
    fake.state.lock().unwrap().archive_put_started = false;
    let mut cli = {
        let mut args = vec!["--json".to_string(), "move".into(), "gone".into()];
        args.extend(move_args(&b, &tf, &[]));
        let a2: Vec<&str> = args.iter().map(String::as_str).collect();
        a.cli_cmd(&a2)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    wait_for(|| fake.state.lock().unwrap().archive_put_started);
    b.kill9();
    let out = {
        use std::io::Read;
        let mut s = String::new();
        cli.stdout.take().unwrap().read_to_string(&mut s).unwrap();
        cli.wait().unwrap();
        s
    };
    fake.clear_faults();
    let v: Value = serde_json::from_str(&out).unwrap();
    aborted(&v, "server_unreachable");
    assert!(
        v["state"]["target"]
            .as_str()
            .unwrap()
            .starts_with("unknown"),
        "{v}"
    );
    assert_eq!(public(&a, "gone").0, 200);
    b.spawn();
    tf = token_file(&s, &b, "b");
    let (rc, v) = mv(&a, &b, &tf, "gone", &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(tree(&b.app_data("gone")), tree(&a.app_data("gone")));
}

#[test]
fn a_target_that_holds_other_bytes_than_the_source_sent_is_not_restored() {
    let s = scratch("move-digest");
    let fake = Fake::start();
    let (a, b) = pair(&s, &fake, None);
    let tf = token_file(&s, &b, "b");
    a.deploy_wat("notes", RESPONDER);
    write_data(&a.app_data("notes"), 8);
    // Another writer under the same names (a clone of the source's data directory with the same
    // node id would be one): what the source uploads is replaced by a backup that is sound on
    // its own, under the same remote id, with other contents.
    fake.set(|f| {
        f.on_put = Some(std::sync::Arc::new(|key: &str, _body: &[u8]| {
            let (base, rest) = key.split_once("/notes/e")?;
            let (epoch, rest) = rest.split_once('/')?;
            let dir = rest.split('/').next()?;
            let rid = format!("{dir}.{}", epoch.parse::<u32>().ok()?);
            let data = b"not what the source had";
            let (archive, manifest) = planted(
                "notes",
                &rid,
                json!([file_entry("app.db", data)]),
                &tar_entry("app.db", b'0', data, ""),
            );
            let at = |leaf: &str| format!("{base}/notes/e{epoch}/{dir}/{leaf}");
            Some(vec![
                (at("archive"), archive),
                (at("manifest.json"), manifest),
            ])
        }));
    });
    let (rc, v) = mv(&a, &b, &tf, "notes", &[]);
    fake.clear_faults();
    assert_eq!(rc, 1, "{v}");
    assert_eq!(v["error"]["kind"], "move_aborted", "{v}");
    assert_eq!(v["cause"]["kind"], "move_digest_mismatch", "{v}");
    assert_eq!(public(&a, "notes").0, 200, "the source serves again");
    let t = app_info(&b, "notes");
    assert!(t["restored_from_remote"].is_null(), "{t}");
    assert_eq!(t["frozen"], true);
    assert!(
        tree(&b.app_data("notes")).is_empty(),
        "the target's data was replaced"
    );
    // The fetched copy stays on the target as a local backup, for whoever looks into it.
    let (_, list) = b.json(&["backups", "notes"]);
    assert!(
        list["backups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["remote"]["status"] == "fetched"),
        "{list}"
    );
}

#[test]
fn a_failure_after_the_restore_leaves_the_source_frozen_and_a_rerun_finishes() {
    let s = scratch("move-after");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, b) = pair(&s, &fake, Some(&key));
    let tf = token_file(&s, &b, "b");
    // This app answers nothing (502 no_response), so the smoke request fails.
    a.deploy("notes");
    write_data(&a.app_data("notes"), 11);
    let smoke = format!("http://127.0.0.1:{}", b.public);
    let (rc, v) = mv(&a, &b, &tf, "notes", &["--smoke-url", &smoke]);
    assert_eq!(rc, 1, "{v}");
    assert_eq!(v["error"]["kind"], "move_incomplete", "{v}");
    assert_eq!(v["cause"]["kind"], "move_smoke_failed", "{v}");
    let fix = v["error"]["fix"].as_str().unwrap();
    assert!(
        fix.contains("spared logs notes --server")
            && fix.contains("run the same `spared move` again"),
        "{v}"
    );
    assert!(
        v["state"]["source"].as_str().unwrap().starts_with("frozen"),
        "{v}"
    );
    assert!(
        v["state"]["target"].as_str().unwrap().starts_with("frozen"),
        "{v}"
    );
    assert_eq!(public(&a, "notes").0, 503, "the source stays frozen");
    assert_eq!(public(&b, "notes").0, 503, "the target was frozen again");
    assert_eq!(app_info(&b, "notes")["epoch"], 2);
    assert_eq!(tree(&b.app_data("notes")), tree(&a.app_data("notes")));
    // The text form names each side's state and the next command.
    let (rc2, plan) = mv(&a, &b, &tf, "notes", &["--dry-run"]);
    assert_eq!(rc2, 0, "{plan}");
    assert_eq!(plan["plan"].as_array().unwrap().len(), 2, "{plan}");

    // A rerun without the smoke request checks the target's copy and finishes; no new backup.
    let uploads = fake.keys().len();
    let (rc, v) = mv(&a, &b, &tf, "notes", &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(fake.keys().len(), uploads, "the rerun took another backup");
    assert!(
        v["steps"][0]
            .as_str()
            .unwrap()
            .contains("an earlier run restored"),
        "{v}"
    );
    assert_eq!(app_info(&b, "notes")["frozen"], false);
    assert_eq!(public(&a, "notes").0, 503);
}

#[test]
fn a_stale_server_is_refused_and_frozen_and_epochs_order_across_a_move_and_back() {
    let s = scratch("move-epochs");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, b) = pair(&s, &fake, Some(&key));
    let (ta, tb) = (token_file(&s, &a, "a"), token_file(&s, &b, "b"));
    a.deploy_wat("notes", RESPONDER);
    write_data(&a.app_data("notes"), 13);
    let (rc, v) = mv(&a, &b, &tb, "notes", &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(v["epoch"], 2);
    // The target's first backup goes out under epoch 2.
    let on_b = backup_uploaded(&b, "notes");
    assert!(
        on_b["remote"]["remote_id"]
            .as_str()
            .unwrap()
            .ends_with(".2"),
        "{on_b}"
    );

    // The source is thawed by mistake and takes writes: its next upload is refused, and it
    // freezes itself.
    let (rc, _) = a.json(&["thaw", "notes"]);
    assert_eq!(rc, 0);
    assert_eq!(public(&a, "notes").0, 200);
    std::fs::write(a.app_data("notes").join("late"), b"a write after the move").unwrap();
    let keys = fake.keys();
    let (rc, v) = a.json(&["backup", "notes"]);
    assert_eq!(rc, 1, "{v}");
    assert_eq!(v["error"]["kind"], "remote_upload_failed", "{v}");
    assert_eq!(v["remote"]["error"]["kind"], "remote_superseded", "{v}");
    let msg = v["remote"]["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("epoch 2") && msg.contains("holds epoch 1"),
        "{msg}"
    );
    assert_eq!(fake.keys(), keys, "a superseded server uploaded");
    assert!(
        a.backups("notes")
            .join(v["backup_id"].as_str().unwrap())
            .is_dir(),
        "the local backup stands"
    );
    let src = app_info(&a, "notes");
    assert_eq!(src["freeze"]["kind"], "remote_superseded", "{src}");
    assert_eq!(public(&a, "notes").0, 503);
    // A move does not lift that freeze.
    let (_, v) = a.json(&["freeze", "notes"]);
    assert_eq!(v["freeze"]["kind"], "remote_superseded", "{v}");

    // Moving back: the stale copy is replaced, and the epoch goes above the bucket's highest.
    std::fs::write(b.app_data("notes").join("on-b"), b"written on b").unwrap();
    let (rc, v) = mv(&b, &a, &ta, "notes", &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(v["epoch"], 3, "{v}");
    assert_eq!(app_info(&a, "notes")["frozen"], false);
    assert_eq!(public(&a, "notes").0, 200);
    assert_eq!(public(&b, "notes").0, 503);
    let mut ours = tree(&a.app_data("notes"));
    ours.retain(|(p, _)| p != "late");
    assert_eq!(ours, tree(&b.app_data("notes")));
    let on_a = backup_uploaded(&a, "notes");
    assert!(
        on_a["remote"]["remote_id"]
            .as_str()
            .unwrap()
            .ends_with(".3"),
        "{on_a}"
    );
    // b now holds epoch 2 and the bucket 3: b's uploads are refused too.
    let (rc, v) = b.json(&["backup", "notes"]);
    assert_eq!(
        (rc, v["remote"]["error"]["kind"].as_str()),
        (1, Some("remote_superseded")),
        "{v}"
    );

    // The listing is newest epoch first, then newest backup.
    let (_, l) = a.json(&["backups", "notes", "--remote"]);
    let epochs: Vec<u64> = l["remote_backups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["epoch"].as_u64().unwrap())
        .collect();
    let mut sorted = epochs.clone();
    sorted.sort_by(|x, y| y.cmp(x));
    assert_eq!(epochs, sorted, "{l}");
    assert_eq!((epochs.first(), epochs.last()), (Some(&3), Some(&1)), "{l}");
}

#[test]
fn backups_from_before_epochs_still_list_restore_and_raise_the_epoch() {
    let s = scratch("move-slice1");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, b) = pair(&s, &fake, Some(&key));
    a.deploy("notes");
    write_data(&a.app_data("notes"), 17);
    // A server that never restored from the bucket has no epoch file and uploads at epoch 1,
    // with the layout of the first remote backup release.
    let rid = backup_uploaded(&a, "notes")["remote"]["remote_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!a.backups("notes").join(".epoch").exists());
    assert!(rid.ends_with(".1"), "{rid}");
    assert!(
        fake.keys().iter().all(|k| k.contains("/e00000001/")),
        "{:?}",
        fake.keys()
    );
    assert_eq!(app_info(&a, "notes")["epoch"], 1);
    b.deploy("notes");
    let (_, l) = b.json(&["backups", "notes", "--remote"]);
    assert_eq!(l["remote_backups"][0]["remote_id"], rid.as_str(), "{l}");
    let (rc, v) = b.json(&[
        "restore",
        "notes",
        "--from",
        &format!("remote:{rid}"),
        "--yes",
    ]);
    assert_eq!((rc, v["epoch"].as_u64()), (0, Some(2)), "{v}");
    assert_eq!(v["fetched"]["next_epoch"], 2, "{v}");
    let epoch_file: Value =
        serde_json::from_slice(&std::fs::read(b.backups("notes").join(".epoch")).unwrap()).unwrap();
    assert_eq!(
        (
            epoch_file["epoch"].as_u64(),
            epoch_file["restored_from"].as_str()
        ),
        (Some(2), Some(rid.as_str()))
    );
    // A local restore keeps the epoch.
    let (_, list) = b.json(&["backups", "notes"]);
    let pre = list["backups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "pre-restore")
        .unwrap()["backup_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (rc, v) = b.json(&["restore", "notes", "--from", &pre, "--yes"]);
    assert_eq!((rc, v["epoch"].as_u64()), (0, Some(2)), "{v}");
    // An epoch file that cannot be read stops uploads, and the local backup stands.
    std::fs::write(b.backups("notes").join(".epoch"), b"garbage").unwrap();
    let (rc, v) = b.json(&["backup", "notes"]);
    assert_eq!(
        (rc, v["remote"]["error"]["kind"].as_str()),
        (1, Some("remote_archive_failed")),
        "{v}"
    );
}

#[test]
fn checks_before_a_move_refuse_and_change_nothing() {
    let s = scratch("move-refuse");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let other_key = key_file(&s.0, "other-key");
    let (a, b) = pair(&s, &fake, Some(&key));
    a.deploy_wat("notes", RESPONDER);
    write_data(&a.app_data("notes"), 19);
    let tb = token_file(&s, &b, "b");
    let unchanged = |what: &str, before: &[(String, Option<Vec<u8>>)]| {
        assert_eq!(
            public(&a, "notes").0,
            200,
            "{what}: the source stopped serving"
        );
        assert_eq!(snapshot(&[&a, &b]), before, "{what}: a data dir changed");
    };
    let refuse = |args: &[&str], tf: &Path, dst: &Node, kind: &str| {
        let before = snapshot(&[&a, &b]);
        let (rc, v) = mv(&a, dst, tf, "notes", args);
        assert_eq!((rc, v["error"]["kind"].as_str()), (1, Some(kind)), "{v}");
        unchanged(kind, &before);
        v
    };

    // No token for the target, and a token that is not the target's.
    let o = a
        .cli_cmd(&[
            "--json",
            "move",
            "notes",
            "--to",
            &format!("http://127.0.0.1:{}", b.admin),
        ])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["error"]["kind"], "no_target_token", "{v}");
    let wrong = token_file(&s, &a, "wrong");
    let v = refuse(&[], &wrong, &b, "unauthorized");
    assert!(
        v["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("--to-token-file"),
        "{v}"
    );
    // The target's token from the environment works the same as the file.
    let o = a
        .cli_cmd(&[
            "--json",
            "move",
            "notes",
            "--to",
            &format!("http://127.0.0.1:{}", b.admin),
            "--dry-run",
        ])
        .env("SPARED_TARGET_TOKEN", &b.token)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    // The source itself, and a target that is not up.
    let ta = token_file(&s, &a, "a");
    refuse(&[], &ta, &a, "move_same_server");
    let o = a
        .cli_cmd(&[
            "--json",
            "move",
            "notes",
            "--to",
            &format!("http://127.0.0.1:{}", free_port()),
            "--to-token-file",
            tb.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["error"]["kind"], "server_unreachable", "{v}");
    assert_eq!(public(&a, "notes").0, 200);
    // A target without a bucket, and one with another key.
    let off = Node::start(&s.0, "off", vec![], vec![]);
    let toff = token_file(&s, &off, "off");
    refuse(&[], &toff, &off, "remote_backups_off");
    let c = Node::start(&s.0, "c", remote_args(&fake, Some(&other_key)), creds());
    let tc = token_file(&s, &c, "c");
    let v = refuse(&[], &tc, &c, "move_remote_mismatch");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("key id"),
        "{v}"
    );
    // An app the source does not have.
    let (rc, v) = mv(&a, &b, &tb, "nothing", &[]);
    assert_eq!(
        (rc, v["error"]["kind"].as_str()),
        (1, Some("no_such_app")),
        "{v}"
    );
    // A target that already serves the app.
    b.deploy_wat("notes", RESPONDER);
    std::fs::write(b.app_data("notes").join("app.db"), b"b's own data").unwrap();
    refuse(&[], &tb, &b, "move_target_in_use");
    assert_eq!(public(&b, "notes").0, 200);
    // Frozen there, the move goes ahead and keeps the target's data as a pre-restore backup.
    let (rc, _) = b.json(&["freeze", "notes"]);
    assert_eq!(rc, 0);
    let (rc, v) = mv(&a, &b, &tb, "notes", &[]);
    assert_eq!(rc, 0, "{v}");
    let (_, list) = b.json(&["backups", "notes"]);
    assert!(
        list["backups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "pre-restore"),
        "{list}"
    );
    let _ = json!(null);
}

#[test]
fn requests_an_earlier_cli_sends_still_work() {
    // The routes and fields the CLI of the first remote backup release uses, sent as it sends
    // them, against a server that has frozen apps.
    let s = scratch("move-oldcli");
    let n = Node::start(&s.0, "n", vec![], vec![]);
    n.deploy_wat("one", RESPONDER);
    n.deploy_wat("two", RESPONDER);
    let (rc, _) = n.json(&["freeze", "two"]);
    assert_eq!(rc, 0);
    let (st, v) = n.admin("GET", "/v1/apps", b"");
    assert_eq!(st, 200);
    for a in v["apps"].as_array().unwrap() {
        for field in [
            "name",
            "state",
            "version",
            "requests",
            "wakes",
            "errors",
            "data_bytes",
            "url",
            "component_sha256",
        ] {
            assert!(
                !a[field].is_null() || field == "data_bytes",
                "{field} missing: {a}"
            );
        }
    }
    assert_eq!(v["apps"][1]["state"], "frozen", "{v}");
    let (st, v) = n.admin("POST", "/v1/apps/one/backup", b"");
    assert_eq!(st, 200, "{v}");
    let id = v["backup_id"].as_str().unwrap();
    let (st, v) = n.admin(
        "POST",
        "/v1/apps/one/restore",
        json!({"backup_id": id}).to_string().as_bytes(),
    );
    assert_eq!(st, 200, "{v}");
    n.deploy_wat("one", RESPONDER);
    assert_eq!(public(&n, "one").0, 200);
}

#[test]
fn review_undo_then_prefrozen_source_rerun_must_carry_new_writes() {
    let s = scratch("move-rev1");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, b) = pair(&s, &fake, Some(&key));
    let tf = token_file(&s, &b, "b");
    a.deploy_wat("notes", RESPONDER);
    write_data(&a.app_data("notes"), 1);
    let (rc, v) = mv(&a, &b, &tf, "notes", &[]);
    assert_eq!(rc, 0, "{v}");
    // The undo the move prints: freeze the target, thaw the source.
    assert_eq!(b.json(&["freeze", "notes"]).0, 0);
    assert_eq!(a.json(&["thaw", "notes"]).0, 0);
    assert_eq!(public(&a, "notes").0, 200);
    std::thread::sleep(std::time::Duration::from_secs(2));
    std::fs::write(
        a.app_data("notes").join("late"),
        b"written on the source after the undo",
    )
    .unwrap();
    // The operator freezes the source by hand, then moves again.
    assert_eq!(a.json(&["freeze", "notes"]).0, 0);
    let (rc, v) = mv(&a, &b, &tf, "notes", &[]);
    eprintln!("REVIEW1 rc={rc} out={v}");
    assert_eq!(rc, 0, "{v}");
    assert_eq!(
        tree(&b.app_data("notes")),
        tree(&a.app_data("notes")),
        "the move reported success but the target lacks the source's later write: {v}"
    );
    assert_eq!(v["already_moved"], false, "{v}");
}

#[test]
fn review_failed_second_move_must_not_thaw_a_moved_away_source() {
    let s = scratch("move-rev2");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, b) = pair(&s, &fake, Some(&key));
    let c = Node::start(&s.0, "c", remote_args(&fake, Some(&key)), creds());
    let (tb, tc) = (token_file(&s, &b, "b"), token_file(&s, &c, "c"));
    a.deploy_wat("notes", RESPONDER);
    write_data(&a.app_data("notes"), 1);
    let (rc, v) = mv(&a, &b, &tb, "notes", &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(public(&a, "notes").0, 503);
    assert_eq!(public(&b, "notes").0, 200);
    // A second move from the old source to a third server fails at the upload.
    fake.set(|f| f.fail_put_5xx = true);
    let (rc, v) = mv(&a, &c, &tc, "notes", &[]);
    fake.clear_faults();
    eprintln!("REVIEW2 rc={rc} out={v}");
    assert_eq!(rc, 1, "{v}");
    assert_eq!(public(&b, "notes").0, 200, "the app lives on b");
    assert_eq!(
        public(&a, "notes").0,
        503,
        "the moved-away source serves again next to b: {v}"
    );
}

#[test]
fn review_rm_on_target_then_killed_rerun_must_not_thaw_an_empty_target() {
    let s = scratch("move-rev3");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let (a, b) = pair(&s, &fake, Some(&key));
    let tf = token_file(&s, &b, "b");
    // This app answers 502, so the smoke request fails after the restore.
    a.deploy("notes");
    write_data(&a.app_data("notes"), 11);
    let smoke = format!("http://127.0.0.1:{}", b.public);
    let (_, v) = mv(&a, &b, &tf, "notes", &["--smoke-url", &smoke]);
    assert_eq!(v["error"]["kind"], "move_incomplete", "{v}");
    // The operator wipes the target's copy and runs the move again; that run is killed while
    // the source uploads its backup.
    assert_eq!(b.json(&["rm", "notes", "--yes"]).0, 0);
    b.deploy("notes");
    let ps = b.json(&["ps"]).1;
    let listed = ps["apps"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"] == "notes"))
        .unwrap_or_else(|| panic!("{ps}"));
    assert!(
        listed.get("restored_from_remote").is_none() || listed["restored_from_remote"].is_null(),
        "a redeploy after `spared rm` still shows the old restore: {ps}"
    );
    assert!(
        app_info(&b, "notes")["restored_from_remote"].is_null(),
        "a redeploy after `spared rm` still shows the old restore"
    );
    assert_eq!(b.json(&["freeze", "notes"]).0, 0);
    fake.set(|f| f.slow_put_ms = 400);
    fake.state.lock().unwrap().archive_put_started = false;
    let mut cli = {
        let mut args = vec!["--json".to_string(), "move".into(), "notes".into()];
        args.extend(move_args(&b, &tf, &[]));
        let a2: Vec<&str> = args.iter().map(String::as_str).collect();
        a.cli_cmd(&a2)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    wait_for(|| fake.state.lock().unwrap().archive_put_started);
    cli.kill().unwrap();
    cli.wait().unwrap();
    // Run it again at once.
    let (rc, v) = mv(&a, &b, &tf, "notes", &[]);
    fake.clear_faults();
    let same = tree(&b.app_data("notes")) == tree(&a.app_data("notes"));
    let t = app_info(&b, "notes");
    eprintln!(
        "REVIEW3 rc={rc} moved={} steps={} target_frozen={} target_files={} same_data={same}",
        v["moved"],
        v["steps"],
        t["frozen"],
        tree(&b.app_data("notes")).len()
    );
    assert!(
        !(rc == 0 && !same),
        "the move reported success and thawed a target whose data is not the source's"
    );
}

#[test]
fn a_move_copies_the_apps_limits_to_a_target_without_its_own() {
    let s = scratch("move-limits");
    let fake = Fake::start();
    let (a, b) = pair(&s, &fake, None);
    a.deploy_wat("lim", RESPONDER);
    a.deploy_wat("own", RESPONDER);
    let (st, v) = a.admin(
        "PUT",
        "/v1/apps/lim/limits",
        br#"{"data_mb": 3, "cpu_ms_per_min": 600}"#,
    );
    assert_eq!(st, 200, "{v}");
    let tf = token_file(&s, &b, "b");

    // The dry run names the step and changes nothing on the target.
    let (rc, v) = mv(&a, &b, &tf, "lim", &["--dry-run"]);
    assert_eq!(rc, 0, "{v}");
    assert!(
        v["plan"][0]
            .as_str()
            .unwrap()
            .starts_with("copy the per-app limits"),
        "{v}"
    );
    assert_eq!(
        b.admin("GET", "/v1/apps/lim/limits", b"").1["override"],
        Value::Null
    );

    let (rc, v) = mv(&a, &b, &tf, "lim", &[]);
    assert_eq!(rc, 0, "{v}\n{}", a.log_text());
    assert!(
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().starts_with("copied the per-app limits")),
        "{v}"
    );
    let (_, t) = b.admin("GET", "/v1/apps/lim/limits", b"");
    assert_eq!(
        t["override"],
        json!({"data_mb": 3, "cpu_ms_per_min": 600}),
        "{t}"
    );
    assert_eq!(t["app_exists"], true);
    assert_eq!(public(&b, "lim").0, 200);

    // A target that has limits of its own for the name keeps them.
    let (st, _) = a.admin("PUT", "/v1/apps/own/limits", br#"{"data_mb": 9}"#);
    assert_eq!(st, 200);
    let (st, _) = b.admin("PUT", "/v1/apps/own/limits", br#"{"data_mb": 2}"#);
    assert_eq!(st, 200);
    let (rc, v) = mv(&a, &b, &tf, "own", &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(
        b.admin("GET", "/v1/apps/own/limits", b"").1["override"],
        json!({"data_mb": 2})
    );
}
