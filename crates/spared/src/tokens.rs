//! App tokens: bearer tokens that work only for one app's admin routes, within a set of scopes.
//!
//! The admin token mints them (`POST /v1/tokens`). The secret is shown once, in that response.
//! The server keeps only its SHA-256, one file per token under `<data-dir>/tokens/` (directory
//! 0700, files 0600, written to a temporary name, synced and renamed). A revoked token keeps its
//! file, marked revoked, so its holder is told `token_revoked` instead of `unauthorized`.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Every app token starts with this, so a secret scanner can find one in a repository or a log.
pub const PREFIX: &str = "spt_";
/// Hex digits after the prefix: 256 bits of randomness.
const SECRET_HEX: usize = 64;
/// Hex digits of the token's SHA-256 that name it in lists, revocations and the access log.
const ID_HEX: usize = 12;
pub const DIR: &str = "tokens";
pub const MAX_LABEL: usize = 100;
/// The longest lifetime `expires_in_secs` may ask for: ten years.
pub const MAX_EXPIRES_SECS: u64 = 3650 * 86_400;
/// A token's `last_used` is written to disk at most this often.
const LAST_USED_EVERY_SECS: u64 = 60;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Scope {
    Deploy,
    Read,
    Evict,
    Backup,
    Restore,
    Freeze,
    Rm,
}

impl Scope {
    pub const ALL: [Scope; 7] = [
        Scope::Deploy,
        Scope::Read,
        Scope::Evict,
        Scope::Backup,
        Scope::Restore,
        Scope::Freeze,
        Scope::Rm,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Scope::Deploy => "deploy",
            Scope::Read => "read",
            Scope::Evict => "evict",
            Scope::Backup => "backup",
            Scope::Restore => "restore",
            Scope::Freeze => "freeze",
            Scope::Rm => "rm",
        }
    }

    fn parse(s: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|x| x.word() == s)
    }
}

/// What a token gets when the operator names no scopes: deploy and look after its app, but not
/// delete it, replace its data or copy it.
pub const DEFAULT_SCOPES: [Scope; 3] = [Scope::Deploy, Scope::Read, Scope::Evict];

/// The scopes in `words`, sorted and without repeats. `Err` names the first unknown one.
pub fn parse_scopes<'a>(words: impl IntoIterator<Item = &'a str>) -> Result<Vec<Scope>, String> {
    let mut out = vec![];
    for w in words {
        let w = w.trim();
        match Scope::parse(w) {
            Some(s) => out.push(s),
            None => return Err(w.to_string()),
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn scope_list() -> String {
    Scope::ALL.map(Scope::word).join(", ")
}

/// Compare without an early exit, so the time taken does not depend on where two values differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The shape of an app token: the prefix and 64 lowercase hex digits.
pub fn looks_like_token(s: &str) -> bool {
    s.strip_prefix(PREFIX).is_some_and(|h| {
        h.len() == SECRET_HEX
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// A token id as lists print it: 12 lowercase hex digits.
pub fn valid_id(s: &str) -> bool {
    s.len() == ID_HEX
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn time_text(secs: u64) -> String {
    crate::backup::created_at(&crate::backup::timestamp_id(secs))
}

#[derive(Clone, Debug)]
pub struct Record {
    pub id: String,
    pub app: String,
    pub scopes: Vec<Scope>,
    sha256: [u8; 32],
    pub created: u64,
    pub expires: Option<u64>,
    pub last_used: Option<u64>,
    pub label: Option<String>,
    pub revoked: Option<u64>,
    /// The token may create its app with a deploy: it was made while no app had the name, and
    /// it has not created it yet. A token creates its app at most once.
    pub may_create: bool,
}

impl Record {
    pub fn status(&self, now: u64) -> &'static str {
        if self.revoked.is_some() {
            "revoked"
        } else if self.expires.is_some_and(|e| e <= now) {
            "expired"
        } else {
            "active"
        }
    }

    /// What lists and responses show: never the secret, never its hash.
    pub fn public_json(&self, now: u64) -> serde_json::Value {
        let t = |v: Option<u64>| v.map(time_text);
        serde_json::json!({
            "id": self.id,
            "app": self.app,
            "scopes": self.scopes.iter().map(|s| s.word()).collect::<Vec<_>>(),
            "status": self.status(now),
            "created_at": time_text(self.created),
            "expires_at": t(self.expires),
            "last_used_at": t(self.last_used),
            "revoked_at": t(self.revoked),
            "label": self.label,
            "may_create": self.may_create,
        })
    }

    fn file_json(&self) -> serde_json::Value {
        serde_json::json!({
            "format": 1,
            "id": self.id,
            "app": self.app,
            "scopes": self.scopes.iter().map(|s| s.word()).collect::<Vec<_>>(),
            "sha256": hex(&self.sha256),
            "created": self.created,
            "expires": self.expires,
            "last_used": self.last_used,
            "label": self.label,
            "revoked": self.revoked,
            "may_create": self.may_create,
        })
    }

    fn from_file_json(v: &serde_json::Value) -> Option<Record> {
        if v["format"].as_u64()? != 1 {
            return None;
        }
        let id = v["id"].as_str().filter(|s| valid_id(s))?.to_string();
        let app = v["app"]
            .as_str()
            .filter(|s| crate::grants::valid_name(s))?
            .to_string();
        let scopes = parse_scopes(
            v["scopes"]
                .as_array()?
                .iter()
                .map(|s| s.as_str().unwrap_or("?")),
        )
        .ok()?;
        let sha256 = unhex32(v["sha256"].as_str()?)?;
        if !hex(&sha256).starts_with(&id) {
            return None;
        }
        let opt = |k: &str| match &v[k] {
            serde_json::Value::Null => Some(None),
            x => x.as_u64().map(Some),
        };
        Some(Record {
            id,
            app,
            scopes,
            sha256,
            created: v["created"].as_u64()?,
            expires: opt("expires")?,
            last_used: opt("last_used")?,
            label: v["label"].as_str().map(str::to_string),
            revoked: opt("revoked")?,
            may_create: v["may_create"].as_bool()?,
        })
    }
}

/// Who an app token speaks for.
#[derive(Clone, Debug)]
pub struct Principal {
    pub id: String,
    pub app: String,
    pub scopes: Vec<Scope>,
    pub may_create: bool,
}

#[derive(Debug)]
pub enum Auth {
    /// No token with that secret.
    Unknown,
    Expired(String),
    Revoked(String),
    Valid(Principal),
}

pub struct Store {
    dir: PathBuf,
    records: Vec<Record>,
}

/// Write `rec` to `<dir>/<id>.json` through a temporary file that is synced and renamed, then
/// sync the directory, so a crash leaves the old file or the new one.
fn write_record(dir: &Path, rec: &Record) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{}.json.tmp", rec.id));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(rec.file_json().to_string().as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, dir.join(format!("{}.json", rec.id)))?;
    std::fs::File::open(dir)?.sync_all()
}

fn random_secret() -> std::io::Result<String> {
    let mut b = [0u8; SECRET_HEX / 2];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(format!("{PREFIX}{}", hex(&b)))
}

impl Store {
    /// Load every token under `<data-dir>/tokens/`, creating the directory. A file that cannot
    /// be read as a token is skipped with a warning naming the file: that token then works for
    /// nothing, which is the safe reading.
    pub fn open(data_dir: &Path) -> std::io::Result<Store> {
        let dir = data_dir.join(DIR);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut records = vec![];
        for e in std::fs::read_dir(&dir)?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let rec = std::fs::read_to_string(e.path())
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| Record::from_file_json(&v))
                .filter(|r| r.id == stem);
            match rec {
                Some(r) => records.push(r),
                None => eprintln!(
                    "warning: {DIR}/{name} is not a token this spared reads; that token is refused"
                ),
            }
        }
        records.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
        Ok(Store { dir, records })
    }

    /// Make a token for `app` and store its hash. Returns the secret, which is not kept.
    pub fn mint(
        &mut self,
        app: &str,
        scopes: Vec<Scope>,
        expires: Option<u64>,
        label: Option<String>,
        may_create: bool,
        now: u64,
    ) -> std::io::Result<(String, Record)> {
        loop {
            let secret = random_secret()?;
            let sha256: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
            let id = hex(&sha256)[..ID_HEX].to_string();
            if self.records.iter().any(|r| r.id == id) {
                continue;
            }
            let rec = Record {
                id,
                app: app.to_string(),
                scopes,
                sha256,
                created: now,
                expires,
                last_used: None,
                label,
                revoked: None,
                may_create,
            };
            write_record(&self.dir, &rec)?;
            self.records.push(rec.clone());
            return Ok((secret, rec));
        }
    }

    pub fn list(&self, app: Option<&str>) -> Vec<Record> {
        self.records
            .iter()
            .filter(|r| app.is_none_or(|a| r.app == a))
            .cloned()
            .collect()
    }

    /// Revoke the token `id`. `None` when there is no such token. The token stops working in
    /// this process even when the file cannot be written; the error says it would work again
    /// after a restart.
    pub fn revoke(&mut self, id: &str, now: u64) -> Option<(Record, bool, std::io::Result<()>)> {
        let rec = self.records.iter_mut().find(|r| r.id == id)?;
        let already = rec.revoked.is_some();
        if already {
            return Some((rec.clone(), true, Ok(())));
        }
        rec.revoked = Some(now);
        let written = write_record(&self.dir, rec);
        Some((rec.clone(), false, written))
    }

    /// Revoke every live token of `app` (its app was removed). Returns how many, and the first
    /// write error.
    pub fn revoke_app(&mut self, app: &str, now: u64) -> (usize, Option<std::io::Error>) {
        let (mut n, mut first_err) = (0, None);
        for rec in self.records.iter_mut() {
            if rec.app == app && rec.revoked.is_none() {
                rec.revoked = Some(now);
                n += 1;
                if let Err(e) = write_record(&self.dir, rec) {
                    first_err.get_or_insert(e);
                }
            }
        }
        (n, first_err)
    }

    /// Find the token whose hash matches `presented`. Every stored hash is compared in full and
    /// without an early exit.
    pub fn authenticate(&mut self, presented: &str, now: u64) -> Auth {
        if !looks_like_token(presented) {
            return Auth::Unknown;
        }
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let mut found = None;
        for (i, r) in self.records.iter().enumerate() {
            if ct_eq(&r.sha256, &digest) {
                found = Some(i);
            }
        }
        let Some(i) = found else {
            return Auth::Unknown;
        };
        let rec = &mut self.records[i];
        match rec.status(now) {
            "revoked" => return Auth::Revoked(rec.id.clone()),
            "expired" => return Auth::Expired(rec.id.clone()),
            _ => {}
        }
        if rec
            .last_used
            .is_none_or(|t| now.saturating_sub(t) >= LAST_USED_EVERY_SECS)
        {
            rec.last_used = Some(now);
            if let Err(e) = write_record(&self.dir, rec) {
                eprintln!("token {}: cannot record its last use: {e}", rec.id);
            }
        }
        Auth::Valid(Principal {
            id: rec.id.clone(),
            app: rec.app.clone(),
            scopes: rec.scopes.clone(),
            may_create: rec.may_create,
        })
    }

    /// The token `id` has created its app: it may not create it again.
    pub fn created_app(&mut self, id: &str) {
        if let Some(rec) = self.records.iter_mut().find(|r| r.id == id)
            && rec.may_create
        {
            rec.may_create = false;
            if let Err(e) = write_record(&self.dir, rec) {
                eprintln!(
                    "token {}: cannot record that it created its app: {e}",
                    rec.id
                );
            }
        }
    }
}

/// Failed authentications per source address. After `MAX_FAILURES` wrong tokens within a window,
/// every request from that address that carries a token is refused until the window ends, the
/// right token included, so guessing learns nothing while it is shut out.
pub struct AuthFailures {
    by_addr: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);
pub const MAX_FAILURES: u32 = 30;
/// Addresses tracked at once; past it, those whose window has ended are dropped first.
const MAX_TRACKED: usize = 4096;

impl Default for AuthFailures {
    fn default() -> Self {
        AuthFailures {
            by_addr: Mutex::new(HashMap::new()),
        }
    }
}

impl AuthFailures {
    /// Seconds until `ip` may try again, when it is shut out.
    pub fn retry_after(&self, ip: IpAddr, now: Instant) -> Option<u64> {
        let m = self.by_addr.lock().unwrap();
        let (start, n) = *m.get(&ip)?;
        let end = start + FAILURE_WINDOW;
        (n >= MAX_FAILURES && now < end).then(|| (end - now).as_secs().max(1))
    }

    pub fn failed(&self, ip: IpAddr, now: Instant) {
        let mut m = self.by_addr.lock().unwrap();
        if m.len() >= MAX_TRACKED && !m.contains_key(&ip) {
            m.retain(|_, (start, _)| now.duration_since(*start) < FAILURE_WINDOW);
            if m.len() >= MAX_TRACKED {
                m.clear();
            }
        }
        let e = m.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= FAILURE_WINDOW {
            *e = (now, 0);
        }
        e.1 = e.1.saturating_add(1);
    }
}

/// What an admin route needs from the caller.
#[derive(Debug, PartialEq)]
pub enum Need {
    /// Only the admin token: metrics, the server, tokens, code export, move deploys, and any
    /// route not listed here.
    Admin,
    /// A token for the app `name` with `scope`.
    App(String, Scope),
    /// `GET /v1/apps`: a token with `read` sees its own app only.
    OwnList,
}

/// The access an admin request needs, from its method, path segments and query. Unknown routes
/// need the admin token, so a route added to the server without a line here is closed to app
/// tokens.
pub fn need(method: &str, segs: &[&str], frozen_param: bool) -> Need {
    use Scope::*;
    let app = |n: &str, s: Scope| Need::App(n.to_string(), s);
    match (method, segs) {
        ("GET", ["v1", "apps"]) => Need::OwnList,
        ("PUT", ["v1", "apps", _]) if frozen_param => Need::Admin,
        ("PUT", ["v1", "apps", n]) => app(n, Deploy),
        ("GET", ["v1", "apps", n]) => app(n, Read),
        ("DELETE", ["v1", "apps", n]) => app(n, Rm),
        ("GET", ["v1", "apps", n, "logs"]) => app(n, Read),
        ("POST", ["v1", "apps", n, "evict"]) => app(n, Evict),
        ("POST", ["v1", "apps", n, "backup"]) => app(n, Backup),
        ("GET", ["v1", "apps", n, "backups"]) => app(n, Backup),
        ("DELETE", ["v1", "apps", n, "backups", _]) => app(n, Backup),
        ("GET", ["v1", "apps", n, "backups", _, "remote"]) => app(n, Backup),
        ("GET", ["v1", "apps", n, "remote-backups"]) => app(n, Backup),
        ("POST", ["v1", "apps", n, "restore"]) => app(n, Restore),
        ("POST", ["v1", "apps", n, "fetch"]) => app(n, Restore),
        ("GET", ["v1", "apps", n, "fetch", _]) => app(n, Restore),
        ("POST", ["v1", "apps", n, "freeze"]) => app(n, Freeze),
        ("POST", ["v1", "apps", n, "thaw"]) => app(n, Freeze),
        _ => Need::Admin,
    }
}

/// `30d`, `12h`, `45m`, `90s` or a bare number of seconds, as seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (&s[..i], c),
        _ => (s, 's'),
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = num.parse().ok()?;
    let mul = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    n.checked_mul(mul).filter(|&v| v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spared-tokens-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn constant_time_compare_matches_only_equal_bytes() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"xbc"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"ab", b"abc"));
        let a = [7u8; 32];
        let mut b = a;
        for i in 0..32 {
            b[i] ^= 1;
            assert!(!ct_eq(&a, &b), "byte {i}");
            b[i] ^= 1;
        }
        assert!(ct_eq(&a, &b));
    }

    #[test]
    fn a_token_has_the_prefix_and_256_bits() {
        let d = scratch("shape");
        let mut s = Store::open(&d).unwrap();
        let (secret, rec) = s
            .mint("app", DEFAULT_SCOPES.to_vec(), None, None, true, 100)
            .unwrap();
        assert!(looks_like_token(&secret), "{secret}");
        assert_eq!(secret.len(), PREFIX.len() + 64);
        assert!(valid_id(&rec.id));
        let (other, _) = s
            .mint("app", DEFAULT_SCOPES.to_vec(), None, None, true, 100)
            .unwrap();
        assert_ne!(secret, other);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_the_hash_is_stored_and_it_survives_a_reload() {
        let d = scratch("store");
        let mut s = Store::open(&d).unwrap();
        let (secret, rec) = s
            .mint(
                "app",
                vec![Scope::Deploy],
                Some(1000),
                Some("ci".into()),
                false,
                100,
            )
            .unwrap();
        for e in std::fs::read_dir(d.join(DIR)).unwrap().flatten() {
            let text = std::fs::read_to_string(e.path()).unwrap();
            assert!(!text.contains(&secret));
            assert!(!text.contains(&secret[PREFIX.len()..]));
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(e.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let mut again = Store::open(&d).unwrap();
        match again.authenticate(&secret, 200) {
            Auth::Valid(p) => {
                assert_eq!((p.id.as_str(), p.app.as_str()), (rec.id.as_str(), "app"));
                assert_eq!(p.scopes, vec![Scope::Deploy]);
                assert!(!p.may_create);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            again.authenticate(&secret, 1000),
            Auth::Expired(_)
        ));
        let mut wrong = secret.clone();
        wrong.replace_range(10..11, if &secret[10..11] == "0" { "1" } else { "0" });
        assert!(matches!(again.authenticate(&wrong, 200), Auth::Unknown));
        assert!(matches!(
            again.authenticate("spt_short", 200),
            Auth::Unknown
        ));
        let (_, already, written) = again.revoke(&rec.id, 300).unwrap();
        assert!(!already && written.is_ok());
        let mut third = Store::open(&d).unwrap();
        assert!(matches!(third.authenticate(&secret, 400), Auth::Revoked(_)));
        assert!(third.revoke("000000000000", 1).is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_damaged_token_file_is_skipped() {
        let d = scratch("damaged");
        let mut s = Store::open(&d).unwrap();
        let (secret, rec) = s
            .mint("app", DEFAULT_SCOPES.to_vec(), None, None, true, 1)
            .unwrap();
        let f = d.join(DIR).join(format!("{}.json", rec.id));
        let text = std::fs::read_to_string(&f).unwrap();
        std::fs::write(&f, text.replace("\"app\":\"app\"", "\"app\":\"BAD NAME\"")).unwrap();
        let mut again = Store::open(&d).unwrap();
        assert!(again.list(None).is_empty());
        assert!(matches!(again.authenticate(&secret, 2), Auth::Unknown));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn removing_an_app_revokes_its_tokens_only() {
        let d = scratch("rm");
        let mut s = Store::open(&d).unwrap();
        let (a, _) = s.mint("a", vec![Scope::Rm], None, None, false, 1).unwrap();
        let (b, _) = s.mint("b", vec![Scope::Rm], None, None, false, 1).unwrap();
        let (n, e) = s.revoke_app("a", 2);
        assert_eq!(n, 1);
        assert!(e.is_none());
        assert!(matches!(s.authenticate(&a, 3), Auth::Revoked(_)));
        assert!(matches!(s.authenticate(&b, 3), Auth::Valid(_)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_token_creates_its_app_once() {
        let d = scratch("create");
        let mut s = Store::open(&d).unwrap();
        let (t, rec) = s
            .mint("a", DEFAULT_SCOPES.to_vec(), None, None, true, 1)
            .unwrap();
        s.created_app(&rec.id);
        let mut again = Store::open(&d).unwrap();
        match again.authenticate(&t, 2) {
            Auth::Valid(p) => assert!(!p.may_create),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn scopes_parse_and_unknown_ones_are_named() {
        assert_eq!(
            parse_scopes(["read", "deploy", "read"]).unwrap(),
            vec![Scope::Deploy, Scope::Read]
        );
        assert_eq!(parse_scopes(["deploy", "admin"]).unwrap_err(), "admin");
        assert_eq!(
            scope_list(),
            "deploy, read, evict, backup, restore, freeze, rm"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30d"), Some(30 * 86_400));
        assert_eq!(parse_duration("12h"), Some(12 * 3600));
        assert_eq!(parse_duration("45m"), Some(2700));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("90"), Some(90));
        for bad in ["", "d", "0d", "-1d", "3w", "1.5d", "99999999999999999999d"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn routes_map_to_scopes_and_unknown_routes_need_the_admin_token() {
        use Scope::*;
        let n = |m: &str, p: &str| {
            let segs: Vec<&str> = p.trim_matches('/').split('/').collect();
            need(m, &segs, false)
        };
        let a = |s| Need::App("x".into(), s);
        assert_eq!(n("GET", "/v1/apps"), Need::OwnList);
        assert_eq!(n("PUT", "/v1/apps/x"), a(Deploy));
        assert_eq!(need("PUT", &["v1", "apps", "x"], true), Need::Admin);
        assert_eq!(n("GET", "/v1/apps/x"), a(Read));
        assert_eq!(n("GET", "/v1/apps/x/logs"), a(Read));
        assert_eq!(n("DELETE", "/v1/apps/x"), a(Rm));
        assert_eq!(n("POST", "/v1/apps/x/evict"), a(Evict));
        assert_eq!(n("POST", "/v1/apps/x/backup"), a(Backup));
        assert_eq!(n("GET", "/v1/apps/x/backups"), a(Backup));
        assert_eq!(
            n("DELETE", "/v1/apps/x/backups/20261110T040506Z"),
            a(Backup)
        );
        assert_eq!(n("GET", "/v1/apps/x/backups/id/remote"), a(Backup));
        assert_eq!(n("GET", "/v1/apps/x/remote-backups"), a(Backup));
        assert_eq!(n("POST", "/v1/apps/x/restore"), a(Restore));
        assert_eq!(n("POST", "/v1/apps/x/fetch"), a(Restore));
        assert_eq!(n("GET", "/v1/apps/x/fetch/r"), a(Restore));
        assert_eq!(n("POST", "/v1/apps/x/freeze"), a(Freeze));
        assert_eq!(n("POST", "/v1/apps/x/thaw"), a(Freeze));
        for (m, p) in [
            ("GET", "/metrics"),
            ("GET", "/v1/server"),
            ("GET", "/v1/apps/x/code"),
            ("GET", "/v1/tokens"),
            ("POST", "/v1/tokens"),
            ("DELETE", "/v1/tokens/abc"),
            ("POST", "/v1/apps/x"),
            ("GET", "/v1/apps/x/unknown"),
            ("GET", "/v2/apps"),
        ] {
            assert_eq!(n(m, p), Need::Admin, "{m} {p}");
        }
    }

    #[test]
    fn wrong_tokens_shut_an_address_out_for_the_window() {
        let f = AuthFailures::default();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let other: IpAddr = "127.0.0.2".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            f.failed(ip, t0);
        }
        assert_eq!(f.retry_after(ip, t0), None);
        f.failed(ip, t0);
        assert_eq!(f.retry_after(ip, t0), Some(60));
        assert_eq!(f.retry_after(other, t0), None);
        assert_eq!(f.retry_after(ip, t0 + FAILURE_WINDOW), None);
        f.failed(ip, t0 + FAILURE_WINDOW);
        assert_eq!(f.retry_after(ip, t0 + FAILURE_WINDOW), None);
    }
}
