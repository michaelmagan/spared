//! Off-machine backups to S3-compatible storage (slice 1): after a local backup, the server writes
//! its archive (`archive.rs`, sealed by `seal.rs` unless encryption is off) and uploads it with
//! the client in `s3.rs`; `spared restore --from remote:<id>` fetches one back into a new local
//! backup, which the ordinary restore then puts in place.
//!
//! Objects, under `<prefix>/spared/v1/<app>/e<epoch:08>/<local id>.<node>/`: `archive`, then
//! `manifest.json`, written last, so a backup without its manifest is never listed. The remote id
//! is `<local id>.<node>.<epoch>`. spared never deletes a remote object: retention is the
//! bucket's lifecycle rule (docs/operating.md).
//!
//! Epochs are advisory. An app starts at epoch 1 on every server. A restore from the bucket
//! raises this server's epoch for the app (`<data-dir>/backups/<app>/.epoch`) to one above the
//! highest epoch in the bucket, and before each upload a server looks for an epoch above its own:
//! if there is one, the app was restored somewhere else, the upload is refused with
//! `remote_superseded` and the server freezes the app. That detects a second writer after the
//! fact; it does not prevent one (a fencing record written with conditional PUTs would).
//!
//! The jobs run on the blocking pool and block on the runtime's handle for each network call, so
//! archive and file work never runs on the runtime's workers (an upload's body is read from its
//! file by a thread of its own, see `s3.rs`). Their state is a
//! JSON file per job under `<data-dir>/backups/<app>/.remote/`, which `spared backups` and the
//! CLI's wait read, and which a restart marks `failed` when the job was cut off.

pub mod archive;
pub mod s3;
pub mod seal;

use crate::backup;
use crate::datadir::DataDirError;
use s3::{Payload, S3Error};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The largest epoch: eight digits, as in the object keys and remote ids.
pub const MAX_EPOCH: u32 = 99_999_999;
/// The file under `<data-dir>/backups/<app>/` that holds this server's epoch for the app. A dot
/// name is never a backup id, so listing and retention pass it by, and `spared rm` keeps it with
/// the backups.
pub const EPOCH_FILE: &str = ".epoch";
/// How many epochs a search for the highest one steps through before it gives up.
const MAX_EPOCH_STEPS: usize = 1000;
/// How many keys one look for a newer epoch reads.
const EPOCH_LISTING: usize = 1000;
/// The largest archive one upload sends (one PUT).
pub const MAX_OBJECT: u64 = 4 << 30;
const MANIFEST_FORMAT: &str = "spared-remote-backup-v1";
const NODE_ID_FILE: &str = "node-id";
pub const REMOTE_DIR: &str = ".remote";
/// The fetch's staging directory under `<data-dir>/backups/<app>/`.
pub const FETCH_TMP: &str = ".tmp-fetch-";
const MAX_MANIFEST: usize = 64 << 10;
const MAX_LISTED: usize = 100_000;

/// The serve flags of remote backups, as given.
pub struct Flags {
    pub to: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub addressing: Option<s3::Addressing>,
    pub key_file: Option<PathBuf>,
    pub encryption_off: bool,
    pub allow_http: bool,
}

/// Remote backup settings, checked at start.
pub struct Settings {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: s3::Endpoint,
    pub region: String,
    pub addressing: s3::Addressing,
    pub creds: s3::Credentials,
    pub key: Option<seal::BackupKey>,
}

fn bad(message: String, fix: &str) -> DataDirError {
    DataDirError {
        kind: "bad_backup_config",
        message,
        fix: fix.into(),
    }
}

/// Read a secret file: its contents without the trailing newline. Warns when other users can
/// read it.
fn read_secret_file(var: &str, path: &str) -> Result<String, DataDirError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        bad(
            format!("cannot read {var} ({path}): {e}"),
            "point it at a readable file, or set the variable without _FILE",
        )
    })?;
    if std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o004 != 0) {
        eprintln!("warning: {path} ({var}) is readable by every local user. chmod 600 it");
    }
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

/// `NAME` or `NAME_FILE` from the environment (never both).
fn env_or_file(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
) -> Result<Option<String>, DataDirError> {
    let file_var = format!("{name}_FILE");
    match (
        env(name).filter(|v| !v.is_empty()),
        env(&file_var).filter(|v| !v.is_empty()),
    ) {
        (Some(_), Some(_)) => Err(bad(
            format!("both {name} and {file_var} are set"),
            &format!("set only one of {name} and {file_var}"),
        )),
        (Some(v), None) => Ok(Some(v)),
        (None, Some(path)) => read_secret_file(&file_var, &path).map(Some),
        (None, None) => Ok(None),
    }
}

impl Settings {
    /// Check the flags and read the credentials and the key. `Ok(None)` when `--backup-to` is not
    /// given. `env` reads the environment (a parameter so tests need not change it).
    pub fn from_flags(
        f: Flags,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Settings>, DataDirError> {
        let Some(to) = f.to.filter(|t| !t.is_empty()) else {
            if f.endpoint.is_some() || f.key_file.is_some() {
                return Err(bad(
                    "--backup-endpoint or --backup-key-file is set without --backup-to".into(),
                    "add --backup-to s3://bucket/prefix, or drop the other backup flags",
                ));
            }
            return Ok(None);
        };
        let (bucket, prefix) = s3::parse_target(&to).map_err(|m| {
            bad(
                m,
                "use --backup-to s3://<bucket>/<prefix>, for example s3://my-backups/spared",
            )
        })?;
        let region = f
            .region
            .filter(|r| !r.is_empty())
            .or_else(|| env("AWS_REGION").filter(|r| !r.is_empty()))
            .unwrap_or_else(|| "us-east-1".into());
        if !region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || region.len() > 64
        {
            return Err(bad(
                format!("'{region}' is not a region name"),
                "pass --backup-region like us-east-1, or auto for R2",
            ));
        }
        let custom = f.endpoint.as_deref().is_some_and(|e| !e.is_empty());
        let endpoint = match f.endpoint.filter(|e| !e.is_empty()) {
            Some(e) => s3::parse_endpoint(&e, f.allow_http).map_err(|m| {
                bad(
                    format!("--backup-endpoint: {m}"),
                    "pass --backup-endpoint https://<host>[:port], with no user, path or query",
                )
            })?,
            None => s3::Endpoint::aws(&region),
        };
        let addressing = f.addressing.unwrap_or(if custom {
            s3::Addressing::Path
        } else {
            s3::Addressing::Virtual
        });
        if addressing == s3::Addressing::Virtual && bucket.contains('.') && endpoint.https {
            return Err(bad(
                format!(
                    "the bucket '{bucket}' has a dot, which breaks TLS with virtual-host addressing"
                ),
                "pass --backup-addressing path",
            ));
        }
        let id = env_or_file(env, "AWS_ACCESS_KEY_ID")?;
        let secret = env_or_file(env, "AWS_SECRET_ACCESS_KEY")?;
        let token = env_or_file(env, "AWS_SESSION_TOKEN")?;
        let (Some(id), Some(secret)) = (id, secret) else {
            return Err(DataDirError {
                kind: "no_backup_credentials",
                message: "--backup-to is set, but AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not".into(),
                fix: "set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (or AWS_ACCESS_KEY_ID_FILE and AWS_SECRET_ACCESS_KEY_FILE) in the server's environment. Credentials are never read from flags or spared.toml".into(),
            });
        };
        let key = match (f.key_file, f.encryption_off) {
            (Some(_), true) => {
                return Err(bad(
                    "--backup-key-file and --backup-encryption off are both given".into(),
                    "drop one: backups are encrypted with the key, or not at all",
                ));
            }
            (None, true) => None,
            (None, false) => {
                return Err(DataDirError {
                    kind: "backup_key_required",
                    message: "--backup-to is set without a backup key: remote backups are encrypted by default".into(),
                    fix: "run `spared backup-key new > /path/to/key`, keep a copy of that key off this machine, and pass --backup-key-file /path/to/key (or SPARED_BACKUP_KEY_FILE). Or pass --backup-encryption off to upload unencrypted archives".into(),
                });
            }
            (Some(path), false) => {
                let p = path.display().to_string();
                let text = read_secret_file("--backup-key-file", &p)?;
                Some(seal::BackupKey::parse(&text).map_err(|m| {
                    bad(
                        format!("{p}: {m}"),
                        "make a key with `spared backup-key new` and point --backup-key-file at it",
                    )
                })?)
            }
        };
        Ok(Some(Settings {
            bucket,
            prefix,
            endpoint,
            region,
            addressing,
            creds: s3::Credentials {
                access_key_id: id,
                secret_access_key: s3::Secret::new(secret),
                session_token: token.map(s3::Secret::new),
            },
            key,
        }))
    }
}

fn create_private_dir(p: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
}

/// This node's id: 6 random bytes in hex, made once into `<data-dir>/node-id` (mode 0600), so two
/// servers never write the same object names.
fn node_id(data_dir: &Path) -> std::io::Result<String> {
    let p = data_dir.join(NODE_ID_FILE);
    let valid = |s: &str| {
        s.len() == 12
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    match std::fs::read_to_string(&p) {
        Ok(t) if valid(t.trim()) => return Ok(t.trim().to_string()),
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "{} is not a node id (12 lowercase hex digits). Restore it from a copy of the data dir, or delete it to get a new id (backups uploaded under the old id stay where they are)",
                p.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut b = [0u8; 6];
    seal::random(&mut b)?;
    let id = archive::hex(&b);
    let tmp = data_dir.join(format!("{NODE_ID_FILE}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(format!("{id}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &p)?;
    Ok(id)
}

/// `20261112T040506Z.7f3a9c0a1b2c.1`: the local id, the node and the epoch. Parsed by bytes, so
/// a remote id from a request can only ever name an object under the app's prefix.
pub fn parse_remote_id(rid: &str) -> Option<(&str, &str, u32)> {
    let mut parts = rid.split('.');
    let (ts, node, epoch) = (parts.next()?, parts.next()?, parts.next()?);
    let node_ok = node.len() == 12
        && node
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let epoch_ok = !epoch.is_empty()
        && epoch.len() <= 8
        && epoch.bytes().all(|b| b.is_ascii_digit())
        && !epoch.starts_with('0');
    (parts.next().is_none()
        && backup::valid_id(ts)
        && !backup::is_pre_restore(ts)
        && node_ok
        && epoch_ok)
        .then(|| (ts, node, epoch.parse().unwrap_or(0)))
}

fn now_text() -> String {
    backup::created_at(&backup::timestamp_id(crate::server::unix_now()))
}

fn s3_err(kind: &'static str, message: String, fix: &str) -> S3Error {
    S3Error {
        kind,
        message,
        fix: fix.into(),
    }
}

const CORRUPT_FIX: &str = "the backup in the bucket was changed or damaged after it was written. Nothing on this server was changed. List the others with `spared backups <name> --remote` and restore an older one";

fn corrupt(message: String) -> S3Error {
    s3_err("remote_backup_corrupt", message, CORRUPT_FIX)
}

fn local_failure(kind: &'static str, what: &str, e: impl std::fmt::Display) -> S3Error {
    s3_err(
        kind,
        format!("{what}: {e}"),
        "check disk space and permissions under the data dir, then try again",
    )
}

/// Write `v` to `path` through a temporary file and a rename (mode 0600).
fn write_json(path: &Path, v: &Value) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        create_private_dir(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(format!("{v}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn epoch_path(data_dir: &Path, app: &str) -> PathBuf {
    data_dir.join("backups").join(app).join(EPOCH_FILE)
}

/// This server's epoch for `app`: 1 when no restore from the bucket ever raised it. An epoch file
/// that cannot be read is an error, not a 1, so a damaged file never lets an upload go out under
/// an epoch the server does not hold.
pub fn local_epoch(data_dir: &Path, app: &str) -> Result<u32, String> {
    let p = epoch_path(data_dir, app);
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v["epoch"].as_u64())
        .and_then(|e| u32::try_from(e).ok())
        .filter(|e| (1..=MAX_EPOCH).contains(e))
        .ok_or_else(|| {
            format!(
                "{} is not an epoch file. Restore it from a copy of the data dir, or restore the app from the bucket again (`spared restore <name> --from remote:<id> --yes`), which writes a new one",
                p.display()
            )
        })
}

/// The epoch file's record (epoch, and the remote backup a restore raised it with), if any.
pub fn epoch_record(data_dir: &Path, app: &str) -> Option<Value> {
    read_json(&epoch_path(data_dir, app))
}

/// After a restore from the bucket: raise this server's epoch for `app` to `epoch` (never lower
/// it), and record which remote backup the data came from. Returns the epoch now held.
pub fn raise_epoch(
    data_dir: &Path,
    app: &str,
    epoch: u32,
    remote_id: &str,
    content_digest: &str,
) -> std::io::Result<u32> {
    let now = local_epoch(data_dir, app).unwrap_or(1).max(epoch);
    write_json(
        &epoch_path(data_dir, app),
        &json!({
            "epoch": now,
            "restored_from": remote_id,
            "content_digest": content_digest,
            "at": now_text(),
        }),
    )?;
    Ok(now)
}

/// Drops the record of which remote backup this server's copy came from, and keeps the epoch.
/// A copy that was removed or replaced by another restore no longer holds that backup, so the
/// record must not vouch for it.
pub fn clear_restore_record(data_dir: &Path, app: &str) -> std::io::Result<()> {
    let p = epoch_path(data_dir, app);
    let Some(rec) = read_json(&p) else {
        return Ok(());
    };
    let Some(epoch) = rec["epoch"].as_u64() else {
        return Ok(());
    };
    if rec.get("restored_from").is_none() && rec.get("content_digest").is_none() {
        return Ok(());
    }
    write_json(&p, &json!({"epoch": epoch}))
}

/// The epoch of a key `<base>e<epoch:08>/<ts>.<node>/<leaf>`, and its node, when the key has
/// that shape.
fn key_epoch(base: &str, key: &str) -> Option<(u32, String)> {
    let rest = key.strip_prefix(base)?;
    let mut parts = rest.split('/');
    let (e, dir) = (parts.next()?, parts.next()?);
    let epoch = e
        .strip_prefix('e')
        .filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))?
        .parse::<u32>()
        .ok()?;
    let rid = format!("{dir}.{epoch}");
    parse_remote_id(&rid).map(|(_, node, _)| (epoch, node.to_string()))
}

/// Removes a file or a tree when dropped.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = backup::remove_tree(&self.0);
    }
}

/// What a fetch needs from the server: the app's name lock (held while the new local id is
/// taken), the retention limit, and the data cap.
pub struct FetchCtx {
    pub name_lock: Arc<tokio::sync::Mutex<()>>,
    pub keep: usize,
    pub max_bytes: u64,
}

pub struct Remote {
    s3: s3::Client,
    prefix: String,
    key: Option<seal::BackupKey>,
    node: String,
    data_dir: PathBuf,
    /// One lock per app: its uploads and fetches run one at a time.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// (app, local id) of each backup an upload is reading; retention leaves them alone.
    pinned: Mutex<HashSet<(String, String)>>,
    /// (app, remote id) of each fetch in progress.
    fetching: Mutex<HashSet<(String, String)>>,
}

impl Remote {
    pub fn new(s: Settings, data_dir: &Path) -> std::io::Result<Remote> {
        let node = node_id(data_dir)?;
        Ok(Remote {
            s3: s3::Client::new(s.endpoint, s.bucket, s.region, s.addressing, s.creds)?,
            prefix: s.prefix,
            key: s.key,
            node,
            data_dir: data_dir.to_path_buf(),
            locks: Mutex::new(HashMap::new()),
            pinned: Mutex::new(HashSet::new()),
            fetching: Mutex::new(HashSet::new()),
        })
    }

    pub fn target(&self) -> String {
        if self.prefix.is_empty() {
            format!("s3://{}", self.s3.bucket)
        } else {
            format!("s3://{}/{}", self.s3.bucket, self.prefix)
        }
    }

    /// One line for the start-up log. Never a secret.
    pub fn describe(&self) -> String {
        format!(
            "remote backups: {} at {} (region {}, {} addressing), node {}, {}",
            self.target(),
            self.s3.endpoint.url(),
            self.s3.region,
            match self.s3.addressing {
                s3::Addressing::Path => "path",
                s3::Addressing::Virtual => "virtual-host",
            },
            self.node,
            match &self.key {
                Some(k) => format!("encrypted (key id {})", k.key_id()),
                None => "NOT encrypted (--backup-encryption off)".into(),
            }
        )
    }

    pub fn remote_id(&self, local_id: &str, epoch: u32) -> String {
        format!("{local_id}.{}.{epoch}", self.node)
    }

    /// This server's node id.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// What identifies the bucket and the key, for `GET /v1/server`: nothing secret. Two servers
    /// with the same bucket, prefix and key id read each other's backups.
    pub fn identity(&self) -> Value {
        json!({
            "target": self.target(),
            "bucket": self.s3.bucket,
            "prefix": self.prefix,
            "endpoint": self.s3.endpoint.url(),
            "region": self.s3.region,
            "encrypted": self.key.is_some(),
            "key_id": self.key.as_ref().map(|k| k.key_id()),
        })
    }

    /// The lowest epoch above `epoch` that has an object of `app` in the bucket, and the node
    /// that wrote it. One listing that starts after every key of epoch `epoch` (`~` sorts after
    /// every character a key holds).
    pub async fn newer_epoch(
        &self,
        app: &str,
        epoch: u32,
    ) -> Result<Option<(u32, String)>, S3Error> {
        let base = self.app_base(app);
        let after = format!("{base}e{epoch:08}/~");
        let (objects, _) = self
            .s3
            .list_after(&format!("{base}e"), Some(&after), EPOCH_LISTING)
            .await?;
        Ok(objects
            .iter()
            .filter_map(|o| key_epoch(&base, &o.key))
            .find(|(e, _)| *e > epoch))
    }

    /// The highest epoch of `app` in the bucket, at least `from`: a step from epoch to epoch.
    async fn highest_epoch(&self, app: &str, from: u32) -> Result<u32, S3Error> {
        let mut e = from;
        for _ in 0..MAX_EPOCH_STEPS {
            match self.newer_epoch(app, e).await? {
                Some((n, _)) => e = n,
                None => return Ok(e),
            }
        }
        Err(s3_err(
            "remote_rejected",
            format!("the bucket holds more than {MAX_EPOCH_STEPS} epochs of '{app}'"),
            "list them with `spared backups <name> --remote`. That many restores of one app is not something spared makes",
        ))
    }

    fn app_base(&self, app: &str) -> String {
        if self.prefix.is_empty() {
            format!("spared/v1/{app}/")
        } else {
            format!("{}/spared/v1/{app}/", self.prefix)
        }
    }

    fn object(&self, app: &str, ts: &str, node: &str, epoch: u32, leaf: &str) -> String {
        format!("{}e{epoch:08}/{ts}.{node}/{leaf}", self.app_base(app))
    }

    fn root(&self, app: &str) -> PathBuf {
        self.data_dir.join("backups").join(app)
    }

    fn lock(&self, app: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(app.to_string())
            .or_default()
            .clone()
    }

    /// Local backup ids of `app` that an upload is reading.
    pub fn pinned(&self, app: &str) -> HashSet<String> {
        self.pinned
            .lock()
            .unwrap()
            .iter()
            .filter(|(a, _)| a == app)
            .map(|(_, id)| id.clone())
            .collect()
    }

    fn encryption_json(&self, salt: &[u8; seal::SALT_LEN]) -> Value {
        match &self.key {
            Some(k) => {
                json!({"alg": seal::ALG, "key_id": k.key_id(), "salt": seal::salt_hex(salt)})
            }
            None => json!({"alg": "none"}),
        }
    }

    /// The upload state of a local backup, if it was ever uploaded or fetched.
    pub fn upload_status(data_dir: &Path, app: &str, local_id: &str) -> Option<Value> {
        read_json(
            &data_dir
                .join("backups")
                .join(app)
                .join(REMOTE_DIR)
                .join(format!("{local_id}.json")),
        )
    }

    /// Upload the local backup `local_id` of `app` in the background, once the app's earlier
    /// uploads are done, under this server's epoch for the app. Returns the state the caller
    /// reports now. `on_superseded` runs (and is waited for) when the bucket holds a newer epoch,
    /// before the failure is recorded: the server freezes the app there.
    pub fn start_upload(
        self: &Arc<Self>,
        app: &str,
        local_id: &str,
        max_bytes: u64,
        on_superseded: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ) -> Value {
        let path = self
            .root(app)
            .join(REMOTE_DIR)
            .join(format!("{local_id}.json"));
        let epoch = match local_epoch(&self.data_dir, app) {
            Ok(e) => e,
            Err(m) => {
                let state = json!({
                    "local_id": local_id,
                    "target": self.target(),
                    "status": "failed",
                    "error": {"kind": "remote_archive_failed", "message": m, "fix": "the local backup stands. Fix the epoch file as the message says, then take a new backup"},
                });
                let _ = write_json(&path, &state);
                return state;
            }
        };
        let rid = self.remote_id(local_id, epoch);
        let state = json!({
            "local_id": local_id,
            "remote_id": rid,
            "epoch": epoch,
            "target": self.target(),
            "status": "uploading",
            "started_at": now_text(),
            "encrypted": self.key.is_some(),
        });
        if let Err(e) = write_json(&path, &state) {
            let mut failed = state.clone();
            failed["status"] = "failed".into();
            failed["error"] = json!({"kind": "remote_archive_failed", "message": format!("cannot record the upload: {e}"), "fix": "check disk space and permissions under the data dir"});
            return failed;
        }
        self.pinned
            .lock()
            .unwrap()
            .insert((app.to_string(), local_id.to_string()));
        let (me, app, local_id) = (self.clone(), app.to_string(), local_id.to_string());
        let lock = self.lock(&app);
        let rt = tokio::runtime::Handle::current();
        let started = state.clone();
        tokio::spawn(async move {
            let _one_at_a_time = lock.lock_owned().await;
            let job = {
                let (me, app, local_id) = (me.clone(), app.clone(), local_id.clone());
                tokio::task::spawn_blocking(move || {
                    me.upload(&rt, &app, &local_id, epoch, max_bytes)
                })
            };
            let result = job.await.unwrap_or_else(|e| {
                Err(s3_err(
                    "remote_upload_failed",
                    format!("the upload task failed: {e}"),
                    "report this",
                ))
            });
            me.pinned
                .lock()
                .unwrap()
                .remove(&(app.clone(), local_id.clone()));
            if matches!(&result, Err(e) if e.kind == "remote_superseded") {
                on_superseded.await;
            }
            let mut done = started;
            done["finished_at"] = now_text().into();
            match result {
                Ok(extra) => {
                    done["status"] = "uploaded".into();
                    for (k, v) in extra.as_object().into_iter().flatten() {
                        done[k] = v.clone();
                    }
                    eprintln!(
                        "app {app}: uploaded backup {local_id} as {} ({} bytes)",
                        done["remote_id"].as_str().unwrap_or(""),
                        done["archive_bytes"]
                    );
                }
                Err(e) => {
                    eprintln!("app {app}: upload of backup {local_id} failed: {e}");
                    done["status"] = "failed".into();
                    done["error"] = json!({"kind": e.kind, "message": e.message, "fix": e.fix});
                }
            }
            if let Err(e) = write_json(&path, &done) {
                eprintln!("app {app}: cannot record the upload of {local_id}: {e}");
            }
        });
        state
    }

    /// Build the archive of `<data-dir>/backups/<app>/<local_id>/` in a temporary file and upload
    /// it, then the manifest. Runs on the blocking pool.
    fn upload(
        &self,
        rt: &tokio::runtime::Handle,
        app: &str,
        local_id: &str,
        epoch: u32,
        max_bytes: u64,
    ) -> Result<Value, S3Error> {
        let rid = self.remote_id(local_id, epoch);
        let root = self.root(app);
        let src = root.join(local_id);
        if !src.is_dir() {
            return Err(s3_err(
                "no_such_backup",
                format!("the local backup {local_id} is gone"),
                "take a new backup with `spared backup <name>`",
            ));
        }
        // Advisory: another server may restore between this listing and the upload below.
        if let Some((newer, node)) = rt.block_on(self.newer_epoch(app, epoch))? {
            let own = if node == self.node {
                " (that is this server's own node id: its epoch file, <data-dir>/backups/<app>/.epoch, was lost or replaced)"
            } else {
                ""
            };
            return Err(s3_err(
                "remote_superseded",
                format!(
                    "the bucket holds backups of '{app}' at epoch {newer} from node {node}{own}, and this server holds epoch {epoch}: the app was restored on another server, so the copy here is stale. Nothing was uploaded, and this server froze the app"
                ),
                "if the app now lives on the other server, remove it here with `spared rm <name> --yes` once nothing points at this server. If this copy holds writes the other one lacks, copy them over by hand: there is no merge. `spared thaw <name>` serves it here again, and the next upload is refused again (docs/operating.md#move-an-app-to-another-server)",
            ));
        }
        let tmp = root
            .join(REMOTE_DIR)
            .join(format!(".tmp-{local_id}.archive"));
        let _cleanup = Cleanup(tmp.clone());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| {
                local_failure("remote_archive_failed", "cannot create the archive file", e)
            })?;
        let salt = seal::new_salt()
            .map_err(|e| local_failure("remote_archive_failed", "no random salt", e))?;
        let names = archive::Names {
            app,
            remote_id: &rid,
            local_id,
        };
        let built = archive::build(
            &src,
            file,
            &names,
            self.key.as_ref().map(|k| (k, &salt)),
            &archive::Limits::new(max_bytes),
            MAX_OBJECT,
        )
        .map_err(|e| {
            if archive::is_too_big(&e) {
                s3_err(
                    "remote_backup_too_big",
                    format!("the archive of {local_id} is too big for one upload in this version: {e} (the object limit is {} GiB)", MAX_OBJECT >> 30),
                    "the local backup stands. Shrink the app's data, or copy <data-dir>/backups/ off the machine by other means",
                )
            } else {
                local_failure("remote_archive_failed", "writing the archive failed", e)
            }
        })?;
        let sha = archive::hex(&built.sha256);
        let akey = self.object(app, local_id, &self.node, epoch, "archive");
        let mkey = self.object(app, local_id, &self.node, epoch, "manifest.json");
        rt.block_on(self.s3.put(
            &akey,
            Payload::File {
                path: &tmp,
                len: built.bytes,
                sha256: &sha,
            },
            "application/octet-stream",
        ))?;
        let digest = archive::hex(&built.digest);
        let manifest = json!({
            "format": MANIFEST_FORMAT,
            "app": app,
            "remote_id": rid,
            "node": self.node,
            "epoch": epoch,
            "created_at": now_text(),
            "local_id": local_id,
            "archive": {"bytes": built.bytes, "sha256": sha},
            "files": built.contents.files,
            "bytes": built.contents.bytes,
            "journal_clean": built.contents.journal_clean,
            "encryption": self.encryption_json(&salt),
            "content_digest": digest,
            "spared_version": crate::VERSION,
        });
        let body = bytes::Bytes::from(serde_json::to_vec_pretty(&manifest).unwrap_or_default());
        // spared never deletes in the bucket: if this fails, the archive stays without its
        // manifest, is never listed or restored, and the bucket's lifecycle rule removes it.
        rt.block_on(self.s3.put(&mkey, Payload::Bytes(body), "application/json"))?;
        Ok(json!({
            "archive_bytes": built.bytes,
            "archive_sha256": sha,
            "content_digest": digest,
            "files": built.contents.files,
            "bytes": built.contents.bytes,
        }))
    }

    /// The app's backups in the bucket, newest first. Only those whose manifest exists are listed.
    pub async fn list(&self, app: &str) -> Result<Value, S3Error> {
        let base = self.app_base(app);
        let (objects, truncated) = self.s3.list(&base, MAX_LISTED).await?;
        // remote id -> (epoch, ts, node, has manifest, archive size)
        let mut seen: HashMap<String, (u32, String, String, bool, Option<u64>)> = HashMap::new();
        for o in objects {
            let Some(rest) = o.key.strip_prefix(&base) else {
                continue;
            };
            let mut parts = rest.split('/');
            let (Some(e), Some(dir), Some(leaf), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Some(epoch) = e
                .strip_prefix('e')
                .filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|d| d.parse::<u32>().ok())
            else {
                continue;
            };
            let rid = format!("{dir}.{epoch}");
            let Some((ts, node, _)) = parse_remote_id(&rid) else {
                continue;
            };
            let entry = seen.entry(rid.clone()).or_insert((
                epoch,
                ts.to_string(),
                node.to_string(),
                false,
                None,
            ));
            match leaf {
                "manifest.json" => entry.3 = true,
                "archive" => entry.4 = Some(o.size),
                _ => {}
            }
        }
        let mut rows: Vec<_> = seen.into_iter().filter(|(_, v)| v.3).collect();
        rows.sort_by(|a, b| (b.1.0, &b.1.1, &b.1.2).cmp(&(a.1.0, &a.1.1, &a.1.2)));
        let items: Vec<Value> = rows
            .into_iter()
            .map(|(rid, (epoch, ts, node, _, size))| {
                json!({
                    "remote_id": rid,
                    "epoch": epoch,
                    "node": node,
                    "this_node": node == self.node,
                    "created_at": backup::created_at(&ts),
                    "bytes": size,
                })
            })
            .collect();
        Ok(json!({
            "name": app,
            "target": self.target(),
            "remote_backups": items,
            "truncated": truncated,
        }))
    }

    pub fn fetch_status(data_dir: &Path, app: &str, rid: &str) -> Option<Value> {
        read_json(
            &data_dir
                .join("backups")
                .join(app)
                .join(REMOTE_DIR)
                .join(format!("fetch-{rid}.json")),
        )
    }

    /// Fetch the remote backup `rid` of `app` into a new local backup, in the background.
    /// Returns the state the caller reports now. `rid` must already be a valid remote id.
    pub fn start_fetch(self: &Arc<Self>, app: &str, rid: &str, ctx: FetchCtx) -> Value {
        let path = self
            .root(app)
            .join(REMOTE_DIR)
            .join(format!("fetch-{rid}.json"));
        let job_key = (app.to_string(), rid.to_string());
        if self.fetching.lock().unwrap().contains(&job_key) {
            if let Some(v) = read_json(&path) {
                return v;
            }
        }
        let state = json!({
            "name": app,
            "remote_id": rid,
            "target": self.target(),
            "status": "fetching",
            "started_at": now_text(),
        });
        if let Err(e) = write_json(&path, &state) {
            let mut failed = state.clone();
            failed["status"] = "failed".into();
            failed["error"] = json!({"kind": "remote_fetch_failed", "message": format!("cannot record the fetch: {e}"), "fix": "check disk space and permissions under the data dir"});
            return failed;
        }
        self.fetching.lock().unwrap().insert(job_key.clone());
        let (me, app, rid) = (self.clone(), app.to_string(), rid.to_string());
        let lock = self.lock(&app);
        let rt = tokio::runtime::Handle::current();
        let started = state.clone();
        tokio::spawn(async move {
            let _one_at_a_time = lock.lock_owned().await;
            let job = {
                let (me, app, rid) = (me.clone(), app.clone(), rid.clone());
                tokio::task::spawn_blocking(move || me.fetch(&rt, &app, &rid, &ctx))
            };
            let result = job.await.unwrap_or_else(|e| {
                Err(s3_err(
                    "remote_fetch_failed",
                    format!("the fetch task failed: {e}"),
                    "report this",
                ))
            });
            let mut done = started;
            done["finished_at"] = now_text().into();
            match result {
                Ok(extra) => {
                    done["status"] = "fetched".into();
                    for (k, v) in extra.as_object().into_iter().flatten() {
                        done[k] = v.clone();
                    }
                    eprintln!(
                        "app {app}: fetched remote backup {rid} as local backup {}",
                        done["local_id"].as_str().unwrap_or("")
                    );
                }
                Err(e) => {
                    eprintln!("app {app}: fetch of remote backup {rid} failed: {e}");
                    done["status"] = "failed".into();
                    done["error"] = json!({"kind": e.kind, "message": e.message, "fix": e.fix});
                }
            }
            if let Err(e) = write_json(&path, &done) {
                eprintln!("app {app}: cannot record the fetch of {rid}: {e}");
            }
            me.fetching.lock().unwrap().remove(&job_key);
        });
        state
    }

    /// Download, verify and unpack a remote backup into a new local backup id, checking in this
    /// order: the manifest names this app and id and this key; the object's size; its SHA-256;
    /// every sealed segment; the tar stream, strictly; each file's SHA-256; then the rename into a
    /// new local id. Nothing is renamed into the backup directory before every check passed, and
    /// the app's `data/` is never touched. Runs on the blocking pool.
    fn fetch(
        &self,
        rt: &tokio::runtime::Handle,
        app: &str,
        rid: &str,
        ctx: &FetchCtx,
    ) -> Result<Value, S3Error> {
        let (ts, node, epoch) = parse_remote_id(rid).ok_or_else(|| {
            s3_err(
                "bad_remote_id",
                format!("'{rid}' is not a remote id"),
                "list them with `spared backups <name> --remote`",
            )
        })?;
        let mkey = self.object(app, ts, node, epoch, "manifest.json");
        let akey = self.object(app, ts, node, epoch, "archive");
        let mbytes = rt
            .block_on(self.s3.get_small(&mkey, MAX_MANIFEST))?
            .ok_or_else(|| {
                s3_err(
                    "no_such_remote_backup",
                    format!("{} has no backup {rid} of app '{app}'", self.target()),
                    "list them with `spared backups <name> --remote`",
                )
            })?;
        let m: Value = serde_json::from_slice(&mbytes)
            .map_err(|e| corrupt(format!("its manifest is not JSON: {e}")))?;
        let s = |k: &str| m[k].as_str().unwrap_or("");
        if s("format") != MANIFEST_FORMAT
            || s("app") != app
            || s("remote_id") != rid
            || s("node") != node
            || m["epoch"].as_u64() != Some(epoch as u64)
        {
            return Err(corrupt(format!(
                "its manifest describes {} of app '{}' (format '{}'), not {rid} of '{app}'",
                s("remote_id"),
                s("app"),
                s("format")
            )));
        }
        let alg = m["encryption"]["alg"].as_str().unwrap_or("");
        match (&self.key, alg) {
            (Some(k), seal::ALG) => {
                if m["encryption"]["key_id"].as_str() != Some(k.key_id().as_str()) {
                    return Err(s3_err(
                        "backup_key_mismatch",
                        format!(
                            "{rid} was sealed with key id {}, and this server's key has id {}",
                            m["encryption"]["key_id"].as_str().unwrap_or("?"),
                            k.key_id()
                        ),
                        "start the server with the key that was used when the backup was taken (--backup-key-file)",
                    ));
                }
            }
            (Some(_), "none") => {
                return Err(s3_err(
                    "backup_key_mismatch",
                    format!(
                        "{rid} is not encrypted, and this server only accepts encrypted backups (it has a key), so it cannot tell it was not planted"
                    ),
                    "restore it on a server started with --backup-encryption off, after checking where it came from",
                ));
            }
            (None, seal::ALG) => {
                return Err(s3_err(
                    "backup_key_mismatch",
                    format!(
                        "{rid} is encrypted, and this server has no key (--backup-encryption off)"
                    ),
                    "start the server with --backup-key-file pointing at the key used when the backup was taken",
                ));
            }
            (None, "none") => {}
            _ => {
                return Err(corrupt(format!(
                    "its manifest names an unknown encryption '{alg}'"
                )));
            }
        }
        let limits = archive::Limits::new(ctx.max_bytes);
        let cap = MAX_OBJECT.min(
            limits
                .max_stream()
                .saturating_add(limits.max_stream() / 64 + (1 << 20)),
        );
        let want_bytes = m["archive"]["bytes"]
            .as_u64()
            .ok_or_else(|| corrupt("its manifest has no archive size".into()))?;
        let want_sha = m["archive"]["sha256"].as_str().unwrap_or("");
        if want_sha.len() != 64 || s("content_digest").len() != 64 {
            return Err(corrupt(
                "its manifest has no archive or content hash".into(),
            ));
        }
        // 1. The object's size, against the manifest's, against what this server may hold.
        if want_bytes > cap {
            return Err(s3_err(
                "remote_backup_too_big",
                format!(
                    "{rid} is {want_bytes} bytes, more than this server's data cap allows (--max-app-data-mb)"
                ),
                "raise --max-app-data-mb to at least the backup's size, then retry",
            ));
        }
        let size = rt
            .block_on(self.s3.head(&akey))?
            .ok_or_else(|| corrupt("its manifest exists, but its archive does not".into()))?;
        if size != want_bytes {
            return Err(corrupt(format!(
                "the archive is {size} bytes, and its manifest says {want_bytes}"
            )));
        }
        let root = self.root(app);
        create_private_dir(&root).map_err(|e| {
            local_failure(
                "remote_fetch_failed",
                "cannot create the backup directory",
                e,
            )
        })?;
        let stage = root.join(format!("{FETCH_TMP}{rid}"));
        let _ = backup::remove_tree(&stage);
        let _cleanup = Cleanup(stage.clone());
        create_private_dir(&stage).map_err(|e| {
            local_failure(
                "remote_fetch_failed",
                "cannot create the staging directory",
                e,
            )
        })?;
        let apath = stage.join("archive");
        // 2. The SHA-256 of what was downloaded.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&apath)
                .map_err(|e| {
                    local_failure("remote_fetch_failed", "cannot create the download file", e)
                })?;
            let mut h = Sha256::new();
            let mut got = 0u64;
            let mut resp = rt
                .block_on(self.s3.get(&akey))?
                .ok_or_else(|| corrupt("its archive disappeared during the fetch".into()))?;
            while let Some(chunk) = rt.block_on(resp.next_chunk())? {
                got += chunk.len() as u64;
                if got > want_bytes {
                    return Err(corrupt(format!(
                        "the archive is longer than the {want_bytes} bytes its manifest says"
                    )));
                }
                h.update(&chunk);
                f.write_all(&chunk).map_err(|e| {
                    local_failure("remote_fetch_failed", "writing the download failed", e)
                })?;
            }
            if got != want_bytes {
                return Err(corrupt(format!(
                    "the archive ended after {got} of {want_bytes} bytes"
                )));
            }
            if archive::hex(&h.finalize()) != want_sha {
                return Err(corrupt(
                    "the archive's SHA-256 does not match its manifest".into(),
                ));
            }
            f.sync_all().map_err(|e| {
                local_failure("remote_fetch_failed", "writing the download failed", e)
            })?;
        }
        let names = archive::Names {
            app,
            remote_id: rid,
            local_id: ts,
        };
        let open = || {
            std::fs::File::open(&apath)
                .map_err(|e| local_failure("remote_fetch_failed", "cannot read the download", e))
        };
        // 3. Every sealed segment opens and the last one is marked last.
        if let Some(k) = &self.key {
            let (_, salt) = seal::verify(k, seal::aad(app, rid), std::io::BufReader::new(open()?))
                .map_err(|e| {
                    corrupt(format!(
                        "the archive does not open with this server's key: {e}"
                    ))
                })?;
            if Some(seal::salt_hex(&salt).as_str()) != m["encryption"]["salt"].as_str() {
                return Err(corrupt(
                    "the archive's salt differs from its manifest's".into(),
                ));
            }
        }
        // 4 and 5. The tar stream, strictly, and each file's hash.
        let data = stage.join("data");
        create_private_dir(&data).map_err(|e| {
            local_failure(
                "remote_fetch_failed",
                "cannot create the staging directory",
                e,
            )
        })?;
        let unpacked = archive::unpack(open()?, self.key.as_ref(), &data, &names, &limits)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    corrupt(format!("the archive does not unpack: {e}"))
                } else {
                    local_failure("remote_fetch_failed", "unpacking failed", e)
                }
            })?;
        let digest = archive::hex(&unpacked.digest);
        if digest != s("content_digest")
            || m["files"].as_u64() != Some(unpacked.contents.files)
            || m["bytes"].as_u64() != Some(unpacked.contents.bytes)
        {
            return Err(corrupt(
                "the archive's contents differ from what its manifest says".into(),
            ));
        }
        let _ = std::fs::remove_file(&apath);
        // The epoch a restore of this backup takes: one above the highest in the bucket.
        let next_epoch = rt.block_on(self.highest_epoch(app, epoch))? + 1;
        if next_epoch > MAX_EPOCH {
            return Err(s3_err(
                "remote_rejected",
                format!("the bucket's epochs of '{app}' are used up ({MAX_EPOCH})"),
                "report this",
            ));
        }
        // 6. A new local backup id.
        let _serial = rt.block_on(ctx.name_lock.lock());
        let id = backup::unique_id(&root, "", crate::server::unix_now());
        std::fs::rename(&data, root.join(&id)).map_err(|e| {
            local_failure(
                "remote_fetch_failed",
                "could not move the fetched backup into place",
                e,
            )
        })?;
        if let Ok(d) = std::fs::File::open(&root) {
            let _ = d.sync_all();
        }
        let _ = write_json(
            &root.join(REMOTE_DIR).join(format!("{id}.json")),
            &json!({
                "local_id": id,
                "remote_id": rid,
                "target": self.target(),
                "status": "fetched",
                "finished_at": now_text(),
                "content_digest": digest,
                "next_epoch": next_epoch,
            }),
        );
        let removed = backup::prune(&root, ctx.keep, &self.pinned(app));
        let mut out = json!({
            "local_id": id,
            "files": unpacked.contents.files,
            "bytes": unpacked.contents.bytes,
            "content_digest": digest,
            "journal_clean": unpacked.contents.journal_clean,
            "next_epoch": next_epoch,
        });
        if !removed.is_empty() {
            out["removed_old"] = removed.into();
        }
        Ok(out)
    }
}

/// At start, before anything else touches the backups: an upload or fetch that a stop cut off is
/// marked failed, and its temporary files are removed. Runs whether or not remote backups are
/// configured now.
pub fn recover(data_dir: &Path) {
    let backups = data_dir.join("backups");
    for app in std::fs::read_dir(&backups).into_iter().flatten().flatten() {
        let root = app.path();
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(FETCH_TMP) {
                let _ = backup::remove_tree(&e.path());
            }
        }
        let dir = root.join(REMOTE_DIR);
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(".tmp-") || name.ends_with(".json.tmp") {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            if !name.ends_with(".json") {
                continue;
            }
            let Some(mut v) = read_json(&e.path()) else {
                continue;
            };
            let what = match v["status"].as_str() {
                Some("uploading") => "upload",
                Some("fetching") => "fetch",
                _ => continue,
            };
            v["status"] = "failed".into();
            v["finished_at"] = now_text().into();
            v["error"] = json!({
                "kind": "remote_interrupted",
                "message": format!("the server stopped during the {what}"),
                "fix": if what == "upload" {
                    "take the backup again with `spared backup <name>`. This local backup stands"
                } else {
                    "run the restore again. The app's data was not changed"
                },
            });
            let _ = write_json(&e.path(), &v);
            eprintln!(
                "app {}: an interrupted remote {what} ({}) is marked failed",
                app.file_name().to_string_lossy(),
                name.trim_end_matches(".json")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_ids_are_strict() {
        assert_eq!(
            parse_remote_id("20261112T040506Z.7f3a9c0a1b2c.1"),
            Some(("20261112T040506Z", "7f3a9c0a1b2c", 1))
        );
        assert_eq!(
            parse_remote_id("20261112T040506Z.7f3a9c0a1b2c.12345678").map(|r| r.2),
            Some(12_345_678)
        );
        for bad in [
            "",
            "20261112T040506Z",
            "20261112T040506Z.7f3a9c0a1b2c",
            "20261112T040506Z.7f3a9c0a1b2c.0",
            "20261112T040506Z.7f3a9c0a1b2c.01",
            "20261112T040506Z.7f3a9c0a1b2c.123456789",
            "20261112T040506Z.7F3A9C0A1B2C.1",
            "20261112T040506Z.7f3a9c.1",
            "20261112T040506Z.7f3a9c0a1b2c.1.1",
            "pre-restore-20261112T040506Z.7f3a9c0a1b2c.1",
            "../x.7f3a9c0a1b2c.1",
            "20261112T040506Z/../.7f3a9c0a1b2c.1",
            "20261112T040506Z.7f3a9c0a1b2c.1/",
        ] {
            assert_eq!(parse_remote_id(bad), None, "{bad:?}");
        }
    }

    fn env_of(pairs: &[(&'static str, String)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    fn flags(to: Option<&str>) -> Flags {
        Flags {
            to: to.map(str::to_string),
            endpoint: None,
            region: None,
            addressing: None,
            key_file: None,
            encryption_off: false,
            allow_http: false,
        }
    }

    fn creds() -> Vec<(&'static str, String)> {
        vec![
            ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE".into()),
            ("AWS_SECRET_ACCESS_KEY", "very-secret".into()),
        ]
    }

    #[test]
    fn start_refuses_a_target_without_a_key_unless_encryption_is_off() {
        assert!(
            Settings::from_flags(flags(None), &env_of(&creds()))
                .unwrap()
                .is_none()
        );
        let e = Settings::from_flags(flags(Some("s3://bucket/p")), &env_of(&creds()))
            .err()
            .unwrap();
        assert_eq!(e.kind, "backup_key_required");
        let mut f = flags(Some("s3://bucket/p"));
        f.encryption_off = true;
        let s = Settings::from_flags(f, &env_of(&creds())).unwrap().unwrap();
        assert!(s.key.is_none());
        assert_eq!(s.region, "us-east-1");
        assert_eq!(s.addressing, s3::Addressing::Virtual);
        assert!(!format!("{:?}", s.creds).contains("very-secret"));
        let mut f = flags(Some("s3://bucket/p"));
        f.encryption_off = true;
        let e = Settings::from_flags(f, &env_of(&[])).err().unwrap();
        assert_eq!(e.kind, "no_backup_credentials");
        assert!(!e.message.contains("very-secret"));
    }

    #[test]
    fn credentials_and_the_key_come_from_files() {
        let d = archive::tests::scratch("settings");
        let (idf, secf, keyf) = (d.0.join("id"), d.0.join("sec"), d.0.join("key"));
        std::fs::write(&idf, "AKIDFILE\n").unwrap();
        std::fs::write(&secf, "secret-from-file\n").unwrap();
        let key = seal::BackupKey::generate().unwrap();
        std::fs::write(&keyf, key.to_hex() + "\n").unwrap();
        let env = env_of(&[
            ("AWS_ACCESS_KEY_ID_FILE", idf.display().to_string()),
            ("AWS_SECRET_ACCESS_KEY_FILE", secf.display().to_string()),
            ("AWS_REGION", "eu-west-1".into()),
        ]);
        let mut f = flags(Some("s3://bucket/p"));
        f.key_file = Some(keyf.clone());
        f.endpoint = Some("http://127.0.0.1:9000".into());
        let s = Settings::from_flags(f, &env).unwrap().unwrap();
        assert_eq!(s.creds.access_key_id, "AKIDFILE");
        assert_eq!(s.region, "eu-west-1");
        assert_eq!(s.addressing, s3::Addressing::Path);
        assert_eq!(s.key.unwrap().key_id(), key.key_id());
        let both = env_of(&[
            ("AWS_ACCESS_KEY_ID", "x".into()),
            ("AWS_ACCESS_KEY_ID_FILE", idf.display().to_string()),
            ("AWS_SECRET_ACCESS_KEY", "y".into()),
        ]);
        let mut f = flags(Some("s3://bucket/p"));
        f.encryption_off = true;
        assert_eq!(
            Settings::from_flags(f, &both).err().unwrap().kind,
            "bad_backup_config"
        );
        let mut f = flags(Some("s3://bucket/p"));
        f.key_file = Some(keyf);
        f.encryption_off = true;
        assert_eq!(
            Settings::from_flags(f, &env_of(&creds()))
                .err()
                .unwrap()
                .kind,
            "bad_backup_config"
        );
        let mut f = flags(Some("s3://bucket/p"));
        f.encryption_off = true;
        f.endpoint = Some("http://user:pw@10.0.0.1:9000".into());
        let e = Settings::from_flags(f, &env_of(&creds())).err().unwrap();
        assert_eq!(e.kind, "bad_backup_config");
        assert!(!e.message.contains("pw@"), "{}", e.message);
    }

    #[test]
    fn epochs_start_at_1_and_a_restore_only_raises_them() {
        let d = archive::tests::scratch("epoch");
        assert_eq!(local_epoch(&d.0, "notes"), Ok(1));
        assert_eq!(raise_epoch(&d.0, "notes", 3, "r3", "d3").unwrap(), 3);
        assert_eq!(raise_epoch(&d.0, "notes", 2, "r2", "d2").unwrap(), 3);
        assert_eq!(local_epoch(&d.0, "notes"), Ok(3));
        assert_eq!(epoch_record(&d.0, "notes").unwrap()["restored_from"], "r2");
        let mode = std::fs::metadata(epoch_path(&d.0, "notes"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        for bad in ["", "{}", r#"{"epoch":0}"#, r#"{"epoch":100000000}"#, "1"] {
            std::fs::write(epoch_path(&d.0, "notes"), bad).unwrap();
            assert!(local_epoch(&d.0, "notes").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn keys_name_their_epoch_and_node_only_in_the_layout() {
        let base = "p/spared/v1/notes/";
        assert_eq!(
            key_epoch(
                base,
                "p/spared/v1/notes/e00000002/20261112T040506Z.7f3a9c0a1b2c/archive"
            ),
            Some((2, "7f3a9c0a1b2c".into()))
        );
        for bad in [
            "p/spared/v1/notes/e2/20261112T040506Z.7f3a9c0a1b2c/archive",
            "p/spared/v1/notes/e00000000/20261112T040506Z.7f3a9c0a1b2c/archive",
            "p/spared/v1/notes/e00000002/20261112T040506Z/archive",
            "p/spared/v1/notes/e00000002/x.7f3a9c0a1b2c/archive",
            "p/spared/v1/other/e00000002/20261112T040506Z.7f3a9c0a1b2c/archive",
            "p/spared/v1/notes/owner.json",
        ] {
            assert_eq!(key_epoch(base, bad), None, "{bad}");
        }
        // `~` sorts after every character of a backup's directory, so a listing that starts
        // after `e<N>/~` skips exactly the keys of epochs up to N.
        let after = format!("{base}e00000002/~");
        assert!(format!("{base}e00000002/20261112T040506Z.7f3a9c0a1b2c/manifest.json") < after);
        assert!(format!("{base}e00000003/20001112T040506Z.000000000000/archive") > after);
        assert!(format!("{base}e00000010/20001112T040506Z.000000000000/archive") > after);
    }

    #[test]
    fn the_node_id_is_made_once_and_private() {
        let d = archive::tests::scratch("node");
        let a = node_id(&d.0).unwrap();
        assert_eq!(a.len(), 12);
        assert_eq!(node_id(&d.0).unwrap(), a);
        let mode = std::fs::metadata(d.0.join(NODE_ID_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::write(d.0.join(NODE_ID_FILE), "nope").unwrap();
        assert!(node_id(&d.0).is_err());
    }
}
