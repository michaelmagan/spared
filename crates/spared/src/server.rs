use crate::backup;
use crate::compile::{self, CompileError, CompileLimits};
use crate::datadir;
use crate::fsguard::AppCaps;
use crate::grants::{Manifest, Maxima, ungranted_imports, valid_name};
use crate::logcap::AppLog;
use crate::obs::{AccessLine, ErrorKind, Evicted, Gauges, Listener, Metrics, RequestIds};
use crate::prefetch;
use crate::quota::{self, AppQuota};
use crate::sandbox::{CallError, Guarded, TrapKind, call_handler};
use crate::tokens::{self, Need, Principal, Scope, ct_eq};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wasmtime::component::Component;
use wasmtime::{Engine, Result, Store};
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::p2::bindings::{Proxy, ProxyPre};

pub struct Config {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub admin: SocketAddr,
    pub idle: Duration,
    pub domain: Option<String>,
    pub examples_dir: Option<PathBuf>,
    /// Serve `/_/<app>/` on any host. Puts every app on one browser origin.
    pub path_routing: bool,
    pub max: Maxima,
    pub admin_token: String,
    /// Time a client gets to send request headers, and separately the request body.
    pub client_timeout: Duration,
    /// Open connections on the public listener.
    pub max_connections: usize,
    /// MB of stored components to read into the page cache at start; 0 turns reading ahead off.
    pub prefetch_mb: u64,
    /// Backups kept per app (and, separately, pre-restore copies); the oldest go first.
    pub max_backups: usize,
    /// Write one line per request to stdout.
    pub access_log: bool,
    /// What one app's guest may ask of the host's filesystem.
    pub caps: AppCaps,
    pub compile: CompileLimits,
    /// Off-machine backups (`--backup-to`), checked at start; `None` when not configured.
    pub remote: Option<crate::remote::Settings>,
}

const ADMIN_CONNECTIONS: usize = 32;
/// The file in the data dir that a running server holds locked.
const DATA_LOCK: &str = ".lock";
/// Largest `wasi:random` request a guest may make; wasmtime's default (64 MiB) is allocated in
/// the host per call.
const DEPLOY_BODY_TIMEOUT: Duration = Duration::from_secs(120);
const LOG_TAIL_BYTES: u64 = 1 << 20;
/// Response header that carries the request's id, the `request_id` of its access-log line.
const REQUEST_ID_HEADER: &str = "x-spared-request-id";
const PATH_ROUTING_WARNING: &str = "path routing (/_/<app>/) is on: every app is served from one browser origin, so any app's pages can read and change other apps' data in a visitor's browser. Use it only for the bundled examples; give each app its own host (<app>.<domain>) for anything else";

struct Live {
    store: Store<Guarded>,
    proxy: Proxy,
}

/// A compiled component and its pre-linked instance template, shared by every app that deployed
/// the same bytes. Per-app state (manifest, limits, store, data) never lives here.
struct SharedComponent {
    hash: String,
    component: Component,
    pre: ProxyPre<Guarded>,
    /// Set at the first wake of the component in this process, which checks the compiled file
    /// against the page cache once.
    warmed: Arc<AtomicBool>,
}

pub struct App {
    name: String,
    dir: PathBuf,
    manifest: Manifest,
    version: u64,
    shared: Arc<SharedComponent>,
    live: Arc<tokio::sync::Mutex<Option<Live>>>,
    /// Set (under the `live` lock) when a redeploy or delete replaced this app. A request that
    /// sees it after taking the lock looks the name up again instead of waking a second
    /// instance on the same data directory.
    retired: AtomicBool,
    /// Requests that are running or waiting for the instance.
    in_flight: AtomicUsize,
    /// Set (under the `live` lock) while the app is frozen: public requests get 503 `app_frozen`.
    /// The `frozen` marker in the app directory carries it across restarts and redeploys.
    frozen: AtomicBool,
    /// What the marker says: when, by what, and why.
    freeze: Mutex<Option<serde_json::Value>>,
    hot: AtomicBool,
    last_ms: AtomicU64,
    requests: AtomicU64,
    wakes: AtomicU64,
    errors: AtomicU64,
    /// The operator's limits for this app and what it has used. One per name, shared with the
    /// app's later deploys (see `Server::quotas`).
    quota: Arc<AppQuota>,
}

pub struct Server {
    cfg: Config,
    engine: Engine,
    apps: RwLock<HashMap<String, Arc<App>>>,
    /// In-memory components by sha256. The mutex also serializes every change to
    /// `<data-dir>/components/`, so a link being made never races a removal.
    components: Mutex<HashMap<String, Weak<SharedComponent>>>,
    /// One lock per app name, held for the whole of a deploy or delete.
    name_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    started: Instant,
    metrics: Metrics,
    ids: RequestIds,
    /// Request body bytes held in memory across all apps, before and while a request runs.
    body_budget: Arc<BodyBudget>,
    /// The bucket backups are uploaded to, when `--backup-to` is set.
    remote: Option<Arc<crate::remote::Remote>>,
    /// App tokens, by the SHA-256 of their secret (`<data-dir>/tokens/`).
    tokens: Mutex<tokens::Store>,
    /// Wrong tokens by source address, to slow down guessing.
    auth_failures: tokens::AuthFailures,
    /// Per-app limits set by the operator and the CPU and egress counters, by app name: one for
    /// each deployed app and each name with a stored override. Kept across redeploys, evictions
    /// and freezes. An app's counters start over when it is removed or the server restarts.
    quotas: Mutex<HashMap<String, Arc<AppQuota>>>,
    /// Held while the files under `<data-dir>/limits/` and the format stamp change, so the stamp
    /// says format 2 whenever an override is on disk.
    limits_io: Mutex<()>,
}

/// Who an admin request speaks for.
enum Caller {
    Admin,
    App(Principal),
}

/// The bytes of request bodies the server may hold in memory at once, over every connection and
/// app. A body is buffered before its request takes a place in its app's queue, so without a
/// total a crowd of slow clients each holding a large upload could use all of the memory.
struct BodyBudget {
    total: u64,
    used: AtomicU64,
}

impl BodyBudget {
    /// A budget of `total` bytes, never less than the largest single body (`one`), so a request
    /// that is within `--max-request-mb` can always proceed when it is the only one.
    fn new(total: u64, one: u64) -> Arc<BodyBudget> {
        Arc::new(BodyBudget {
            total: total.max(one),
            used: AtomicU64::new(0),
        })
    }

    fn permit(self: &Arc<Self>) -> BodyPermit {
        BodyPermit {
            budget: self.clone(),
            held: 0,
        }
    }
}

/// Bytes taken from a [`BodyBudget`]; all of them go back when it is dropped.
struct BodyPermit {
    budget: Arc<BodyBudget>,
    held: u64,
}

impl BodyPermit {
    /// Hold `n` bytes in all; false, with nothing more taken, when the budget has less than that
    /// to give.
    fn grow_to(&mut self, n: u64) -> bool {
        let Some(extra) = n.checked_sub(self.held).filter(|e| *e > 0) else {
            return true;
        };
        let total = self.budget.total;
        let taken = self
            .budget
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |u| {
                u.checked_add(extra).filter(|u| *u <= total)
            });
        if taken.is_err() {
            return false;
        }
        self.held = n;
        true
    }
}

impl Drop for BodyPermit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.held, Ordering::AcqRel);
    }
}

enum BodyFailure {
    TooLarge,
    NoRoom,
    Read(hyper::Error),
}

/// Read a request body of at most `max` bytes. The memory it holds is taken from `permit` before
/// it is allocated: all of `declared` (the content length, when the client gave one) up front, so
/// a server with its budget used up refuses before reading anything, and otherwise as the body
/// grows.
async fn collect_body(
    mut body: Incoming,
    max: u64,
    declared: Option<u64>,
    permit: &mut BodyPermit,
) -> std::result::Result<Bytes, BodyFailure> {
    let mut buf = bytes::BytesMut::new();
    if let Some(n) = declared.filter(|n| *n > 0) {
        let n = n.min(max);
        if !permit.grow_to(n) {
            return Err(BodyFailure::NoRoom);
        }
        buf.reserve(n as usize);
    }
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(BodyFailure::Read)?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let need = (buf.len() + data.len()) as u64;
        if need > max {
            return Err(BodyFailure::TooLarge);
        }
        if need > buf.capacity() as u64 {
            let grown = need.max(buf.capacity() as u64 * 2).min(max);
            if !permit.grow_to(grown) {
                return Err(BodyFailure::NoRoom);
            }
            buf.reserve(grown as usize - buf.len());
        }
        buf.extend_from_slice(&data);
    }
    Ok(buf.freeze())
}

type Resp = Response<Full<Bytes>>;

impl App {
    fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    fn freeze_info(&self) -> Option<serde_json::Value> {
        self.freeze.lock().unwrap().clone()
    }

    /// Record a freeze or a thaw in memory. The caller holds the `live` lock and has written or
    /// removed the marker.
    fn set_freeze(&self, info: Option<serde_json::Value>) {
        self.frozen.store(info.is_some(), Ordering::Release);
        *self.freeze.lock().unwrap() = info;
    }
}

/// A place among the requests one app has running or waiting; given back when dropped.
struct InFlight(Arc<App>);

impl InFlight {
    /// A place for a request to `app`, or `None` when `max` are already taken.
    fn enter(app: &Arc<App>, max: usize) -> Option<InFlight> {
        let n = app.in_flight.fetch_add(1, Ordering::AcqRel);
        let place = InFlight(app.clone());
        (n < max).then_some(place)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 503 for a request the app cannot take now: the client may try again shortly.
fn app_busy(name: &str, why: &str) -> Resp {
    let mut r = err_resp(
        StatusCode::SERVICE_UNAVAILABLE,
        "app_busy",
        format!("app '{name}' {why}"),
        "retry shortly; one app handles one request at a time, and the server's --max-app-queue limits how many may wait",
    );
    r.headers_mut().insert("retry-after", "1".parse().unwrap());
    r
}

fn json_resp(status: StatusCode, v: serde_json::Value) -> Resp {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(v.to_string() + "\n")))
        .unwrap()
}

/// An error produced by spared itself. The kind also rides in the response's extensions (never
/// sent to the client) for the access log and the error counters.
fn err_resp(status: StatusCode, kind: &'static str, message: String, fix: &str) -> Resp {
    let mut r = json_resp(
        status,
        serde_json::json!({"error": {"kind": kind, "message": message, "fix": fix}}),
    );
    r.extensions_mut().insert(ErrorKind(kind));
    r
}

fn no_such_app(name: &str) -> Resp {
    err_resp(
        StatusCode::NOT_FOUND,
        "no_such_app",
        format!("no app '{name}'"),
        "list apps with `spared ps`",
    )
}

fn host_app(host: &str, domain: Option<&str>) -> Option<String> {
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    if let Some(n) = host.strip_suffix(".localhost") {
        return Some(n.to_string());
    }
    let d = domain?.trim_matches('.').to_ascii_lowercase();
    if d.is_empty() {
        return None;
    }
    host.strip_suffix(&format!(".{d}")).map(str::to_string)
}

fn split_prefix(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/_/")?;
    match rest.find('/') {
        Some(i) => Some((&rest[..i], &rest[i..])),
        None => Some((rest, "")),
    }
}

/// An admin `Host` must be an IP literal or `localhost`, with an optional port. A DNS name that
/// a web page rebound to 127.0.0.1 fails this check.
fn admin_host_ok(host: &str) -> bool {
    let port_ok = |p: &str| {
        p.is_empty()
            || p.strip_prefix(':')
                .is_some_and(|p| p.parse::<u16>().is_ok())
    };
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((ip, port)) => ip.parse::<Ipv6Addr>().is_ok() && port_ok(port),
            None => false,
        };
    }
    let (name, port) = match host.find(':') {
        Some(i) => host.split_at(i),
        None => (host, ""),
    };
    port_ok(port) && (name.eq_ignore_ascii_case("localhost") || name.parse::<Ipv4Addr>().is_ok())
}

pub fn check_admin_bind(
    admin: SocketAddr,
    public_ok: bool,
) -> std::result::Result<(), datadir::DataDirError> {
    if admin.ip().is_loopback() || public_ok {
        return Ok(());
    }
    Err(datadir::DataDirError {
        kind: "admin_bind_refused",
        message: format!(
            "refusing to bind the admin listener to {admin}: the admin API deploys code and any app's outbound fetch could reach it on a public address"
        ),
        fix: "use --admin-listen 127.0.0.1:8081 (the default), or pass --admin-public-i-know only if something you control restricts access to that address".into(),
    })
}

fn create_private_dir(p: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
}

/// Create or replace a private file with `contents`, synced to disk before it returns.
fn write_private(p: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(contents)?;
    f.sync_all()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Make `dst` another name for `src`'s file (hard link), or a private copy where the file
/// system cannot link. Replaces `dst` atomically when it exists.
fn link_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".link");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(src, &tmp).is_err() {
        write_private(&tmp, &std::fs::read(src)?)?;
    }
    std::fs::rename(&tmp, dst)
}

fn random_token() -> std::io::Result<String> {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Shortest admin token accepted from --admin-token or $SPARED_ADMIN_TOKEN.
const MIN_TOKEN_LEN: usize = 16;

/// The admin token: `given` (from --admin-token or $SPARED_ADMIN_TOKEN), else
/// `<data-dir>/admin-token`, else a new random token written there with mode 0600. A given token
/// shorter than 16 characters, or a token file that other users can read or that another user
/// owns, is refused.
pub fn admin_token(data_dir: &Path, given: Option<String>) -> Result<String> {
    let file = data_dir.join("admin-token");
    let given = given
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if let Some(t) = &given {
        if t.len() < MIN_TOKEN_LEN {
            return Err(wasmtime::Error::msg(format!(
                "the admin token given is {} characters; at least {MIN_TOKEN_LEN} are required. Fix: use a long random token (openssl rand -hex 32), or leave it unset to have one generated",
                t.len()
            )));
        }
    } else if let Ok(m) = std::fs::symlink_metadata(&file) {
        let me = unsafe { libc::geteuid() };
        if !m.is_file() || m.uid() != me || m.mode() & 0o077 != 0 {
            return Err(wasmtime::Error::msg(format!(
                "{} must be a regular file owned by uid {me} with mode 0600 (it is mode {:o}, owner {}). Fix: chmod 600 it and chown it to the server's user, or delete it to have a new token generated",
                file.display(),
                m.mode() & 0o777,
                m.uid()
            )));
        }
    }
    let token = match given {
        Some(t) => t,
        None => match std::fs::read_to_string(&file).map(|t| t.trim().to_string()) {
            Ok(t) if !t.is_empty() => t,
            _ => {
                create_private_dir(data_dir)?;
                let t = random_token()?;
                write_private(&file, t.as_bytes())?;
                eprintln!("admin token written to {}", file.display());
                t
            }
        },
    };
    Ok(token)
}

/// For single-user local use: copy the token to `~/.spared/token` (where the CLI looks after
/// $SPARED_TOKEN) when that file does not exist yet.
pub fn share_token_with_cli(token: &str) {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let user = home.join(".spared/token");
    if user.exists() {
        return;
    }
    let r = create_private_dir(&home.join(".spared"))
        .and_then(|_| write_private(&user, token.as_bytes()));
    match r {
        Ok(()) => eprintln!("admin token copied to {} for the CLI", user.display()),
        Err(e) => eprintln!("note: could not write {}: {e}", user.display()),
    }
}

#[derive(Debug)]
struct DeployError {
    status: StatusCode,
    kind: &'static str,
    message: String,
    fix: String,
}

impl DeployError {
    fn new(
        status: StatusCode,
        kind: &'static str,
        message: String,
        fix: impl Into<String>,
    ) -> Self {
        DeployError {
            status,
            kind,
            message,
            fix: fix.into(),
        }
    }

    fn into_resp(self) -> Resp {
        err_resp(self.status, self.kind, self.message, &self.fix)
    }
}

fn compile_error_text(e: CompileError) -> String {
    match e {
        CompileError::Invalid(m) | CompileError::TooComplex(m) | CompileError::Failed(m) => m,
    }
}

fn check_name(name: &str) -> std::result::Result<(), DeployError> {
    if valid_name(name) {
        return Ok(());
    }
    Err(DeployError::new(
        StatusCode::BAD_REQUEST,
        "bad_name",
        format!("invalid app name '{name}'"),
        "use 1 to 40 lowercase letters, digits and dashes",
    ))
}

/// The last `max` bytes of a file as text; invalid UTF-8 becomes U+FFFD instead of hiding the log.
fn read_tail(p: &Path, max: u64) -> String {
    let mut buf = vec![];
    if let Ok(mut f) = std::fs::File::open(p) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len > max {
            let _ = f.seek(SeekFrom::Start(len - max));
        }
        let _ = f.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// hyper's timer, on tokio (header read timeouts need one).
#[derive(Clone)]
struct TokioTimer;

struct TokioSleep(Pin<Box<tokio::time::Sleep>>);

impl std::future::Future for TokioSleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.0.as_mut().poll(cx)
    }
}

impl hyper::rt::Sleep for TokioSleep {}

impl hyper::rt::Timer for TokioTimer {
    fn sleep(&self, d: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(TokioSleep(Box::pin(tokio::time::sleep(d))))
    }
    fn sleep_until(&self, at: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(TokioSleep(Box::pin(tokio::time::sleep_until(at.into()))))
    }
}

/// The longest a public connection stays open, however busy: 30 client timeouts (300 s by
/// default), at least 60 s. Then the server finishes the request in progress and closes it.
fn connection_lifetime(client_timeout: Duration) -> Duration {
    (client_timeout * 30).max(Duration::from_secs(60))
}

/// A connection whose writes fail once one has made no progress for `limit`: a client that
/// stops reading cannot hold its connection slot and its buffered response for long.
struct WriteDeadline<T> {
    inner: T,
    limit: Duration,
    stalled: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<T> WriteDeadline<T> {
    fn new(inner: T, limit: Duration) -> Self {
        WriteDeadline {
            inner,
            limit,
            stalled: None,
        }
    }

    /// Track a write-side poll: progress clears the timer, a pending poll starts it, and an
    /// expired timer turns the pending poll into an error.
    fn track<R>(
        &mut self,
        cx: &mut Context<'_>,
        r: Poll<std::io::Result<R>>,
    ) -> Poll<std::io::Result<R>> {
        if r.is_ready() {
            self.stalled = None;
            return r;
        }
        let limit = self.limit;
        let t = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        if t.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the client stopped reading",
            )));
        }
        Poll::Pending
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for WriteDeadline<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for WriteDeadline<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let me = self.get_mut();
        let r = Pin::new(&mut me.inner).poll_write(cx, buf);
        me.track(cx, r)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let me = self.get_mut();
        let r = Pin::new(&mut me.inner).poll_write_vectored(cx, bufs);
        me.track(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let me = self.get_mut();
        let r = Pin::new(&mut me.inner).poll_flush(cx);
        me.track(cx, r)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let me = self.get_mut();
        let r = Pin::new(&mut me.inner).poll_shutdown(cx);
        me.track(cx, r)
    }
}

type IdleGuard = tokio::sync::OwnedMutexGuard<Option<Live>>;

const RESTORE_TMP: &str = "data.restore-tmp";
const RESTORE_READY: &str = "data.restore-ready";

const BACKUP_CONTAINS: &str = "app data only (the app's data/ directory, so /data/app.db and any other files it wrote); not its code, manifest or logs";

fn no_such_backup(name: &str, id: &str) -> Resp {
    err_resp(
        StatusCode::NOT_FOUND,
        "no_such_backup",
        format!("app '{name}' has no backup '{id}'"),
        "list them with `spared backups <name>`",
    )
}

fn check_backup_id(id: &str) -> std::result::Result<(), DeployError> {
    if backup::valid_id(id) {
        return Ok(());
    }
    Err(DeployError::new(
        StatusCode::BAD_REQUEST,
        "bad_backup_id",
        format!(
            "'{}' is not a backup id",
            id.chars().take(60).collect::<String>()
        ),
        "use an id from `spared backups <name>`, like 20261110T040506Z",
    ))
}

fn check_remote_id(rid: &str) -> std::result::Result<(), DeployError> {
    if crate::remote::parse_remote_id(rid).is_some() {
        return Ok(());
    }
    Err(DeployError::new(
        StatusCode::BAD_REQUEST,
        "bad_remote_id",
        format!(
            "'{}' is not a remote backup id",
            rid.chars().take(80).collect::<String>()
        ),
        "use an id from `spared backups <name> --remote`, like 20261112T040506Z.7f3a9c0a1b2c.1",
    ))
}

/// The value of `key` in a query string, as sent (no decoding: every value spared reads from a
/// query is plain ASCII).
fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?
        .split('&')
        .find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
}

/// Who may create an app with a deploy, when no app has the name yet.
enum Creator {
    Admin,
    /// An app token made while the name was free, which has not created its app yet.
    Token(String),
    /// An app token that may only redeploy its app.
    No(String),
}

impl Creator {
    /// The app token a deploy was authenticated with.
    fn token_id(&self) -> Option<&str> {
        match self {
            Creator::Admin => None,
            Creator::Token(id) | Creator::No(id) => Some(id),
        }
    }
}

fn token_revoked(id: &str) -> DeployError {
    DeployError::new(
        StatusCode::UNAUTHORIZED,
        "token_revoked",
        format!("app token {id} was revoked"),
        "ask the operator for a new token (`spared token create <app>` with the admin token)",
    )
}

fn cannot_create(name: &str) -> DeployError {
    DeployError::new(
        StatusCode::FORBIDDEN,
        "wrong_scope",
        format!(
            "this app token cannot create '{name}': it was made while an app of that name existed, or it has created it once already"
        ),
        format!(
            "deploy '{name}' once with the admin token, or have the operator make a new token for it (`spared token create {name}`)"
        ),
    )
}

fn deploy_refused_frozen(name: &str) -> DeployError {
    DeployError::new(
        StatusCode::CONFLICT,
        "app_frozen",
        format!(
            "app '{name}' is frozen on this server, so the deploy was refused. Nothing changed"
        ),
        format!(
            "`spared ps` shows why it is frozen. If this server should serve it again, run `spared thaw {name}`, then deploy. If it was moved to another server, deploy there instead"
        ),
    )
}

/// The current UTC time as `2026-11-12T04:05:06Z`.
fn now_text() -> String {
    backup::created_at(&backup::timestamp_id(unix_now()))
}

/// A remote operation that failed at the store (502) or here (500).
fn remote_error(e: crate::remote::s3::S3Error) -> Resp {
    let status = if e.kind.starts_with("remote_") {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    err_resp(status, e.kind, e.message, &e.fix)
}

fn backup_task_failed(e: tokio::task::JoinError) -> Resp {
    err_resp(
        StatusCode::INTERNAL_SERVER_ERROR,
        "backup_failed",
        format!("the backup task failed: {e}"),
        "report this",
    )
}

/// Run an admin operation as its own task and wait for its answer. The connection future that
/// calls this is dropped when the admin client goes away (Ctrl-C, a proxy timeout, the CLI's
/// own timeout); a task is not, so the operation runs to the end and the locks it holds are
/// released only then. Only the wait for the answer is cancelled. This is the admin twin of what
/// `serve_app` does for a public request.
async fn detached<F>(operation: F) -> Resp
where
    F: std::future::Future<Output = Resp> + Send + 'static,
{
    tokio::spawn(operation).await.unwrap_or_else(|e| {
        err_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("the operation ended without a result: {e}"),
            "check the server log, then `spared ps`: the operation may not have completed",
        )
    })
}

fn backup_entry_json(e: &backup::Entry) -> serde_json::Value {
    serde_json::json!({
        "backup_id": e.id,
        "kind": if e.pre_restore { "pre-restore" } else { "backup" },
        "created_at": backup::created_at(&e.id),
        "bytes": e.stats.bytes,
        "files": e.stats.files,
    })
}

/// Nanoseconds since the epoch plus a counter: unique names for the trash.
fn unique_suffix() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{ns}-{}", N.fetch_add(1, Ordering::Relaxed))
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn io_fail(kind: &'static str, what: &str, e: impl std::fmt::Display) -> DeployError {
    DeployError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        kind,
        format!("{what}: {e}"),
        "check disk space and permissions of the data dir",
    )
}

/// A failed tree copy: `backup_corrupt` when a copied file does not match what was read,
/// otherwise `kind` (the copy hit a limit or an I/O error).
fn copy_fail(kind: &'static str, what: &str, e: std::io::Error) -> DeployError {
    if e.kind() == std::io::ErrorKind::InvalidData {
        return DeployError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "backup_corrupt",
            format!("{what}: {e}"),
            "the disk returned different bytes than were written; check the disk, then list older backups with `spared backups <name>`",
        );
    }
    let limit = e.to_string().contains("nothing was copied");
    DeployError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        kind,
        format!("{what}: {e}"),
        if limit {
            "the app's data tree is over the backup limits (64 levels, 100000 entries, and --max-app-data-mb of apparent size); the app and its data are unchanged"
        } else {
            "check disk space and permissions of the data dir"
        },
    )
}

/// Where deleted app directories go before they are removed, under the data dir.
const TRASH: &str = "trash";

fn empty_trash(trash: &Path) {
    for e in std::fs::read_dir(trash).into_iter().flatten().flatten() {
        if let Err(err) = backup::remove_tree(&e.path()) {
            eprintln!("could not remove {}: {err}", e.path().display());
        }
    }
}

/// A restore that a crash interrupted: a complete copy (`data.restore-ready`) with no `data/`
/// is moved into place; any other staged copy is removed and the data stays as it was.
fn finish_interrupted_restore(name: &str, dir: &Path) {
    let (data, ready) = (dir.join("data"), dir.join(RESTORE_READY));
    if ready.is_dir() && !data.exists() {
        match std::fs::rename(&ready, &data) {
            Ok(()) => eprintln!("app {name}: finished a restore that was interrupted"),
            Err(e) => eprintln!("app {name}: could not finish an interrupted restore: {e}"),
        }
    }
    for leftover in [RESTORE_READY, RESTORE_TMP] {
        if dir.join(leftover).exists() {
            let _ = backup::remove_tree(&dir.join(leftover));
        }
    }
}

/// Append a line from the server to an app's log (read with `spared logs`).
fn note_in_app_log(dir: &Path, text: &str) {
    let line: String = text.chars().take(2000).collect();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("app.log"))
    {
        let _ = writeln!(f, "spared: {line}");
    }
}

fn sync_dir(p: &Path) {
    if let Ok(f) = std::fs::File::open(p) {
        let _ = f.sync_all();
    }
}

/// The marker in an app directory that keeps the app frozen across restarts and redeploys.
const FROZEN_MARKER: &str = "frozen";
/// What can freeze an app, as the marker records it: `spared freeze`, `spared move`, or an upload
/// that found a newer epoch in the bucket.
const FREEZE_KINDS: [&str; 3] = ["manual", "move", "remote_superseded"];
/// What a client of a frozen app is told to wait before it tries again, in seconds.
const FROZEN_RETRY_AFTER: &str = "30";

/// The freeze recorded in `<app dir>/frozen`, when the app is frozen. A marker that cannot be read
/// still freezes the app: the safe reading of a damaged marker is "do not serve".
fn read_freeze_marker(dir: &Path) -> Option<serde_json::Value> {
    let unreadable = || serde_json::json!({"kind": "unknown", "reason": "the frozen marker could not be read. `spared thaw <name>` removes it"});
    match std::fs::read(dir.join(FROZEN_MARKER)) {
        Ok(b) => Some(
            serde_json::from_slice::<serde_json::Value>(&b)
                .ok()
                .filter(|v| v.is_object())
                .unwrap_or_else(unreadable),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(unreadable()),
    }
}

/// Write the marker through a temporary file and a rename, synced, so a crash leaves the old
/// marker or the new one.
fn write_freeze_marker(dir: &Path, v: &serde_json::Value) -> std::io::Result<()> {
    let tmp = dir.join(format!("{FROZEN_MARKER}.tmp"));
    write_private(&tmp, format!("{v}\n").as_bytes())?;
    std::fs::rename(&tmp, dir.join(FROZEN_MARKER))?;
    sync_dir(dir);
    Ok(())
}

fn remove_freeze_marker(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(dir.join(FROZEN_MARKER)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    sync_dir(dir);
    Ok(())
}

/// 503 for a public request to a frozen app. It names no reason and no other server: the client
/// may be anyone.
fn app_frozen(name: &str) -> Resp {
    let mut r = err_resp(
        StatusCode::SERVICE_UNAVAILABLE,
        "app_frozen",
        format!("app '{name}' is frozen on this server and takes no requests here"),
        "retry later. It may be moving to another server. The operator sees why with `spared ps` and serves it here again with `spared thaw <name>`",
    );
    r.headers_mut()
        .insert("retry-after", FROZEN_RETRY_AFTER.parse().unwrap());
    r
}

/// 503 for a public request to an app that has used up the CPU budget its operator set. It names
/// no figure: the client may be anyone.
fn cpu_budget_exhausted(name: &str, secs: u64) -> Resp {
    let mut r = err_resp(
        StatusCode::SERVICE_UNAVAILABLE,
        "cpu_budget_exhausted",
        format!("app '{name}' has used its CPU budget for now"),
        "retry after Retry-After seconds. The operator sets this app's budget with `spared limits set`",
    );
    r.headers_mut()
        .insert("retry-after", secs.to_string().parse().unwrap());
    r
}

const LIMITS_USAGE: &str = r#"send a JSON object with at least one of data_mb, entries, cpu_ms_per_min, egress_bytes_per_hour, each a whole number of at least 1 (spared limits set <app> --data-mb N --entries N --cpu-percent P --egress-mb-per-hour N)"#;

fn limits_write_failed(what: &str, e: impl std::fmt::Display) -> Resp {
    err_resp(
        StatusCode::INTERNAL_SERVER_ERROR,
        "limits_write_failed",
        format!("{what}: {e}. The limits in force are the ones `spared limits show` reports"),
        "check disk space and the permissions of <data-dir>/limits/ and <data-dir>/format.json, then run the command again",
    )
}

fn write_failed(e: std::io::Error) -> DeployError {
    DeployError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "write_failed",
        format!("{e}"),
        "check disk space and permissions of the data dir",
    )
}

/// The marker that commits a deploy: while it exists, the staged `*.tmp` files in the app
/// directory are the new release, and a start finishes the renames.
const RELEASE_PENDING: &str = "release.pending";
/// What a release is made of in an app directory. Each is staged as `<name>.tmp`.
const RELEASE_FILES: [&str; 5] = [
    "component.cwasm",
    "component.wasm",
    "component.sha256",
    "manifest.toml",
    "version",
];

fn staged_name(dir: &Path, file: &str) -> PathBuf {
    dir.join(format!("{file}.tmp"))
}

/// Remove staged files and the commit marker: a deploy that did not commit.
fn discard_staged(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(RELEASE_PENDING));
    for f in RELEASE_FILES {
        let _ = std::fs::remove_file(staged_name(dir, f));
    }
}

/// Move the staged release into place and drop the marker. Safe to run again after a crash: a
/// file already moved is skipped.
fn finish_release(dir: &Path) -> std::io::Result<()> {
    for f in RELEASE_FILES {
        match std::fs::rename(staged_name(dir, f), dir.join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    sync_dir(dir);
    std::fs::remove_file(dir.join(RELEASE_PENDING))?;
    sync_dir(dir);
    Ok(())
}

/// Commit a staged release. Every staged file is already synced. Creating the marker (synced, and
/// its directory entry synced) is the commit point: a crash before it leaves the old release, a
/// crash after it is completed by `finish_interrupted_deploy` at the next start. The renames after
/// the marker are the only moment the app directory holds files of two releases, and the marker
/// says how to resolve it.
fn commit_release(dir: &Path) -> std::io::Result<()> {
    write_private(&dir.join(RELEASE_PENDING), b"")?;
    sync_dir(dir);
    if let Err(e) = finish_release(dir) {
        eprintln!(
            "deploy in {}: the new release is committed but a rename failed ({e}); the next start completes it",
            dir.display()
        );
    }
    Ok(())
}

/// A deploy that a crash interrupted: committed ones (marker present) are completed, anything
/// else staged is removed and the previous release stays.
fn finish_interrupted_deploy(name: &str, dir: &Path) {
    if dir.join(RELEASE_PENDING).exists() {
        match finish_release(dir) {
            Ok(()) => eprintln!("app {name}: finished a deploy that was interrupted"),
            Err(e) => eprintln!("app {name}: could not finish an interrupted deploy: {e}"),
        }
    } else {
        discard_staged(dir);
    }
}

/// Copy `src` (an app's data directory, with its instance stopped and locked) to
/// `<root>/<timestamp>/` through a `.tmp-` directory and a rename, so a backup either exists
/// whole or not at all.
fn take_backup(
    name: &str,
    src: &Path,
    root: &Path,
    keep: usize,
    max_bytes: u64,
    pinned: &std::collections::HashSet<String>,
) -> std::result::Result<serde_json::Value, DeployError> {
    create_private_dir(root)
        .map_err(|e| io_fail("backup_failed", "cannot create the backup directory", e))?;
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        // A fetch in progress stages under `.tmp-fetch-`; a restart clears those.
        if n.starts_with(".tmp-") && !n.starts_with(crate::remote::FETCH_TMP) {
            let _ = backup::remove_tree(&e.path());
        }
    }
    let id = backup::unique_id(root, "", unix_now());
    let tmp = root.join(format!(".tmp-{id}"));
    let stats = match backup::copy_tree(src, &tmp, max_bytes) {
        Ok(s) => s,
        Err(e) => {
            let _ = backup::remove_tree(&tmp);
            return Err(copy_fail(
                "backup_failed",
                "copying the data directory failed",
                e,
            ));
        }
    };
    let journal_clean = backup::journal_clean(&tmp);
    if let Err(e) = std::fs::rename(&tmp, root.join(&id)) {
        let _ = backup::remove_tree(&tmp);
        return Err(io_fail(
            "backup_failed",
            "could not move the copy into place",
            e,
        ));
    }
    sync_dir(root);
    let removed = backup::prune(root, keep, pinned);
    let mut v = serde_json::json!({
        "name": name,
        "backup_id": id,
        "bytes": stats.bytes,
        "files": stats.files,
        "created_at": backup::created_at(&id),
        "journal_clean": journal_clean,
        "integrity": backup::INTEGRITY,
        "contains": BACKUP_CONTAINS,
    });
    if !journal_clean {
        v["note"] = "app.db had a non-empty journal or WAL file; every file was copied together, and SQLite recovers from them when the backup is restored and the app next opens it".into();
    }
    if stats.skipped > 0 {
        v["skipped_links_or_special_files"] = stats.skipped.into();
    }
    if !removed.is_empty() {
        v["removed_old"] = removed.into();
    }
    Ok(v)
}

/// Replace `<app_dir>/data` with a copy of `<root>/<id>`. The old data moves to
/// `<root>/pre-restore-<timestamp>/` first, so a restore can be undone with another restore.
fn do_restore(
    name: &str,
    app_dir: &Path,
    root: &Path,
    id: &str,
    keep: usize,
    max_bytes: u64,
    pinned: &std::collections::HashSet<String>,
) -> std::result::Result<serde_json::Value, DeployError> {
    let data = app_dir.join("data");
    let tmp = app_dir.join(RESTORE_TMP);
    let ready = app_dir.join(RESTORE_READY);
    let _ = backup::remove_tree(&tmp);
    let _ = backup::remove_tree(&ready);
    let stats = match backup::copy_tree(&root.join(id), &tmp, max_bytes) {
        Ok(s) => s,
        Err(e) => {
            let _ = backup::remove_tree(&tmp);
            return Err(copy_fail("restore_failed", "copying the backup failed", e));
        }
    };
    // `data.restore-ready` exists only once the copy is complete, so a start after a crash can
    // tell a finished copy (move it into place) from a partial one (remove it); see `load_app`.
    if let Err(e) = std::fs::rename(&tmp, &ready) {
        let _ = backup::remove_tree(&tmp);
        return Err(io_fail("restore_failed", "could not stage the copy", e));
    }
    sync_dir(app_dir);
    let tmp = ready;
    let pre = data
        .is_dir()
        .then(|| backup::unique_id(root, backup::PRE_RESTORE, unix_now()));
    if let Some(p) = &pre {
        if let Err(e) = std::fs::rename(&data, root.join(p)) {
            let _ = backup::remove_tree(&tmp);
            return Err(io_fail(
                "restore_failed",
                "could not move the current data aside",
                e,
            ));
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &data) {
        if let Some(p) = &pre {
            let _ = std::fs::rename(root.join(p), &data);
        }
        let _ = backup::remove_tree(&tmp);
        return Err(io_fail(
            "restore_failed",
            "could not move the backup into place",
            e,
        ));
    }
    sync_dir(app_dir);
    sync_dir(root);
    backup::prune(root, keep, pinned);
    let mut v = serde_json::json!({
        "name": name,
        "restored_from": id,
        "bytes": stats.bytes,
        "files": stats.files,
        "integrity": backup::INTEGRITY,
        "state": "idle",
        "contains": BACKUP_CONTAINS,
    });
    if let Some(p) = pre {
        v["undo"] = format!("spared restore {name} --from {p} --yes").into();
        v["pre_restore_id"] = p.into();
    }
    Ok(v)
}

impl Server {
    pub fn new(mut cfg: Config, engine: Engine) -> Result<Arc<Server>> {
        create_private_dir(&cfg.data_dir.join("apps"))?;
        let owner = std::fs::metadata(&cfg.data_dir)?.uid();
        let me = unsafe { libc::geteuid() };
        if owner != me {
            return Err(wasmtime::Error::msg(format!(
                "the data dir {} belongs to uid {owner}, but the server runs as uid {me}. Whoever owns it can place compiled components that the server runs as native code. Fix: use a data dir owned by the server's user (chown it, or pick another --data-dir)",
                cfg.data_dir.display()
            )));
        }
        // Before anything under the data dir is read or changed: a layout this build does not
        // understand is refused, and a directory without a stamp gets one. Format 2 while any
        // app has an operator-set limit, so a release that would ignore it refuses to start.
        let stored_limits = quota::stored_names(&cfg.data_dir).map_err(|e| {
            let r = quota::unreadable(&cfg.data_dir, &e);
            wasmtime::Error::msg(format!("{} ({}). Fix: {}", r.message, r.kind, r.fix))
        })?;
        let format = if stored_limits.is_empty() {
            datadir::BASE_FORMAT
        } else {
            datadir::LIMITS_FORMAT
        };
        datadir::stamp(&cfg.data_dir, format).map_err(|e| {
            wasmtime::Error::msg(format!("{} ({}). Fix: {}", e.message, e.kind, e.fix))
        })?;
        let mut quotas = HashMap::new();
        for name in stored_limits {
            let q = match quota::read(&cfg.data_dir, &name) {
                Ok(o) => AppQuota::new(o),
                Err(why) => {
                    eprintln!(
                        "app {name}: its limits file cannot be read ({why}). The app is not served until `spared limits set` or `spared limits clear` replaces it"
                    );
                    AppQuota::damaged(why)
                }
            };
            quotas.insert(name, Arc::new(q));
        }
        create_private_dir(&cfg.data_dir.join("components"))?;
        create_private_dir(&cfg.data_dir.join("backups"))?;
        create_private_dir(&cfg.data_dir.join(TRASH))?;
        std::fs::set_permissions(&cfg.data_dir, std::fs::Permissions::from_mode(0o700))?;
        crate::remote::recover(&cfg.data_dir);
        let app_tokens = tokens::Store::open(&cfg.data_dir)?;
        let remote = match cfg.remote.take() {
            Some(s) => Some(Arc::new(
                crate::remote::Remote::new(s, &cfg.data_dir).map_err(|e| {
                    wasmtime::Error::msg(format!("remote backups cannot start: {e}"))
                })?,
            )),
            None => None,
        };
        let body_budget = BodyBudget::new(
            cfg.caps.body_buffer_bytes,
            (cfg.max.request_mb as u64) << 20,
        );
        let s = Arc::new(Server {
            cfg,
            engine,
            apps: RwLock::new(HashMap::new()),
            components: Mutex::new(HashMap::new()),
            name_locks: Mutex::new(HashMap::new()),
            started: Instant::now(),
            metrics: Metrics::default(),
            ids: RequestIds::new(),
            body_budget,
            remote,
            tokens: Mutex::new(app_tokens),
            auth_failures: tokens::AuthFailures::default(),
            quotas: Mutex::new(quotas),
            limits_io: Mutex::new(()),
        });
        for e in std::fs::read_dir(s.cfg.data_dir.join("apps"))?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            match s.load_app(&name) {
                Ok(app) => {
                    s.apps.write().unwrap().insert(name, Arc::new(app));
                }
                Err(err) => eprintln!("skipping app {name}: {err:#}"),
            }
        }
        s.sweep_components();
        let trash = s.cfg.data_dir.join(TRASH);
        std::thread::spawn(move || empty_trash(&trash));
        Ok(s)
    }

    fn wasm_path(&self, hash: &str) -> PathBuf {
        self.cfg
            .data_dir
            .join("components")
            .join(format!("{hash}.wasm"))
    }

    fn component_path(&self, hash: &str) -> PathBuf {
        self.cfg
            .data_dir
            .join("components")
            .join(format!("{hash}.cwasm"))
    }

    /// Whether a loaded app runs the component with this `hash`.
    fn component_in_use(&self, hash: &str) -> bool {
        self.apps
            .read()
            .unwrap()
            .values()
            .any(|a| a.shared.hash == hash)
    }

    /// Remove leftovers of an interrupted deploy and stored files (`.cwasm` and `.wasm`) no
    /// loaded app runs and no app directory links to any more. A file system that cannot hard
    /// link gives each app directory its own copy and the store file a link count of one, so
    /// the link count alone never says a file is unused: an app that names the hash keeps it.
    fn sweep_components(&self) {
        let _g = self.components.lock().unwrap();
        let Ok(rd) = std::fs::read_dir(self.cfg.data_dir.join("components")) else {
            return;
        };
        let named: std::collections::HashSet<String> = self
            .apps
            .read()
            .unwrap()
            .values()
            .map(|a| a.shared.hash.clone())
            .collect();
        for e in rd.flatten() {
            let file = e.file_name().to_string_lossy().to_string();
            let hash = file.split('.').next().unwrap_or_default();
            let unused = !named.contains(hash) && e.metadata().is_ok_and(|m| m.nlink() <= 1);
            if unused || file.ends_with(".tmp") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Called after an app stopped using `hash` (deleted, or redeployed with other code): drop
    /// the stored files when no loaded app runs them and no app directory links to them.
    fn release_component(&self, hash: &str) {
        let mut cache = self.components.lock().unwrap();
        if !self.component_in_use(hash) {
            for path in [self.component_path(hash), self.wasm_path(hash)] {
                if std::fs::metadata(&path).is_ok_and(|m| m.nlink() <= 1) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        cache.retain(|_, w| w.strong_count() > 0);
    }

    /// Put a store file back from the app directory's own copy when the store lost it: a store
    /// on a file system without hard links keeps one file per app directory and nothing else.
    fn reseed_store(&self, hash: &str, app_file: &Path, app_wasm: &Path) {
        let (stored, stored_wasm) = (self.component_path(hash), self.wasm_path(hash));
        if !stored_wasm.is_file()
            && let Ok(w) = std::fs::read(app_wasm)
            && sha256_hex(&w) == hash
        {
            let _ = Self::store_file(&stored_wasm, &w);
        }
        if !stored.is_file()
            && let Ok(c) = std::fs::read(app_file)
        {
            let _ = Self::store_file(&stored, &c);
        }
    }

    /// The shared compiled component behind `<dir>/component.cwasm`, keyed by the recorded
    /// `component.sha256` (the hash of the deployed wasm). The app directory is pointed at the
    /// stored files, which a recompile may have replaced.
    fn attach_component(&self, dir: &Path) -> Result<Arc<SharedComponent>> {
        let mut cache = self.components.lock().unwrap();
        let (app_file, app_wasm) = (dir.join("component.cwasm"), dir.join("component.wasm"));
        let hash = std::fs::read_to_string(dir.join("component.sha256"))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| is_sha256_hex(t))
            .ok_or_else(|| {
                wasmtime::Error::msg(
                    "no valid component.sha256 in the app directory. Fix: redeploy the app",
                )
            })?;
        let (stored, stored_wasm) = (self.component_path(&hash), self.wasm_path(&hash));
        self.reseed_store(&hash, &app_file, &app_wasm);
        let shared = self.shared_component(&mut cache, &hash)?;
        if !same_file(&stored, &app_file) {
            link_replace(&stored, &app_file)?;
        }
        if stored_wasm.is_file() && !same_file(&stored_wasm, &app_wasm) {
            link_replace(&stored_wasm, &app_wasm)?;
        }
        Ok(shared)
    }

    /// Compile the stored wasm for `hash` again and replace the stored `.cwasm` (new file,
    /// renamed into place: the old file may still be mapped). Used when the compiled file was
    /// made by another wasmtime version or engine configuration, which `deserialize_file`
    /// refuses. The wasm is checked against its hash first.
    fn recompile(&self, hash: &str) -> Result<()> {
        let wasm = std::fs::read(self.wasm_path(hash)).map_err(|_| {
            wasmtime::Error::msg(
                "the stored wasm to recompile from is missing. Fix: redeploy the app",
            )
        })?;
        if sha256_hex(&wasm) != hash {
            return Err(wasmtime::Error::msg(format!(
                "the stored wasm for {hash} does not match its hash; not compiling it. Fix: redeploy the app"
            )));
        }
        let cwasm = compile::compile(&self.engine, &self.cfg.compile, &wasm, false)
            .map_err(|e| wasmtime::Error::msg(compile_error_text(e)))?;
        let path = self.component_path(hash);
        let tmp = path.with_extension("cwasm.tmp");
        write_private(&tmp, &cwasm)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn compile_failure(&self, e: CompileError) -> DeployError {
        match e {
            CompileError::Invalid(m) => DeployError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_component",
                m,
                "build for wasm32-wasip2 as a component",
            ),
            CompileError::TooComplex(m) => DeployError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "component_too_complex",
                m,
                "split the work across smaller functions or build with --release; the operator can raise --max-function-kb, --max-function-locals, --compile-memory-mb or --compile-timeout-secs",
            ),
            CompileError::Failed(m) => DeployError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "precompile_failed",
                m,
                "report this",
            ),
        }
    }

    /// Link `component` against the host's WASI and WASI HTTP interfaces.
    fn link(&self, component: &Component) -> Result<ProxyPre<Guarded>> {
        crate::sandbox::link(&self.engine, component)
    }

    fn shared_component(
        &self,
        cache: &mut HashMap<String, Weak<SharedComponent>>,
        hash: &str,
    ) -> Result<Arc<SharedComponent>> {
        if let Some(s) = cache.get(hash).and_then(Weak::upgrade) {
            return Ok(s);
        }
        let path = self.component_path(hash);
        let component = match unsafe { Component::deserialize_file(&self.engine, &path) } {
            Ok(c) => c,
            Err(e) => {
                self.recompile(hash).map_err(|r| {
                    wasmtime::Error::msg(format!(
                        "the compiled component cannot be loaded ({e:#}) and recompiling failed: {r:#}"
                    ))
                })?;
                eprintln!(
                    "component {hash}: recompiled from the stored wasm (the compiled file was refused: {e:#})"
                );
                unsafe { Component::deserialize_file(&self.engine, &path)? }
            }
        };
        let pre = self.link(&component)?;
        let s = Arc::new(SharedComponent {
            hash: hash.to_string(),
            component,
            pre,
            warmed: Arc::new(AtomicBool::new(false)),
        });
        cache.retain(|_, w| w.strong_count() > 0);
        cache.insert(hash.to_string(), Arc::downgrade(&s));
        Ok(s)
    }

    /// Put `bytes` in the store at `path` unless already there (write to a temporary name, then
    /// rename), with the cache lock held by the caller.
    fn store_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if !path.is_file() {
            let mut tmp = path.as_os_str().to_owned();
            tmp.push(".tmp");
            write_private(Path::new(&tmp), bytes)?;
            std::fs::rename(&tmp, path)?;
            if let Some(parent) = path.parent() {
                sync_dir(parent);
            }
        }
        Ok(())
    }

    fn name_lock(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.name_locks
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    /// The app's manifest, with limits above the server's maxima clamped to them.
    fn parse_manifest(&self, name: &str, text: &str) -> Result<Manifest> {
        let (mut manifest, warnings) = Manifest::parse_stored(text)?;
        for w in warnings {
            eprintln!(
                "warning: app {name}: {w}; the entry is ignored and the app keeps serving. Fix: redeploy it with a spared.toml this version accepts"
            );
        }
        let asked = manifest.limits.clone();
        if manifest.limits.clamp(&self.cfg.max) {
            eprintln!(
                "app {name}: manifest limits {asked:?} exceed the server maxima; using {:?}",
                manifest.limits
            );
        }
        Ok(manifest)
    }

    fn make_app(
        &self,
        name: &str,
        dir: PathBuf,
        manifest: Manifest,
        shared: Arc<SharedComponent>,
        version: u64,
    ) -> Result<App> {
        let bad = ungranted_imports(&self.engine, &shared.component, &manifest.grants);
        if !bad.is_empty() {
            return Err(wasmtime::Error::msg(format!(
                "imports not granted by manifest.toml: {}",
                bad.join(", ")
            )));
        }
        let quota = self.quota(name);
        if let Some(why) = quota.damage() {
            return Err(wasmtime::Error::msg(format!(
                "its limits file cannot be read ({why}). Fix: `spared limits set {name} ...` or `spared limits clear {name}`"
            )));
        }
        let freeze = read_freeze_marker(&dir);
        Ok(App {
            name: name.to_string(),
            dir,
            manifest,
            version,
            shared,
            live: Arc::new(tokio::sync::Mutex::new(None)),
            retired: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            frozen: AtomicBool::new(freeze.is_some()),
            freeze: Mutex::new(freeze),
            hot: AtomicBool::new(false),
            last_ms: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            wakes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            quota,
        })
    }

    /// The limits and counters of `name`, made (with no override) the first time it is asked for.
    fn quota(&self, name: &str) -> Arc<AppQuota> {
        self.quotas
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(AppQuota::new(None)))
            .clone()
    }

    fn load_app(&self, name: &str) -> Result<App> {
        let dir = self.cfg.data_dir.join("apps").join(name);
        finish_interrupted_restore(name, &dir);
        finish_interrupted_deploy(name, &dir);
        let manifest =
            self.parse_manifest(name, &std::fs::read_to_string(dir.join("manifest.toml"))?)?;
        let shared = self.attach_component(&dir)?;
        let version = std::fs::read_to_string(dir.join("version"))
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1);
        self.make_app(name, dir, manifest, shared, version)
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Start reading the stored components into the page cache, newest first, within
    /// `--prefetch-mb`. `None` when that is 0.
    fn start_prefetch(&self) -> Option<prefetch::Prefetch> {
        if self.cfg.prefetch_mb == 0 {
            return None;
        }
        let asked = self.cfg.prefetch_mb.saturating_mul(1 << 20);
        let budget = prefetch::cap_budget(asked, prefetch::memory_headroom());
        if budget < asked {
            eprintln!(
                "prefetch: reading at most {} MB, half of the memory that is free (--prefetch-mb is {})",
                budget >> 20,
                self.cfg.prefetch_mb
            );
        }
        let apps: Vec<_> = self
            .apps
            .read()
            .unwrap()
            .values()
            .map(|a| (a.shared.hash.clone(), a.dir.clone()))
            .collect();
        Some(prefetch::Prefetch::start(
            budget,
            prefetch::Pace::default(),
            self.metrics.prefetch_bytes.clone(),
            move || prefetch::plan(apps),
        ))
    }

    /// Before the first wake of a component in this process: if its file is mostly missing from
    /// the page cache, have the kernel read it in the background while the wake goes on.
    fn hint_cold_component(&self, shared: &Arc<SharedComponent>) {
        if self.cfg.prefetch_mb == 0 || shared.warmed.swap(true, Ordering::AcqRel) {
            return;
        }
        let image = shared.component.image_range();
        let (start, len) = (
            image.start as usize,
            image.end as usize - image.start as usize,
        );
        if !prefetch::looks_cold(start, len) {
            return;
        }
        let counter = self.metrics.prefetch_bytes.clone();
        let path = self.component_path(&shared.hash);
        tokio::task::spawn_blocking(move || {
            counter.fetch_add(prefetch::read_ahead(&path), Ordering::Relaxed);
        });
    }

    async fn wake(&self, app: &App) -> Result<Live> {
        let m = &app.manifest;
        let data = app.dir.join("data");
        create_private_dir(&data)?;
        let log_path = app.dir.join("app.log");
        if std::fs::metadata(&log_path)
            .map(|m| m.len() > 1 << 20)
            .unwrap_or(false)
        {
            let _ = std::fs::rename(&log_path, app.dir.join("app.log.1"));
        }
        let log = AppLog::open(log_path, self.cfg.caps.log)?;
        let caps = app.quota.caps(&self.cfg.caps);
        let mut store =
            crate::sandbox::app_store(&self.engine, m, &data, &log, &caps, Some(app.quota.clone()))
                .await?;
        store.data_mut().log = Some(log);
        let proxy = app.shared.pre.instantiate_async(&mut store).await?;
        Ok(Live { store, proxy })
    }

    async fn serve_app(
        self: &Arc<Self>,
        app: Arc<App>,
        req: Request<Incoming>,
        path_form: bool,
    ) -> Resp {
        if app.is_frozen() {
            return app_frozen(&app.name);
        }
        // Before the body is read, so an app over its CPU budget holds no request bytes.
        if let Err(secs) = app.quota.cpu_admit() {
            return cpu_budget_exhausted(&app.name, secs);
        }
        let (parts, body) = req.into_parts();
        let max = (self.cfg.max.request_mb as u64) << 20;
        let declared = parts
            .headers
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let mut permit = self.body_budget.permit();
        let collect = collect_body(body, max, declared, &mut permit);
        let body = match tokio::time::timeout(self.cfg.client_timeout, collect).await {
            Ok(Ok(b)) => b,
            Ok(Err(BodyFailure::TooLarge)) => {
                return err_resp(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    format!("request body over {} MB", self.cfg.max.request_mb),
                    "send a smaller body (the server's limit is --max-request-mb)",
                );
            }
            Ok(Err(BodyFailure::NoRoom)) => {
                let mut r = err_resp(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server_busy",
                    "the server is holding as many request bytes in memory as it may".into(),
                    "retry shortly; the operator can raise --max-body-buffer-mb",
                );
                r.headers_mut().insert("retry-after", "1".parse().unwrap());
                return r;
            }
            Ok(Err(BodyFailure::Read(e))) => {
                return err_resp(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("reading the request body failed: {e}"),
                    "send a complete request body",
                );
            }
            Err(_) => {
                return err_resp(
                    StatusCode::REQUEST_TIMEOUT,
                    "request_timeout",
                    format!(
                        "request body not received within {} s",
                        self.cfg.client_timeout.as_secs()
                    ),
                    "send the whole body promptly",
                );
            }
        };
        // The place in the queue is taken once the whole request has arrived, so clients that
        // are still sending (or stalled) cannot fill it and keep the app's real requests out.
        let Some(place) = InFlight::enter(&app, self.cfg.caps.max_queue) else {
            return app_busy(&app.name, "has too many requests waiting");
        };
        // The request runs in its own task, so a client that goes away cannot cancel it half way:
        // a cancelled call would leave the instance mid-call and fail every later request. A
        // request still waiting for the app's instance lock is dropped when its client goes
        // away, so only the request already running outlives its connection (one per app).
        let (mut tx, rx) = tokio::sync::oneshot::channel();
        let s = self.clone();
        let wait = Duration::from_millis(app.manifest.limits.deadline_ms);
        tokio::spawn(async move {
            let _place = place;
            let _body_bytes = permit;
            let name = app.name.clone();
            let locked = tokio::select! {
                l = tokio::time::timeout(wait, s.lock_live(app)) => match l {
                    Ok(l) => l,
                    Err(_) => {
                        let _ = tx.send(app_busy(&name, "did not get to this request within its deadline_ms"));
                        return;
                    }
                },
                _ = tx.closed() => return,
            };
            let resp = match locked {
                // A freeze sets the flag while it holds the instance lock, so a request that was
                // waiting for the lock sees it here and never runs after the freeze.
                Ok((app, _)) if app.is_frozen() => app_frozen(&app.name),
                // Again once it is this request's turn: requests run one at a time, so a request
                // overdraws the budget by at most its own cpu_ms.
                Ok((app, _)) if let Err(secs) = app.quota.cpu_admit() => {
                    cpu_budget_exhausted(&app.name, secs)
                }
                Ok((app, guard)) => {
                    s.run_request(
                        app,
                        guard,
                        http::Request::from_parts(parts, body),
                        path_form,
                    )
                    .await
                }
                Err(r) => r,
            };
            let _ = tx.send(resp);
        });
        rx.await.unwrap_or_else(|_| {
            err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "the request task ended without a response".into(),
                "retry; if it repeats, check the server log",
            )
        })
    }

    /// Take `app`'s instance lock, following redeploys: a request that finds its app retired
    /// looks the name up again instead of waking a second instance on the same data directory.
    async fn lock_live(
        &self,
        mut app: Arc<App>,
    ) -> std::result::Result<(Arc<App>, IdleGuard), Resp> {
        loop {
            let guard = app.live.clone().lock_owned().await;
            if !app.retired.load(Ordering::Acquire) {
                return Ok((app, guard));
            }
            drop(guard);
            let next = self.apps.read().unwrap().get(&app.name).cloned();
            match next {
                Some(a) if !Arc::ptr_eq(&a, &app) => app = a,
                _ => return Err(no_such_app(&app.name)),
            }
        }
    }

    /// Wake the instance if needed and run one request, both inside the app's `deadline_ms`.
    /// Any failure drops the instance. Details of a failure go to the app's log, not to the
    /// client, which may be anyone.
    async fn run_request(
        &self,
        app: Arc<App>,
        mut guard: IdleGuard,
        req: http::Request<Bytes>,
        path_form: bool,
    ) -> Resp {
        let limits = &app.manifest.limits;
        let deadline = Duration::from_millis(limits.deadline_ms);
        let started = Instant::now();
        if guard.is_none() {
            self.hint_cold_component(&app.shared);
            let woke_at = Instant::now();
            let woke = tokio::time::timeout(deadline, self.wake(&app)).await;
            app.quota.cpu_charge(woke_at.elapsed().as_nanos() as u64);
            match woke {
                Ok(Ok(l)) => {
                    *guard = Some(l);
                    app.hot.store(true, Ordering::Relaxed);
                    app.wakes.fetch_add(1, Ordering::Relaxed);
                    self.metrics.wake();
                }
                Ok(Err(e)) => {
                    app.errors.fetch_add(1, Ordering::Relaxed);
                    note_in_app_log(&app.dir, &format!("wake failed: {e:#}"));
                    return err_resp(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "wake_failed",
                        "the app could not start; the reason is in its log".into(),
                        "check `spared logs <name>` and the manifest grants",
                    );
                }
                Err(_) => {
                    app.errors.fetch_add(1, Ordering::Relaxed);
                    note_in_app_log(&app.dir, "wake took longer than deadline_ms");
                    return err_resp(
                        StatusCode::GATEWAY_TIMEOUT,
                        "deadline",
                        format!(
                            "starting the app took longer than {} ms",
                            limits.deadline_ms
                        ),
                        "make the app's start-up faster or raise limits.deadline_ms in spared.toml",
                    );
                }
            }
        }
        let live = guard.as_mut().unwrap();
        let outcome = call_handler(
            &mut live.store,
            &live.proxy,
            req,
            deadline.saturating_sub(started.elapsed()),
            self.cfg.max.response_mb << 20,
        )
        .await;
        app.quota.cpu_charge(live.store.data().clock.used_ns());
        app.requests.fetch_add(1, Ordering::Relaxed);
        app.last_ms.store(self.now_ms(), Ordering::Relaxed);
        let e = match outcome {
            Ok(resp) => {
                let (mut p, b) = resp.into_parts();
                if path_form {
                    p.headers.remove("service-worker-allowed");
                }
                return Response::from_parts(p, Full::new(b));
            }
            Err(CallError::BadRequest(msg)) => {
                return err_resp(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    msg,
                    "send a request with a Host header",
                );
            }
            Err(e) => e,
        };
        app.errors.fetch_add(1, Ordering::Relaxed);
        *guard = None;
        app.hot.store(false, Ordering::Relaxed);
        self.metrics.eviction(Evicted::Error);
        match e {
            CallError::Deadline => err_resp(
                StatusCode::GATEWAY_TIMEOUT,
                "deadline",
                format!("request exceeded {} ms", limits.deadline_ms),
                "make the handler faster or raise limits.deadline_ms in spared.toml",
            ),
            CallError::Trap { kind, text } => {
                note_in_app_log(
                    &app.dir,
                    &format!("request stopped ({}): {text}", kind.as_str()),
                );
                let (status, message, fix) = match kind {
                    TrapKind::CpuLimit => (
                        StatusCode::from_u16(508).unwrap(),
                        format!("request used more than {} ms of compute", limits.cpu_ms),
                        "reduce work per request or raise limits.cpu_ms in spared.toml",
                    ),
                    TrapKind::MemoryLimit => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("the app hit its memory limit ({} MB)", limits.memory_mb),
                        "use less memory or raise limits.memory_mb in spared.toml",
                    ),
                    TrapKind::AppTrap => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "the app trapped (panic or abort)".to_string(),
                        "the app crashed; run `spared logs` for the panic message",
                    ),
                };
                err_resp(status, kind.as_str(), message, fix)
            }
            CallError::ResponseTooLarge => err_resp(
                StatusCode::BAD_GATEWAY,
                "response_too_large",
                format!("response body over {} MB", self.cfg.max.response_mb),
                "send a smaller response (the server's limit is --max-response-mb)",
            ),
            CallError::ResponseBody(msg) => err_resp(
                StatusCode::BAD_GATEWAY,
                "response_body",
                msg,
                "check the handler's response body",
            ),
            CallError::NoResponse | CallError::BadRequest(_) => err_resp(
                StatusCode::BAD_GATEWAY,
                "no_response",
                "the handler returned without sending a response".into(),
                "return a Response from the handler",
            ),
        }
    }

    /// `GET /healthz` on the public listener, for a load balancer or a platform health check.
    /// It says the server is up and which version it is, and nothing about the apps on it.
    fn healthz(&self) -> Resp {
        json_resp(
            StatusCode::OK,
            serde_json::json!({
                "status": "ok",
                "version": crate::VERSION,
                "uptime_seconds": self.started.elapsed().as_secs(),
            }),
        )
    }

    /// Route a public request. Returns the app it reached, if any, for the access log.
    async fn handle_public(self: Arc<Self>, req: Request<Incoming>) -> (Option<String>, Resp) {
        let host = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let by_host = host_app(&host, self.cfg.domain.as_deref())
            .and_then(|n| self.apps.read().unwrap().get(&n).cloned());
        if let Some(app) = by_host {
            let name = app.name.clone();
            return (Some(name), self.serve_app(app, req, false).await);
        }
        let path = req.uri().path().to_string();
        // Only for a host that names no app, so an app's own /healthz route is still its own.
        if path == "/healthz" && matches!(*req.method(), Method::GET | Method::HEAD) {
            return (None, self.healthz());
        }
        if let Some((name, rest)) = split_prefix(&path) {
            if !self.cfg.path_routing {
                return (
                    None,
                    err_resp(
                        StatusCode::NOT_FOUND,
                        "path_routing_off",
                        format!("path routing (/_/{name}/) is off on this server"),
                        "request <app>.localhost (or <app>.<domain> with --domain); to serve /_/<app>/ start the server with --allow-path-routing (env SPARED_PATH_ROUTING=1), which puts every app on one browser origin",
                    ),
                );
            }
            let app = self.apps.read().unwrap().get(name).cloned();
            if let Some(app) = app {
                let logged = Some(app.name.clone());
                let query = req
                    .uri()
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                if rest.is_empty() {
                    let redirect = Response::builder()
                        .status(StatusCode::PERMANENT_REDIRECT)
                        .header("location", format!("/_/{name}/{query}"))
                        .body(Full::new(Bytes::new()))
                        .unwrap();
                    return (logged, redirect);
                }
                let (mut parts, body) = req.into_parts();
                match format!("{rest}{query}").parse::<hyper::Uri>() {
                    Ok(uri) => parts.uri = uri,
                    Err(_) => {
                        return (
                            logged,
                            err_resp(
                                StatusCode::BAD_REQUEST,
                                "bad_request",
                                format!("cannot route path '{path}'"),
                                "send a well-formed URL",
                            ),
                        );
                    }
                }
                let resp = self
                    .serve_app(app, Request::from_parts(parts, body), true)
                    .await;
                return (logged, resp);
            }
        }
        let host = host.split(':').next().unwrap_or("");
        let fix = if self.cfg.path_routing {
            "request <app>.localhost (or <app>.<domain> with --domain), or /_/<app>/ on any host; list apps with `spared ps`"
        } else {
            "request <app>.localhost (or <app>.<domain> with --domain); list apps with `spared ps`"
        };
        (
            None,
            err_resp(
                StatusCode::NOT_FOUND,
                "no_such_app",
                format!("no app for host '{host}' and path '{path}'"),
                fix,
            ),
        )
    }

    /// `app_json` for each app, with `data_bytes` and `data_entries` measured off the async
    /// runtime and outside the app-table lock (an app controls how big and deep its data tree is).
    /// With `admin`, each also carries `quota`: its effective limits and what it used. An app
    /// token never sees its limits.
    async fn apps_json(&self, apps: Vec<Arc<App>>, admin: bool) -> Vec<serde_json::Value> {
        let dirs: Vec<PathBuf> = apps.iter().map(|a| a.dir.join("data")).collect();
        let sizes = tokio::task::spawn_blocking(move || {
            dirs.iter()
                .map(|d| backup::tree_counts(d))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        apps.iter()
            .enumerate()
            .map(|(i, a)| {
                let mut v = self.app_json(a);
                if let Some((st, entries)) = sizes.get(i) {
                    v["data_bytes"] = st.bytes.into();
                    v["data_entries"] = (*entries).into();
                    if st.truncated {
                        v["data_bytes_partial"] = true.into();
                    }
                }
                if admin {
                    v["quota"] = self.quota_json(a.quota.override_now().as_ref(), Some(&a.quota));
                }
                v
            })
            .collect()
    }

    async fn one_app_json(&self, a: Arc<App>) -> serde_json::Value {
        self.apps_json(vec![a], false)
            .await
            .pop()
            .unwrap_or_default()
    }

    fn app_json(&self, a: &App) -> serde_json::Value {
        let last = a.last_ms.load(Ordering::Relaxed);
        let freeze = a.freeze_info();
        let state = if freeze.is_some() {
            "frozen"
        } else if a.hot.load(Ordering::Relaxed) {
            "hot"
        } else {
            "idle"
        };
        let mut v = serde_json::json!({
            "name": a.name,
            "version": a.version,
            "component_sha256": a.shared.hash,
            "state": state,
            "frozen": freeze.is_some(),
            "requests": a.requests.load(Ordering::Relaxed),
            "wakes": a.wakes.load(Ordering::Relaxed),
            "errors": a.errors.load(Ordering::Relaxed),
            "last_request_secs_ago": if last == 0 { serde_json::Value::Null } else { serde_json::json!((self.now_ms().saturating_sub(last)) / 1000) },
            "data_bytes": serde_json::Value::Null,
            "data_entries": serde_json::Value::Null,
            "grants": {"data": a.manifest.grants.data, "fetch": a.manifest.grants.fetch},
            "limits": {"memory_mb": a.manifest.limits.memory_mb, "cpu_ms": a.manifest.limits.cpu_ms, "deadline_ms": a.manifest.limits.deadline_ms, "max_outbound": a.manifest.limits.max_outbound},
            "url": format!("http://{}.localhost:{}/", a.name, self.cfg.listen.port()),
        });
        if self.cfg.path_routing {
            v["path_url"] = format!("/_/{}/", a.name).into();
        }
        if let Some(f) = freeze {
            v["freeze"] = f;
        }
        match crate::remote::local_epoch(&self.cfg.data_dir, &a.name) {
            Ok(e) => v["epoch"] = e.into(),
            Err(m) => v["epoch_error"] = m.into(),
        }
        if let Some(r) = crate::remote::epoch_record(&self.cfg.data_dir, &a.name)
            && r["restored_from"].is_string()
        {
            v["restored_from_remote"] = serde_json::json!({
                "remote_id": r["restored_from"],
                "content_digest": r["content_digest"],
                "at": r["at"],
            });
        }
        v
    }

    async fn deploy(
        self: &Arc<Self>,
        name: &str,
        req: Request<Incoming>,
        creator: Creator,
    ) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        // Before the body is read and compiled; `install` checks again under the name lock.
        if matches!(creator, Creator::No(_)) && !self.apps.read().unwrap().contains_key(name) {
            return cannot_create(name).into_resp();
        }
        let mtoml = req
            .headers()
            .get("x-spared-manifest")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| base64::engine::general_purpose::STANDARD.decode(h).ok());
        let manifest_text = match mtoml.and_then(|b| String::from_utf8(b).ok()) {
            Some(t) => t,
            None => {
                return err_resp(
                    StatusCode::BAD_REQUEST,
                    "no_manifest",
                    "missing x-spared-manifest header".into(),
                    "deploy with `spared deploy`",
                );
            }
        };
        // `?frozen=move`: the app ends this deploy frozen, also when it is new, so it never serves
        // here before `spared move` has restored its data. A plain deploy over a frozen app is
        // refused.
        let frozen_for_move = match query_param(req.uri().query(), "frozen") {
            None => false,
            Some("move") => true,
            Some(_) => {
                return err_resp(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "the frozen query parameter takes only the value move".into(),
                    "leave it out, or pass ?frozen=move (spared move does)",
                );
            }
        };
        let cap = self.cfg.compile.max_component_bytes;
        let collect = Limited::new(req.into_body(), cap).collect();
        let wasm = match tokio::time::timeout(DEPLOY_BODY_TIMEOUT, collect).await {
            Ok(Ok(b)) => b.to_bytes(),
            Ok(Err(_)) => {
                return err_resp(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "component_too_large",
                    format!("component over {} MB", cap >> 20),
                    "build with --release; the operator can raise the limit with --max-component-mb",
                );
            }
            Err(_) => {
                return err_resp(
                    StatusCode::REQUEST_TIMEOUT,
                    "request_timeout",
                    "component upload took over 120 s".into(),
                    "retry from a faster connection",
                );
            }
        };
        // Before the compile, which a frozen app would waste; `install` checks again under the
        // name lock. After the body is read, so a client still sending is not cut off.
        let frozen_now = self
            .apps
            .read()
            .unwrap()
            .get(name)
            .is_some_and(|a| a.is_frozen());
        if frozen_now && !frozen_for_move {
            return deploy_refused_frozen(name).into_resp();
        }
        let (server, name) = (self.clone(), name.to_string());
        detached(async move {
            match server
                .install(&name, &manifest_text, &wasm, frozen_for_move, &creator)
                .await
            {
                Ok(info) => json_resp(StatusCode::OK, info),
                Err(e) => e.into_resp(),
            }
        })
        .await
    }

    /// The token that authenticated a deploy, asked again under the name lock: the body can take
    /// minutes, and a token revoked, expired or spent since authentication changes nothing.
    fn check_token_still_allows(
        &self,
        id: &str,
        name: &str,
        creating: bool,
    ) -> std::result::Result<(), DeployError> {
        let rec = self.tokens.lock().unwrap().get(id);
        let Some(rec) = rec else {
            return Err(token_revoked(id));
        };
        match rec.status(unix_now()) {
            "revoked" => Err(token_revoked(id)),
            "expired" => Err(DeployError::new(
                StatusCode::UNAUTHORIZED,
                "token_expired",
                format!("app token {id} has expired"),
                "ask the operator for a new token (`spared token create <app>` with the admin token)",
            )),
            _ if creating && !rec.may_create => Err(cannot_create(name)),
            _ => Ok(()),
        }
    }

    async fn install(
        self: &Arc<Self>,
        name: &str,
        manifest_text: &str,
        wasm: &[u8],
        frozen_for_move: bool,
        creator: &Creator,
    ) -> std::result::Result<serde_json::Value, DeployError> {
        check_name(name)?;
        let mut manifest = match Manifest::parse(manifest_text) {
            Ok(m) => m,
            Err(e) => {
                return Err(DeployError::new(
                    StatusCode::BAD_REQUEST,
                    e.kind,
                    e.message,
                    e.fix,
                ));
            }
        };
        if manifest.name != name {
            return Err(DeployError::new(
                StatusCode::BAD_REQUEST,
                "name_mismatch",
                format!("spared.toml name '{}' differs from '{name}'", manifest.name),
                "make the names match",
            ));
        }
        if let Some((field, asked, max, flag)) = manifest.limits.over(&self.cfg.max) {
            return Err(DeployError::new(
                StatusCode::BAD_REQUEST,
                "limit_too_high",
                format!("limits.{field} = {asked} is above this server's maximum of {max}"),
                format!(
                    "set limits.{field} to at most {max} in spared.toml, or have the operator raise it with `spared serve {flag} N`"
                ),
            ));
        }
        let hash = sha256_hex(wasm);
        // Everything that can reject the component happens here, before the name lock is taken
        // and before anything on disk changes: parse, imports, handler, and the link step that
        // used to run only when the app was loaded after its files were already replaced.
        let reusable = if self.component_path(&hash).is_file() {
            let mut cache = self.components.lock().unwrap();
            match self.shared_component(&mut cache, &hash) {
                Ok(s) => Some(s),
                Err(_) => {
                    let _ = std::fs::remove_file(self.component_path(&hash));
                    None
                }
            }
        } else {
            None
        };
        let reused = reusable.is_some();
        let mut compiled = None;
        let comp = match &reusable {
            Some(s) => s.component.clone(),
            None => {
                let (server, bytes) = (self.clone(), wasm.to_vec());
                let cwasm = tokio::task::spawn_blocking(move || {
                    compile::compile(&server.engine, &server.cfg.compile, &bytes, true)
                })
                .await
                .map_err(|e| {
                    DeployError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "precompile_failed",
                        format!("the compile task stopped: {e}"),
                        "report this",
                    )
                })?
                .map_err(|e| self.compile_failure(e))?;
                let comp =
                    unsafe { Component::deserialize(&self.engine, &cwasm) }.map_err(|e| {
                        DeployError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "precompile_failed",
                            format!("the compiled component cannot be loaded: {e:#}"),
                            "report this",
                        )
                    })?;
                compiled = Some(cwasm);
                comp
            }
        };
        if let Err(r) = crate::sandbox::check_component(&self.engine, &comp, &manifest.grants) {
            return Err(DeployError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                r.kind,
                r.message,
                r.fix,
            ));
        }
        if !reused {
            if let Err(e) = self.link(&comp) {
                return Err(DeployError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_component",
                    format!(
                        "the component cannot be linked against the host's WASI HTTP interfaces: {e:#}"
                    ),
                    "build for wasm32-wasip2 with #[wstd::http_server], and with a wstd version this spared supports",
                ));
            }
        }
        let cwasm = compiled;
        let lock = self.name_lock(name);
        let _deploying = lock.lock().await;
        let dir = self.cfg.data_dir.join("apps").join(name);
        let created = !dir.exists();
        let data_kept = dir.join("data").is_dir();
        let prev = self.apps.read().unwrap().get(name).cloned();
        if !frozen_for_move && prev.as_ref().is_some_and(|a| a.is_frozen()) {
            return Err(deploy_refused_frozen(name));
        }
        if let Some(id) = creator.token_id() {
            self.check_token_still_allows(id, name, prev.is_none())?;
        }
        if prev.is_none() && matches!(creator, Creator::No(_)) {
            return Err(cannot_create(name));
        }
        let version = prev.as_ref().map(|a| a.version + 1).unwrap_or(1);
        // Stage the new release next to the old one, under temporary names. Until the commit
        // below, nothing the running or restarting server reads has changed.
        let staged = self.stage_release(&dir, &hash, wasm, cwasm, manifest_text, version);
        let shared = match staged {
            Ok(s) => s,
            Err(e) => {
                self.abandon_release(&dir, &hash, created);
                return Err(e);
            }
        };
        // Before the commit and before the new app is in the table, so it never serves unfrozen.
        let marked = frozen_for_move && read_freeze_marker(&dir).is_none();
        if marked {
            let info = serde_json::json!({"kind": "move", "reason": "deployed frozen by spared move", "at": now_text()});
            if let Err(e) = write_freeze_marker(&dir, &info) {
                self.abandon_release(&dir, &hash, created);
                return Err(write_failed(e));
            }
        }
        manifest.limits.clamp(&self.cfg.max);
        let unmark = || {
            if marked {
                let _ = remove_freeze_marker(&dir);
            }
        };
        let app = match self.make_app(name, dir.clone(), manifest, shared, version) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                unmark();
                self.abandon_release(&dir, &hash, created);
                return Err(DeployError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "load_failed",
                    format!("{e:#}"),
                    "report this",
                ));
            }
        };
        // Retire the old instance before the new app can serve, so two instances never hold the
        // same SQLite file. Requests queued on the old app see `retired` and move to the new one.
        let old_live = match &prev {
            Some(old) => {
                let mut g = old.live.lock().await;
                if g.take().is_some() {
                    self.metrics.eviction(Evicted::Deploy);
                }
                old.hot.store(false, Ordering::Relaxed);
                old.retired.store(true, Ordering::Release);
                Some(g)
            }
            None => None,
        };
        if let Err(e) = commit_release(&dir) {
            if let Some(old) = &prev {
                old.retired.store(false, Ordering::Release);
            }
            unmark();
            self.abandon_release(&dir, &hash, created);
            return Err(write_failed(e));
        }
        self.apps
            .write()
            .unwrap()
            .insert(name.to_string(), app.clone());
        drop(old_live);
        if let Some(old) = &prev {
            self.release_component(&old.shared.hash);
        }
        if let (None, Creator::Token(id)) = (&prev, creator) {
            self.tokens.lock().unwrap().created_app(id);
        }
        let mut info = self.one_app_json(app).await;
        info["data_kept"] = data_kept.into();
        info["component_reused"] = reused.into();
        if data_kept {
            info["note"] = format!(
                "redeploy keeps the existing data/ of '{name}'; run `spared rm {name} --yes` first for a fresh start"
            )
            .into();
        }
        Ok(info)
    }

    /// Everything a deploy writes before its commit: the app directory and `data/`, the component
    /// in the shared store (synced and renamed into place), and the new manifest, hash, version
    /// and component links under `*.tmp` names, all synced. Returns the linked component. The
    /// files the running app and a restart read are untouched.
    fn stage_release(
        &self,
        dir: &Path,
        hash: &str,
        wasm: &[u8],
        cwasm: Option<Vec<u8>>,
        manifest_text: &str,
        version: u64,
    ) -> std::result::Result<Arc<SharedComponent>, DeployError> {
        create_private_dir(&dir.join("data")).map_err(write_failed)?;
        let shared = {
            let mut cache = self.components.lock().unwrap();
            let stored = self.component_path(hash);
            if !stored.is_file() {
                let cwasm = match cwasm {
                    Some(c) => c,
                    None => compile::compile(&self.engine, &self.cfg.compile, wasm, false)
                        .map_err(|e| self.compile_failure(e))?,
                };
                Self::store_file(&stored, &cwasm).map_err(write_failed)?;
            }
            // The deployed wasm is kept too, so a wasmtime upgrade can recompile it.
            let stored_wasm = self.wasm_path(hash);
            Self::store_file(&stored_wasm, wasm).map_err(write_failed)?;
            let shared = self.shared_component(&mut cache, hash).map_err(|e| {
                DeployError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "load_failed",
                    format!("{e:#}"),
                    "report this",
                )
            })?;
            for (from, to) in [
                (&stored, "component.cwasm.tmp"),
                (&stored_wasm, "component.wasm.tmp"),
            ] {
                let tmp = dir.join(to);
                let _ = std::fs::remove_file(&tmp);
                std::fs::hard_link(from, &tmp)
                    .or_else(|_| write_private(&tmp, &std::fs::read(from)?))
                    .map_err(write_failed)?;
            }
            shared
        };
        write_private(&dir.join("component.sha256.tmp"), hash.as_bytes()).map_err(write_failed)?;
        write_private(&dir.join("manifest.toml.tmp"), manifest_text.as_bytes())
            .map_err(write_failed)?;
        write_private(&dir.join("version.tmp"), version.to_string().as_bytes())
            .map_err(write_failed)?;
        sync_dir(dir);
        Ok(shared)
    }

    /// Undo a deploy that failed before its commit: remove the staged files, the store files no
    /// app links to, and an app directory this deploy created.
    fn abandon_release(&self, dir: &Path, hash: &str, created: bool) {
        discard_staged(dir);
        if created {
            let _ = backup::remove_tree(dir);
        }
        self.release_component(hash);
    }

    async fn delete(&self, name: &str) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        if !self.apps.read().unwrap().contains_key(name) {
            return no_such_app(name);
        }
        let lock = self.name_lock(name);
        let _deleting = lock.lock().await;
        let Some(a) = self.apps.write().unwrap().remove(name) else {
            return no_such_app(name);
        };
        // The app is gone, and so is every token for it: a token never outlives its app to
        // create a new one under the same name, also when the directory cannot be moved.
        let (revoked, failed) = self.tokens.lock().unwrap().revoke_app(name, unix_now());
        let revoke_failed = failed.is_some();
        if let Some(e) = failed {
            eprintln!(
                "delete {name}: its tokens are revoked in this process, but writing that failed ({e}); they would work again after a restart. Revoke them with `spared token revoke <id>`"
            );
        }
        let mut g = a.live.lock().await;
        if g.take().is_some() {
            self.metrics.eviction(Evicted::Deploy);
        }
        a.retired.store(true, Ordering::Release);
        drop(g);
        // Move the whole directory out of apps/ in one rename, so the app is either there or
        // gone; the tree is then removed off the async runtime without recursion (an app
        // controls how deep it is). A removal that fails is retried at the next start.
        let trash = self.cfg.data_dir.join(TRASH);
        let doomed = trash.join(format!("{name}.{}", unique_suffix()));
        let moved = create_private_dir(&trash).and_then(|_| std::fs::rename(&a.dir, &doomed));
        if let Err(e) = moved {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "delete_failed",
                format!("app '{name}' is stopped but its directory remains: {e}"),
                "remove <data-dir>/apps/<name> by hand before deploying the name again",
            );
        }
        let removed = tokio::task::spawn_blocking(move || backup::remove_tree(&doomed)).await;
        self.release_component(&a.shared.hash);
        // An operator's override stays, and so does what the app used under it: both are server
        // policy for the name, and removing and deploying the app again must not hand out a fresh
        // CPU or egress budget. Without an override the counters go with the app.
        let kept_limits = {
            let mut qs = self.quotas.lock().unwrap();
            let kept = qs.get(name).is_some_and(|q| q.override_now().is_some());
            if !kept {
                qs.remove(name);
            }
            kept
        };
        let mut out = serde_json::json!({"deleted": name});
        if kept_limits {
            out["limits_kept"] = true.into();
        }
        if revoked > 0 {
            out["tokens_revoked"] = revoked.into();
        }
        if revoke_failed {
            out["tokens_revoke_failed"] = true.into();
        }
        if let Err(e) = crate::remote::clear_restore_record(&self.cfg.data_dir, name) {
            eprintln!(
                "delete {name}: the epoch file still names the remote backup it was restored from: {e}"
            );
        }
        if !matches!(removed, Ok(Ok(()))) {
            eprintln!("delete {name}: leftover files stay under {TRASH}/ until the next start");
            out["cleanup_pending"] = true.into();
        }
        let root = self.backups_root(name);
        let kept = tokio::task::spawn_blocking(move || backup::ids(&root).len())
            .await
            .unwrap_or(0);
        if kept > 0 {
            out["backups_kept"] = kept.into();
            out["note"] = format!(
                "{kept} backup(s) of '{name}' remain under <data-dir>/backups/{name}/ and can be restored into a new app with this name; delete that directory to remove the data for good"
            )
            .into();
        }
        json_resp(StatusCode::OK, out)
    }

    fn backups_root(&self, name: &str) -> PathBuf {
        self.cfg.data_dir.join("backups").join(name)
    }

    /// Stop `name`'s live instance and keep its instance lock, so no request runs and no handle
    /// holds the database until the guard drops. Waits for a request in flight to finish.
    async fn lock_idle(&self, name: &str) -> std::result::Result<(Arc<App>, IdleGuard), Resp> {
        loop {
            let Some(app) = self.apps.read().unwrap().get(name).cloned() else {
                return Err(no_such_app(name));
            };
            let mut g = app.live.clone().lock_owned().await;
            if app.retired.load(Ordering::Acquire) {
                drop(g);
                let next = self.apps.read().unwrap().get(name).cloned();
                match next {
                    Some(a) if !Arc::ptr_eq(&a, &app) => continue,
                    _ => return Err(no_such_app(name)),
                }
            }
            if g.take().is_some() {
                self.metrics.eviction(Evicted::Admin);
            }
            app.hot.store(false, Ordering::Relaxed);
            return Ok((app, g));
        }
    }

    fn require_app(&self, name: &str) -> std::result::Result<(), DeployError> {
        check_name(name)?;
        if self.apps.read().unwrap().contains_key(name) {
            Ok(())
        } else {
            Err(DeployError::new(
                StatusCode::NOT_FOUND,
                "no_such_app",
                format!("no app '{name}'"),
                "list apps with `spared ps`",
            ))
        }
    }

    async fn evict(&self, name: &str) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        match self.lock_idle(name).await {
            Ok((app, g)) => {
                drop(g);
                json_resp(StatusCode::OK, self.one_app_json(app).await)
            }
            Err(r) => r,
        }
    }

    /// Freeze `name`: under its name lock and its instance lock, drop the instance, write the
    /// marker and set the flag, so no request runs after this returns and none runs after a
    /// restart. An app that is already frozen keeps its freeze when `kind` is `move` (a move never
    /// takes over a freeze something else made, so it never thaws one) or when that freeze came
    /// from a newer epoch in the bucket; otherwise the new freeze replaces it.
    async fn freeze_app(
        &self,
        name: &str,
        kind: &str,
        reason: &str,
    ) -> std::result::Result<serde_json::Value, Resp> {
        check_name(name).map_err(DeployError::into_resp)?;
        let lock = self.name_lock(name);
        let _serial = lock.lock().await;
        let (app, _instance) = self.lock_idle(name).await?;
        if let Some(info) = app.freeze_info() {
            if kind == "move" || info["kind"] == kind || info["kind"] == "remote_superseded" {
                return Ok(serde_json::json!({
                    "name": name, "frozen": true, "already_frozen": true, "freeze": info,
                }));
            }
        }
        let info = serde_json::json!({"kind": kind, "reason": reason, "at": now_text()});
        write_freeze_marker(&app.dir, &info).map_err(|e| {
            io_fail("freeze_failed", "cannot write the frozen marker", e).into_resp()
        })?;
        app.set_freeze(Some(info.clone()));
        eprintln!("app {name}: frozen ({kind}): {reason}");
        Ok(serde_json::json!({
            "name": name, "frozen": true, "already_frozen": false, "freeze": info,
        }))
    }

    /// Thaw `name`: remove the marker and clear the flag. With `only_kind`, only a freeze of
    /// that kind is lifted (the move's own clean-up), and any other is reported and kept.
    async fn thaw_app(
        &self,
        name: &str,
        only_kind: Option<&str>,
    ) -> std::result::Result<serde_json::Value, Resp> {
        check_name(name).map_err(DeployError::into_resp)?;
        let lock = self.name_lock(name);
        let _serial = lock.lock().await;
        let (app, _instance) = self.lock_idle(name).await?;
        let Some(info) = app.freeze_info() else {
            return Ok(serde_json::json!({"name": name, "frozen": false, "was_frozen": false}));
        };
        if only_kind.is_some_and(|k| info["kind"] != k) {
            return Ok(serde_json::json!({
                "name": name, "frozen": true, "thawed": false, "freeze": info,
            }));
        }
        remove_freeze_marker(&app.dir).map_err(|e| {
            io_fail("thaw_failed", "cannot remove the frozen marker", e).into_resp()
        })?;
        app.set_freeze(None);
        eprintln!("app {name}: thawed");
        let mut v = serde_json::json!({
            "name": name, "frozen": false, "was_frozen": true, "thawed": true, "was": info,
        });
        if info["kind"] == "remote_superseded" {
            v["note"] = "this app was frozen because the bucket holds a newer epoch of it: another server restored it. It serves here again, and its next upload is refused and freezes it again".into();
        }
        Ok(v)
    }

    async fn freeze_route(&self, name: &str, req: Request<Incoming>) -> Resp {
        let usage = r#"send nothing, or {"reason":"<text>"}"#;
        let body = match self.small_json_body(req, usage).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let reason = body["reason"].as_str().unwrap_or("frozen by spared freeze");
        let kind = body["kind"].as_str().unwrap_or("manual");
        if reason.chars().count() > 200 || reason.chars().any(char::is_control) {
            return err_resp(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "the reason must be at most 200 characters, with no control characters".into(),
                usage,
            );
        }
        if !FREEZE_KINDS.contains(&kind) || kind == "remote_superseded" {
            return err_resp(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!(
                    "'{}' is not a kind of freeze",
                    kind.chars().take(40).collect::<String>()
                ),
                "leave kind out (manual), or pass \"move\" (spared move does)",
            );
        }
        match self.freeze_app(name, kind, reason).await {
            Ok(v) => json_resp(StatusCode::OK, v),
            Err(r) => r,
        }
    }

    async fn thaw_route(&self, name: &str, req: Request<Incoming>) -> Resp {
        let usage = r#"send nothing, or {"if_kind":"move"}"#;
        let body = match self.small_json_body(req, usage).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        match self.thaw_app(name, body["if_kind"].as_str()).await {
            Ok(v) => json_resp(StatusCode::OK, v),
            Err(r) => r,
        }
    }

    /// An optional JSON object body of at most 4 KiB: `Null` when the body is empty.
    async fn small_json_body(
        &self,
        req: Request<Incoming>,
        usage: &str,
    ) -> std::result::Result<serde_json::Value, Resp> {
        let collect = Limited::new(req.into_body(), 4096).collect();
        let bad = |m: &str| err_resp(StatusCode::BAD_REQUEST, "bad_request", m.into(), usage);
        let body = match tokio::time::timeout(self.cfg.client_timeout, collect).await {
            Ok(Ok(b)) => b.to_bytes(),
            _ => return Err(bad("could not read the request body")),
        };
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .filter(serde_json::Value::is_object)
            .ok_or_else(|| bad("the body must be a JSON object"))
    }

    /// What `spared limits show` prints for `name`: the stored override, what is in force, the
    /// server's caps, the fields the caps lowered, and what the app has used. Works for a name with
    /// an override and no app, and for neither.
    fn limits_json(&self, name: &str) -> serde_json::Value {
        let q = self.quotas.lock().unwrap().get(name).cloned();
        let over = q.as_ref().and_then(|q| q.override_now());
        let mut v = self.quota_json(over.as_ref(), q.as_deref());
        v["name"] = name.into();
        v["app_exists"] = self.apps.read().unwrap().contains_key(name).into();
        v["override"] = match &over {
            Some(o) => serde_json::to_value(o).unwrap_or_default(),
            None => serde_json::Value::Null,
        };
        v["set_at"] = quota::set_at(&self.cfg.data_dir, name).into();
        if let Some(why) = q.as_ref().and_then(|q| q.damage()) {
            v["damaged"] = why.into();
        }
        v
    }

    /// The effective limits, the fields set by an override, the fields the server's caps lowered,
    /// and the CPU and egress use. Admin callers only: an app token never sees its limits.
    fn quota_json(
        &self,
        over: Option<&quota::Override>,
        q: Option<&AppQuota>,
    ) -> serde_json::Value {
        let caps = &self.cfg.caps;
        let (eff, clamped) = quota::Override::effective(over, caps);
        let set: Vec<&str> = over
            .map(|o| {
                [
                    ("data_mb", o.data_mb.is_some()),
                    ("entries", o.entries.is_some()),
                    ("cpu_ms_per_min", o.cpu_ms_per_min.is_some()),
                    ("egress_bytes_per_hour", o.egress_bytes_per_hour.is_some()),
                ]
                .into_iter()
                .filter_map(|(k, on)| on.then_some(k))
                .collect()
            })
            .unwrap_or_default();
        serde_json::json!({
            "effective": {
                "data_mb": eff.data_bytes >> 20,
                "entries": eff.entries,
                "cpu_ms_per_min": eff.cpu_ms_per_min,
                "egress_bytes_per_hour": eff.egress_bytes_per_hour,
            },
            "server_caps": {
                "data_mb": caps.max_data_bytes >> 20,
                "entries": caps.max_entries,
                "cpu_ms_per_min": quota::CPU_MS_PER_MIN_MAX,
            },
            "overridden": set,
            "clamped": clamped,
            "usage": q.map_or_else(|| AppQuota::new(None).usage_json(), AppQuota::usage_json),
        })
    }

    async fn show_limits(&self, name: &str) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        let mut v = self.limits_json(name);
        let app = self.apps.read().unwrap().get(name).cloned();
        if let Some(a) = app {
            let data = a.dir.join("data");
            let st = tokio::task::spawn_blocking(move || backup::tree_counts(&data))
                .await
                .unwrap_or_default();
            v["usage"]["data_bytes"] = st.0.bytes.into();
            v["usage"]["data_entries"] = st.1.into();
        }
        json_resp(StatusCode::OK, v)
    }

    /// `PUT /v1/apps/{name}/limits`: store the override (replacing any earlier one), put it in
    /// force, and drop the app's instance so the next one wakes with the new disk caps. The CPU and
    /// egress budgets apply at once. The name need not have an app yet.
    async fn set_limits(&self, name: &str, req: Request<Incoming>) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        let collect = Limited::new(req.into_body(), 4096).collect();
        let body = match tokio::time::timeout(self.cfg.client_timeout, collect).await {
            Ok(Ok(b)) => b.to_bytes(),
            _ => {
                return err_resp(
                    StatusCode::BAD_REQUEST,
                    "bad_limits",
                    "could not read a request body of at most 4 KiB".into(),
                    LIMITS_USAGE,
                );
            }
        };
        let over = match quota::Override::from_request(&body) {
            Ok(o) => o,
            Err(m) => return err_resp(StatusCode::BAD_REQUEST, "bad_limits", m, LIMITS_USAGE),
        };
        let lock = self.name_lock(name);
        let _serial = lock.lock().await;
        {
            let _io = self.limits_io.lock().unwrap();
            // The stamp first: an override on disk under a format-1 stamp is what an older
            // release would open and ignore.
            if let Err(e) = datadir::stamp(&self.cfg.data_dir, datadir::LIMITS_FORMAT) {
                return limits_write_failed("cannot raise the data dir's format stamp", e.message);
            }
            if let Err(e) = quota::write(&self.cfg.data_dir, name, &over, &now_text()) {
                return limits_write_failed("cannot write the limits file", e);
            }
        }
        self.quota(name).set(Some(over));
        eprintln!("app {name}: limits set");
        let evicted = self.after_limits_change(name).await;
        let mut v = self.limits_json(name);
        v["evicted"] = evicted.into();
        json_resp(StatusCode::OK, v)
    }

    /// `DELETE /v1/apps/{name}/limits`: remove the override, so the server's caps apply again.
    /// The last one removed puts the format stamp back to 1.
    async fn clear_limits(&self, name: &str) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        let lock = self.name_lock(name);
        let _serial = lock.lock().await;
        let removed = {
            let _io = self.limits_io.lock().unwrap();
            let removed = match quota::remove(&self.cfg.data_dir, name) {
                Ok(r) => r,
                Err(e) => return limits_write_failed("cannot remove the limits file", e),
            };
            match quota::stored_names(&self.cfg.data_dir) {
                Ok(left) if left.is_empty() => {
                    if let Err(e) = datadir::stamp(&self.cfg.data_dir, datadir::BASE_FORMAT) {
                        eprintln!(
                            "warning: no app has limits any more, but the format stamp stays at 2 ({}). Releases before per-app limits refuse this data dir until the next start",
                            e.message
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!(
                    "warning: the limits directory cannot be listed ({e}), so the format stamp stays at 2"
                ),
            }
            removed
        };
        let app_exists = self.apps.read().unwrap().contains_key(name);
        {
            let mut qs = self.quotas.lock().unwrap();
            if app_exists || self.cfg.data_dir.join("apps").join(name).is_dir() {
                if let Some(q) = qs.get(name) {
                    q.set(None);
                }
            } else {
                qs.remove(name);
            }
        }
        if removed {
            eprintln!("app {name}: limits cleared");
        }
        let evicted = self.after_limits_change(name).await;
        let mut v = self.limits_json(name);
        v["cleared"] = removed.into();
        v["evicted"] = evicted.into();
        json_resp(StatusCode::OK, v)
    }

    /// After an override changed: drop the app's instance so its next wake reads the new disk
    /// caps, or load the app when a damaged limits file kept it out of the table at start. True
    /// when an instance was dropped. The caller holds the name lock.
    async fn after_limits_change(&self, name: &str) -> bool {
        let app = self.apps.read().unwrap().get(name).cloned();
        if let Some(app) = app {
            let mut g = app.live.lock().await;
            let had = g.take().is_some();
            if had {
                app.hot.store(false, Ordering::Relaxed);
                self.metrics.eviction(Evicted::Admin);
            }
            return had;
        }
        if self
            .cfg
            .data_dir
            .join("apps")
            .join(name)
            .join("manifest.toml")
            .is_file()
        {
            match self.load_app(name) {
                Ok(a) => {
                    eprintln!("app {name}: loaded now that its limits file is readable");
                    self.apps
                        .write()
                        .unwrap()
                        .insert(name.to_string(), Arc::new(a));
                }
                Err(e) => eprintln!("app {name}: still not loaded: {e:#}"),
            }
        }
        false
    }

    /// The app's deployed code: the original wasm as the body and its `spared.toml` in the
    /// `x-spared-manifest` header (base64), the shape a deploy takes, so `spared move` can deploy
    /// it on another server, which compiles it itself. Never data, logs or the compiled file; a
    /// manifest holds no secrets (a `[secrets]` table is refused at deploy).
    async fn export_code(&self, name: &str) -> Resp {
        let app = match check_name(name).map(|_| self.apps.read().unwrap().get(name).cloned()) {
            Ok(Some(a)) => a,
            Ok(None) => return no_such_app(name),
            Err(e) => return e.into_resp(),
        };
        // Under the name lock, so a deploy cannot swap the files between the two reads.
        let lock = self.name_lock(name);
        let _serial = lock.lock().await;
        let dir = app.dir.clone();
        let read = tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<u8>, String)> {
            Ok((
                std::fs::read(dir.join("component.wasm"))?,
                std::fs::read_to_string(dir.join("manifest.toml"))?,
            ))
        })
        .await;
        let (wasm, manifest) = match read {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                return err_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "code_unavailable",
                    format!("the deployed code of '{name}' cannot be read here: {e}"),
                    "deploy the app on the other server from its source with `spared deploy`",
                );
            }
            Err(e) => return backup_task_failed(e),
        };
        let hash = sha256_hex(&wasm);
        if hash != app.shared.hash {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "code_unavailable",
                format!("the stored wasm of '{name}' does not match its recorded SHA-256"),
                "redeploy the app here with `spared deploy`, or deploy it on the other server from its source",
            );
        }
        Response::builder()
            .header("content-type", "application/wasm")
            .header(
                "x-spared-manifest",
                base64::engine::general_purpose::STANDARD.encode(manifest.as_bytes()),
            )
            .header("x-spared-component-sha256", hash)
            .body(Full::new(Bytes::from(wasm)))
            .unwrap()
    }

    /// `GET /v1/server`: this server's version, node id and the identity of its bucket (no
    /// credential, no key), so `spared move` can check that two servers share a bucket and a key.
    fn server_info(&self) -> Resp {
        json_resp(
            StatusCode::OK,
            serde_json::json!({
                "version": crate::VERSION,
                "node": self.remote.as_ref().map(|r| r.node()),
                "remote": self.remote.as_ref().map(|r| r.identity()),
                "public_port": self.cfg.listen.port(),
            }),
        )
    }

    /// `POST /v1/tokens` with `{"app", "scopes"?, "expires_in_secs"?, "label"?}`: mint an app
    /// token. The response is the only place its secret ever appears.
    async fn create_token(&self, req: Request<Incoming>) -> Resp {
        let usage = r#"send {"app": "<name>", "scopes": ["deploy", "read", "evict"], "expires_in_secs": 2592000, "label": "ci"}; only app is required"#;
        let body = match self.small_json_body(req, usage).await {
            Ok(serde_json::Value::Null) => serde_json::json!({}),
            Ok(v) => v,
            Err(r) => return r,
        };
        let bad = |kind: &'static str, m: String, fix: &str| {
            err_resp(StatusCode::BAD_REQUEST, kind, m, fix)
        };
        let Some(app) = body["app"].as_str().filter(|a| valid_name(a)) else {
            return bad(
                "bad_name",
                "the token needs the name of the app it is for".into(),
                "pass an app name: 1 to 40 lowercase letters, digits or dashes",
            );
        };
        let scopes = match &body["scopes"] {
            serde_json::Value::Null => tokens::DEFAULT_SCOPES.to_vec(),
            serde_json::Value::Array(a) if !a.is_empty() => {
                match tokens::parse_scopes(a.iter().map(|w| w.as_str().unwrap_or("?"))) {
                    Ok(s) => s,
                    Err(w) => {
                        return bad(
                            "bad_scope",
                            format!(
                                "'{}' is not a scope",
                                w.chars().take(40).collect::<String>()
                            ),
                            &format!("use some of: {}", tokens::scope_list()),
                        );
                    }
                }
            }
            _ => {
                return bad(
                    "bad_scope",
                    "scopes must be a non-empty list".into(),
                    &format!("use some of: {}", tokens::scope_list()),
                );
            }
        };
        let now = unix_now();
        let expires = match &body["expires_in_secs"] {
            serde_json::Value::Null => None,
            v => match v
                .as_u64()
                .filter(|&s| (1..=tokens::MAX_EXPIRES_SECS).contains(&s))
            {
                Some(s) => Some(now + s),
                None => {
                    return bad(
                        "bad_expiry",
                        "expires_in_secs must be a whole number of seconds from 1 to ten years"
                            .into(),
                        "pass --expires 30d (or 12h, 45m, 90s), or leave it out for a token that does not expire",
                    );
                }
            },
        };
        let label = match &body["label"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(l)
                if !l.is_empty()
                    && l.chars().count() <= tokens::MAX_LABEL
                    && !l.chars().any(char::is_control) =>
            {
                Some(l.clone())
            }
            _ => {
                return bad(
                    "bad_label",
                    format!(
                        "the label must be 1 to {} characters, without control characters",
                        tokens::MAX_LABEL
                    ),
                    "pass a short name such as --name ci",
                );
            }
        };
        let may_create = !self.apps.read().unwrap().contains_key(app);
        let minted = self
            .tokens
            .lock()
            .unwrap()
            .mint(app, scopes, expires, label, may_create, now);
        match minted {
            Ok((secret, rec)) => {
                let mut v = rec.public_json(now);
                v["token"] = secret.into();
                v["note"] = "this is the only time the token is shown; the server keeps only its SHA-256. Store it in a secret store now".into();
                let mut r = json_resp(StatusCode::CREATED, v);
                r.headers_mut()
                    .insert("cache-control", "no-store".parse().unwrap());
                r
            }
            Err(e) => err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token_write_failed",
                format!(
                    "cannot write the token under <data-dir>/{}: {e}",
                    tokens::DIR
                ),
                "check disk space and permissions under the data directory; no token was made",
            ),
        }
    }

    /// `GET /v1/tokens[?app=<name>]`: ids and metadata, never a secret or a hash.
    fn list_tokens(&self, app: Option<&str>) -> Resp {
        let now = unix_now();
        let list: Vec<_> = self
            .tokens
            .lock()
            .unwrap()
            .list(app)
            .iter()
            .map(|r| r.public_json(now))
            .collect();
        json_resp(StatusCode::OK, serde_json::json!({"tokens": list}))
    }

    /// `DELETE /v1/tokens/{id}`: the token stops working at once.
    fn revoke_token(&self, id: &str) -> Resp {
        let now = unix_now();
        let done = if tokens::valid_id(id) {
            self.tokens.lock().unwrap().revoke(id, now)
        } else {
            None
        };
        let Some((rec, already, written)) = done else {
            return err_resp(
                StatusCode::NOT_FOUND,
                "no_such_token",
                format!(
                    "no token with id '{}'",
                    id.chars().take(40).collect::<String>()
                ),
                "list tokens with `spared token list` and pass an id from it",
            );
        };
        if let Err(e) = written {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token_write_failed",
                format!(
                    "token {} is refused by this process, but writing its revocation failed ({e}); after a restart it would work again",
                    rec.id
                ),
                "fix the disk under <data-dir>/tokens, then run `spared token revoke` again",
            );
        }
        let mut v = rec.public_json(now);
        v["revoked"] = true.into();
        v["already_revoked"] = already.into();
        json_resp(StatusCode::OK, v)
    }

    async fn backup(self: &Arc<Self>, name: &str) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        let taken = {
            let lock = self.name_lock(name);
            let _serial = lock.lock().await;
            let (app, _instance) = match self.lock_idle(name).await {
                Ok(x) => x,
                Err(r) => return r,
            };
            let (name, src, root, keep, max_bytes, pinned) = (
                name.to_string(),
                app.dir.join("data"),
                self.backups_root(name),
                self.cfg.max_backups,
                self.cfg.caps.max_data_bytes,
                self.pinned(name),
            );
            tokio::task::spawn_blocking(move || {
                take_backup(&name, &src, &root, keep, max_bytes, &pinned)
            })
            .await
        };
        // The instance lock and the name lock are released: the app serves again and can be
        // redeployed while the upload runs.
        match taken {
            Ok(Ok(mut v)) => {
                if let (Some(r), Some(id)) = (&self.remote, v["backup_id"].as_str()) {
                    // Run only when the bucket holds a newer epoch of the app: this copy is
                    // stale, so it takes no more writes.
                    let freeze = {
                        let (s, n) = (self.clone(), name.to_string());
                        Box::pin(async move {
                            let why = "the bucket holds a newer epoch of this app: it was restored on another server";
                            if let Err(r) = s.freeze_app(&n, "remote_superseded", why).await {
                                eprintln!(
                                    "app {n}: could not freeze it after a refused upload ({})",
                                    r.status()
                                );
                            }
                        })
                    };
                    v["remote"] = r.start_upload(name, id, self.cfg.caps.max_data_bytes, freeze);
                }
                json_resp(StatusCode::OK, v)
            }
            Ok(Err(e)) => e.into_resp(),
            Err(e) => backup_task_failed(e),
        }
    }

    fn pinned(&self, name: &str) -> std::collections::HashSet<String> {
        self.remote
            .as_ref()
            .map(|r| r.pinned(name))
            .unwrap_or_default()
    }

    fn remote_or_refuse(&self) -> std::result::Result<&Arc<crate::remote::Remote>, Resp> {
        self.remote.as_ref().ok_or_else(|| {
            err_resp(
                StatusCode::BAD_REQUEST,
                "remote_backups_off",
                "this server has no bucket for backups".into(),
                "start `spared serve` with --backup-to s3://<bucket>/<prefix> (docs/operating.md, off-machine backups)",
            )
        })
    }

    /// The upload state of one local backup: what `spared backup` waits on.
    async fn upload_status(&self, name: &str, id: &str) -> Resp {
        if let Err(e) = check_name(name).and_then(|_| check_backup_id(id)) {
            return e.into_resp();
        }
        match crate::remote::Remote::upload_status(&self.cfg.data_dir, name, id) {
            Some(v) => json_resp(StatusCode::OK, v),
            None if self.backups_root(name).join(id).is_dir() => json_resp(
                StatusCode::OK,
                serde_json::json!({"local_id": id, "status": "not_uploaded"}),
            ),
            None => no_such_backup(name, id),
        }
    }

    /// The app's backups in the bucket. Works without the app deployed, so an operator can look
    /// before deploying on a new machine.
    async fn list_remote(&self, name: &str) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        let r = match self.remote_or_refuse() {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        match r.list(name).await {
            Ok(v) => json_resp(StatusCode::OK, v),
            Err(e) => remote_error(e),
        }
    }

    /// Start fetching a remote backup into a new local backup (`spared restore --from remote:`
    /// polls the state, then restores the local id).
    async fn fetch(&self, name: &str, req: Request<Incoming>) -> Resp {
        if let Err(e) = check_name(name) {
            return e.into_resp();
        }
        let r = match self.remote_or_refuse() {
            Ok(r) => r.clone(),
            Err(resp) => return resp,
        };
        let usage = r#"send {"remote_id":"<id>"} (ids: `spared backups <name> --remote`)"#;
        let collect = Limited::new(req.into_body(), 4096).collect();
        let body = match tokio::time::timeout(self.cfg.client_timeout, collect).await {
            Ok(Ok(b)) => b.to_bytes(),
            _ => {
                return err_resp(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "could not read the request body".into(),
                    usage,
                );
            }
        };
        let rid = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["remote_id"].as_str().map(str::to_string));
        let Some(rid) = rid else {
            return err_resp(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "the body must be JSON with a string \"remote_id\"".into(),
                usage,
            );
        };
        if let Err(e) = check_remote_id(&rid) {
            return e.into_resp();
        }
        let ctx = crate::remote::FetchCtx {
            name_lock: self.name_lock(name),
            keep: self.cfg.max_backups,
            max_bytes: self.cfg.caps.max_data_bytes,
        };
        json_resp(StatusCode::ACCEPTED, r.start_fetch(name, &rid, ctx))
    }

    async fn fetch_status(&self, name: &str, rid: &str) -> Resp {
        if let Err(e) = check_name(name).and_then(|_| check_remote_id(rid)) {
            return e.into_resp();
        }
        match crate::remote::Remote::fetch_status(&self.cfg.data_dir, name, rid) {
            Some(v) => json_resp(StatusCode::OK, v),
            None => err_resp(
                StatusCode::NOT_FOUND,
                "no_such_fetch",
                format!("no fetch of {rid} for app '{name}' was started"),
                "start one with `spared restore <name> --from remote:<id> --yes`",
            ),
        }
    }

    async fn list_backups(&self, name: &str) -> Resp {
        if let Err(e) = self.require_app(name) {
            return e.into_resp();
        }
        let root = self.backups_root(name);
        let keep = self.cfg.max_backups;
        let items = tokio::task::spawn_blocking(move || backup::list(&root))
            .await
            .unwrap_or_default();
        let items: Vec<_> = items
            .iter()
            .map(|e| {
                let mut v = backup_entry_json(e);
                if let Some(r) =
                    crate::remote::Remote::upload_status(&self.cfg.data_dir, name, &e.id)
                {
                    v["remote"] = r;
                }
                v
            })
            .collect();
        json_resp(
            StatusCode::OK,
            serde_json::json!({"name": name, "backups": items, "max_backups": keep, "contains": BACKUP_CONTAINS}),
        )
    }

    async fn delete_backup(&self, name: &str, id: &str) -> Resp {
        if let Err(e) = self.require_app(name) {
            return e.into_resp();
        }
        if let Err(e) = check_backup_id(id) {
            return e.into_resp();
        }
        let lock = self.name_lock(name);
        let _serial = lock.lock().await;
        let dir = self.backups_root(name).join(id);
        if !dir.is_dir() {
            return no_such_backup(name, id);
        }
        if self.pinned(name).contains(id) {
            return err_resp(
                StatusCode::CONFLICT,
                "backup_uploading",
                format!("backup {id} of {name} is being uploaded to the bucket"),
                "wait for the upload to end (`spared backups <name>` shows it), then delete it",
            );
        }
        let _ = std::fs::remove_file(
            self.backups_root(name)
                .join(crate::remote::REMOTE_DIR)
                .join(format!("{id}.json")),
        );
        let doomed = self.backups_root(name).join(format!(".tmp-rm-{id}"));
        let removed = match std::fs::rename(&dir, &doomed) {
            Ok(()) => tokio::task::spawn_blocking(move || backup::remove_tree(&doomed))
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string()))),
            Err(e) => Err(e),
        };
        match removed {
            Ok(()) => json_resp(
                StatusCode::OK,
                serde_json::json!({"name": name, "deleted": id}),
            ),
            Err(e) => err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "delete_failed",
                format!("could not remove {}: {e}", dir.display()),
                "remove that directory by hand",
            ),
        }
    }

    async fn restore(self: &Arc<Self>, name: &str, req: Request<Incoming>) -> Resp {
        if let Err(e) = self.require_app(name) {
            return e.into_resp();
        }
        let collect = Limited::new(req.into_body(), 4096).collect();
        let body = match tokio::time::timeout(self.cfg.client_timeout, collect).await {
            Ok(Ok(b)) => b.to_bytes(),
            _ => {
                return err_resp(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "could not read the request body".into(),
                    r#"send {"backup_id":"<id>"} (ids: `spared backups <name>`)"#,
                );
            }
        };
        let id = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["backup_id"].as_str().map(str::to_string));
        let Some(id) = id else {
            return err_resp(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "the body must be JSON with a string \"backup_id\"".into(),
                r#"send {"backup_id":"<id>"} (ids: `spared backups <name>`)"#,
            );
        };
        if let Err(e) = check_backup_id(&id) {
            return e.into_resp();
        }
        let (server, name) = (self.clone(), name.to_string());
        detached(async move { server.restore_backup(&name, &id).await }).await
    }

    async fn restore_backup(&self, name: &str, id: &str) -> Resp {
        let lock = self.name_lock(name);
        let _serial = lock.lock().await;
        let root = self.backups_root(name);
        if !root.join(id).is_dir() {
            return no_such_backup(name, id);
        }
        let (app, _instance) = match self.lock_idle(name).await {
            Ok(x) => x,
            Err(r) => return r,
        };
        let (app_name, backup_id) = (name.to_string(), id.to_string());
        let (name, id, dir, keep, max_bytes, pinned) = (
            name.to_string(),
            id.to_string(),
            app.dir.clone(),
            self.cfg.max_backups,
            self.cfg.caps.max_data_bytes,
            self.pinned(name),
        );
        match tokio::task::spawn_blocking(move || {
            do_restore(&name, &dir, &root, &id, keep, max_bytes, &pinned)
        })
        .await
        {
            Ok(Ok(mut v)) => {
                self.raise_epoch_after_restore(&app_name, &backup_id, &mut v);
                json_resp(StatusCode::OK, v)
            }
            Ok(Err(e)) => e.into_resp(),
            Err(e) => backup_task_failed(e),
        }
    }

    /// After a restore of a backup fetched from the bucket: this server now holds the app's
    /// newest lineage, so its epoch for the app goes to the one the fetch found free. The data is
    /// in place first, so a crash between the two leaves the old epoch, never a raised epoch over
    /// old data.
    fn raise_epoch_after_restore(&self, name: &str, id: &str, v: &mut serde_json::Value) {
        let dd = &self.cfg.data_dir;
        let fetched = crate::remote::Remote::upload_status(dd, name, id)
            .filter(|st| st["status"] == "fetched");
        if let Some(st) = fetched
            && let (Some(next), Some(rid)) = (st["next_epoch"].as_u64(), st["remote_id"].as_str())
        {
            let digest = st["content_digest"].as_str().unwrap_or("");
            let next = u32::try_from(next).unwrap_or(crate::remote::MAX_EPOCH);
            match crate::remote::raise_epoch(dd, name, next, rid, digest) {
                Ok(e) => {
                    eprintln!("app {name}: restored from remote backup {rid}, epoch now {e}");
                    v["restored_from_remote"] = rid.into();
                    v["content_digest"] = digest.into();
                }
                Err(e) => {
                    eprintln!("app {name}: restored, but the epoch could not be recorded: {e}");
                    v["epoch_error"] = format!(
                        "the data was restored, but the new epoch could not be written ({e}). Free disk space under the data dir and restore again"
                    )
                    .into();
                }
            }
        } else if let Err(e) = crate::remote::clear_restore_record(dd, name) {
            eprintln!(
                "app {name}: the epoch file still names the remote backup the app was restored from before: {e}"
            );
        }
        if let Ok(e) = crate::remote::local_epoch(dd, name) {
            v["epoch"] = e.into();
        }
    }

    /// Returns how many examples were deployed now and how many were already there.
    async fn deploy_examples(
        self: &Arc<Self>,
        dir: &Path,
        stop: &mut (dyn FnMut() -> bool + Send),
    ) -> (usize, usize) {
        let mut dirs: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect(),
            Err(e) => {
                eprintln!("examples: cannot read {}: {e}", dir.display());
                return (0, 0);
            }
        };
        dirs.sort();
        let (mut deployed, mut present) = (0, 0);
        for d in dirs {
            if stop() {
                break;
            }
            let (wasm_path, toml_path) = (d.join("app.wasm"), d.join("spared.toml"));
            if !wasm_path.is_file() || !toml_path.is_file() {
                continue;
            }
            let Some(name) = d.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            if self.apps.read().unwrap().contains_key(&name) {
                eprintln!("examples: {name} already deployed, left alone");
                present += 1;
                continue;
            }
            let read = || -> std::io::Result<(String, Vec<u8>)> {
                Ok((
                    std::fs::read_to_string(&toml_path)?,
                    std::fs::read(&wasm_path)?,
                ))
            };
            let (manifest, wasm) = match read() {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("examples: {name}: cannot read files: {e}");
                    continue;
                }
            };
            match self
                .install(&name, &manifest, &wasm, false, &Creator::Admin)
                .await
            {
                Ok(_) => {
                    eprintln!("examples: deployed {name}");
                    deployed += 1;
                }
                Err(e) => eprintln!(
                    "examples: {name} not deployed: {} ({}); fix: {}",
                    e.message, e.kind, e.fix
                ),
            }
        }
        (deployed, present)
    }

    /// Refuse browsers (any `Origin`), rebound DNS names (`Host` must be an IP or localhost),
    /// and requests without a valid token; then say whose token it is. The admin token is
    /// compared in constant time, an app token by the SHA-256 of what was sent. A source address
    /// that sent too many wrong tokens is refused for a while, whatever it sends.
    fn authenticate(
        &self,
        req: &Request<Incoming>,
        peer: IpAddr,
    ) -> std::result::Result<Caller, Resp> {
        let h = req.headers();
        if h.contains_key(hyper::header::ORIGIN) {
            return Err(err_resp(
                StatusCode::FORBIDDEN,
                "origin_refused",
                "admin requests from a web page (with an Origin header) are refused".into(),
                "call the admin API from the spared CLI or curl, not from a browser",
            ));
        }
        let host = h
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if h.get_all(hyper::header::HOST).iter().count() > 1 {
            return Err(err_resp(
                StatusCode::FORBIDDEN,
                "bad_host",
                "admin requests must carry exactly one Host header".into(),
                "send one Host header (an IP address or localhost)",
            ));
        }
        if !admin_host_ok(host) {
            return Err(err_resp(
                StatusCode::FORBIDDEN,
                "bad_host",
                format!("admin Host '{host}' is not localhost or an IP address"),
                "connect to the admin listener by IP (http://127.0.0.1:8081) or localhost; other names are refused to block DNS rebinding",
            ));
        }
        let given = h
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .trim();
        let unauthorized = || {
            err_resp(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or wrong admin token".into(),
                "send `Authorization: Bearer <token>`; the CLI reads $SPARED_TOKEN, then ~/.spared/token. This server's token is in <data-dir>/admin-token",
            )
        };
        if given.is_empty() {
            return Err(unauthorized());
        }
        if ct_eq(given.as_bytes(), self.cfg.admin_token.as_bytes()) {
            return Ok(Caller::Admin);
        }
        let now = Instant::now();
        if let Some(secs) = self.auth_failures.retry_after(peer, now) {
            let mut r = err_resp(
                StatusCode::TOO_MANY_REQUESTS,
                "auth_rate_limited",
                format!(
                    "too many wrong tokens from {peer}; every token from it is refused for {secs} s"
                ),
                "wait for the Retry-After seconds, or use the admin token, which is never limited; find what keeps sending a wrong token (a script with an old token)",
            );
            r.headers_mut()
                .insert("retry-after", secs.to_string().parse().unwrap());
            return Err(r);
        }
        let auth = self.tokens.lock().unwrap().authenticate(given, unix_now());
        match auth {
            tokens::Auth::Valid(p) => Ok(Caller::App(p)),
            tokens::Auth::Expired(id) => Err(err_resp(
                StatusCode::UNAUTHORIZED,
                "token_expired",
                format!("app token {id} has expired"),
                "ask the operator for a new token (`spared token create <app>` with the admin token)",
            )),
            tokens::Auth::Revoked(id) => Err(err_resp(
                StatusCode::UNAUTHORIZED,
                "token_revoked",
                format!("app token {id} was revoked"),
                "ask the operator for a new token (`spared token create <app>` with the admin token)",
            )),
            tokens::Auth::Unknown => {
                self.auth_failures.failed(peer, now);
                Err(unauthorized())
            }
        }
    }

    /// Whether `caller` may use this route. An app token gets the same answer for another app's
    /// route whether or not that app exists, and before anything is looked up.
    fn authorize(
        &self,
        caller: &Caller,
        method: &Method,
        segs: &[&str],
        query: &str,
    ) -> Option<Resp> {
        let Caller::App(p) = caller else {
            return None;
        };
        let frozen = query_param(Some(query), "frozen").is_some();
        let fix_admin = "use the admin token for this; an app token works only on its own app's routes (AGENTS.md lists them by scope)";
        let wrong = |message: String, fix: &str| {
            Some(err_resp(StatusCode::FORBIDDEN, "wrong_scope", message, fix))
        };
        match tokens::need(method.as_str(), segs, frozen) {
            Need::Admin => wrong(
                format!(
                    "app token {} (for app '{}') cannot use {method} /{}",
                    p.id,
                    p.app,
                    segs.join("/")
                ),
                fix_admin,
            ),
            Need::OwnList if !p.scopes.contains(&Scope::Read) => wrong(
                format!("app token {} has no read scope", p.id),
                "ask the operator for a token with the read scope (`spared token create <app> --scope deploy,read,evict`)",
            ),
            Need::OwnList => None,
            Need::App(name, _) if name != p.app => wrong(
                format!(
                    "app token {} is for app '{}' and cannot act on '{name}'",
                    p.id, p.app
                ),
                &format!(
                    "use a token made for '{name}' (the operator runs `spared token create {name}`), or the admin token"
                ),
            ),
            Need::App(_, scope) if !p.scopes.contains(&scope) => wrong(
                format!(
                    "app token {} has no {} scope (it has: {})",
                    p.id,
                    scope.word(),
                    p.scopes
                        .iter()
                        .map(|s| s.word())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                &format!(
                    "ask the operator for a token with the {} scope: `spared token create {} --scope ...,{}`",
                    scope.word(),
                    p.app,
                    scope.word()
                ),
            ),
            Need::App(..) => None,
        }
    }

    /// The counters and gauges behind `GET /metrics`, in the Prometheus text format.
    fn metrics_text(&self) -> String {
        let (deployed, hot) = {
            let apps = self.apps.read().unwrap();
            let hot = apps
                .values()
                .filter(|a| a.hot.load(Ordering::Relaxed))
                .count();
            (apps.len(), hot)
        };
        let components_loaded = self
            .components
            .lock()
            .unwrap()
            .values()
            .filter(|w| w.strong_count() > 0)
            .count();
        self.metrics.render(&Gauges {
            version: crate::VERSION,
            uptime: self.started.elapsed(),
            apps_deployed: deployed,
            apps_hot: hot,
            components_loaded,
            resident_bytes: crate::obs::resident_bytes(),
        })
    }

    /// Handle an admin request. Returns the app the route names, if any, and the id of the app
    /// token used, if any, for the access log.
    async fn handle_admin(
        self: Arc<Self>,
        req: Request<Incoming>,
        peer: IpAddr,
    ) -> (Option<String>, Option<String>, Resp) {
        let caller = match self.authenticate(&req, peer) {
            Ok(c) => c,
            Err(r) => return (None, None, r),
        };
        let token_id = match &caller {
            Caller::App(p) => Some(p.id.clone()),
            Caller::Admin => None,
        };
        let path = req.uri().path().to_string();
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let app = match segs[..] {
            ["v1", "apps", name, ..] if valid_name(name) => Some(name.to_string()),
            _ => None,
        };
        let query = req.uri().query().unwrap_or("");
        if let Some(r) = self.authorize(&caller, req.method(), &segs, query) {
            return (app, token_id, r);
        }
        (app, token_id, self.admin_route(&path, req, caller).await)
    }

    async fn admin_route(
        self: Arc<Self>,
        path: &str,
        req: Request<Incoming>,
        caller: Caller,
    ) -> Resp {
        let query = req.uri().query().unwrap_or("").to_string();
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (req.method().clone(), segs.as_slice()) {
            (Method::GET, ["metrics"]) => Response::builder()
                .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                .body(Full::new(Bytes::from(self.metrics_text())))
                .unwrap(),
            (Method::GET, ["v1", "apps"]) => {
                let mut v: Vec<_> = self.apps.read().unwrap().values().cloned().collect();
                if let Caller::App(p) = &caller {
                    v.retain(|a| a.name == p.app);
                }
                v.sort_by(|a, b| a.name.cmp(&b.name));
                json_resp(
                    StatusCode::OK,
                    serde_json::json!({"apps": self.apps_json(v, matches!(caller, Caller::Admin)).await}),
                )
            }
            (Method::PUT, ["v1", "apps", name]) => {
                let creator = match &caller {
                    Caller::Admin => Creator::Admin,
                    Caller::App(p) if p.may_create => Creator::Token(p.id.clone()),
                    Caller::App(p) => Creator::No(p.id.clone()),
                };
                self.deploy(name, req, creator).await
            }
            (Method::POST, ["v1", "tokens"]) => self.create_token(req).await,
            (Method::GET, ["v1", "tokens"]) => self.list_tokens(query_param(Some(&query), "app")),
            (Method::DELETE, ["v1", "tokens", id]) => self.revoke_token(id),
            (Method::GET, ["v1", "apps", name]) => {
                let app = self.apps.read().unwrap().get(*name).cloned();
                match app {
                    Some(a) => {
                        let admin = matches!(caller, Caller::Admin);
                        let v = self.apps_json(vec![a], admin).await.pop();
                        json_resp(StatusCode::OK, v.unwrap_or_default())
                    }
                    None => no_such_app(name),
                }
            }
            (Method::DELETE, ["v1", "apps", name]) => {
                let name = name.to_string();
                detached(async move { self.delete(&name).await }).await
            }
            (Method::POST, ["v1", "apps", name, "evict"]) => {
                let name = name.to_string();
                detached(async move { self.evict(&name).await }).await
            }
            (Method::POST, ["v1", "apps", name, "backup"]) => {
                let name = name.to_string();
                detached(async move { self.backup(&name).await }).await
            }
            (Method::GET, ["v1", "apps", name, "backups"]) => self.list_backups(name).await,
            (Method::DELETE, ["v1", "apps", name, "backups", id]) => {
                let (name, id) = (name.to_string(), id.to_string());
                detached(async move { self.delete_backup(&name, &id).await }).await
            }
            (Method::POST, ["v1", "apps", name, "restore"]) => self.restore(name, req).await,
            (Method::GET, ["v1", "apps", name, "backups", id, "remote"]) => {
                self.upload_status(name, id).await
            }
            (Method::GET, ["v1", "apps", name, "remote-backups"]) => self.list_remote(name).await,
            (Method::POST, ["v1", "apps", name, "freeze"]) => {
                let name = name.to_string();
                detached(async move { self.freeze_route(&name, req).await }).await
            }
            (Method::POST, ["v1", "apps", name, "thaw"]) => {
                let name = name.to_string();
                detached(async move { self.thaw_route(&name, req).await }).await
            }
            (Method::GET, ["v1", "apps", name, "code"]) => self.export_code(name).await,
            (Method::GET, ["v1", "apps", name, "limits"]) => self.show_limits(name).await,
            (Method::PUT, ["v1", "apps", name, "limits"]) => {
                let name = name.to_string();
                detached(async move { self.set_limits(&name, req).await }).await
            }
            (Method::DELETE, ["v1", "apps", name, "limits"]) => {
                let name = name.to_string();
                detached(async move { self.clear_limits(&name).await }).await
            }
            (Method::GET, ["v1", "server"]) => self.server_info(),
            (Method::POST, ["v1", "apps", name, "fetch"]) => self.fetch(name, req).await,
            (Method::GET, ["v1", "apps", name, "fetch", rid]) => self.fetch_status(name, rid).await,
            (Method::GET, ["v1", "apps", name, "logs"]) => {
                let n: usize = query
                    .split('&')
                    .find_map(|p| p.strip_prefix("lines="))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(50);
                let dir = self.apps.read().unwrap().get(*name).map(|a| a.dir.clone());
                match dir {
                    Some(d) => {
                        let text = read_tail(&d.join("app.log"), LOG_TAIL_BYTES);
                        let lines: Vec<&str> = text.lines().collect();
                        let tail = lines[lines.len().saturating_sub(n)..].join("\n");
                        Response::builder()
                            .header("content-type", "text/plain; charset=utf-8")
                            .body(Full::new(Bytes::from(tail + "\n")))
                            .unwrap()
                    }
                    None => no_such_app(name),
                }
            }
            _ => err_resp(
                StatusCode::NOT_FOUND,
                "no_route",
                format!("no admin route {path}"),
                "the admin API lives under /v1/ (for example GET /v1/apps); see AGENTS.md",
            ),
        }
    }

    /// Answer one request on either listener, count it, and write its access-log line. The line
    /// holds the path without its query string, and no header or body.
    async fn handle(
        self: Arc<Self>,
        listener: Listener,
        req: Request<Incoming>,
        peer: IpAddr,
    ) -> Resp {
        let (started, time) = (Instant::now(), SystemTime::now());
        let method: String = req.method().as_str().chars().take(16).collect();
        let path = req.uri().path().to_string();
        let id = self.ids.next();
        let (app, token_id, mut resp) = match listener {
            Listener::Public => {
                let (app, resp) = self.clone().handle_public(req).await;
                (app, None, resp)
            }
            Listener::Admin => self.clone().handle_admin(req, peer).await,
        };
        let error = resp.extensions().get::<ErrorKind>().copied();
        let status = resp.status().as_u16();
        self.metrics.request(listener, status, error);
        if let Ok(v) = hyper::header::HeaderValue::from_str(&id) {
            resp.headers_mut().insert(REQUEST_ID_HEADER, v);
        }
        if self.cfg.access_log {
            AccessLine {
                time,
                listener,
                app: app.as_deref(),
                method: &method,
                path: &path,
                status,
                duration: started.elapsed(),
                id: &id,
                error,
                token_id: token_id.as_deref(),
            }
            .write();
        }
        resp
    }
}

/// How long a shutdown waits for requests in flight (and for instances to be released) before it
/// gives up on them. Below the 5 s a Fly machine gets by default after its stop signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(4);

/// The server's shutdown signal: a future that may be asked "already?" without waiting, any number
/// of times, also after it has resolved.
struct Shutdown {
    fut: Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    done: bool,
}

impl Shutdown {
    fn new(fut: impl std::future::Future<Output = ()> + Send + 'static) -> Self {
        Shutdown {
            fut: Box::pin(fut),
            done: false,
        }
    }

    fn requested(&mut self) -> bool {
        if !self.done {
            let mut cx = Context::from_waker(std::task::Waker::noop());
            self.done = self.fut.as_mut().poll(&mut cx).is_ready();
        }
        self.done
    }

    async fn wait(&mut self) {
        if !self.done {
            self.fut.as_mut().await;
            self.done = true;
        }
    }
}

/// Resolves once the stop flag is set (or its sender is gone).
async fn stop_requested(rx: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// How long an accept loop sleeps after an accept error that is not the client's doing (no
/// descriptors left, no memory): the listening socket stays readable, so retrying at once would
/// spin a core until something frees up.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// An accept error is logged at most this often per listener.
const ACCEPT_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// A failed accept that says nothing about the server: the peer reset or abandoned the connection
/// before it was accepted. Retry at once and say nothing.
fn accept_error_is_the_peers(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted};
    matches!(e.kind(), ConnectionAborted | ConnectionReset | Interrupted)
}

/// Whether an accept failed because the process or the system has no descriptors left.
fn out_of_descriptors(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

/// One descriptor held back for the admin listener. When the process runs out of descriptors the
/// admin accept loop gives it up, accepts the connection waiting there and serves it, so an
/// operator can still reach the server to find and remove whatever is using them all.
struct SpareFd(Mutex<Option<std::fs::File>>);

impl SpareFd {
    fn new() -> Self {
        SpareFd(Mutex::new(std::fs::File::open("/dev/null").ok()))
    }

    /// Close the spare descriptor; false when there is none to give up.
    fn release(&self) -> bool {
        self.0.lock().unwrap().take().is_some()
    }

    /// Hold a spare again if there is none and a descriptor can be had.
    fn refill(&self) {
        let mut slot = self.0.lock().unwrap();
        if slot.is_none() {
            *slot = std::fs::File::open("/dev/null").ok();
        }
    }
}

#[derive(Default)]
struct AcceptBackoff {
    last_logged: Option<Instant>,
    suppressed: u64,
}

impl AcceptBackoff {
    /// Count a failure and log it unless one was logged within `ACCEPT_LOG_INTERVAL`.
    fn failed(&mut self, e: &std::io::Error, is_admin: bool) {
        if self
            .last_logged
            .is_some_and(|t| t.elapsed() < ACCEPT_LOG_INTERVAL)
        {
            self.suppressed += 1;
            return;
        }
        let which = if is_admin { "admin" } else { "public" };
        let more = if self.suppressed > 0 {
            format!(" ({} more since the last line)", self.suppressed)
        } else {
            String::new()
        };
        eprintln!(
            "accept on the {which} listener failed: {e}{more}; retrying every {} ms, and logging this at most every {} s",
            ACCEPT_BACKOFF.as_millis(),
            ACCEPT_LOG_INTERVAL.as_secs()
        );
        self.last_logged = Some(Instant::now());
        self.suppressed = 0;
    }
}

/// Raise the soft limit on open files to the hard limit (a connection, a log file, an app's
/// data handles and every outbound request each cost one). Returns the soft limit before and
/// after (`rlim_t` is `u64` on the 64-bit Linux targets this builds for). A hard limit above what the kernel allows (`fs.nr_open`) is retried at lower values.
fn raise_nofile_limit() -> (u64, u64) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return (0, 0);
    }
    let before = lim.rlim_cur;
    let hard = lim.rlim_max;
    if before >= hard {
        return (before, before);
    }
    for want in [hard, 1 << 20, 1 << 16, 1 << 12] {
        if want <= before || want > hard {
            continue;
        }
        let raised = libc::rlimit {
            rlim_cur: want,
            rlim_max: lim.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            return (before, want);
        }
    }
    (before, before)
}

/// The two sockets a server listens on, bound before anything is written to the data dir.
pub struct Listeners {
    public: tokio::net::TcpListener,
    admin: tokio::net::TcpListener,
}

fn bind_one(
    addr: SocketAddr,
    flag: &str,
) -> std::result::Result<tokio::net::TcpListener, datadir::DataDirError> {
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()
    } else {
        tokio::net::TcpSocket::new_v6()
    };
    let bound = socket.and_then(|s| {
        s.set_reuseaddr(true)?;
        s.bind(addr)?;
        s.listen(1024)
    });
    bound.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            datadir::DataDirError {
                kind: "address_in_use",
                message: format!("{addr} is already in use"),
                fix: format!(
                    "stop the other `spared serve` or `spared dev` (or whatever holds the port), or pass {flag} ADDR:PORT to `spared serve` to listen elsewhere"
                ),
            }
        } else {
            datadir::DataDirError {
                kind: "bind_failed",
                message: format!("cannot listen on {addr}: {e}"),
                fix: format!("pass a different {flag} ADDR:PORT, one that is an address of this machine"),
            }
        }
    })
}

/// Bind the public and admin listeners. Nothing else has been touched when this fails, so a
/// server that cannot have its ports leaves no token file or data dir behind.
pub fn bind_listeners(
    listen: SocketAddr,
    admin: SocketAddr,
) -> std::result::Result<Listeners, datadir::DataDirError> {
    Ok(Listeners {
        public: bind_one(listen, "--listen")?,
        admin: bind_one(admin, "--admin-listen")?,
    })
}

/// The exclusive lock on `<data-dir>/.lock`, held until the process exits. Two servers on one
/// data dir would run two instances of each app on the same SQLite file.
pub struct DataLock {
    _file: Option<std::fs::File>,
}

/// Lock the data dir for this process, creating the directory and the lock file when they are
/// not there. The file holds the pid of the holder, written once the lock is ours; a failed
/// attempt reads it and never changes it. A file system that does not support locks gets a
/// warning, not a refusal.
pub fn lock_data_dir(data_dir: &Path) -> std::result::Result<DataLock, datadir::DataDirError> {
    let fail = |what: &str, e: std::io::Error| datadir::DataDirError {
        kind: "io",
        message: format!("cannot {what} in {}: {e}", data_dir.display()),
        fix: "make the data dir writable by the server's user".into(),
    };
    create_private_dir(data_dir).map_err(|e| fail("create the data dir", e))?;
    if std::fs::metadata(data_dir)
        .map_err(|e| fail("read the data dir", e))?
        .uid()
        != unsafe { libc::geteuid() }
    {
        eprintln!(
            "warning: {} is owned by another user, so it is not locked; two servers on one data dir would not be stopped",
            data_dir.display()
        );
        return Ok(DataLock { _file: None });
    }
    let path = data_dir.join(DATA_LOCK);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| fail("open the lock file", e))?;
    if unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::WouldBlock {
            eprintln!(
                "warning: cannot lock {} ({e}); two servers on one data dir would not be stopped",
                path.display()
            );
            return Ok(DataLock { _file: None });
        }
        let mut held = String::new();
        let _ = file.read_to_string(&mut held);
        let who = held
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<u32>().ok())
            .map(|pid| format!("process {pid}"))
            .unwrap_or_else(|| "another process".into());
        return Err(datadir::DataDirError {
            kind: "data_dir_in_use",
            message: format!(
                "the data dir {} is in use by {who}: another spared serve or dev has it locked ({})",
                data_dir.display(),
                path.display()
            ),
            fix: "stop that server first, or give this one its own --data-dir; leave the lock file alone, it is released when the other server exits".into(),
        });
    }
    let _ = file.set_len(0);
    let _ = file.write_all(format!("{}\n", std::process::id()).as_bytes());
    Ok(DataLock { _file: Some(file) })
}

/// Run the server until `shutdown` resolves, then stop gracefully: stop accepting connections,
/// let requests in flight finish (up to `SHUTDOWN_GRACE`), stop every instance, flush the log.
pub async fn serve(
    cfg: Config,
    engine: Engine,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    listeners: Listeners,
    _data_lock: DataLock,
) -> Result<()> {
    let mut shutdown = Shutdown::new(shutdown);
    // Everything the server and its apps create (data, logs, databases) is private to its user.
    unsafe { libc::umask(0o077) };
    crate::sandbox::start_epoch_ticker(&engine);
    let (listen, admin, idle) = (cfg.listen, cfg.admin, cfg.idle);
    let (domain, examples) = (cfg.domain.clone(), cfg.examples_dir.clone());
    let (path_routing, timeout, max_conns) =
        (cfg.path_routing, cfg.client_timeout, cfg.max_connections);
    let (access_log, data_dir) = (cfg.access_log, cfg.data_dir.clone());
    eprintln!("spared {} starting", crate::VERSION_LONG);
    let (nofile_before, nofile) = raise_nofile_limit();
    if nofile_before != nofile {
        eprintln!("open files: limit {nofile} (was {nofile_before})");
    } else {
        eprintln!("open files: limit {nofile}");
    }
    let wanted = (max_conns + ADMIN_CONNECTIONS) as u64 + 64;
    if nofile < wanted {
        eprintln!(
            "warning: the open-files limit ({nofile}) is below what {max_conns} connections need (about {wanted}); when it runs out the server stops accepting until descriptors free up. Fix: raise the limit (ulimit -n, LimitNOFILE=) or lower --max-connections"
        );
    }
    let server = Server::new(cfg, engine)?;
    if shutdown.requested() {
        eprintln!("shutdown requested while starting; nothing was served");
        return Ok(());
    }
    if path_routing {
        eprintln!("warning: {PATH_ROUTING_WARNING}");
    }
    let loaded = server.apps.read().unwrap().len();
    let examples_note = match examples {
        Some(dir) => {
            let (deployed, present) = server
                .deploy_examples(&dir, &mut || shutdown.requested())
                .await;
            if shutdown.requested() {
                eprintln!("shutdown requested while deploying examples; nothing was served");
                return Ok(());
            }
            format!(
                "; examples from {}: {deployed} deployed now, {present} already there",
                dir.display()
            )
        }
        None => String::new(),
    };
    {
        let s = server.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let apps: Vec<_> = s.apps.read().unwrap().values().cloned().collect();
                for a in apps {
                    if !a.hot.load(Ordering::Relaxed) {
                        continue;
                    }
                    let idle_for = s.now_ms().saturating_sub(a.last_ms.load(Ordering::Relaxed));
                    if idle_for >= idle.as_millis() as u64 {
                        if let Ok(mut g) = a.live.try_lock() {
                            if g.take().is_some() {
                                s.metrics.eviction(Evicted::Idle);
                            }
                            a.hot.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
    }
    let Listeners {
        public,
        admin: adminl,
    } = listeners;
    let also = domain.map(|d| format!(", <app>.{d}")).unwrap_or_default();
    let path = if path_routing { ", or /_/<app>/" } else { "" };
    let shown_dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir);
    // The admin token is never printed, here or anywhere else.
    let format = datadir::check(&shown_dir)
        .ok()
        .flatten()
        .map_or(datadir::BASE_FORMAT, |s| s.format);
    eprintln!(
        "data dir: {} (format {format}), {loaded} apps loaded{examples_note}",
        shown_dir.display(),
    );
    eprintln!(
        "apps:  http://{listen} (hosts <app>.localhost{also}{path}; health check: GET /healthz)"
    );
    eprintln!("admin: http://{admin} (needs the bearer token; metrics: GET /metrics)");
    if let Some(r) = &server.remote {
        eprintln!("{}", r.describe());
    }
    eprintln!(
        "access log: {}",
        if access_log {
            "on, one JSON line per request on stdout (--access-log off to stop it)"
        } else {
            "off"
        }
    );
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let spare = Arc::new(SpareFd::new());
    let mut gates = vec![];
    for (l, is_admin) in [(public, false), (adminl, true)] {
        let server = server.clone();
        let cap = if is_admin {
            ADMIN_CONNECTIONS
        } else {
            max_conns
        }
        .max(1);
        let slots = Arc::new(tokio::sync::Semaphore::new(cap));
        gates.push((slots.clone(), cap));
        let stop = stop_rx.clone();
        let spare = spare.clone();
        tokio::spawn(async move {
            let mut backoff = AcceptBackoff::default();
            loop {
                let mut stopping = stop.clone();
                if is_admin {
                    spare.refill();
                }
                let slot = tokio::select! {
                    s = slots.clone().acquire_owned() => match s {
                        Ok(s) => s,
                        Err(_) => return,
                    },
                    _ = stop_requested(&mut stopping) => return,
                };
                let mut accepted = tokio::select! {
                    a = l.accept() => a,
                    _ = stop_requested(&mut stopping) => return,
                };
                if is_admin && accepted.as_ref().is_err_and(out_of_descriptors) && spare.release() {
                    accepted = tokio::select! {
                        a = l.accept() => a,
                        _ = stop_requested(&mut stopping) => return,
                    };
                }
                let (stream, peer) = match accepted {
                    Ok((stream, addr)) => (stream, addr.ip()),
                    Err(e) => {
                        if !accept_error_is_the_peers(&e) {
                            backoff.failed(&e, is_admin);
                            tokio::select! {
                                _ = tokio::time::sleep(ACCEPT_BACKOFF) => {}
                                _ = stop_requested(&mut stopping) => return,
                            }
                        }
                        continue;
                    }
                };
                let server = server.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let listener = if is_admin {
                        Listener::Admin
                    } else {
                        Listener::Public
                    };
                    let svc = service_fn(move |req| {
                        let s = server.clone();
                        async move {
                            Ok::<_, std::convert::Infallible>(s.handle(listener, req, peer).await)
                        }
                    });
                    let io = TokioIo::new(WriteDeadline::new(stream, timeout));
                    let conn = http1::Builder::new()
                        .timer(TokioTimer)
                        .header_read_timeout(timeout)
                        .keep_alive(true)
                        .serve_connection(io, svc);
                    tokio::pin!(conn);
                    let lifetime = tokio::time::sleep(connection_lifetime(timeout));
                    tokio::pin!(lifetime);
                    let mut closing = false;
                    loop {
                        tokio::select! {
                            _ = conn.as_mut() => return,
                            _ = &mut lifetime, if !is_admin && !closing => {
                                conn.as_mut().graceful_shutdown();
                                let _ = tokio::time::timeout(timeout, conn).await;
                                return;
                            }
                            _ = stop_requested(&mut stopping), if !closing => {
                                conn.as_mut().graceful_shutdown();
                                closing = true;
                            }
                        }
                    }
                });
            }
        });
    }
    let prefetch = server.start_prefetch();
    shutdown.wait().await;
    if let Some(p) = &prefetch {
        p.cancel();
    }
    eprintln!(
        "shutting down: no new connections; requests in flight get up to {} s",
        SHUTDOWN_GRACE.as_secs()
    );
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    let _ = stop_tx.send(true);
    let mut abandoned = 0;
    for (slots, cap) in &gates {
        match tokio::time::timeout_at(deadline, slots.acquire_many(*cap as u32)).await {
            Ok(_) => {}
            Err(_) => abandoned += cap - slots.available_permits(),
        }
    }
    let apps: Vec<_> = server.apps.read().unwrap().values().cloned().collect();
    let mut stopped = 0;
    for a in apps {
        if let Ok(mut g) = tokio::time::timeout_at(deadline, a.live.lock()).await {
            if g.take().is_some() {
                stopped += 1;
            }
            a.hot.store(false, Ordering::Relaxed);
        }
    }
    {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    eprintln!(
        "stopped: {stopped} instances released, {abandoned} connections cut after the grace period"
    );
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_app_answers_503_with_retry_after() {
        let r = app_busy("slow", "has too many requests waiting");
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers()["retry-after"], "1");
        assert_eq!(
            r.extensions().get::<ErrorKind>().map(|k| k.0),
            Some("app_busy")
        );
    }

    #[test]
    fn a_frozen_app_answers_503_with_retry_after_and_no_reason() {
        let r = app_frozen("notes");
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers()["retry-after"], FROZEN_RETRY_AFTER);
        assert_eq!(
            r.extensions().get::<ErrorKind>().map(|k| k.0),
            Some("app_frozen")
        );
    }

    #[test]
    fn the_freeze_marker_survives_and_a_damaged_one_still_freezes() {
        let dir = std::env::temp_dir().join(format!("spared-frozen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_freeze_marker(&dir), None);
        let info = serde_json::json!({"kind": "manual", "reason": "r", "at": "t"});
        write_freeze_marker(&dir, &info).unwrap();
        assert_eq!(read_freeze_marker(&dir), Some(info));
        std::fs::write(dir.join(FROZEN_MARKER), b"\xff not json").unwrap();
        assert_eq!(read_freeze_marker(&dir).unwrap()["kind"], "unknown");
        remove_freeze_marker(&dir).unwrap();
        remove_freeze_marker(&dir).unwrap();
        assert_eq!(read_freeze_marker(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn query_params() {
        assert_eq!(query_param(Some("frozen=move"), "frozen"), Some("move"));
        assert_eq!(query_param(Some("a=1&frozen=x"), "frozen"), Some("x"));
        assert_eq!(query_param(Some("unfrozen=move"), "frozen"), None);
        assert_eq!(query_param(Some("frozen"), "frozen"), None);
        assert_eq!(query_param(None, "frozen"), None);
    }

    #[test]
    fn host_routing() {
        assert_eq!(
            host_app("notes.localhost:8080", None).as_deref(),
            Some("notes")
        );
        assert_eq!(host_app("notes.example.test", None), None);
        assert_eq!(
            host_app("Notes.Example.Test:443", Some("example.test")).as_deref(),
            Some("notes")
        );
        assert_eq!(host_app("example.test", Some("example.test")), None);
        assert_eq!(host_app("notes.other.test", Some("example.test")), None);
        assert_eq!(
            host_app("notes.localhost", Some("example.test")).as_deref(),
            Some("notes")
        );
    }

    #[test]
    fn accept_errors_back_off_and_are_logged_once_per_interval() {
        let full = std::io::Error::from_raw_os_error(libc::EMFILE);
        assert!(!accept_error_is_the_peers(&full));
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(accept_error_is_the_peers(&reset));
        let mut b = AcceptBackoff::default();
        b.failed(&full, false);
        assert!(b.last_logged.is_some() && b.suppressed == 0);
        for _ in 0..50 {
            b.failed(&full, false);
        }
        assert_eq!(b.suppressed, 50);
    }

    #[test]
    fn path_prefix() {
        assert_eq!(split_prefix("/_/notes/api/x"), Some(("notes", "/api/x")));
        assert_eq!(split_prefix("/_/notes/"), Some(("notes", "/")));
        assert_eq!(split_prefix("/_/notes"), Some(("notes", "")));
        assert_eq!(split_prefix("/notes/"), None);
        assert_eq!(split_prefix("/_notes/"), None);
    }

    #[test]
    fn admin_bind_guard() {
        let lo: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        let lo6: SocketAddr = "[::1]:8081".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:8081".parse().unwrap();
        assert!(check_admin_bind(lo, false).is_ok());
        assert!(check_admin_bind(lo6, false).is_ok());
        let e = check_admin_bind(any, false).unwrap_err();
        assert_eq!(e.kind, "admin_bind_refused");
        assert!(e.fix.contains("--admin-public-i-know"), "{}", e.fix);
        assert!(check_admin_bind(any, true).is_ok());
    }

    #[test]
    fn admin_hosts() {
        for ok in [
            "127.0.0.1:8081",
            "127.0.0.1",
            "localhost:8081",
            "LOCALHOST",
            "[::1]:8081",
            "[::1]",
            "10.0.0.5:9000",
        ] {
            assert!(admin_host_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "attacker.example:8081",
            "evil.attacker.example",
            "localhost.attacker.example",
            "127.0.0.1.nip.io:8081",
            "localhost:x",
            "[::1",
            "[evil]:8081",
            "2130706433:8081",
        ] {
            assert!(!admin_host_ok(bad), "{bad}");
        }
    }

    #[test]
    fn token_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn token_file_created_private() {
        let dir = std::env::temp_dir().join(format!("spared-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let t = admin_token(&dir, None).unwrap();
        assert_eq!(t.len(), 64);
        let mode = std::fs::metadata(dir.join("admin-token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(admin_token(&dir, None).unwrap(), t);
        let long = "given-token-0123456789";
        assert_eq!(admin_token(&dir, Some(long.into())).unwrap(), long);
        let e = admin_token(&dir, Some("short".into()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("at least 16"), "{e}");
        let f = dir.join("admin-token");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = admin_token(&dir, None).unwrap_err().to_string();
        assert!(e.contains("mode 644") && e.contains("chmod 600"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_tail_is_lossy() {
        let p = std::env::temp_dir().join(format!("spared-log-{}", std::process::id()));
        std::fs::write(&p, b"panicked at main.rs\n\xff\xfe\n").unwrap();
        let t = read_tail(&p, 1 << 20);
        assert!(t.contains("panicked at main.rs"), "{t}");
        assert!(t.contains('\u{fffd}'), "{t}");
        assert_eq!(read_tail(&p, 4), "\n\u{fffd}\u{fffd}\n");
        std::fs::remove_file(&p).unwrap();
    }
}
