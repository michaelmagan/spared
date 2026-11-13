//! `spared move <name> --to <admin url>`: move an app to another spared server through the bucket
//! both servers back up to, driven from one machine that reaches both admin APIs.
//!
//! The steps: check both servers, copy the app's code (the deployed wasm and its spared.toml, never
//! data) to the target, which compiles it itself and keeps the app frozen, freeze the app on the
//! source, back it up and wait for the upload, fetch that backup on the target and compare its
//! content digest with the source's, restore it there (which raises the target's epoch), and thaw
//! it there. The source stays frozen and is never deleted, and DNS is the owner's to change.
//!
//! Until the restore on the target has completed, the source holds the truth: any failure before
//! then thaws the source again (only a freeze this run made). After it, a failure leaves the source
//! frozen and says what each side holds and what to run next. A rerun works out where an earlier
//! run stopped from the two servers' state, so the move can be run again until it completes.
//!
//! Tokens: the source's comes from `$SPARED_TOKEN` or `~/.spared/token` like every command, the
//! target's from `--to-token-file` or `$SPARED_TARGET_TOKEN`, never from an argument. Neither is
//! ever printed.

use super::{
    CliError, R, Response, admin, check_app_name, connect, err, json_body, out_json, parse_base,
    server_url, timed_get, token,
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Args {
    pub name: String,
    pub to: String,
    pub to_token_file: Option<PathBuf>,
    pub smoke_url: Option<String>,
    pub dry_run: bool,
    pub server: Option<String>,
}

const TARGET_UNAUTHORIZED_FIX: &str = "the target server has a different token than the one given: put the target's <data-dir>/admin-token in a file only you can read and pass --to-token-file FILE, or set SPARED_TARGET_TOKEN";
const POLL: Duration = Duration::from_millis(500);
const SMOKE_TIMEOUT: Duration = Duration::from_secs(30);

/// One of the two servers.
struct Side {
    base: String,
    token: String,
    label: &'static str,
}

impl Side {
    /// Name the server in an error, and give the target's own fix for a wrong token.
    fn tag(&self, mut e: CliError) -> CliError {
        e.message = format!("{} server {}: {}", self.label, self.base, e.message);
        if e.kind == "unauthorized" && self.label == "target" {
            e.fix = TARGET_UNAUTHORIZED_FIX.into();
        }
        e
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Bytes,
    ) -> R<Response> {
        admin(&self.base, &self.token, method, path, headers, body)
            .await
            .map_err(|e| self.tag(e))
    }

    async fn get(&self, path: &str) -> R<Value> {
        let r = self.call("GET", path, &[], Bytes::new()).await?;
        json_body(&r).map_err(|e| self.tag(e))
    }

    async fn post(&self, path: &str, body: Value) -> R<Value> {
        let r = self
            .call(
                "POST",
                path,
                &[("content-type", "application/json".to_string())],
                Bytes::from(body.to_string()),
            )
            .await?;
        json_body(&r).map_err(|e| self.tag(e))
    }

    /// Ask `path` until its `status` is no longer `pending`.
    async fn poll(&self, path: &str, pending: &str) -> R<Value> {
        loop {
            let v = self.get(path).await?;
            if v["status"].as_str() != Some(pending) {
                return Ok(v);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The app on this server, or `None` when it has no app of that name.
    async fn app(&self, name: &str) -> R<Option<Value>> {
        match self.get(&format!("/v1/apps/{name}")).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind == "no_such_app" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `GET /v1/server`. A server without it predates `spared move`.
    async fn info(&self) -> R<Value> {
        self.get("/v1/server").await.map_err(|e| {
            if e.kind == "no_route" {
                err(
                    "move_unsupported",
                    format!(
                        "the {} server {} is a spared version without `spared move` (it has no GET /v1/server)",
                        self.label, self.base
                    ),
                    "upgrade spared on that server to this CLI's version, then run the move again",
                )
            } else {
                e
            }
        })
    }
}

/// The target's admin token: the file named by `--to-token-file`, else `$SPARED_TARGET_TOKEN`.
fn target_token(file: Option<&Path>) -> R<String> {
    let from = match file {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| {
                err(
                    "no_target_token",
                    format!("cannot read --to-token-file {}: {e}", p.display()),
                    "point --to-token-file at a file holding the target server's <data-dir>/admin-token",
                )
            })?;
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o004 != 0) {
                eprintln!(
                    "warning: {} is readable by every local user. chmod 600 it",
                    p.display()
                );
            }
            text
        }
        None => std::env::var("SPARED_TARGET_TOKEN").unwrap_or_default(),
    };
    let t = from.trim().to_string();
    if t.is_empty() {
        return Err(err(
            "no_target_token",
            "no admin token for the target server: no --to-token-file, and SPARED_TARGET_TOKEN is not set",
            "copy the target's <data-dir>/admin-token to a file only you can read and pass --to-token-file FILE, or set SPARED_TARGET_TOKEN. A token is never taken as an argument, where every local user could read it",
        ));
    }
    Ok(t)
}

/// What the move does, worked out from both servers before anything changes.
enum Plan {
    /// Copy, freeze, back up, fetch, restore, check, thaw.
    Full,
    /// An earlier run restored the source's last backup on the target and stopped before the
    /// end: check it and thaw.
    Verify { rid: String, digest: String },
    /// An earlier run completed.
    Done { rid: String, digest: String },
}

fn describe(app: Option<&Value>) -> String {
    match app {
        None => "no app of that name".into(),
        Some(a) if a["frozen"] == true => format!(
            "frozen ({}: {}), epoch {}",
            a["freeze"]["kind"].as_str().unwrap_or("?"),
            a["freeze"]["reason"].as_str().unwrap_or(""),
            a["epoch"]
        ),
        Some(a) => format!("serving, epoch {}", a["epoch"]),
    }
}

/// A move in progress: both servers, what has been done, for the output.
struct Mover {
    name: String,
    src: Side,
    dst: Side,
    json: bool,
    steps: Vec<String>,
    smoke_url: Option<String>,
    /// Set once the freeze call returns: whether this run, not an earlier one, froze the source.
    froze_source: bool,
}

impl Mover {
    fn step(&mut self, s: String) {
        if !self.json {
            println!("move: {s}");
        }
        self.steps.push(s);
    }

    /// A failure before the restore completed on the target: the source still holds the truth,
    /// so a freeze this run made there is lifted. Builds `move_aborted` around `cause`.
    async fn abort(&mut self, cause: CliError) -> CliError {
        let name = self.name.clone();
        let source = if self.froze_source {
            match self
                .src
                .post(&format!("/v1/apps/{name}/thaw"), json!({"if_kind": "move"}))
                .await
            {
                Ok(v) if v["frozen"] == false && v["was_frozen"] == true => {
                    "thawed again by this move: it serves as before".to_string()
                }
                Ok(v) if v["frozen"] == false => {
                    "serving, as before (this move had not frozen it)".into()
                }
                Ok(v) => format!(
                    "still frozen by something other than a move ({}: {}), as before the move",
                    v["freeze"]["kind"].as_str().unwrap_or("?"),
                    v["freeze"]["reason"].as_str().unwrap_or("")
                ),
                Err(e) => format!(
                    "could not be thawed ({}: {}). It may still be frozen: run `spared thaw {name} --server {}`",
                    e.kind, e.message, self.src.base
                ),
            }
        } else {
            match self.src.app(&name).await {
                Ok(Some(a)) if a["frozen"] == true => format!(
                    "still frozen, as before this run ({}: {}); `spared thaw {name} --server {}` if it should serve",
                    a["freeze"]["kind"].as_str().unwrap_or("?"),
                    a["freeze"]["reason"].as_str().unwrap_or(""),
                    self.src.base
                ),
                Ok(a) => format!("{}, as before this run", describe(a.as_ref())),
                Err(e) => format!("unknown ({}: {})", e.kind, e.message),
            }
        };
        let target = match self.dst.app(&name).await {
            Ok(a) => describe(a.as_ref()),
            Err(e) => format!("unknown ({}: {})", e.kind, e.message),
        };
        let fix = format!(
            "{} Then run the same `spared move` again: it starts over and is safe to repeat. The target's copy of {name} stays frozen and does not serve. If you give the move up, remove it there with `spared rm {name} --yes --server {}`",
            if cause.fix.is_empty() {
                "fix the cause."
            } else {
                cause.fix.as_str()
            },
            self.dst.base
        );
        let mut e = err(
            "move_aborted",
            format!(
                "moving {name} stopped before the restore on the target completed: {}: {}. Source: {source}. Target: {target}",
                cause.kind, cause.message
            ),
            fix,
        );
        e.extra = Some(json!({
            "cause": {"kind": cause.kind, "message": cause.message, "fix": cause.fix},
            "state": {"source": source, "target": target},
            "steps": self.steps,
        }));
        e
    }

    /// A failure after the restore completed on the target: the source stays frozen.
    async fn incomplete(&mut self, cause: CliError, next: String) -> CliError {
        let name = self.name.clone();
        let source = match self.src.app(&name).await {
            Ok(a) => describe(a.as_ref()),
            Err(e) => format!("unknown ({}: {})", e.kind, e.message),
        };
        let target = match self.dst.app(&name).await {
            Ok(a) => describe(a.as_ref()),
            Err(e) => format!("unknown ({}: {})", e.kind, e.message),
        };
        let mut e = err(
            "move_incomplete",
            format!(
                "moving {name} stopped after the restore on the target: {}: {}. Source: {source} (left frozen: the target holds the restored copy). Target: {target}",
                cause.kind, cause.message
            ),
            next,
        );
        e.extra = Some(json!({
            "cause": {"kind": cause.kind, "message": cause.message, "fix": cause.fix},
            "state": {"source": source, "target": target},
            "steps": self.steps,
        }));
        e
    }

    /// Copy the code, freeze the source, back it up, fetch and compare on the target, restore.
    /// Returns the remote id and content digest restored.
    async fn copy_and_restore(&mut self) -> R<(String, String)> {
        let name = self.name.clone();
        // 1. The code: the wasm as deployed and its spared.toml, never the compiled file.
        let code = self
            .src
            .call("GET", &format!("/v1/apps/{name}/code"), &[], Bytes::new())
            .await?;
        let manifest = code
            .headers
            .get("x-spared-manifest")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let sha = code
            .headers
            .get("x-spared-component-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if manifest.is_empty() || sha != sha256_hex(&code.body) {
            return Err(err(
                "bad_response",
                format!(
                    "the source sent the code of {name} without its manifest, or with a SHA-256 that does not match"
                ),
                "check that both servers run this CLI's version of spared",
            ));
        }
        let deployed = self
            .dst
            .call(
                "PUT",
                &format!("/v1/apps/{name}?frozen=move"),
                &[("x-spared-manifest", manifest)],
                code.body.clone(),
            )
            .await?;
        let deployed = json_body(&deployed)?;
        self.step(format!(
            "copied the code of {name} ({} bytes, sha256 {}) to the target, which compiled it. The app is frozen there (version {})",
            code.body.len(),
            &sha[..12],
            deployed["version"]
        ));
        // 2. Freeze the source.
        let f = self
            .src
            .post(
                &format!("/v1/apps/{name}/freeze"),
                json!({"kind": "move", "reason": format!("moving to {}", self.dst.base)}),
            )
            .await?;
        self.froze_source = f["already_frozen"] != true;
        self.step(format!(
            "froze {name} on the source ({}): its public requests get 503 app_frozen",
            if f["already_frozen"] == true {
                format!(
                    "it was already frozen: {}",
                    f["freeze"]["kind"].as_str().unwrap_or("?")
                )
            } else {
                "no request runs there from now on".into()
            }
        ));
        // 3. The final backup, uploaded.
        let b = self
            .src
            .post(&format!("/v1/apps/{name}/backup"), json!({}))
            .await?;
        let id = b["backup_id"].as_str().unwrap_or("").to_string();
        let mut remote = b["remote"].clone();
        if remote["status"] == "uploading" {
            remote = self
                .src
                .poll(&format!("/v1/apps/{name}/backups/{id}/remote"), "uploading")
                .await?;
        }
        if remote["status"] != "uploaded" {
            let c = &remote["error"];
            return Err(err(
                c["kind"].as_str().unwrap_or("remote_upload_failed"),
                format!(
                    "the final backup {id} on the source was not uploaded: {}",
                    c["message"].as_str().unwrap_or("no upload state")
                ),
                c["fix"].as_str().unwrap_or(""),
            ));
        }
        let rid = remote["remote_id"].as_str().unwrap_or("").to_string();
        let digest = remote["content_digest"].as_str().unwrap_or("").to_string();
        self.step(format!(
            "backed up {name} on the source as {id} and uploaded it as {rid} (content digest {})",
            &digest[..digest.len().min(16)]
        ));
        // 4. Fetch it on the target and compare.
        let mut fetched = self
            .dst
            .post(&format!("/v1/apps/{name}/fetch"), json!({"remote_id": rid}))
            .await?;
        if fetched["status"] == "fetching" {
            fetched = self
                .dst
                .poll(&format!("/v1/apps/{name}/fetch/{rid}"), "fetching")
                .await?;
        }
        if fetched["status"] != "fetched" {
            let c = &fetched["error"];
            return Err(err(
                c["kind"].as_str().unwrap_or("remote_fetch_failed"),
                format!(
                    "the target could not fetch {rid}: {}",
                    c["message"].as_str().unwrap_or("no fetch state")
                ),
                c["fix"].as_str().unwrap_or(""),
            ));
        }
        let got = fetched["content_digest"].as_str().unwrap_or("");
        if got != digest {
            return Err(err(
                "move_digest_mismatch",
                format!(
                    "the source uploaded {rid} with content digest {digest}, and the target unpacked content digest {got}: what reached the target is not what the source sent. The target's data was not replaced"
                ),
                "find out who else writes to the bucket under this prefix (the bucket's access log), then run the move again",
            ));
        }
        let local = fetched["local_id"].as_str().unwrap_or("").to_string();
        self.step(format!(
            "fetched {rid} on the target as local backup {local}. Every hash and seal checked, and its content digest matches the source's"
        ));
        // 5. Restore it on the target.
        let r = self
            .dst
            .post(
                &format!("/v1/apps/{name}/restore"),
                json!({"backup_id": local}),
            )
            .await?;
        self.step(format!(
            "restored {name} on the target from {local}: {} bytes in {} files, epoch {}",
            r["bytes"], r["files"], r["epoch"]
        ));
        Ok((rid, digest))
    }

    /// After the restore: the target holds `rid` with `digest`; thaw it and smoke it.
    async fn finish(&mut self, rid: &str, digest: &str) -> R<Value> {
        let name = self.name.clone();
        let src_cmd = format!("--server {}", self.src.base);
        let rerun = format!(
            "fix the cause, then run the same `spared move` again: it checks the target's copy and finishes. To keep the app on the source instead: `spared thaw {name} {src_cmd}`, then `spared rm {name} --yes --server {}`",
            self.dst.base
        );
        let t = match self.dst.app(&name).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                let c = err(
                    "no_such_app",
                    "the target has no app of that name any more",
                    "",
                );
                return Err(self.incomplete(c, rerun).await);
            }
            Err(c) => return Err(self.incomplete(c, rerun).await),
        };
        let held = &t["restored_from_remote"];
        if held["remote_id"] != rid || held["content_digest"] != digest {
            let c = err(
                "move_digest_mismatch",
                format!(
                    "the target's copy came from {} with content digest {}, not from {rid} with {digest}",
                    held["remote_id"], held["content_digest"]
                ),
                "",
            );
            return Err(self.incomplete(c, rerun).await);
        }
        self.step(format!(
            "checked the target: it holds {rid}, content digest {}, at epoch {}",
            &digest[..digest.len().min(16)],
            t["epoch"]
        ));
        if let Err(c) = self
            .dst
            .post(&format!("/v1/apps/{name}/thaw"), json!({}))
            .await
        {
            return Err(self.incomplete(c, rerun).await);
        }
        self.step(format!("thawed {name} on the target: it serves there now"));
        if let Some(url) = self.smoke_url.clone() {
            let host = format!("{name}.localhost");
            let outcome = smoke(&url, &host).await;
            match outcome {
                Ok(status) if status < 500 => {
                    self.step(format!("GET / on {url} with Host {host}: {status}"))
                }
                other => {
                    let what = match other {
                        Ok(status) => format!("answered {status}"),
                        Err(e) => format!("failed: {}", e.message),
                    };
                    // Not serving a broken copy: freeze the target again until the rerun.
                    let refrozen = self
                        .dst
                        .post(
                            &format!("/v1/apps/{name}/freeze"),
                            json!({"kind": "move", "reason": "the smoke request after a move failed"}),
                        )
                        .await
                        .is_ok();
                    let c = err(
                        "move_smoke_failed",
                        format!(
                            "GET / on {url} with Host {host} {what}{}",
                            if refrozen {
                                ". The target was frozen again"
                            } else {
                                ". The target could not be frozen again and may serve"
                            }
                        ),
                        "",
                    );
                    let next = format!(
                        "read `spared logs {name} --server {}`, fix the app or the --smoke-url, then run the same `spared move` again. To keep the app on the source instead: `spared thaw {name} {src_cmd}`",
                        self.dst.base
                    );
                    return Err(self.incomplete(c, next).await);
                }
            }
        }
        // As it is now, thawed.
        Ok(self.dst.app(&name).await.ok().flatten().unwrap_or(t))
    }
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// One GET / with `host` as the Host header: the status.
async fn smoke(url: &str, host: &str) -> R<u16> {
    let addr = parse_base(url.trim_end_matches('/'))?;
    let get = async {
        let mut sender = connect(&addr).await?;
        timed_get(&mut sender, host, "/").await
    };
    match tokio::time::timeout(SMOKE_TIMEOUT, get).await {
        Ok(r) => r.map(|(s, _)| s),
        Err(_) => Err(err(
            "move_smoke_failed",
            format!("no answer within {} s", SMOKE_TIMEOUT.as_secs()),
            "",
        )),
    }
}

/// The source's newest backup that was uploaded: its remote id, content digest and creation time.
async fn last_uploaded(src: &Side, name: &str) -> R<Option<(String, String, String)>> {
    let v = src.get(&format!("/v1/apps/{name}/backups")).await?;
    Ok(v["backups"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["kind"] == "backup" && b["remote"]["status"] == "uploaded")
        .map(|b| {
            (
                b["remote"]["remote_id"].as_str().unwrap_or("").to_string(),
                b["remote"]["content_digest"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                b["created_at"].as_str().unwrap_or("").to_string(),
            )
        }))
}

pub async fn cmd_move(a: Args, json: bool) -> R<()> {
    let name = a.name.clone();
    check_app_name(&name)?;
    let src = Side {
        base: server_url(a.server),
        token: token()?,
        label: "source",
    };
    let to = a.to.trim_end_matches('/').to_string();
    for u in std::iter::once(&to).chain(a.smoke_url.as_ref()) {
        if u.contains('@') {
            return Err(err(
                "bad_server_url",
                "a URL given to spared move holds a user name or password",
                "pass http://IP:PORT only. The target's token goes in --to-token-file or SPARED_TARGET_TOKEN",
            ));
        }
        parse_base(u.trim_end_matches('/'))?;
    }
    if parse_base(&to)? == parse_base(&src.base)? {
        return Err(err(
            "move_same_server",
            format!("--to {to} is the source server itself"),
            "pass the target's admin URL with --to, and the source's with --server or $SPARED_URL",
        ));
    }
    let dst = Side {
        base: to.clone(),
        token: target_token(a.to_token_file.as_deref())?,
        label: "target",
    };

    // Checks. Nothing changes until they all pass.
    let (si, ti) = (src.info().await?, dst.info().await?);
    for (side, info) in [(&src, &si), (&dst, &ti)] {
        if info["remote"].is_null() {
            return Err(err(
                "remote_backups_off",
                format!(
                    "the {} server {} was started without --backup-to, and a move goes through the bucket both servers back up to",
                    side.label, side.base
                ),
                "start both servers with the same --backup-to and the same --backup-key-file (docs/operating.md#move-an-app-to-another-server)",
            ));
        }
    }
    let (sr, tr) = (&si["remote"], &ti["remote"]);
    let same = ["bucket", "prefix", "encrypted", "key_id"]
        .iter()
        .all(|k| sr[k] == tr[k]);
    if !same {
        let show = |r: &Value| {
            format!(
                "{} (key id {})",
                r["target"].as_str().unwrap_or("?"),
                r["key_id"].as_str().unwrap_or("none: not encrypted")
            )
        };
        return Err(err(
            "move_remote_mismatch",
            format!(
                "the servers back up to different places: the source to {}, the target to {}",
                show(sr),
                show(tr)
            ),
            "start the target with the source's --backup-to and a copy of the source's backup key (--backup-key-file). The key id must match",
        ));
    }
    if si["node"] == ti["node"] {
        return Err(err(
            "move_same_server",
            format!(
                "both servers have node id {}: they are one server, or one data directory was copied to make the other",
                si["node"]
            ),
            "on a copied data directory, stop that server, delete its <data-dir>/node-id and start it again (docs/upgrading.md)",
        ));
    }
    let Some(sapp) = src.app(&name).await? else {
        return Err(err(
            "no_such_app",
            format!("the source server {} has no app '{name}'", src.base),
            "check the name with `spared ps`. If the app was moved and removed here already, there is nothing to move",
        ));
    };
    let tapp = dst.app(&name).await?;
    // Per-app limits are server state and do not travel in a backup. The move copies the source's
    // override to a target that has none for this name, before the target's copy can serve. A
    // target that cannot hold limits is refused while nothing has changed, so a moved app never
    // runs without the limits its operator set.
    let src_limits = match src.get(&format!("/v1/apps/{name}/limits")).await {
        Ok(v) => v["override"].clone(),
        Err(e) if e.kind == "no_route" => Value::Null,
        Err(e) => return Err(e),
    };
    let dst_limits = if src_limits.is_null() {
        Value::Null
    } else {
        match dst.get(&format!("/v1/apps/{name}/limits")).await {
            Ok(v) => v["override"].clone(),
            Err(e) if e.kind == "no_route" => {
                return Err(err(
                    "move_unsupported",
                    format!(
                        "the target server {} is a spared version without per-app limits, and the source has limits set for {name} ({src_limits}) that the target would not enforce. Nothing was changed",
                        dst.base
                    ),
                    "upgrade spared on the target to this CLI's version, then run the move again",
                ));
            }
            Err(e) => return Err(e),
        }
    };
    let copy_limits = !src_limits.is_null() && dst_limits.is_null();
    // Both can read the bucket: a bad credential shows here, before anything changes.
    src.get(&format!("/v1/apps/{name}/remote-backups")).await?;
    dst.get(&format!("/v1/apps/{name}/remote-backups")).await?;
    let last = last_uploaded(&src, &name).await?;
    let restored_from = |t: &Value| -> Option<(String, String)> {
        let r = &t["restored_from_remote"];
        Some((
            r["remote_id"].as_str()?.to_string(),
            r["content_digest"].as_str()?.to_string(),
        ))
    };
    let stayed_frozen = |l: &(String, String, String)| {
        sapp["freeze"]["at"]
            .as_str()
            .is_some_and(|at| at <= l.2.as_str())
    };
    let plan = match (&tapp, &last) {
        (Some(t), Some(l))
            if sapp["frozen"] == true
                && stayed_frozen(l)
                && restored_from(t).as_ref() == Some(&(l.0.clone(), l.1.clone())) =>
        {
            // Only a freeze a move made is lifted; one the owner made after the move stays.
            if t["frozen"] == true && t["freeze"]["kind"] == "move" {
                Plan::Verify {
                    rid: l.0.clone(),
                    digest: l.1.clone(),
                }
            } else {
                Plan::Done {
                    rid: l.0.clone(),
                    digest: l.1.clone(),
                }
            }
        }
        (Some(t), _) if t["frozen"] != true => {
            return Err(err(
                "move_target_in_use",
                format!(
                    "the target server {} already serves an app '{name}' ({}), and a move would replace its data. Source: {}. Nothing was changed",
                    dst.base,
                    describe(Some(t)),
                    describe(Some(&sapp))
                ),
                format!(
                    "if the target's copy is not wanted, freeze it with `spared freeze {name} --server {}` and run the move again (the restore keeps its data as a pre-restore backup). If both copies serve, decide which one holds the writes to keep first: there is no merge",
                    dst.base
                ),
            ));
        }
        _ => Plan::Full,
    };
    let summary = json!({
        "name": name,
        "source": {"server": src.base, "node": si["node"], "version": si["version"], "app": describe(Some(&sapp))},
        "target": {"server": dst.base, "node": ti["node"], "version": ti["version"], "app": describe(tapp.as_ref())},
        "bucket": sr["target"],
        "key_id": sr["key_id"],
    });
    let smoke_line = a.smoke_url.as_ref().map(|u| {
        format!("GET / on {u} with Host {name}.localhost. A 5xx or no answer freezes the target again and stops")
    });
    let mut steps_planned: Vec<String> = match &plan {
        Plan::Full => vec![
            "copy the code: the deployed wasm and spared.toml from the source, deployed on the target, which compiles it and keeps the app frozen".into(),
            format!("freeze {name} on the source: public requests get 503 app_frozen"),
            format!("back up {name} on the source and wait for the upload"),
            "fetch that backup on the target, check it, and compare its content digest with the source's".into(),
            format!("restore it on the target, whose epoch for {name} becomes one above the bucket's highest"),
            format!("thaw {name} on the target"),
        ],
        Plan::Verify { rid, .. } => vec![
            format!("an earlier run restored {rid} on the target: check the target holds it"),
            format!("thaw {name} on the target"),
        ],
        Plan::Done { rid, .. } => vec![format!(
            "nothing: an earlier run moved {name} ({rid}) and the source is frozen. The target is {}",
            describe(tapp.as_ref())
        )],
    };
    if !matches!(plan, Plan::Done { .. }) {
        if copy_limits {
            steps_planned.insert(
                0,
                format!("copy the per-app limits set on the source ({src_limits}) to the target, before its copy can serve"),
            );
        } else if !src_limits.is_null() && dst_limits != src_limits {
            steps_planned.insert(
                0,
                format!("keep the target's own per-app limits for {name} ({dst_limits}). The source's ({src_limits}) are not copied"),
            );
        }
        steps_planned.extend(smoke_line);
    }
    if a.dry_run {
        if json {
            let mut v = summary;
            v["dry_run"] = true.into();
            v["plan"] = json!(steps_planned);
            out_json(v);
        } else {
            println!(
                "move {name} from {} (node {}, {}) to {} (node {}, {})",
                src.base,
                si["node"].as_str().unwrap_or("?"),
                describe(Some(&sapp)),
                dst.base,
                ti["node"].as_str().unwrap_or("?"),
                describe(tapp.as_ref())
            );
            println!(
                "through {} (key id {})",
                sr["target"].as_str().unwrap_or("?"),
                sr["key_id"].as_str().unwrap_or("none: not encrypted")
            );
            println!("plan:");
            for (i, s) in steps_planned.iter().enumerate() {
                println!("  {}. {s}", i + 1);
            }
            println!("dry run: nothing was changed");
        }
        return Ok(());
    }

    let mut m = Mover {
        name: name.clone(),
        src,
        dst,
        json,
        steps: vec![],
        smoke_url: a.smoke_url,
        froze_source: false,
    };
    if copy_limits && !matches!(plan, Plan::Done { .. }) {
        m.dst
            .call(
                "PUT",
                &format!("/v1/apps/{name}/limits"),
                &[("content-type", "application/json".to_string())],
                Bytes::from(src_limits.to_string()),
            )
            .await?;
        m.step(format!(
            "copied the per-app limits of {name} ({src_limits}) to the target"
        ));
    }
    let (rid, digest, already) = match plan {
        Plan::Full => match m.copy_and_restore().await {
            Ok((r, d)) => (r, d, false),
            Err(cause) => return Err(m.abort(cause).await),
        },
        Plan::Verify { rid, digest } => {
            m.step(format!(
                "an earlier run restored {rid} on the target and stopped. Checking it"
            ));
            (rid, digest, false)
        }
        Plan::Done { rid, digest } => (rid, digest, true),
    };
    let t = if already {
        m.step(format!(
            "{name} was already moved: the target holds {rid} ({}) and the source is frozen",
            describe(tapp.as_ref())
        ));
        tapp.unwrap_or_default()
    } else {
        m.finish(&rid, &digest).await?
    };
    let (sb, tb) = (m.src.base.clone(), m.dst.base.clone());
    let next = vec![
        format!(
            "point DNS (or your proxy) for {name} at the target {tb}'s public address. spared never changes DNS"
        ),
        format!(
            "keep the source frozen until the old DNS records have expired, then remove it there: spared rm {name} --yes --server {sb} (its backups stay)"
        ),
        format!(
            "take a backup on the target (spared backup {name} --server {tb}): from then on a server still holding the old epoch has its uploads refused with remote_superseded"
        ),
    ];
    let undo = format!(
        "before any client writes to the target: spared freeze {name} --server {tb}, then spared thaw {name} --server {sb}"
    );
    if json {
        let mut v = summary;
        v["moved"] = true.into();
        v["already_moved"] = already.into();
        v["remote_id"] = rid.into();
        v["content_digest"] = digest.into();
        v["epoch"] = t["epoch"].clone();
        v["state"] = json!({"source": "frozen", "target": describe(Some(&t))});
        v["steps"] = json!(m.steps);
        v["next"] = json!(next);
        v["undo"] = undo.into();
        out_json(v);
    } else {
        println!(
            "moved {name}: the target is {}, and the source is frozen",
            describe(Some(&t))
        );
        for n in &next {
            println!("next: {n}");
        }
        println!("undo: {undo}");
    }
    Ok(())
}
