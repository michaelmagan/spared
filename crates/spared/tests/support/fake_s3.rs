//! A small S3 server for the remote backup tests: path-style PUT, GET, HEAD and
//! ListObjectsV2 (two keys a page, so continuation tokens are exercised), If-None-Match on PUT,
//! and faults to inject: 5xx, connection resets, truncated bodies, 412, a skewed clock,
//! PermanentRedirect, and slow bodies.
//!
//! Every request's SigV4 signature is checked by [`signature`], written here from the AWS
//! documentation and independent of the client in `src/remote/s3.rs`; it is itself checked
//! against the committed botocore vectors. A request that fails the check gets 403
//! SignatureDoesNotMatch and is recorded in `State::auth_failures`.

#![allow(dead_code)]

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const BUCKET: &str = "spared-test";
pub const REGION: &str = "test-region-1";
pub const ACCESS_KEY: &str = "AKIDFAKES3TEST";
pub const SECRET_KEY: &str = "fake-s3-secret-dont-print-me-7d1c";
pub const SESSION_TOKEN: &str = "fake-session-token-never-logged-93ab";

#[derive(Default, Clone)]
pub struct Faults {
    /// Answer this many of the next requests with 503 SlowDown (`u32::MAX`: all of them).
    pub fail_5xx: u32,
    /// Close the connection, without an answer, partway through a PUT of an archive.
    pub reset_put: bool,
    /// Send only half of an archive on GET, then close the connection.
    pub truncate_get: bool,
    /// This many seconds added to the fake's clock.
    pub skew_secs: i64,
    /// Answer every request with 301 PermanentRedirect naming this region.
    pub redirect_region: Option<String>,
    /// Answer every PUT with 412 PreconditionFailed.
    pub put_412: bool,
    /// Answer every PUT of a `manifest.json` with 500 InternalError (the archive PUT succeeds).
    pub fail_manifest_put: bool,
    /// Wait this long after each body frame of an archive PUT.
    pub slow_put_ms: u64,
    /// Wait this long after each 64 KiB of an archive GET.
    pub slow_get_ms: u64,
}

#[derive(Default)]
pub struct State {
    /// `<key>` (without the bucket) to bytes.
    pub objects: BTreeMap<String, Vec<u8>>,
    pub faults: Faults,
    /// `METHOD /path?query` of every request that passed the signature check.
    pub log: Vec<String>,
    pub auth_failures: Vec<String>,
    /// Requests that sent `UNSIGNED-PAYLOAD` or a body that does not match its signed hash.
    pub payload_failures: Vec<String>,
    /// Keys a DELETE was sent for. spared never deletes, so this stays empty; the fake answers
    /// a DELETE with 403, as a bucket policy without s3:DeleteObject would.
    pub deletes: Vec<String>,
    /// An archive PUT has started sending its body.
    pub archive_put_started: bool,
    /// An archive GET has started sending its body.
    pub archive_get_started: bool,
    /// Region the signatures must use.
    pub region: String,
    pub secret: String,
    pub token: Option<String>,
}

pub struct Fake {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<State>>,
}

impl Fake {
    pub fn start() -> Fake {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            region: REGION.into(),
            secret: SECRET_KEY.into(),
            token: Some(SESSION_TOKEN.into()),
            ..Default::default()
        }));
        let st = state.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    let Ok((s, _)) = l.accept().await else {
                        continue;
                    };
                    let st = st.clone();
                    tokio::spawn(async move {
                        let svc = hyper::service::service_fn(move |req| handle(st.clone(), req));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(wasmtime_wasi_http::io::TokioIo::new(s), svc)
                            .await;
                    });
                }
            });
        });
        Fake { addr, state }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set(&self, f: impl FnOnce(&mut Faults)) {
        f(&mut self.state.lock().unwrap().faults);
    }

    pub fn clear_faults(&self) {
        self.state.lock().unwrap().faults = Faults::default();
    }

    pub fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().objects.keys().cloned().collect()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().objects.get(key).cloned()
    }

    pub fn put(&self, key: &str, data: Vec<u8>) {
        self.state.lock().unwrap().objects.insert(key.into(), data);
    }

    pub fn remove(&self, key: &str) {
        self.state.lock().unwrap().objects.remove(key);
    }
}

// ---------------------------------------------------------------- SigV4, from the AWS docs

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// RFC 3986 encoding as SigV4 wants it: unreserved characters stay, everything else is %XX.
fn encode(b: &[u8], slash: bool) -> String {
    b.iter()
        .map(|&c| {
            if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) || (slash && c == b'/') {
                (c as char).to_string()
            } else {
                format!("%{c:02X}")
            }
        })
        .collect()
}

pub fn query_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (
                String::from_utf8_lossy(&percent_decode(k)).into_owned(),
                String::from_utf8_lossy(&percent_decode(v)).into_owned(),
            )
        })
        .collect()
}

/// The signature a request should carry. `headers` are (lowercase name, value) as received.
#[allow(clippy::too_many_arguments)]
pub fn signature(
    method: &str,
    raw_path: &str,
    raw_query: &str,
    headers: &[(String, String)],
    signed: &str,
    payload: &str,
    amz_date: &str,
    region: &str,
    secret: &str,
) -> String {
    let uri = encode(&percent_decode(raw_path), true);
    let mut q: Vec<(String, String)> = query_pairs(raw_query)
        .into_iter()
        .map(|(k, v)| (encode(k.as_bytes(), false), encode(v.as_bytes(), false)))
        .collect();
    q.sort();
    let query = q
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut canon_headers = String::new();
    for name in signed.split(';') {
        let v = headers
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join(",");
        canon_headers.push_str(&format!("{name}:{v}\n"));
    }
    let creq = format!("{method}\n{uri}\n{query}\n{canon_headers}\n{signed}\n{payload}");
    let date = &amz_date[..8.min(amz_date.len())];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let sts = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(creq.as_bytes()))
    );
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    hex(&hmac(&k, sts.as_bytes()))
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn amz_secs(d: &str) -> Option<i64> {
    let n = |a: usize, b: usize| d.get(a..b)?.parse::<i64>().ok();
    if d.len() != 16 {
        return None;
    }
    Some(
        days_from_civil(n(0, 4)?, n(4, 6)?, n(6, 8)?) * 86_400
            + n(9, 11)? * 3600
            + n(11, 13)? * 60
            + n(13, 15)?,
    )
}

fn http_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    // civil from days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let mon = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    format!(
        "{wd}, {d:02} {mon} {y} {:02}:{:02}:{:02} GMT",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------- the server

/// A body sent whole, or a piece at a time from a channel (slow or truncated answers).
pub enum FakeBody {
    Full(Option<Bytes>),
    Chan(tokio::sync::mpsc::Receiver<Bytes>),
}

impl Body for FakeBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            FakeBody::Full(b) => Poll::Ready(b.take().map(|b| Ok(Frame::data(b)))),
            FakeBody::Chan(rx) => rx.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b)))),
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            FakeBody::Full(b) => {
                http_body::SizeHint::with_exact(b.as_ref().map_or(0, |b| b.len() as u64))
            }
            FakeBody::Chan(_) => http_body::SizeHint::default(),
        }
    }
}

type Resp = Response<FakeBody>;

fn resp(status: u16, headers: &[(&str, String)], body: impl Into<Bytes>) -> Resp {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, v);
    }
    b.body(FakeBody::Full(Some(body.into()))).unwrap()
}

fn s3_error(status: u16, code: &str, extra: &str, headers: &[(&str, String)]) -> Resp {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>fake s3 says {code}</Message>{extra}<RequestId>FAKE-REQ-1</RequestId></Error>"
    );
    let mut h = headers.to_vec();
    h.push(("x-amz-request-id", "FAKE-REQ-1".into()));
    h.push(("content-type", "application/xml".into()));
    resp(status, &h, body)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn handle(st: Arc<Mutex<State>>, req: Request<Incoming>) -> Result<Resp, std::io::Error> {
    let method = req.method().as_str().to_string();
    let raw_path = req.uri().path().to_string();
    let raw_query = req.uri().query().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let h = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let path = String::from_utf8_lossy(&percent_decode(&raw_path)).into_owned();
    let key = path
        .strip_prefix(&format!("/{BUCKET}/"))
        .map(str::to_string);
    let is_archive = key.as_deref().is_some_and(|k| k.ends_with("/archive"));
    let (faults, region, secret, token) = {
        let s = st.lock().unwrap();
        (
            s.faults.clone(),
            s.region.clone(),
            s.secret.clone(),
            s.token.clone(),
        )
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + faults.skew_secs;
    let date_hdr = ("date", http_date(now));

    // Read the body first (slowly or cut off, when asked), so its hash can be checked.
    let mut body = Vec::new();
    let mut incoming = req.into_body();
    while let Some(f) = incoming.frame().await {
        let Ok(f) = f else {
            return Err(std::io::Error::other("client went away"));
        };
        if let Ok(d) = f.into_data() {
            body.extend_from_slice(&d);
            if method == "PUT" && is_archive {
                st.lock().unwrap().archive_put_started = true;
                if faults.reset_put && body.len() > 1000 {
                    return Err(std::io::Error::other("reset on purpose"));
                }
                if faults.slow_put_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(faults.slow_put_ms)).await;
                }
            }
        }
    }

    // The signature.
    let auth = h("authorization");
    let fields: BTreeMap<&str, &str> = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .unwrap_or("")
        .split(", ")
        .filter_map(|p| p.split_once('='))
        .collect();
    let cred = fields.get("Credential").copied().unwrap_or("");
    let signed = fields.get("SignedHeaders").copied().unwrap_or("");
    let given = fields.get("Signature").copied().unwrap_or("");
    let amz_date = h("x-amz-date");
    let payload = h("x-amz-content-sha256");
    let mut problems = vec![];
    let cparts: Vec<&str> = cred.split('/').collect();
    if cparts.len() != 5
        || cparts[0] != ACCESS_KEY
        || cparts[1] != amz_date.get(..8).unwrap_or("?")
        || cparts[2] != region
        || cparts[3] != "s3"
        || cparts[4] != "aws4_request"
    {
        problems.push(format!("credential scope {cred}"));
    }
    for must in ["host", "x-amz-content-sha256", "x-amz-date"] {
        if !signed.split(';').any(|s| s == must) {
            problems.push(format!("{must} is not signed"));
        }
    }
    if token.is_some() && h("x-amz-security-token") != token.clone().unwrap_or_default() {
        problems.push("wrong or missing session token".into());
    }
    if token.is_some() && !signed.split(';').any(|s| s == "x-amz-security-token") {
        problems.push("session token not signed".into());
    }
    let want = signature(
        &method, &raw_path, &raw_query, &headers, signed, &payload, &amz_date, &region, &secret,
    );
    if given != want {
        problems.push("signature differs".into());
    }
    let line = format!("{method} {raw_path}?{raw_query}");
    if !problems.is_empty() {
        st.lock()
            .unwrap()
            .auth_failures
            .push(format!("{line}: {}", problems.join("; ")));
        return Ok(s3_error(403, "SignatureDoesNotMatch", "", &[date_hdr]));
    }
    if payload == "UNSIGNED-PAYLOAD" || payload != hex(&Sha256::digest(&body)) {
        st.lock().unwrap().payload_failures.push(line.clone());
        return Ok(s3_error(400, "XAmzContentSHA256Mismatch", "", &[date_hdr]));
    }
    if amz_secs(&amz_date).is_none_or(|t| (t - now).abs() > 15 * 60) {
        return Ok(s3_error(
            403,
            "RequestTimeTooSkewed",
            &format!(
                "<RequestTime>{amz_date}</RequestTime><ServerTime>{}</ServerTime>",
                date_hdr.1
            ),
            &[date_hdr],
        ));
    }
    st.lock().unwrap().log.push(line);
    if let Some(r) = &faults.redirect_region {
        return Ok(s3_error(
            301,
            "PermanentRedirect",
            &format!("<Endpoint>{BUCKET}.s3.{r}.amazonaws.com</Endpoint>"),
            &[
                ("x-amz-bucket-region", r.clone()),
                ("location", format!("https://s3.{r}.amazonaws.com/")),
            ],
        ));
    }
    {
        let mut s = st.lock().unwrap();
        if s.faults.fail_5xx > 0 {
            if s.faults.fail_5xx != u32::MAX {
                s.faults.fail_5xx -= 1;
            }
            return Ok(s3_error(503, "SlowDown", "", &[]));
        }
    }
    if !path.starts_with(&format!("/{BUCKET}")) {
        return Ok(s3_error(404, "NoSuchBucket", "", &[]));
    }

    match (method.as_str(), key) {
        ("GET", None) if raw_query.contains("list-type=2") => {
            let q = query_pairs(&raw_query);
            let get = |n: &str| q.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
            let prefix = get("prefix").unwrap_or_default();
            let after = get("continuation-token").map(|t| {
                String::from_utf8_lossy(&percent_decode(
                    t.trim_start_matches("t/").trim_end_matches("=="),
                ))
                .into_owned()
            });
            let s = st.lock().unwrap();
            let all: Vec<(&String, &Vec<u8>)> = s
                .objects
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .filter(|(k, _)| after.as_ref().is_none_or(|a| *k > a))
                .collect();
            let page = &all[..all.len().min(2)];
            let mut x = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{BUCKET}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>2</MaxKeys><IsTruncated>{}</IsTruncated>",
                xml_escape(&prefix),
                page.len(),
                all.len() > 2
            );
            for (k, v) in page {
                x.push_str(&format!(
                    "<Contents><Key>{}</Key><LastModified>2026-11-12T04:05:06.000Z</LastModified><ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                    xml_escape(k),
                    &hex(&Sha256::digest(v))[..32],
                    v.len()
                ));
            }
            if all.len() > 2 {
                let last = page.last().unwrap().0;
                x.push_str(&format!(
                    "<NextContinuationToken>t/{}==</NextContinuationToken>",
                    encode(last.as_bytes(), false)
                ));
            }
            x.push_str("</ListBucketResult>");
            Ok(resp(200, &[("content-type", "application/xml".into())], x))
        }
        ("PUT", Some(k)) if faults.fail_manifest_put && k.ends_with("/manifest.json") => {
            Ok(s3_error(500, "InternalError", "", &[]))
        }
        ("PUT", Some(k)) => {
            let mut s = st.lock().unwrap();
            if s.faults.put_412 || (h("if-none-match") == "*" && s.objects.contains_key(&k)) {
                drop(s);
                return Ok(s3_error(412, "PreconditionFailed", "", &[]));
            }
            s.objects.insert(k, body);
            Ok(resp(200, &[("etag", "\"fake\"".into())], Bytes::new()))
        }
        ("GET", Some(k)) | ("HEAD", Some(k)) => {
            let data = st.lock().unwrap().objects.get(&k).cloned();
            let Some(data) = data else {
                if method == "HEAD" {
                    return Ok(resp(404, &[], Bytes::new()));
                }
                return Ok(s3_error(404, "NoSuchKey", "", &[]));
            };
            let len = ("content-length", data.len().to_string());
            if method == "HEAD" {
                let mut r = resp(200, &[len], Bytes::new());
                *r.body_mut() = FakeBody::Full(None);
                return Ok(r);
            }
            if is_archive && (faults.truncate_get || faults.slow_get_ms > 0) {
                st.lock().unwrap().archive_get_started = true;
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                let send = if faults.truncate_get {
                    data[..data.len() / 2].to_vec()
                } else {
                    data.clone()
                };
                let slow = faults.slow_get_ms;
                tokio::spawn(async move {
                    for c in send.chunks(64 << 10) {
                        if tx.send(Bytes::copy_from_slice(c)).await.is_err() {
                            return;
                        }
                        if slow > 0 {
                            tokio::time::sleep(Duration::from_millis(slow)).await;
                        }
                    }
                });
                let r = Response::builder()
                    .status(200)
                    .header(len.0, len.1)
                    .body(FakeBody::Chan(rx))
                    .unwrap();
                return Ok(r);
            }
            Ok(resp(200, &[len], data))
        }
        ("DELETE", Some(k)) => {
            st.lock().unwrap().deletes.push(k);
            Ok(s3_error(403, "AccessDenied", "", &[]))
        }
        _ => Ok(s3_error(400, "NotImplemented", "", &[])),
    }
}
