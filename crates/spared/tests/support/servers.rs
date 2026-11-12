//! Servers for the tests that run `spared serve` for real: scratch directories, free ports, a
//! server process per node with its own data directory and `$HOME`, plain HTTP, and the CLI run
//! against a node. Each test starts its own servers on scratch ports and temp directories.

#![allow(dead_code)]

use crate::fake_s3::{self, Fake};
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

pub const SPARED: &str = env!("CARGO_BIN_EXE_spared");

pub const HANDLER: &str = r#"(component
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
/// A component that answers every request with 200 and an empty body: it builds a response with
/// no headers and sets it. The import names the types `response-outparam.set` needs, error-code
/// included, as wasi:http 0.2 defines them.
pub const RESPONDER: &str = r#"(component
  (import "wasi:http/types@0.2.4" (instance $types
    (export "fields" (type $fields (sub resource)))
    (export "incoming-request" (type (sub resource)))
    (export "outgoing-response" (type $oresp (sub resource)))
    (export "response-outparam" (type $rout (sub resource)))
    (type $dns-def (record (field "rcode" (option string)) (field "info-code" (option u16))))
    (export "DNS-error-payload" (type $dns (eq $dns-def)))
    (type $tls-def (record (field "alert-id" (option u8)) (field "alert-message" (option string))))
    (export "TLS-alert-received-payload" (type $tls (eq $tls-def)))
    (type $fsp-def (record (field "field-name" (option string)) (field "field-size" (option u32))))
    (export "field-size-payload" (type $fsp (eq $fsp-def)))
    (type $ec-def (variant
      (case "DNS-timeout") (case "DNS-error" $dns) (case "destination-not-found")
      (case "destination-unavailable") (case "destination-IP-prohibited")
      (case "destination-IP-unroutable") (case "connection-refused")
      (case "connection-terminated") (case "connection-timeout")
      (case "connection-read-timeout") (case "connection-write-timeout")
      (case "connection-limit-reached") (case "TLS-protocol-error")
      (case "TLS-certificate-error") (case "TLS-alert-received" $tls)
      (case "HTTP-request-denied") (case "HTTP-request-length-required")
      (case "HTTP-request-body-size" (option u64)) (case "HTTP-request-method-invalid")
      (case "HTTP-request-URI-invalid") (case "HTTP-request-URI-too-long")
      (case "HTTP-request-header-section-size" (option u32))
      (case "HTTP-request-header-size" (option $fsp))
      (case "HTTP-request-trailer-section-size" (option u32))
      (case "HTTP-request-trailer-size" $fsp) (case "HTTP-response-incomplete")
      (case "HTTP-response-header-section-size" (option u32))
      (case "HTTP-response-header-size" $fsp)
      (case "HTTP-response-body-size" (option u64))
      (case "HTTP-response-trailer-section-size" (option u32))
      (case "HTTP-response-trailer-size" $fsp)
      (case "HTTP-response-transfer-coding" (option string))
      (case "HTTP-response-content-coding" (option string))
      (case "HTTP-response-timeout") (case "HTTP-upgrade-failed") (case "HTTP-protocol-error")
      (case "loop-detected") (case "configuration-error") (case "internal-error" (option string))))
    (export "error-code" (type $ec (eq $ec-def)))
    (export "[constructor]fields" (func (result (own $fields))))
    (export "[constructor]outgoing-response" (func (param "headers" (own $fields)) (result (own $oresp))))
    (export "[static]response-outparam.set"
      (func (param "param" (own $rout)) (param "response" (result (own $oresp) (error $ec)))))))
  (alias export $types "incoming-request" (type $ireq))
  (alias export $types "response-outparam" (type $rout))
  (core module $memory (memory (export "mem") 1))
  (core instance $mi (instantiate $memory))
  (alias core export $mi "mem" (core memory $mem))
  (core func $fields-new (canon lower (func $types "[constructor]fields")))
  (core func $resp-new (canon lower (func $types "[constructor]outgoing-response")))
  (core func $set (canon lower (func $types "[static]response-outparam.set") (memory $mem)))
  (core module $m
    (import "h" "fields" (func $fields (result i32)))
    (import "h" "resp" (func $resp (param i32) (result i32)))
    (import "h" "set" (func $set (param i32 i32 i32 i32 i64 i32 i32 i32 i32)))
    (func (export "handle") (param i32 i32)
      local.get 1
      i32.const 0
      call $fields
      call $resp
      i32.const 0
      i64.const 0
      i32.const 0
      i32.const 0
      i32.const 0
      i32.const 0
      call $set))
  (core instance $hi (instantiate $m (with "h" (instance
    (export "fields" (func $fields-new))
    (export "resp" (func $resp-new))
    (export "set" (func $set))))))
  (func $handle (param "request" (own $ireq)) (param "response-out" (own $rout))
    (canon lift (core func $hi "handle")))
  (instance $h (export "handle" (func $handle)))
  (export "wasi:http/incoming-handler@0.2.4" (instance $h)))"#;

pub struct Scratch(pub PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn scratch(tag: &str) -> Scratch {
    let d = std::env::temp_dir().join(format!("spared-remote-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Scratch(d)
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Every variable a server or the CLI could pick up from the test runner's environment.
pub const CLEARED: [&str; 16] = [
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

pub fn cleared(cmd: &mut Command) -> &mut Command {
    for v in CLEARED {
        cmd.env_remove(v);
    }
    cmd
}

/// The environment a test server gets: the fake's credentials.
pub fn creds() -> Vec<(&'static str, String)> {
    vec![
        ("AWS_ACCESS_KEY_ID", fake_s3::ACCESS_KEY.into()),
        ("AWS_SECRET_ACCESS_KEY", fake_s3::SECRET_KEY.into()),
        ("AWS_SESSION_TOKEN", fake_s3::SESSION_TOKEN.into()),
    ]
}

/// Remote backup flags for a server that uploads to `fake` under `prefix`.
pub fn remote_args(fake: &Fake, key: Option<&Path>) -> Vec<String> {
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

pub struct Node {
    pub root: PathBuf,
    pub data: PathBuf,
    pub public: u16,
    pub admin: u16,
    pub args: Vec<String>,
    pub env: Vec<(&'static str, String)>,
    pub child: Option<Child>,
    pub token: String,
    pub log: PathBuf,
}

impl Node {
    pub fn start(
        root: &Path,
        tag: &str,
        args: Vec<String>,
        env: Vec<(&'static str, String)>,
    ) -> Node {
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

    pub fn spawn(&mut self) {
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

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub fn admin(&self, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
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

    pub fn deploy(&self, name: &str) {
        self.deploy_wat(name, HANDLER);
    }

    /// Deploy the component `wat` as `name`, with `data = true`, and make its data directory.
    pub fn deploy_wat(&self, name: &str, wat: &str) {
        let wasm = wat::parse_str(wat).unwrap();
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

    pub fn app_data(&self, name: &str) -> PathBuf {
        self.data.join("apps").join(name).join("data")
    }

    pub fn backups(&self, name: &str) -> PathBuf {
        self.data.join("backups").join(name)
    }

    pub fn cli_cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(SPARED);
        cleared(&mut c)
            .args(args)
            .args(["--server", &format!("http://127.0.0.1:{}", self.admin)])
            .env("HOME", self.root.join("home"))
            .env("SPARED_TOKEN", &self.token);
        c
    }

    pub fn cli(&self, args: &[&str]) -> Output {
        self.cli_cmd(args).output().unwrap()
    }

    /// `spared --json <args>`: exit code and the JSON it printed.
    pub fn json(&self, args: &[&str]) -> (i32, Value) {
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

    pub fn kill9(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    pub fn restart(&mut self) {
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
pub fn http(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let (status, _, body) = http_full(port, method, path, host, headers, body);
    (status, body)
}

/// A plain HTTP/1.1 request on its own connection: status, response head (lowercase) and body.
pub fn http_full(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return (0, String::new(), vec![]);
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
        return (0, String::new(), vec![]);
    }
    let mut out = vec![];
    let _ = s.read_to_end(&mut out);
    let Some(split) = out.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (0, String::new(), vec![]);
    };
    let head = String::from_utf8_lossy(&out[..split]).to_ascii_lowercase();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head, out[split + 4..].to_vec())
}

/// Every entry under `p`, sorted: (relative path, kind and bytes).
pub fn tree(p: &Path) -> Vec<(String, Option<Vec<u8>>)> {
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

pub fn key_file(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    let o = cleared(&mut Command::new(SPARED))
        .args(["backup-key", "new", "--out"])
        .arg(&p)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    p
}

pub fn local_ids(n: &Node, app: &str) -> Vec<String> {
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
pub fn backup_uploaded(n: &Node, app: &str) -> Value {
    let (rc, v) = n.json(&["backup", app]);
    assert_eq!(rc, 0, "{v}\n{}", n.log_text());
    assert_eq!(v["remote"]["status"], "uploaded", "{v}");
    v
}

pub fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

pub fn walk_files(p: &Path) -> Vec<PathBuf> {
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

pub fn wait_for(cond: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < until, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A ustar header for a hand-built tar stream.
pub fn tar_header(name: &str, flag: u8, size: usize, link: &str) -> Vec<u8> {
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

pub fn tar_entry(name: &str, flag: u8, data: &[u8], link: &str) -> Vec<u8> {
    let mut v = tar_header(name, flag, data.len(), link);
    v.extend_from_slice(data);
    v.resize(v.len().div_ceil(512) * 512, 0);
    v
}

pub fn file_entry(path: &str, data: &[u8]) -> Value {
    serde_json::json!({"path": path, "type": "file", "size": data.len(), "sha256": sha_hex(data)})
}

/// An unencrypted backup of `app` under the remote id `rid` whose tar stream lists `entries` in
/// its inner manifest and holds `body` after it: the archive and its manifest.
pub fn planted(app: &str, rid: &str, entries: Value, body: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (dir, epoch) = rid.rsplit_once('.').unwrap();
    let (local, node) = dir.split_once('.').unwrap();
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
    let inner = serde_json::to_vec(&serde_json::json!({
        "format": "spared-backup-contents-v1", "app": app, "remote_id": rid,
        "local_id": local, "spared_version": "test",
        "journal_clean": true, "files": files, "bytes": bytes, "entries": entries,
    }))
    .unwrap();
    let mut tar = tar_entry(".spared-manifest.json", b'0', &inner, "");
    tar.extend_from_slice(body);
    tar.extend_from_slice(&[0u8; 1024]);
    let z = zstd::stream::encode_all(&tar[..], 3).unwrap();
    let manifest = serde_json::json!({
        "format": "spared-remote-backup-v1", "app": app, "remote_id": rid, "node": node,
        "epoch": epoch.parse::<u64>().unwrap(), "created_at": "2026-11-12T04:00:00Z", "local_id": local,
        "archive": {"bytes": z.len(), "sha256": sha_hex(&z)},
        "files": files, "bytes": bytes, "encryption": {"alg": "none"},
        "content_digest": sha_hex(&inner),
    });
    (z, serde_json::to_vec(&manifest).unwrap())
}
