//! A minimal S3 client for remote backups: AWS Signature Version 4 over the hyper, rustls and ring
//! stack the binary already has, for exactly PUT, GET, HEAD and ListObjectsV2. It has no DELETE:
//! spared never removes an object from the bucket.
//!
//! What it never does: follow a redirect (a 3xx is an error that names the bucket's region when
//! the store says it), send `UNSIGNED-PAYLOAD` (every request signs its body's SHA-256), go through
//! a proxy, or put the `Authorization` header or a secret into a log line or an error. Errors carry
//! the store's `Code`, `Message` and request id only. Response bodies are capped: XML at 1 MiB,
//! error bodies at 64 KiB. One connection per request, its address resolved once.

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wasmtime_wasi_http::io::TokioIo;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A request that makes no progress (no body bytes sent or received, no answer) for this long
/// is given up.
const STALL: Duration = Duration::from_secs(60);
const MAX_XML: usize = 1 << 20;
const MAX_ERROR_BODY: usize = 64 << 10;
const ATTEMPTS: u32 = 3;
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A secret string (the secret access key, the session token). `Debug` prints `***`.
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Secret {
        Secret(s)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

#[derive(Debug)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: Secret,
    pub session_token: Option<Secret>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Addressing {
    /// `https://<endpoint>/<bucket>/<key>`
    Path,
    /// `https://<bucket>.<endpoint>/<key>`
    Virtual,
}

/// Where the store is: scheme, host and port, checked by [`parse_endpoint`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub https: bool,
    /// A DNS name, an IPv4 address, or an IPv6 address without brackets.
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn aws(region: &str) -> Endpoint {
        Endpoint {
            https: true,
            host: format!("s3.{region}.amazonaws.com"),
            port: 443,
        }
    }

    fn host_header(&self, host: &str) -> String {
        let h = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        if self.port == if self.https { 443 } else { 80 } {
            h
        } else {
            format!("{h}:{}", self.port)
        }
    }

    pub fn url(&self) -> String {
        format!(
            "{}://{}",
            if self.https { "https" } else { "http" },
            self.host_header(&self.host)
        )
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Check an operator's `--backup-endpoint`: https (http only for a loopback host, or with
/// `allow_http`), a host and an optional port, and nothing else: no user name or password, no
/// path, query or fragment.
pub fn parse_endpoint(text: &str, allow_http: bool) -> Result<Endpoint, String> {
    let (https, rest) = if let Some(r) = text.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = text.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(
            "the endpoint must start with https:// (or http:// for a loopback address)".into(),
        );
    };
    if rest.contains('@') {
        return Err("the endpoint must not hold a user name or password (user@host)".into());
    }
    if rest.contains('?') || rest.contains('#') {
        return Err("the endpoint must not have a query or a fragment".into());
    }
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains('/') {
        return Err("the endpoint must not have a path: give the bucket in --backup-to".into());
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, p) = v6
            .split_once(']')
            .ok_or("an IPv6 endpoint needs its closing ]")?;
        if h.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(format!("'{h}' is not an IPv6 address"));
        }
        (h.to_string(), p)
    } else {
        match authority.find(':') {
            Some(i) => (authority[..i].to_string(), &authority[i..]),
            None => (authority.to_string(), ""),
        }
    };
    let port = match port.strip_prefix(':') {
        None if port.is_empty() => {
            if https {
                443
            } else {
                80
            }
        }
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|&p| p > 0)
            .ok_or_else(|| format!("'{p}' is not a port"))?,
        None => return Err("the endpoint is not host[:port]".into()),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':');
    if !host_ok {
        return Err(format!("'{host}' is not a host name or an IP address"));
    }
    if !https && !allow_http && !is_loopback(&host) {
        return Err(format!(
            "http:// sends the backup traffic unencrypted to {host}. Use https://, or pass --backup-allow-http for a store on a network you trust"
        ));
    }
    Ok(Endpoint {
        https,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// `s3://bucket/prefix` into the bucket name and the prefix (no leading or trailing `/`).
pub fn parse_target(text: &str) -> Result<(String, String), String> {
    let rest = text
        .strip_prefix("s3://")
        .ok_or("--backup-to must look like s3://bucket/prefix")?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    check_bucket(bucket)?;
    let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
    check_prefix(prefix)?;
    Ok((bucket.to_string(), prefix.to_string()))
}

/// The S3 bucket naming rules: 3 to 63 lowercase letters, digits, dots and dashes, starting and
/// ending with a letter or digit, no `..`, and not shaped like an IP address.
pub fn check_bucket(b: &str) -> Result<(), String> {
    let ok = (3..=63).contains(&b.len())
        && b.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
        && b.as_bytes()[0].is_ascii_alphanumeric()
        && b.as_bytes()[b.len() - 1].is_ascii_alphanumeric()
        && !b.contains("..")
        && !b.contains(".-")
        && !b.contains("-.")
        && b.parse::<std::net::Ipv4Addr>().is_err();
    if ok {
        Ok(())
    } else {
        Err(format!(
            "'{b}' is not a valid bucket name (3 to 63 lowercase letters, digits, dots and dashes, starting and ending with a letter or digit)"
        ))
    }
}

/// A prefix holds only `[a-z0-9._/-]`, has no empty, `.` or `..` part and does not start with
/// `/`; at most 512 characters.
pub fn check_prefix(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Ok(());
    }
    let chars = p.bytes().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'/' | b'-')
    });
    let parts = p.split('/').all(|s| !s.is_empty() && s != "." && s != "..");
    if chars && parts && p.len() <= 512 {
        Ok(())
    } else {
        Err(format!(
            "the prefix '{p}' may hold only a-z, 0-9, '.', '_', '-' and '/', with no empty, '.' or '..' part"
        ))
    }
}

/// Percent-encode everything but the unreserved characters (and `/` when `keep_slash`), as
/// SigV4 and S3 require.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> ring::hmac::Tag {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), data)
}

/// What a request is, for signing.
pub struct ToSign<'a> {
    pub method: &'a str,
    /// The path before encoding, `/bucket/key` or `/key`.
    pub path: &'a str,
    /// Query parameters before encoding.
    pub query: &'a [(&'a str, &'a str)],
    /// Every header to sign, the `host` header included; names in any case.
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_sha256: &'a str,
    pub region: &'a str,
    /// `20261112T040506Z`
    pub amz_date: &'a str,
}

pub fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    q.sort();
    q.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The canonical request, the signed-headers list and the `Authorization` header value.
pub fn sign(r: &ToSign, creds: &Credentials) -> (String, String, String) {
    let mut h: Vec<(String, String)> = r
        .headers
        .iter()
        .map(|(k, v)| {
            let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
            (k.to_ascii_lowercase(), v)
        })
        .collect();
    h.sort();
    let signed = h
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        r.method,
        uri_encode(r.path, true),
        canonical_query(r.query),
        h.iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect::<String>(),
        signed,
        r.payload_sha256
    );
    let date = &r.amz_date[..8];
    let scope = format!("{date}/{}/s3/aws4_request", r.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        r.amz_date,
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let k = hmac(
        format!("AWS4{}", creds.secret_access_key.expose()).as_bytes(),
        date.as_bytes(),
    );
    let k = hmac(k.as_ref(), r.region.as_bytes());
    let k = hmac(k.as_ref(), b"s3");
    let k = hmac(k.as_ref(), b"aws4_request");
    let sig = hex(hmac(k.as_ref(), to_sign.as_bytes()).as_ref());
    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}",
        creds.access_key_id
    );
    (canonical, signed, auth)
}

// ---------------------------------------------------------------- XML

/// Decode the five XML entities and character references. `None` for a malformed one.
fn xml_unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find(';').filter(|&e| e <= 10)?;
        let ent = &after[..end];
        let c = match ent {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let n = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()?
                } else {
                    ent.strip_prefix('#')?.parse::<u32>().ok()?
                };
                char::from_u32(n)?
            }
        };
        out.push(c);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// The raw contents of each `<tag>...</tag>` in `xml`, in order, not nested. Only the exact form
/// without attributes is matched, which is what S3 sends for the elements read here.
pub fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find(&close) else { break };
        out.push(&body[..j]);
        rest = &body[j + close.len()..];
    }
    out
}

/// The text of the first `<tag>` in `xml`, entity-decoded. `Some("")` for `<tag/>`.
pub fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let first = xml_blocks(xml, tag).into_iter().next();
    match first {
        Some(b) if !b.contains('<') => xml_unescape(b),
        Some(_) => None,
        None => xml.contains(&format!("<{tag}/>")).then(String::new),
    }
}

// ---------------------------------------------------------------- errors

/// A failed S3 operation, in the shape of every spared error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Error {
    pub kind: &'static str,
    pub message: String,
    pub fix: String,
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

pub const FIX_UNAVAILABLE: &str = "check that the store is up and reachable from this server (DNS, firewall, TLS certificates, SPARED_EXTRA_CA_FILE for a private CA), then take the backup again";

fn unavailable(what: impl std::fmt::Display) -> S3Error {
    S3Error {
        kind: "remote_unavailable",
        message: format!("the bucket could not be reached: {what}"),
        fix: FIX_UNAVAILABLE.into(),
    }
}

/// The store's error, from its status, its headers and its XML body.
fn store_error(status: u16, headers: &http::HeaderMap, body: &[u8]) -> S3Error {
    let xml = String::from_utf8_lossy(body);
    let code = xml_text(&xml, "Code").unwrap_or_default();
    let msg = xml_text(&xml, "Message").unwrap_or_default();
    let hdr = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(200).collect::<String>())
    };
    let request_id = xml_text(&xml, "RequestId").or_else(|| hdr("x-amz-request-id"));
    let clip = |s: &str| s.chars().take(300).collect::<String>();
    let what = format!(
        "the store answered {status}{}{}{}",
        if code.is_empty() {
            String::new()
        } else {
            format!(" {}", clip(&code))
        },
        if msg.is_empty() {
            String::new()
        } else {
            format!(": {}", clip(&msg))
        },
        request_id
            .map(|r| format!(" (request id {})", clip(&r)))
            .unwrap_or_default()
    );
    let region = hdr("x-amz-bucket-region").or_else(|| xml_text(&xml, "Region"));
    let (kind, fix): (&'static str, String) = if (300..400).contains(&status)
        || code == "PermanentRedirect"
        || (code == "AuthorizationHeaderMalformed" && region.is_some())
    {
        (
            "remote_wrong_region",
            match &region {
                Some(r) => format!(
                    "the bucket is in region {}: restart the server with --backup-region {}",
                    clip(r),
                    clip(r)
                ),
                None => "the store redirected the request, which spared never follows: check --backup-region and --backup-endpoint".into(),
            },
        )
    } else if code == "RequestTimeTooSkewed" {
        let store = hdr("date").unwrap_or_else(|| "unknown".into());
        let local = crate::backup::created_at(&crate::backup::timestamp_id(now_secs()));
        return S3Error {
            kind: "remote_clock_skew",
            message: format!(
                "{what}. The store's clock says {store}, this server's says {local}"
            ),
            fix: "set this server's clock right (NTP, `timedatectl set-ntp true`). Requests are refused when the clocks differ by more than 15 minutes".into(),
        };
    } else if status >= 500 {
        return unavailable(what);
    } else if status == 401
        || status == 403
        || matches!(
            code.as_str(),
            "InvalidAccessKeyId"
                | "SignatureDoesNotMatch"
                | "AccessDenied"
                | "ExpiredToken"
                | "InvalidToken"
        )
    {
        (
            "remote_auth_failed",
            "check AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (and AWS_SESSION_TOKEN) in the server's environment, and that the key may PutObject, GetObject and ListBucket on the prefix (docs/operating.md)".into(),
        )
    } else if code == "NoSuchBucket" {
        (
            "remote_no_bucket",
            "create the bucket, or fix the name in --backup-to".into(),
        )
    } else {
        (
            "remote_rejected",
            "read the store's message above. docs/operating.md lists what the bucket needs".into(),
        )
    };
    S3Error {
        kind,
        message: what,
        fix,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- bodies

/// A request body: nothing, bytes in memory, or a file read as it is sent. The file is read by a
/// thread of its own, a bounded queue ahead of the connection, so no blocking read runs on the
/// runtime's workers. The thread ends when the body is dropped.
enum ReqBody {
    Bytes(Option<Bytes>),
    File {
        rx: tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>,
        left: u64,
        progress: Arc<AtomicU64>,
        start: Instant,
    },
}

const FILE_CHUNK: u64 = 256 << 10;

/// Start reading `len` bytes of `file` from its start on a thread, in chunks, into a queue.
fn read_ahead(
    mut file: std::fs::File,
    len: u64,
) -> tokio::sync::mpsc::Receiver<std::io::Result<Bytes>> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    std::thread::spawn(move || {
        let mut left = len;
        while left > 0 {
            let mut buf = vec![0u8; left.min(FILE_CHUNK) as usize];
            let r = file.read_exact(&mut buf).map(|()| Bytes::from(buf));
            let failed = r.is_err();
            if tx.blocking_send(r).is_err() || failed {
                return;
            }
            left -= left.min(FILE_CHUNK);
        }
    });
    rx
}

impl Body for ReqBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        match self.get_mut() {
            ReqBody::Bytes(b) => Poll::Ready(b.take().map(|b| Ok(Frame::data(b)))),
            ReqBody::File {
                rx,
                left,
                progress,
                start,
            } => {
                if *left == 0 {
                    return Poll::Ready(None);
                }
                match rx.poll_recv(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(None) => Poll::Ready(Some(Err(std::io::Error::other(
                        "the archive file ended early",
                    )))),
                    Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
                    Poll::Ready(Some(Ok(b))) => {
                        *left -= b.len() as u64;
                        progress.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                        Poll::Ready(Some(Ok(Frame::data(b))))
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ReqBody::Bytes(b) => b.is_none(),
            ReqBody::File { left, .. } => *left == 0,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            ReqBody::Bytes(b) => {
                http_body::SizeHint::with_exact(b.as_ref().map_or(0, |b| b.len() as u64))
            }
            ReqBody::File { left, .. } => http_body::SizeHint::with_exact(*left),
        }
    }
}

/// What to send.
pub enum Payload<'a> {
    Empty,
    Bytes(Bytes),
    /// A file of `len` bytes whose SHA-256 is `sha256` (hex), sent from its start.
    File {
        path: &'a std::path::Path,
        len: u64,
        sha256: &'a str,
    },
}

trait Io: hyper::rt::Read + hyper::rt::Write + Send + Unpin {}
impl<T: hyper::rt::Read + hyper::rt::Write + Send + Unpin> Io for T {}

/// Aborts the connection task when the response is done with.
struct ConnGuard(tokio::task::JoinHandle<()>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct Response {
    pub status: u16,
    pub headers: http::HeaderMap,
    body: Incoming,
    _conn: ConnGuard,
}

impl Response {
    /// The whole body, refused past `cap` bytes.
    pub async fn bytes(mut self, cap: usize) -> Result<Bytes, S3Error> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            if out.len() + chunk.len() > cap {
                return Err(S3Error {
                    kind: "remote_rejected",
                    message: format!("the store sent a body over {cap} bytes"),
                    fix: "check --backup-endpoint: it does not answer like S3".into(),
                });
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out.into())
    }

    /// The next piece of the body, `None` at its end. Waiting longer than the stall limit for a
    /// piece fails.
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, S3Error> {
        loop {
            match tokio::time::timeout(STALL, self.body.frame()).await {
                Err(_) => return Err(unavailable("the store stopped sending")),
                Ok(None) => return Ok(None),
                Ok(Some(Err(e))) => return Err(unavailable(format!("reading the answer: {e}"))),
                Ok(Some(Ok(f))) => {
                    if let Ok(d) = f.into_data() {
                        return Ok(Some(d));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- the client

pub struct Client {
    pub endpoint: Endpoint,
    pub bucket: String,
    pub region: String,
    pub addressing: Addressing,
    creds: Credentials,
    tls: Option<tokio_rustls::TlsConnector>,
}

/// One object from a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
}

impl Client {
    pub fn new(
        endpoint: Endpoint,
        bucket: String,
        region: String,
        addressing: Addressing,
        creds: Credentials,
    ) -> std::io::Result<Client> {
        let tls = if endpoint.https {
            Some(tokio_rustls::TlsConnector::from(Arc::new(
                crate::pinned_send::tls_client_config()?,
            )))
        } else {
            None
        };
        Ok(Client {
            endpoint,
            bucket,
            region,
            addressing,
            creds,
            tls,
        })
    }

    /// The host to connect to and the path for `key` (`None`: the bucket itself).
    fn host_and_path(&self, key: Option<&str>) -> (String, String) {
        let key = key.unwrap_or("");
        match self.addressing {
            Addressing::Path if key.is_empty() => {
                (self.endpoint.host.clone(), format!("/{}", self.bucket))
            }
            Addressing::Path => (
                self.endpoint.host.clone(),
                format!("/{}/{key}", self.bucket),
            ),
            Addressing::Virtual => (
                format!("{}.{}", self.bucket, self.endpoint.host),
                format!("/{key}"),
            ),
        }
    }

    async fn connect(&self, host: &str) -> Result<Box<dyn Io>, S3Error> {
        let addr = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::lookup_host((host, self.endpoint.port)),
        )
        .await
        .map_err(|_| unavailable(format!("looking up {host} timed out")))?
        .map_err(|e| unavailable(format!("cannot resolve {host}: {e}")))?
        .next()
        .ok_or_else(|| unavailable(format!("{host} has no address")))?;
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr))
            .await
            .map_err(|_| unavailable(format!("connecting to {addr} timed out")))?
            .map_err(|e| unavailable(format!("cannot connect to {addr}: {e}")))?;
        let _ = tcp.set_nodelay(true);
        match &self.tls {
            None => Ok(Box::new(TokioIo::new(tcp))),
            Some(tls) => {
                let name = crate::pinned_send::tls_server_name(host)
                    .map_err(|e| unavailable(format!("'{host}' is not a TLS server name: {e}")))?;
                let s = tokio::time::timeout(CONNECT_TIMEOUT, tls.connect(name, tcp))
                    .await
                    .map_err(|_| unavailable(format!("the TLS handshake with {host} timed out")))?
                    .map_err(|e| unavailable(format!("TLS with {host} failed: {e}")))?;
                Ok(Box::new(TokioIo::new(s)))
            }
        }
    }

    /// Send one signed request and return the answer, whatever its status. Retries a request
    /// that could not be sent or got a 5xx, twice, with a pause.
    pub async fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        extra: &[(&str, &str)],
        payload: &Payload<'_>,
    ) -> Result<Response, S3Error> {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * 3u64.pow(attempt - 1))).await;
            }
            match self.send_once(method, key, query, extra, payload).await {
                Ok(r) if r.status >= 500 => {
                    let (status, headers) = (r.status, r.headers.clone());
                    let body = r.bytes(MAX_ERROR_BODY).await.unwrap_or_default();
                    last = Some(store_error(status, &headers, &body));
                }
                Ok(r) => return Ok(r),
                Err(e) if e.kind == "remote_unavailable" => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.expect("at least one attempt"))
    }

    async fn send_once(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        extra: &[(&str, &str)],
        payload: &Payload<'_>,
    ) -> Result<Response, S3Error> {
        let (host, path) = self.host_and_path(key);
        let host_header = self.endpoint.host_header(&host);
        let amz_date = crate::backup::timestamp_id(now_secs());
        let origin = Instant::now();
        let (sha, len, body) = match payload {
            Payload::Empty => (EMPTY_SHA256.to_string(), 0, ReqBody::Bytes(None)),
            Payload::Bytes(b) => (
                hex(&Sha256::digest(b)),
                b.len() as u64,
                ReqBody::Bytes(Some(b.clone())),
            ),
            Payload::File { path, len, sha256 } => {
                let file = std::fs::File::open(path)
                    .map_err(|e| unavailable(format!("cannot read the archive file: {e}")))?;
                (
                    sha256.to_string(),
                    *len,
                    ReqBody::File {
                        rx: read_ahead(file, *len),
                        left: *len,
                        progress: Arc::new(AtomicU64::new(0)),
                        start: origin,
                    },
                )
            }
        };
        let progress = match &body {
            ReqBody::File { progress, .. } => progress.clone(),
            ReqBody::Bytes(_) => Arc::new(AtomicU64::new(0)),
        };
        let token = self
            .creds
            .session_token
            .as_ref()
            .map(|t| t.expose().to_string());
        let mut signed: Vec<(&str, &str)> = vec![
            ("host", &host_header),
            ("x-amz-content-sha256", &sha),
            ("x-amz-date", &amz_date),
        ];
        if let Some(t) = &token {
            signed.push(("x-amz-security-token", t));
        }
        signed.extend_from_slice(extra);
        let (_, _, auth) = sign(
            &ToSign {
                method,
                path: &path,
                query,
                headers: &signed,
                payload_sha256: &sha,
                region: &self.region,
                amz_date: &amz_date,
            },
            &self.creds,
        );
        let q = canonical_query(query);
        let uri = if q.is_empty() {
            uri_encode(&path, true)
        } else {
            format!("{}?{q}", uri_encode(&path, true))
        };
        let mut b = hyper::Request::builder().method(method).uri(uri);
        for (k, v) in &signed {
            b = b.header(*k, *v);
        }
        b = b.header("authorization", auth);
        if !matches!(payload, Payload::Empty) || method == "PUT" {
            b = b.header("content-length", len.to_string());
        }
        let req = b
            .body(body)
            .map_err(|e| unavailable(format!("cannot build the request: {e}")))?;
        let io = self.connect(&host).await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| unavailable(format!("HTTP handshake: {e}")))?;
        let conn = ConnGuard(tokio::spawn(async move {
            let _ = conn.await;
        }));
        let watchdog = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let last = Duration::from_millis(progress.load(Ordering::Relaxed));
                if origin.elapsed().saturating_sub(last) > STALL {
                    return;
                }
            }
        };
        let resp = tokio::select! {
            r = sender.send_request(req) => r.map_err(|e| unavailable(format!("{method} failed: {e}")))?,
            _ = watchdog => return Err(unavailable(format!("{method} made no progress for {} s", STALL.as_secs()))),
        };
        let (parts, body) = resp.into_parts();
        Ok(Response {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body,
            _conn: conn,
        })
    }

    /// Turn a non-2xx answer into an error.
    async fn check(r: Response) -> Result<Response, S3Error> {
        if (200..300).contains(&r.status) {
            return Ok(r);
        }
        let (status, headers) = (r.status, r.headers.clone());
        let body = r.bytes(MAX_ERROR_BODY).await.unwrap_or_default();
        Err(store_error(status, &headers, &body))
    }

    /// `None` for a 404 `NoSuchKey`; a missing bucket is an error.
    async fn found(r: Response) -> Result<Option<Response>, S3Error> {
        if r.status == 404 {
            let headers = r.headers.clone();
            let body = r.bytes(MAX_ERROR_BODY).await.unwrap_or_default();
            let e = store_error(404, &headers, &body);
            return if e.kind == "remote_no_bucket" {
                Err(e)
            } else {
                Ok(None)
            };
        }
        Self::check(r).await.map(Some)
    }

    pub async fn put(
        &self,
        key: &str,
        payload: Payload<'_>,
        content_type: &str,
    ) -> Result<(), S3Error> {
        let r = self
            .send(
                "PUT",
                Some(key),
                &[],
                &[("content-type", content_type)],
                &payload,
            )
            .await?;
        Self::check(r).await.map(|_| ())
    }

    /// The object's whole body, `None` when there is no such key; refused past `cap` bytes.
    pub async fn get_small(&self, key: &str, cap: usize) -> Result<Option<Bytes>, S3Error> {
        let r = self
            .send("GET", Some(key), &[], &[], &Payload::Empty)
            .await?;
        match Self::found(r).await? {
            Some(r) => r.bytes(cap).await.map(Some),
            None => Ok(None),
        }
    }

    /// The object's answer to stream from, `None` when there is no such key.
    pub async fn get(&self, key: &str) -> Result<Option<Response>, S3Error> {
        let r = self
            .send("GET", Some(key), &[], &[], &Payload::Empty)
            .await?;
        Self::found(r).await
    }

    /// The object's size, `None` when there is no such key.
    pub async fn head(&self, key: &str) -> Result<Option<u64>, S3Error> {
        let r = self
            .send("HEAD", Some(key), &[], &[], &Payload::Empty)
            .await?;
        if r.status == 404 {
            return Ok(None);
        }
        let r = Self::check(r).await?;
        Ok(r.headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok()))
    }

    /// Every object under `prefix` (ListObjectsV2, page by page), up to `max` of them; the
    /// second value says whether there were more.
    pub async fn list(&self, prefix: &str, max: usize) -> Result<(Vec<Object>, bool), S3Error> {
        self.list_after(prefix, None, max).await
    }

    /// Like [`Client::list`], starting after the key `after` (ListObjectsV2 `start-after`), so
    /// a caller can skip to the keys that sort above it.
    pub async fn list_after(
        &self,
        prefix: &str,
        after: Option<&str>,
        max: usize,
    ) -> Result<(Vec<Object>, bool), S3Error> {
        let mut out = vec![];
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type", "2"), ("prefix", prefix)];
            if let (Some(a), None) = (after, &token) {
                q.push(("start-after", a));
            }
            if let Some(t) = &token {
                q.push(("continuation-token", t));
            }
            let r = self.send("GET", None, &q, &[], &Payload::Empty).await?;
            let body = Self::check(r).await?.bytes(MAX_XML).await?;
            let xml = String::from_utf8_lossy(&body);
            let bad = || S3Error {
                kind: "remote_rejected",
                message: "the store's listing is not a ListObjectsV2 answer".into(),
                fix: "check --backup-endpoint: it does not answer like S3".into(),
            };
            if !xml.contains("<ListBucketResult") {
                return Err(bad());
            }
            for c in xml_blocks(&xml, "Contents") {
                let key = xml_text(c, "Key").ok_or_else(bad)?;
                let size = xml_text(c, "Size")
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(bad)?;
                out.push(Object { key, size });
                if out.len() >= max {
                    return Ok((out, true));
                }
            }
            let truncated = xml_text(&xml, "IsTruncated").as_deref() == Some("true");
            token = xml_text(&xml, "NextContinuationToken").filter(|t| !t.is_empty());
            if !truncated {
                return Ok((out, false));
            }
            if token.is_none() {
                return Err(bad());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(token: Option<&str>) -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: Secret::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into()),
            session_token: token.map(|t| Secret::new(t.into())),
        }
    }

    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = vec![];
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// The committed vectors from botocore (scripts/sigv4-vectors.py). The first is also AWS's
    /// published "GET Object" example, whose signature is f0e8bdb8...
    #[test]
    fn signatures_match_the_botocore_vectors() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/data/sigv4-vectors.json")).unwrap();
        let cases = v["cases"].as_array().unwrap();
        assert!(cases.len() >= 6);
        assert_eq!(
            cases[0]["signature"],
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        for c in cases {
            let url = c["url"].as_str().unwrap();
            let after = url.split_once("://").unwrap().1;
            let path_q = &after[after.find('/').unwrap()..];
            let (path, q) = path_q.split_once('?').unwrap_or((path_q, ""));
            let path = percent_decode(path);
            let qd: Vec<(String, String)> = q
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                    (percent_decode(k), percent_decode(v))
                })
                .collect();
            let query: Vec<(&str, &str)> =
                qd.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let hs: Vec<(String, String)> = c["headers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| {
                    (
                        h[0].as_str().unwrap().to_string(),
                        h[1].as_str().unwrap().to_string(),
                    )
                })
                .filter(|(k, _)| {
                    c["signed_headers"]
                        .as_str()
                        .unwrap()
                        .split(';')
                        .any(|s| s == k.to_ascii_lowercase())
                })
                .collect();
            let host = after.split('/').next().unwrap().to_string();
            let mut headers: Vec<(&str, &str)> =
                hs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            headers.push(("host", &host));
            let (canonical, signed, auth) = sign(
                &ToSign {
                    method: c["method"].as_str().unwrap(),
                    path: &path,
                    query: &query,
                    headers: &headers,
                    payload_sha256: c["payload_sha256"].as_str().unwrap(),
                    region: c["region"].as_str().unwrap(),
                    amz_date: c["amz_date"].as_str().unwrap(),
                },
                &creds(c["session_token"].as_str()),
            );
            let name = c["name"].as_str().unwrap();
            assert_eq!(
                canonical,
                c["canonical_request"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(signed, c["signed_headers"].as_str().unwrap(), "{name}");
            assert_eq!(auth, c["authorization"].as_str().unwrap(), "{name}");
        }
    }

    #[test]
    fn secrets_print_as_stars() {
        let c = creds(Some("tok-secret"));
        let d = format!("{c:?}");
        assert!(!d.contains("wJalr") && !d.contains("tok-secret"), "{d}");
        assert!(d.contains("***"));
    }

    #[test]
    fn endpoints_are_checked() {
        let ok = |s: &str| parse_endpoint(s, false).unwrap();
        assert_eq!(
            ok("https://s3.example.com"),
            Endpoint {
                https: true,
                host: "s3.example.com".into(),
                port: 443
            }
        );
        assert_eq!(ok("https://S3.Example.com:8443/").port, 8443);
        assert_eq!(ok("http://127.0.0.1:9000").port, 9000);
        assert_eq!(ok("http://[::1]:9000").host, "::1");
        assert_eq!(ok("http://localhost:9000").host, "localhost");
        assert_eq!(ok("http://[::1]:9000").url(), "http://[::1]:9000");
        for bad in [
            "s3.example.com",
            "ftp://s3.example.com",
            "https://user:pass@s3.example.com",
            "https://user@s3.example.com",
            "https://s3.example.com/bucket",
            "https://s3.example.com?x=1",
            "https://s3.example.com#f",
            "https://",
            "https://:443",
            "https://s3.example.com:0",
            "https://s3.example.com:99999",
            "https://s3.exa mple.com",
            "https://[nope]:1",
            "http://10.0.0.5:9000",
            "http://s3.example.com",
        ] {
            assert!(parse_endpoint(bad, false).is_err(), "{bad}");
        }
        assert!(parse_endpoint("http://10.0.0.5:9000", true).is_ok());
    }

    #[test]
    fn targets_buckets_and_prefixes_are_checked() {
        assert_eq!(
            parse_target("s3://my-bucket/prod/spared/").unwrap(),
            ("my-bucket".into(), "prod/spared".into())
        );
        assert_eq!(
            parse_target("s3://my.bucket").unwrap(),
            ("my.bucket".into(), "".into())
        );
        for bad in [
            "my-bucket/x",
            "s3://",
            "s3://ab",
            "s3://Upper",
            "s3://-start",
            "s3://end-",
            "s3://a..b",
            "s3://192.168.1.1",
            "s3://bucket/../x",
            "s3://bucket//x",
            "s3://bucket/./x",
            "s3://bucket/Upper",
            "s3://bucket/sp ace",
            "s3://bucket/a%2f",
        ] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn xml_fields_and_entities() {
        let x = "<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>true</IsTruncated>\
                 <Contents><Key>a&amp;b&lt;c&#62;&#x41;</Key><Size>12</Size></Contents>\
                 <Contents><Key>z</Key><Size>3</Size></Contents>\
                 <NextContinuationToken>t/1==</NextContinuationToken><Empty/></ListBucketResult>";
        let c = xml_blocks(x, "Contents");
        assert_eq!(c.len(), 2);
        assert_eq!(xml_text(c[0], "Key").unwrap(), "a&b<c>A");
        assert_eq!(xml_text(c[1], "Size").unwrap(), "3");
        assert_eq!(xml_text(x, "IsTruncated").unwrap(), "true");
        assert_eq!(xml_text(x, "Empty").unwrap(), "");
        assert_eq!(xml_text(x, "Missing"), None);
        assert_eq!(xml_text("<K>a&bogus;</K>", "K"), None);
        assert_eq!(xml_text("<K>a&#xD800;</K>", "K"), None);
        assert_eq!(xml_text("<K>a&amp</K>", "K"), None);
        assert_eq!(xml_text("<K><X>1</X></K>", "K"), None);
        assert!(xml_blocks("<K>unclosed", "K").is_empty());
    }

    /// Random and mutated input never panics the extractor, and text it extracts never holds a
    /// tag. A fixed seed, so a failure reproduces.
    #[test]
    fn xml_extractor_survives_fuzzing() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: Vec<char> = "<>/&;#x0aKeyContentsSize \u{e9}\u{1F600}=\"'"
            .chars()
            .collect();
        let base = "<ListBucketResult><Contents><Key>a&amp;b</Key><Size>1</Size></Contents><IsTruncated>false</IsTruncated></ListBucketResult>";
        for round in 0..20_000 {
            let s: String = if round % 2 == 0 {
                (0..(next() % 80))
                    .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                    .collect()
            } else {
                let mut v: Vec<char> = base.chars().collect();
                for _ in 0..(1 + next() % 4) {
                    let i = (next() % v.len() as u64) as usize;
                    match next() % 3 {
                        0 => {
                            v.remove(i);
                        }
                        1 => v.insert(i, alphabet[(next() % alphabet.len() as u64) as usize]),
                        _ => v[i] = alphabet[(next() % alphabet.len() as u64) as usize],
                    }
                }
                v.into_iter().collect()
            };
            for tag in ["Key", "Size", "Contents", "IsTruncated", "K"] {
                for b in xml_blocks(&s, tag) {
                    if let Some(t) = xml_text(b, "Key") {
                        assert!(!t.contains("<Key>"), "{s:?}");
                    }
                }
                let _ = xml_text(&s, tag);
            }
        }
    }

    #[test]
    fn store_errors_map_to_kinds_without_secrets() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            let mut m = http::HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, v.parse().unwrap());
            }
            m
        };
        let err = |code: &str| {
            format!(
                "<Error><Code>{code}</Code><Message>m</Message><RequestId>r1</RequestId></Error>"
            )
        };
        let e = store_error(
            403,
            &h(&[("date", "Wed, 12 Nov 2026 05:00:00 GMT")]),
            err("RequestTimeTooSkewed").as_bytes(),
        );
        assert_eq!(e.kind, "remote_clock_skew");
        assert!(
            e.message.contains("05:00:00") && e.message.contains("request id r1"),
            "{}",
            e.message
        );
        let e = store_error(
            301,
            &h(&[("x-amz-bucket-region", "eu-west-1")]),
            err("PermanentRedirect").as_bytes(),
        );
        assert_eq!(e.kind, "remote_wrong_region");
        assert!(e.fix.contains("--backup-region eu-west-1"), "{}", e.fix);
        assert_eq!(store_error(302, &h(&[]), b"").kind, "remote_wrong_region");
        assert_eq!(
            store_error(403, &h(&[]), err("SignatureDoesNotMatch").as_bytes()).kind,
            "remote_auth_failed"
        );
        assert_eq!(
            store_error(404, &h(&[]), err("NoSuchBucket").as_bytes()).kind,
            "remote_no_bucket"
        );
        assert_eq!(
            store_error(503, &h(&[]), err("SlowDown").as_bytes()).kind,
            "remote_unavailable"
        );
        assert_eq!(
            store_error(412, &h(&[]), err("PreconditionFailed").as_bytes()).kind,
            "remote_rejected"
        );
        let e = store_error(
            400,
            &h(&[]),
            err("AuthorizationHeaderMalformed")
                .replace("</Error>", "<Region>ap-south-1</Region></Error>")
                .as_bytes(),
        );
        assert_eq!(e.kind, "remote_wrong_region");
    }
}
