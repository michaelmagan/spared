//! Off-machine backups through the real server: `spared serve --backup-to` uploading to a fake
//! S3 that runs in this test process (`support/fake_s3.rs`), and `spared restore --from remote:`
//! bringing a backup back on another server. Each test starts its own fake and servers, on
//! scratch ports and temp directories, with its own `$HOME`.
//!
//! The app is a component that does nothing; its data is written straight into
//! `<data-dir>/apps/<name>/data/`, which is all a backup reads.

#[path = "support/fake_s3.rs"]
mod fake_s3;

use base64::Engine as _;
use fake_s3::Fake;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const SPARED: &str = env!("CARGO_BIN_EXE_spared");
const HANDLER: &str = r#"(component
  (import "wasi:http/types@0.2.4" (instance $types
    (export "incoming-request" (type (sub resource)))
    (export "response-outparam" (type (sub resource)))))
  (alias export $types "incoming-request" (type $ireq))
  (alias export $types "response-outparam" (type $rout))
  (core module $m (func (export "handle") (param i32 i32)))
  (core instance $i (instantiate $m))
  (func $handle (param "request" (own $ireq)) (param "response-out" (own $rout))
    (canon lift (core func $i "handle")))
  (instance $h (export "handle" (func $handle)))
  (export "wasi:http/incoming-handler@0.2.4" (instance $h)))"#;
const SEGMENT: usize = 64 * 1024;
/// A string written into the app data, which must never appear in an encrypted archive.
const MARKER: &[u8] = b"PLAINTEXT-MARKER-0123456789-must-not-reach-the-bucket";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Scratch {
    let d = std::env::temp_dir().join(format!("spared-remote-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Scratch(d)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Every variable a server or the CLI could pick up from the test runner's environment.
const CLEARED: [&str; 16] = [
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_ACCESS_KEY_ID_FILE",
    "AWS_SECRET_ACCESS_KEY_FILE",
    "AWS_SESSION_TOKEN_FILE",
    "AWS_REGION",
    "SPARED_ADMIN_TOKEN",
    "SPARED_TOKEN",
    "SPARED_URL",
    "SPARED_BACKUP_TO",
    "SPARED_BACKUP_ENDPOINT",
    "SPARED_BACKUP_REGION",
    "SPARED_BACKUP_KEY_FILE",
    "SPARED_BACKUP_ENCRYPTION",
    "PORT",
];

fn cleared(cmd: &mut Command) -> &mut Command {
    for v in CLEARED {
        cmd.env_remove(v);
    }
    cmd
}

/// The environment a test server gets: the fake's credentials.
fn creds() -> Vec<(&'static str, String)> {
    vec![
        ("AWS_ACCESS_KEY_ID", fake_s3::ACCESS_KEY.into()),
        ("AWS_SECRET_ACCESS_KEY", fake_s3::SECRET_KEY.into()),
        ("AWS_SESSION_TOKEN", fake_s3::SESSION_TOKEN.into()),
    ]
}

/// Remote backup flags for a server that uploads to `fake` under `prefix`.
fn remote_args(fake: &Fake, key: Option<&Path>) -> Vec<String> {
    let mut a = vec![
        "--backup-to".into(),
        format!("s3://{}/prod/spared", fake_s3::BUCKET),
        "--backup-endpoint".into(),
        fake.endpoint(),
        "--backup-region".into(),
        fake_s3::REGION.into(),
    ];
    match key {
        Some(k) => a.extend(["--backup-key-file".into(), k.display().to_string()]),
        None => a.extend(["--backup-encryption".into(), "off".into()]),
    }
    a
}

struct Node {
    root: PathBuf,
    data: PathBuf,
    public: u16,
    admin: u16,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
    child: Option<Child>,
    token: String,
    log: PathBuf,
}

impl Node {
    fn start(root: &Path, tag: &str, args: Vec<String>, env: Vec<(&'static str, String)>) -> Node {
        let root = root.join(tag);
        std::fs::create_dir_all(root.join("home")).unwrap();
        let mut n = Node {
            data: root.join("data"),
            log: root.join("serve.log"),
            root,
            public: 0,
            admin: 0,
            args,
            env,
            child: None,
            token: String::new(),
        };
        n.spawn();
        n
    }

    fn spawn(&mut self) {
        self.public = free_port();
        self.admin = free_port();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let child = cleared(&mut Command::new(SPARED))
            .args(["serve", "--data-dir"])
            .arg(&self.data)
            .args(["--listen", &format!("127.0.0.1:{}", self.public)])
            .args(["--admin-listen", &format!("127.0.0.1:{}", self.admin)])
            .args(&self.args)
            .env("HOME", self.root.join("home"))
            .envs(self.env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(st) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("the server exited at start ({st}):\n{}", self.log_text());
            }
            if http(self.public, "GET", "/healthz", "", &[], b"").0 == 200 {
                break;
            }
            assert!(
                Instant::now() < until,
                "the server did not come up:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        self.token = std::fs::read_to_string(self.data.join("admin-token"))
            .unwrap()
            .trim()
            .to_string();
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn admin(&self, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
        let auth = format!("Bearer {}", self.token);
        let (st, b) = http(
            self.admin,
            method,
            path,
            &format!("127.0.0.1:{}", self.admin),
            &[("authorization", &auth)],
            body,
        );
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    fn deploy(&self, name: &str) {
        let wasm = wat::parse_str(HANDLER).unwrap();
        let manifest = format!("name = \"{name}\"\n[grants]\ndata = true\n");
        let m = base64::engine::general_purpose::STANDARD.encode(manifest);
        let auth = format!("Bearer {}", self.token);
        let (st, b) = http(
            self.admin,
            "PUT",
            &format!("/v1/apps/{name}"),
            &format!("127.0.0.1:{}", self.admin),
            &[("authorization", &auth), ("x-spared-manifest", &m)],
            &wasm,
        );
        assert_eq!(st, 200, "deploy: {}", String::from_utf8_lossy(&b));
        std::fs::create_dir_all(self.app_data(name)).unwrap();
    }

    fn app_data(&self, name: &str) -> PathBuf {
        self.data.join("apps").join(name).join("data")
    }

    fn backups(&self, name: &str) -> PathBuf {
        self.data.join("backups").join(name)
    }

    fn cli_cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(SPARED);
        cleared(&mut c)
            .args(args)
            .args(["--server", &format!("http://127.0.0.1:{}", self.admin)])
            .env("HOME", self.root.join("home"))
            .env("SPARED_TOKEN", &self.token);
        c
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.cli_cmd(args).output().unwrap()
    }

    /// `spared --json <args>`: exit code and the JSON it printed.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let o = self.cli(&a);
        let v = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| {
            panic!(
                "not JSON from {args:?}: {}\n{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        });
        (o.status.code().unwrap_or(-1), v)
    }

    fn kill9(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    fn restart(&mut self) {
        self.kill9();
        self.spawn();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.kill9();
    }
}

/// A plain HTTP/1.1 request on its own connection: status and body.
fn http(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return (0, vec![]);
    };
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let host = if host.is_empty() {
        format!("127.0.0.1:{port}")
    } else {
        host.to_string()
    };
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nhost: {host}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    if s.write_all(req.as_bytes()).is_err() || s.write_all(body).is_err() {
        return (0, vec![]);
    }
    let mut out = vec![];
    let _ = s.read_to_end(&mut out);
    let Some(split) = out.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (0, vec![]);
    };
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, out[split + 4..].to_vec())
}

/// Every entry under `p`, sorted: (relative path, kind and bytes).
fn tree(p: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out = vec![];
    let mut stack = vec![(p.to_path_buf(), String::new())];
    while let Some((d, pre)) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let n = format!("{pre}{}", e.file_name().to_string_lossy());
            let t = e.file_type().unwrap();
            if t.is_dir() {
                out.push((n.clone(), None));
                stack.push((e.path(), n + "/"));
            } else if t.is_symlink() {
                out.push((n, Some(b"<symlink>".to_vec())));
            } else {
                out.push((n, Some(std::fs::read(e.path()).unwrap())));
            }
        }
    }
    out.sort();
    out
}

/// Bytes that do not compress, so an archive spans several sealed segments.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// App data with the shapes a real data tree has: a database with a run of zeros, a sparse
/// file, nested directories, an empty file and a name longer than 100 bytes.
fn write_sample(data: &Path, big: usize) {
    std::fs::create_dir_all(data.join("sub/deeper")).unwrap();
    let mut db = noise(big, 7);
    db.extend_from_slice(MARKER);
    db.extend(std::iter::repeat_n(0u8, 200_000));
    db.extend(noise(10_000, 9));
    std::fs::write(data.join("app.db"), &db).unwrap();
    let f = std::fs::File::create(data.join("sparse.bin")).unwrap();
    f.set_len(4 << 20).unwrap();
    use std::os::unix::fs::FileExt;
    f.write_all_at(b"tail", (4 << 20) - 4).unwrap();
    std::fs::write(data.join("sub/deeper/x.txt"), b"nested").unwrap();
    std::fs::write(data.join("empty"), b"").unwrap();
    std::fs::write(data.join("sub").join("n".repeat(150)), b"long name").unwrap();
}

fn key_file(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    let o = cleared(&mut Command::new(SPARED))
        .args(["backup-key", "new", "--out"])
        .arg(&p)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    p
}

fn rid_dir(rid: &str) -> String {
    rid.rsplit_once('.').unwrap().0.to_string()
}

fn object(app: &str, rid: &str, leaf: &str) -> String {
    format!(
        "prod/spared/spared/v1/{app}/e00000001/{}/{leaf}",
        rid_dir(rid)
    )
}

/// What a refused restore must leave alone: the app's data, its local backups, and no staging.
fn assert_untouched(
    n: &Node,
    app: &str,
    data_before: &[(String, Option<Vec<u8>>)],
    backups_before: &[String],
) {
    assert_eq!(
        tree(&n.app_data(app)),
        data_before,
        "the app's data changed"
    );
    assert_eq!(
        local_ids(n, app),
        backups_before,
        "a local backup appeared or went"
    );
    let staged: Vec<_> = std::fs::read_dir(n.backups(app))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(staged.is_empty(), "staging left behind: {staged:?}");
}

fn local_ids(n: &Node, app: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(n.backups(app))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.'))
        .collect();
    v.sort();
    v
}

/// Backs up `app` on `n` and returns the CLI's JSON, which must say uploaded.
fn backup_uploaded(n: &Node, app: &str) -> Value {
    let (rc, v) = n.json(&["backup", app]);
    assert_eq!(rc, 0, "{v}\n{}", n.log_text());
    assert_eq!(v["remote"]["status"], "uploaded", "{v}");
    v
}

#[test]
fn a_backup_round_trips_byte_for_byte_to_another_server_and_secrets_stay_out_of_logs() {
    let s = scratch("roundtrip");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let mut a = Node::start(&s.0, "a", remote_args(&fake, Some(&key)), creds());
    assert!(
        a.log_text()
            .contains("remote backups: s3://spared-test/prod/spared"),
        "{}",
        a.log_text()
    );
    a.deploy("notes");
    write_sample(&a.app_data("notes"), 3 * SEGMENT);
    let original = tree(&a.app_data("notes"));

    let v = backup_uploaded(&a, "notes");
    let rid = v["remote"]["remote_id"].as_str().unwrap().to_string();
    let local = v["backup_id"].as_str().unwrap();
    assert!(
        rid.starts_with(&format!("{local}.")) && rid.ends_with(".1"),
        "{rid}"
    );
    assert_eq!(v["remote"]["encrypted"], true);
    let digest = v["remote"]["content_digest"].as_str().unwrap().to_string();
    assert_eq!(
        fake.keys(),
        [
            object("notes", &rid, "archive"),
            object("notes", &rid, "manifest.json")
        ]
    );
    let archive = fake.get(&object("notes", &rid, "archive")).unwrap();
    assert!(archive.starts_with(b"SPRDSEAL"));
    assert!(
        !archive.windows(MARKER.len()).any(|w| w == MARKER),
        "the archive holds the app data in the clear"
    );
    let m: Value =
        serde_json::from_slice(&fake.get(&object("notes", &rid, "manifest.json")).unwrap())
            .unwrap();
    assert_eq!(m["encryption"]["alg"], "chacha20-poly1305-stream-64k");
    assert_eq!(m["archive"]["bytes"], archive.len());
    assert_eq!(m["archive"]["sha256"].as_str().unwrap(), sha_hex(&archive));
    assert!(
        !String::from_utf8_lossy(&serde_json::to_vec(&m).unwrap()).contains("app.db"),
        "file names stay inside the sealed archive"
    );
    let o = a.cli(&["backups", "notes"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains(&format!("uploaded {rid}")));

    // A second server: listing works before the app is deployed there.
    let mut b = Node::start(&s.0, "b", remote_args(&fake, Some(&key)), creds());
    let (rc, l) = b.json(&["backups", "notes", "--remote"]);
    assert_eq!(rc, 0, "{l}");
    assert_eq!(l["remote_backups"][0]["remote_id"], rid);
    assert_eq!(l["remote_backups"][0]["this_node"], false);
    assert_eq!(l["remote_backups"][0]["bytes"], archive.len());
    let (rc, e) = b.json(&[
        "restore",
        "notes",
        "--from",
        &format!("remote:{rid}"),
        "--yes",
    ]);
    assert_eq!(
        (rc, e["error"]["kind"].as_str()),
        (1, Some("no_such_app")),
        "{e}"
    );
    b.deploy("notes");
    std::fs::write(b.app_data("notes").join("app.db"), b"b's own data").unwrap();
    let (rc, r) = b.json(&[
        "restore",
        "notes",
        "--from",
        &format!("remote:{rid}"),
        "--yes",
    ]);
    assert_eq!(rc, 0, "{r}\n{}", b.log_text());
    assert_eq!(r["fetched"]["content_digest"], digest.as_str());
    assert!(
        r["pre_restore_id"]
            .as_str()
            .unwrap()
            .starts_with("pre-restore-")
    );
    assert_eq!(
        tree(&b.app_data("notes")),
        original,
        "the restored data differs"
    );
    let sparse = std::fs::metadata(b.app_data("notes").join("sparse.bin")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert!(
        sparse.blocks() * 512 < 1 << 20,
        "a run of zeros came back as a hole"
    );
    // The fetched copy stays as an ordinary local backup that knows where it came from.
    let (_, list) = b.json(&["backups", "notes"]);
    assert!(list["backups"].as_array().unwrap().iter().any(|e| e["remote"]["status"] == "fetched" && e["remote"]["remote_id"] == rid.as_str()), "{list}");

    // Text mode says what happened.
    let o = b.cli(&[
        "restore",
        "notes",
        "--from",
        &format!("remote:{rid}"),
        "--yes",
    ]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains(&format!("fetched {rid} as local backup"))
            && out.contains("restored notes from"),
        "{out}"
    );

    // The fake checked every signature on its own, and every request signed its payload.
    {
        let st = fake.state.lock().unwrap();
        assert!(st.auth_failures.is_empty(), "{:?}", st.auth_failures);
        assert!(st.payload_failures.is_empty(), "{:?}", st.payload_failures);
        assert!(st.deletes.is_empty(), "spared deleted {:?}", st.deletes);
        assert!(
            st.log
                .iter()
                .any(|l| l.contains("list-type=2&prefix=prod%2Fspared")),
            "{:?}",
            st.log
        );
    }

    // Secrets appear in no log and no CLI output.
    a.kill9();
    b.kill9();
    let key_hex = std::fs::read_to_string(&key).unwrap().trim().to_string();
    let mut all = a.log_text() + &b.log_text();
    for o in [
        a.cli(&["backups", "notes"]),
        b.cli(&["--json", "backups", "notes"]),
    ] {
        all += &String::from_utf8_lossy(&o.stdout);
        all += &String::from_utf8_lossy(&o.stderr);
    }
    for secret in [
        fake_s3::SECRET_KEY,
        fake_s3::SESSION_TOKEN,
        &key_hex,
        "Signature=",
        "AWS4-HMAC",
    ] {
        assert!(!all.contains(secret), "{secret} appears in a log");
    }
    for p in walk_files(&a.data).into_iter().chain(walk_files(&b.data)) {
        let bytes = std::fs::read(&p).unwrap_or_default();
        for secret in [
            fake_s3::SECRET_KEY.as_bytes(),
            fake_s3::SESSION_TOKEN.as_bytes(),
            key_hex.as_bytes(),
        ] {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret),
                "a secret is stored in {}",
                p.display()
            );
        }
    }
}

fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn walk_files(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let t = e.file_type().unwrap();
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    out
}

#[test]
fn a_bucket_that_is_down_keeps_the_local_backup_and_exits_1() {
    let s = scratch("down");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    // Nothing listens on the endpoint at all.
    let mut args = remote_args(&fake, Some(&key));
    let closed = format!("http://127.0.0.1:{}", free_port());
    let at = args.iter().position(|a| a == "--backup-endpoint").unwrap();
    args[at + 1] = closed;
    let n = Node::start(&s.0, "closed", args, creds());
    n.deploy("notes");
    write_sample(&n.app_data("notes"), 1000);
    let (rc, v) = n.json(&["backup", "notes"]);
    assert_eq!(rc, 1, "{v}");
    assert_eq!(v["error"]["kind"], "remote_upload_failed");
    assert_eq!(v["remote"]["error"]["kind"], "remote_unavailable", "{v}");
    let id = v["backup_id"].as_str().unwrap();
    assert!(
        n.backups("notes").join(id).is_dir(),
        "the local backup stands"
    );
    assert!(
        v["error"]["fix"]
            .as_str()
            .unwrap()
            .contains(&format!("--from {id}")),
        "{v}"
    );
    let o = n.cli(&["backup", "notes"]);
    let (out, err) = (
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(out.contains("backed up notes as"), "{out}");
    assert!(
        err.contains("error[remote_upload_failed]") && err.contains("remote_unavailable"),
        "{err}"
    );
    let (_, list) = n.json(&["backups", "notes"]);
    assert!(
        list["backups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["remote"]["status"] == "failed"),
        "{list}"
    );

    // A store that answers 503 to everything: the same, after the retries.
    let n2 = Node::start(&s.0, "503", remote_args(&fake, Some(&key)), creds());
    n2.deploy("notes");
    fake.set(|f| f.fail_5xx = u32::MAX);
    let (rc, v) = n2.json(&["backup", "notes"]);
    assert_eq!(
        (rc, v["remote"]["error"]["kind"].as_str()),
        (1, Some("remote_unavailable")),
        "{v}"
    );
    assert!(
        v["remote"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("SlowDown"),
        "{v}"
    );
    // Two 503s, then the store recovers: the retries carry the upload through.
    fake.set(|f| f.fail_5xx = 2);
    backup_uploaded(&n2, "notes");
    // Only `--no-wait` returns before the upload ends.
    fake.clear_faults();
    let (rc, v) = n2.json(&["backup", "notes", "--no-wait"]);
    assert_eq!(
        (rc, v["remote"]["status"].as_str()),
        (0, Some("uploading")),
        "{v}"
    );
}

#[test]
fn store_errors_become_clear_kinds() {
    let s = scratch("errors");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let n = Node::start(&s.0, "n", remote_args(&fake, Some(&key)), creds());
    n.deploy("notes");
    write_sample(&n.app_data("notes"), 5000);
    let fails = |f: &dyn Fn(&mut fake_s3::Faults), kind: &str, text: &str| {
        fake.clear_faults();
        fake.set(f);
        let (rc, v) = n.json(&["backup", "notes"]);
        assert_eq!(rc, 1, "{v}");
        let e = &v["remote"]["error"];
        assert_eq!(e["kind"], kind, "{v}");
        let all = format!("{} {}", e["message"], e["fix"]);
        assert!(all.contains(text), "wanted {text:?} in {all}");
    };
    fails(&|f| f.skew_secs = 3600, "remote_clock_skew", "GMT");
    fails(
        &|f| f.redirect_region = Some("eu-west-3".into()),
        "remote_wrong_region",
        "--backup-region eu-west-3",
    );
    fails(
        &|f| f.put_412 = true,
        "remote_rejected",
        "PreconditionFailed",
    );
    fails(&|f| f.reset_put = true, "remote_unavailable", "PUT failed");
    fake.clear_faults();
    // None of those uploads stored anything.
    assert!(fake.keys().is_empty(), "{:?}", fake.keys());
    // The archive is stored and its manifest PUT fails: the archive stays (spared never deletes
    // in the bucket), and without its manifest it is never listed.
    fails(
        &|f| f.fail_manifest_put = true,
        "remote_unavailable",
        "InternalError",
    );
    fake.clear_faults();
    let keys = fake.keys();
    assert!(keys.len() == 1 && keys[0].ends_with("/archive"), "{keys:?}");
    let (rc, v) = n.json(&["backups", "notes", "--remote"]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(v["remote_backups"], json!([]), "{v}");
    assert!(fake.state.lock().unwrap().deletes.is_empty());

    // A wrong secret, and a bucket that does not exist.
    let mut bad = creds();
    bad[1].1 = "not-the-secret".into();
    let n2 = Node::start(&s.0, "badsecret", remote_args(&fake, Some(&key)), bad);
    n2.deploy("notes");
    let (rc, v) = n2.json(&["backup", "notes"]);
    assert_eq!(
        (rc, v["remote"]["error"]["kind"].as_str()),
        (1, Some("remote_auth_failed")),
        "{v}"
    );
    assert!(!fake.state.lock().unwrap().auth_failures.is_empty());
    let mut args = remote_args(&fake, Some(&key));
    args[1] = "s3://no-such-bucket/x".into();
    let n3 = Node::start(&s.0, "nobucket", args, creds());
    n3.deploy("notes");
    let (rc, v) = n3.json(&["backup", "notes"]);
    assert_eq!(
        (rc, v["remote"]["error"]["kind"].as_str()),
        (1, Some("remote_no_bucket")),
        "{v}"
    );
    let (rc, v) = n3.json(&["backups", "notes", "--remote"]);
    assert_eq!(
        (rc, v["error"]["kind"].as_str()),
        (1, Some("remote_no_bucket")),
        "{v}"
    );
}

/// Restore `rid` into `app` on `n`, expecting a refusal of `kind`; nothing may change.
fn refused(n: &Node, app: &str, rid: &str, kind: &str) -> Value {
    let before = tree(&n.app_data(app));
    let ids = local_ids(n, app);
    let (rc, v) = n.json(&["restore", app, "--from", &format!("remote:{rid}"), "--yes"]);
    assert_eq!(rc, 1, "{v}");
    assert_eq!(v["error"]["kind"], kind, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unchanged"),
        "{v}"
    );
    assert_untouched(n, app, &before, &ids);
    v
}

#[test]
fn tampered_or_misnamed_backups_are_refused_and_the_data_is_untouched() {
    let s = scratch("tamper");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let a = Node::start(&s.0, "a", remote_args(&fake, Some(&key)), creds());
    a.deploy("notes");
    write_sample(&a.app_data("notes"), 4 * SEGMENT);
    let rid = backup_uploaded(&a, "notes")["remote"]["remote_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (akey, mkey) = (
        object("notes", &rid, "archive"),
        object("notes", &rid, "manifest.json"),
    );
    let (archive, manifest) = (fake.get(&akey).unwrap(), fake.get(&mkey).unwrap());
    let m: Value = serde_json::from_slice(&manifest).unwrap();
    let with_archive = |bytes: &[u8]| {
        let mut m2 = m.clone();
        m2["archive"] = json!({"bytes": bytes.len(), "sha256": sha_hex(bytes)});
        serde_json::to_vec(&m2).unwrap()
    };
    let restore_originals = || {
        fake.put(&akey, archive.clone());
        fake.put(&mkey, manifest.clone());
    };

    let b = Node::start(&s.0, "b", remote_args(&fake, Some(&key)), creds());
    b.deploy("notes");
    b.deploy("other");
    write_sample(&b.app_data("notes"), 100);
    std::fs::write(b.app_data("other").join("app.db"), b"other's data").unwrap();

    // Cut short: the size check refuses it.
    fake.put(&akey, archive[..archive.len() - 100].to_vec());
    let v = refused(&b, "notes", &rid, "remote_backup_corrupt");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("bytes"),
        "{v}"
    );
    // One byte flipped: the hash check refuses it.
    let mut flipped = archive.clone();
    flipped[archive.len() / 2] ^= 1;
    fake.put(&akey, flipped.clone());
    let v = refused(&b, "notes", &rid, "remote_backup_corrupt");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("SHA-256"),
        "{v}"
    );
    // One byte flipped by someone who also rewrote the manifest's hash: the seal refuses it.
    fake.put(&mkey, with_archive(&flipped));
    let v = refused(&b, "notes", &rid, "remote_backup_corrupt");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not open"),
        "{v}"
    );
    // Cut at a segment boundary, manifest rewritten to match: the last-segment flag refuses it.
    let cut = archive[..41 + 2 * (SEGMENT + 16)].to_vec();
    fake.put(&akey, cut.clone());
    fake.put(&mkey, with_archive(&cut));
    let v = refused(&b, "notes", &rid, "remote_backup_corrupt");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not open"),
        "{v}"
    );
    restore_originals();
    // A GET cut off by the store.
    fake.set(|f| f.truncate_get = true);
    refused(&b, "notes", &rid, "remote_unavailable");
    fake.clear_faults();

    // The same archive renamed to another app, and to another id of the same app, with
    // manifests rewritten to match: the associated data binds the name, so neither opens.
    let mut m2 = m.clone();
    m2["app"] = "other".into();
    fake.put(&object("other", &rid, "archive"), archive.clone());
    fake.put(
        &object("other", &rid, "manifest.json"),
        serde_json::to_vec(&m2).unwrap(),
    );
    let v = refused(&b, "other", &rid, "remote_backup_corrupt");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not open"),
        "{v}"
    );
    let other_rid = format!("20000101T000000Z.{}", rid.split_once('.').unwrap().1);
    let mut m3 = m.clone();
    m3["remote_id"] = other_rid.clone().into();
    fake.put(&object("notes", &other_rid, "archive"), archive.clone());
    fake.put(
        &object("notes", &other_rid, "manifest.json"),
        serde_json::to_vec(&m3).unwrap(),
    );
    refused(&b, "notes", &other_rid, "remote_backup_corrupt");

    // A manifest that is missing, and ids that are not ids.
    fake.remove(&mkey);
    refused(&b, "notes", &rid, "no_such_remote_backup");
    restore_originals();
    for bad in [
        "../../x",
        "20261112T040506Z",
        "20261112T040506Z.7f3a9c0a1b2c.1/../..",
    ] {
        let (rc, v) = b.json(&[
            "restore",
            "notes",
            "--from",
            &format!("remote:{bad}"),
            "--yes",
        ]);
        assert_eq!(
            (rc, v["error"]["kind"].as_str()),
            (1, Some("bad_remote_id")),
            "{bad}: {v}"
        );
    }

    // Another key: refused before the archive is downloaded. A manifest that lies about the key
    // id gets as far as the seal, which refuses it.
    let other_key = key_file(&s.0, "other-key");
    let c = Node::start(&s.0, "c", remote_args(&fake, Some(&other_key)), creds());
    c.deploy("notes");
    let before = fake.state.lock().unwrap().log.len();
    let v = refused(&c, "notes", &rid, "backup_key_mismatch");
    assert!(
        v["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("--backup-key-file"),
        "{v}"
    );
    let gets = fake.state.lock().unwrap().log[before..]
        .iter()
        .filter(|l| l.contains("/archive"))
        .count();
    assert_eq!(gets, 0, "the archive was fetched with the wrong key");
    let c_id = c.log_text().split("key id ").nth(1).unwrap()[..16].to_string();
    let mut m4 = m.clone();
    m4["encryption"]["key_id"] = c_id.into();
    fake.put(&mkey, serde_json::to_vec(&m4).unwrap());
    refused(&c, "notes", &rid, "remote_backup_corrupt");
    restore_originals();

    // A server without a key cannot read a sealed backup; a server with a key refuses an
    // unsealed one, which anyone with write access to the bucket could have planted.
    let d = Node::start(&s.0, "d", remote_args(&fake, None), creds());
    d.deploy("notes");
    refused(&d, "notes", &rid, "backup_key_mismatch");
    let plain = backup_uploaded(&d, "notes")["remote"]["remote_id"]
        .as_str()
        .unwrap()
        .to_string();
    refused(&b, "notes", &plain, "backup_key_mismatch");
}

// ---------------------------------------------------------------- hostile archives

/// A ustar header for a hand-built tar stream.
fn tar_header(name: &str, flag: u8, size: usize, link: &str) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    let oct = |h: &mut Vec<u8>, at: usize, len: usize, v: usize| {
        let s = format!("{:0w$o}\0", v, w = len - 1);
        h[at..at + len].copy_from_slice(s.as_bytes());
    };
    oct(&mut h, 100, 8, 0o644);
    oct(&mut h, 108, 8, 0);
    oct(&mut h, 116, 8, 0);
    oct(&mut h, 124, 12, size);
    oct(&mut h, 136, 12, 0);
    h[156] = flag;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].fill(b' ');
    let sum: usize = h.iter().map(|&b| b as usize).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

fn tar_entry(name: &str, flag: u8, data: &[u8], link: &str) -> Vec<u8> {
    let mut v = tar_header(name, flag, data.len(), link);
    v.extend_from_slice(data);
    v.resize(v.len().div_ceil(512) * 512, 0);
    v
}

/// Plant an unencrypted backup of `app` whose tar stream lists `entries` in its inner manifest
/// and holds `body` after it. Returns its remote id.
fn plant(fake: &Fake, app: &str, n: u32, entries: Value, body: &[u8]) -> String {
    let rid = format!("20261112T0400{n:02}Z.0123456789ab.1");
    let files = entries
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "file")
        .count();
    let bytes: u64 = entries
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["size"].as_u64().unwrap_or(0))
        .sum();
    let inner = serde_json::to_vec(&json!({
        "format": "spared-backup-contents-v1", "app": app, "remote_id": rid,
        "local_id": rid_dir(&rid).split('.').next().unwrap(), "spared_version": "test",
        "journal_clean": true, "files": files, "bytes": bytes, "entries": entries,
    }))
    .unwrap();
    let mut tar = tar_entry(".spared-manifest.json", b'0', &inner, "");
    tar.extend_from_slice(body);
    tar.extend_from_slice(&[0u8; 1024]);
    let z = zstd::stream::encode_all(&tar[..], 3).unwrap();
    let manifest = json!({
        "format": "spared-remote-backup-v1", "app": app, "remote_id": rid, "node": "0123456789ab",
        "epoch": 1, "created_at": "2026-11-12T04:00:00Z", "local_id": "x",
        "archive": {"bytes": z.len(), "sha256": sha_hex(&z)},
        "files": files, "bytes": bytes, "encryption": {"alg": "none"},
        "content_digest": sha_hex(&inner),
    });
    fake.put(&object(app, &rid, "archive"), z);
    fake.put(
        &object(app, &rid, "manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    );
    rid
}

fn file_entry(path: &str, data: &[u8]) -> Value {
    json!({"path": path, "type": "file", "size": data.len(), "sha256": sha_hex(data)})
}

#[test]
fn hostile_tar_streams_are_refused_and_nothing_escapes() {
    let s = scratch("hostile");
    let fake = Fake::start();
    let mut args = remote_args(&fake, None);
    args.extend(["--max-app-data-mb".into(), "1".into()]);
    let n = Node::start(&s.0, "n", args, creds());
    n.deploy("notes");
    std::fs::write(n.app_data("notes").join("app.db"), b"the real data").unwrap();
    let outside = n.data.join("backups");
    let refuse = |rid: &str, text: &str| {
        let v = refused(&n, "notes", rid, "remote_backup_corrupt");
        assert!(
            v["error"]["message"].as_str().unwrap().contains(text),
            "wanted {text:?}: {v}"
        );
        assert!(
            !outside.join("evil").exists()
                && !n.data.join("evil").exists()
                && !s.0.join("evil").exists()
        );
    };
    let x = b"x";
    refuse(
        &plant(
            &fake,
            "notes",
            1,
            json!([file_entry("../evil", x)]),
            &tar_entry("../evil", b'0', x, ""),
        ),
        "unsafe",
    );
    refuse(
        &plant(
            &fake,
            "notes",
            2,
            json!([file_entry("ok", x)]),
            &tar_entry("../../evil", b'0', x, ""),
        ),
        "entry 1",
    );
    refuse(
        &plant(
            &fake,
            "notes",
            3,
            json!([file_entry("/tmp/evil", x)]),
            &tar_entry("/tmp/evil", b'0', x, ""),
        ),
        "unsafe",
    );
    refuse(
        &plant(
            &fake,
            "notes",
            4,
            json!([file_entry("link", b"")]),
            &tar_entry("link", b'2', b"", "/etc/passwd"),
        ),
        "symlink",
    );
    refuse(
        &plant(
            &fake,
            "notes",
            5,
            json!([file_entry("hard", b"")]),
            &tar_entry("hard", b'1', b"", "app.db"),
        ),
        "hard link",
    );
    refuse(
        &plant(
            &fake,
            "notes",
            6,
            json!([file_entry("fifo", b"")]),
            &tar_entry("fifo", b'6', b"", ""),
        ),
        "fifo",
    );
    // A bomb: 64 MiB of zeros, a few KB compressed, on a server whose data cap is 1 MB.
    let zeros = vec![0u8; 64 << 20];
    refuse(
        &plant(
            &fake,
            "notes",
            7,
            json!([file_entry("bomb", &zeros)]),
            &tar_entry("bomb", b'0', &zeros, ""),
        ),
        "data cap",
    );
    // The same bytes, listed small: the entry does not match its listing.
    refuse(
        &plant(
            &fake,
            "notes",
            8,
            json!([file_entry("bomb", x)]),
            &tar_entry("bomb", b'0', &zeros, ""),
        ),
        "entry 1",
    );
    // A sound archive restores, so the refusals above are about their contents.
    let good = plant(
        &fake,
        "notes",
        9,
        json!([{"path": "d", "type": "dir"}, file_entry("d/f", b"fine")]),
        &[
            tar_entry("d/", b'5', b"", ""),
            tar_entry("d/f", b'0', b"fine", ""),
        ]
        .concat(),
    );
    let (rc, v) = n.json(&[
        "restore",
        "notes",
        "--from",
        &format!("remote:{good}"),
        "--yes",
    ]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(
        std::fs::read(n.app_data("notes").join("d/f")).unwrap(),
        b"fine"
    );
}

#[test]
fn kill_9_during_an_upload_or_a_fetch_leaves_a_clean_state() {
    let s = scratch("kill");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let mut n = Node::start(&s.0, "n", remote_args(&fake, Some(&key)), creds());
    n.deploy("notes");
    write_sample(&n.app_data("notes"), 3 << 20);
    let original = tree(&n.app_data("notes"));

    // During an upload.
    fake.set(|f| f.slow_put_ms = 300);
    let mut cli = n
        .cli_cmd(&["backup", "notes"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(|| fake.state.lock().unwrap().archive_put_started);
    n.restart();
    let _ = cli.kill();
    let _ = cli.wait();
    fake.clear_faults();
    let ids = local_ids(&n, "notes");
    assert_eq!(ids.len(), 1, "the local backup stands");
    let (st, v) = n.admin(
        "GET",
        &format!("/v1/apps/notes/backups/{}/remote", ids[0]),
        b"",
    );
    assert_eq!(st, 200);
    assert_eq!(
        (v["status"].as_str(), v["error"]["kind"].as_str()),
        (Some("failed"), Some("remote_interrupted")),
        "{v}"
    );
    assert!(
        fake.keys().iter().all(|k| !k.ends_with("manifest.json")),
        "{:?}",
        fake.keys()
    );
    let leftovers: Vec<_> = std::fs::read_dir(n.backups("notes").join(".remote"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .filter(|f| f.to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(n.log_text().contains("interrupted remote upload"));

    // During a fetch.
    let rid = backup_uploaded(&n, "notes")["remote"]["remote_id"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(
        n.app_data("notes").join("app.db"),
        b"changed after the backup",
    )
    .unwrap();
    let before = tree(&n.app_data("notes"));
    let ids = local_ids(&n, "notes");
    fake.set(|f| f.slow_get_ms = 300);
    let mut cli = n
        .cli_cmd(&[
            "restore",
            "notes",
            "--from",
            &format!("remote:{rid}"),
            "--yes",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(|| fake.state.lock().unwrap().archive_get_started);
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        std::fs::read_dir(n.backups("notes"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".tmp-fetch-")),
        "the fetch stages"
    );
    n.restart();
    let _ = cli.kill();
    let _ = cli.wait();
    fake.clear_faults();
    assert_untouched(&n, "notes", &before, &ids);
    let (_, v) = n.admin("GET", &format!("/v1/apps/notes/fetch/{rid}"), b"");
    assert_eq!(
        (v["status"].as_str(), v["error"]["kind"].as_str()),
        (Some("failed"), Some("remote_interrupted")),
        "{v}"
    );
    // And the same restore, run again, completes.
    let (rc, v) = n.json(&[
        "restore",
        "notes",
        "--from",
        &format!("remote:{rid}"),
        "--yes",
    ]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(tree(&n.app_data("notes")), original);
}

fn wait_for(cond: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < until, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_server_refuses_to_start_with_settings_that_cannot_work() {
    let s = scratch("refuse");
    let fake = Fake::start();
    let key = key_file(&s.0, "key");
    let start = |args: &[String], env: &[(&str, String)]| {
        let data = s.0.join("never");
        let o = cleared(&mut Command::new(SPARED))
            .args(["--json", "serve", "--data-dir"])
            .arg(&data)
            .args(["--listen", &format!("127.0.0.1:{}", free_port())])
            .args(["--admin-listen", &format!("127.0.0.1:{}", free_port())])
            .args(args)
            .env("HOME", &s.0)
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            .output()
            .unwrap();
        assert_eq!(
            o.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(!data.exists(), "a refused start wrote to the data dir");
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        v["error"].clone()
    };
    let mut no_key = remote_args(&fake, Some(&key));
    no_key.truncate(6);
    let e = start(&no_key, &creds());
    assert_eq!(e["kind"], "backup_key_required");
    assert!(
        e["fix"].as_str().unwrap().contains("spared backup-key new"),
        "{e}"
    );
    assert_eq!(
        start(&remote_args(&fake, Some(&key)), &[])["kind"],
        "no_backup_credentials"
    );
    let with = |flag: &str, value: &str| {
        let mut a = remote_args(&fake, Some(&key));
        let i = a.iter().position(|x| x == flag).unwrap();
        a[i + 1] = value.into();
        a
    };
    let e = start(
        &with("--backup-endpoint", "https://admin:hunter2@s3.example.com"),
        &creds(),
    );
    assert_eq!(e["kind"], "bad_backup_config");
    assert!(!e.to_string().contains("hunter2"), "{e}");
    assert_eq!(
        start(
            &with("--backup-endpoint", "http://s3.example.com"),
            &creds()
        )["kind"],
        "bad_backup_config"
    );
    assert_eq!(
        start(
            &with("--backup-endpoint", "https://s3.example.com/bucket"),
            &creds()
        )["kind"],
        "bad_backup_config"
    );
    assert_eq!(
        start(&with("--backup-to", "s3://bucket/../up"), &creds())["kind"],
        "bad_backup_config"
    );
    assert_eq!(
        start(&with("--backup-to", "s3://Bad_Bucket/x"), &creds())["kind"],
        "bad_backup_config"
    );
    let bad_key = s.0.join("bad-key");
    std::fs::write(&bad_key, "not a key").unwrap();
    assert_eq!(
        start(
            &with("--backup-key-file", bad_key.to_str().unwrap()),
            &creds()
        )["kind"],
        "bad_backup_config"
    );

    // Encryption off is a choice the server states at start.
    let n = Node::start(&s.0, "plain", remote_args(&fake, None), creds());
    assert!(n.log_text().contains("NOT encrypted"), "{}", n.log_text());
    let mode = std::fs::metadata(n.data.join("node-id")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.permissions().mode() & 0o777, 0o600);
    // A server without --backup-to says so when asked for remote backups.
    let off = Node::start(&s.0, "off", vec![], vec![]);
    let (rc, v) = off.json(&["backups", "notes", "--remote"]);
    assert_eq!(
        (rc, v["error"]["kind"].as_str()),
        (1, Some("remote_backups_off")),
        "{v}"
    );
}

#[test]
fn backup_key_new_prints_a_key_or_writes_a_private_file() {
    let s = scratch("keys");
    let o = cleared(&mut Command::new(SPARED))
        .args(["backup-key", "new"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success());
    assert_eq!(out.trim().len(), 64, "only the key on stdout: {out}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("off this machine"));
    let p = key_file(&s.0, "k");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let o = cleared(&mut Command::new(SPARED))
        .args(["--json", "backup-key", "new", "--out"])
        .arg(&p)
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(1),
        "an existing file is never written over"
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["error"]["kind"], "write_failed");
}

#[test]
fn the_fake_s3_verifier_agrees_with_the_botocore_vectors() {
    let v: Value = serde_json::from_str(include_str!("data/sigv4-vectors.json")).unwrap();
    for c in v["cases"].as_array().unwrap() {
        let url = c["url"].as_str().unwrap();
        let after = url.split_once("://").unwrap().1;
        let pq = &after[after.find('/').unwrap()..];
        let (path, query) = pq.split_once('?').unwrap_or((pq, ""));
        let host = after.split('/').next().unwrap();
        let mut headers: Vec<(String, String)> = c["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h[0].as_str().unwrap().to_ascii_lowercase(),
                    h[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        headers.push(("host".into(), host.into()));
        let sig = fake_s3::signature(
            c["method"].as_str().unwrap(),
            path,
            query,
            &headers,
            c["signed_headers"].as_str().unwrap(),
            c["payload_sha256"].as_str().unwrap(),
            c["amz_date"].as_str().unwrap(),
            c["region"].as_str().unwrap(),
            c["secret_access_key"].as_str().unwrap(),
        );
        assert_eq!(sig, c["signature"].as_str().unwrap(), "{}", c["name"]);
    }
}
