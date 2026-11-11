//! What an operator can see: the access log (one JSON line per request on stdout) and the
//! counters behind the admin listener's `/metrics`.
//!
//! The access log never holds a query string, a header or a body: only the time, the listener,
//! the app name, the method, the path, the status, how long the request took and a request id.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The longest path written to the access log; longer ones are cut and marked.
const MAX_LOGGED_PATH: usize = 512;

/// The kind of a response that spared (not the app) produced as an error. Carried in the
/// response's extensions, which are not sent to the client.
#[derive(Clone, Copy)]
pub struct ErrorKind(pub &'static str);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Listener {
    Public,
    Admin,
}

impl Listener {
    pub fn word(self) -> &'static str {
        match self {
            Listener::Public => "public",
            Listener::Admin => "admin",
        }
    }
}

/// Why a live instance was dropped.
#[derive(Clone, Copy)]
pub enum Evicted {
    /// No request for `--idle-secs`.
    Idle,
    /// A trap, a limit or a deadline.
    Error,
    /// `spared evict`, a backup or a restore.
    Admin,
    /// A redeploy or a delete.
    Deploy,
}

const EVICT_REASONS: [&str; 4] = ["idle", "error", "admin", "deploy"];

#[derive(Default)]
pub struct Metrics {
    /// Requests by listener (public, admin) and status class (1xx to 5xx).
    requests: [[AtomicU64; 5]; 2],
    wakes: AtomicU64,
    /// Bytes of component files the server read or asked the kernel to read ahead.
    pub prefetch_bytes: Arc<AtomicU64>,
    evictions: [AtomicU64; 4],
    /// Error responses made by spared, by listener and kind. The kinds are string literals in
    /// the server, so the set is small and fixed.
    errors: Mutex<BTreeMap<(Listener, &'static str), u64>>,
}

/// Numbers read at scrape time rather than counted.
pub struct Gauges {
    pub version: &'static str,
    pub uptime: Duration,
    pub apps_deployed: usize,
    pub apps_hot: usize,
    pub components_loaded: usize,
    pub resident_bytes: Option<u64>,
}

impl Metrics {
    pub fn request(&self, listener: Listener, status: u16, error: Option<ErrorKind>) {
        let class = (status / 100).clamp(1, 5) as usize - 1;
        self.requests[listener as usize][class].fetch_add(1, Ordering::Relaxed);
        if let Some(ErrorKind(kind)) = error {
            *self
                .errors
                .lock()
                .unwrap()
                .entry((listener, kind))
                .or_default() += 1;
        }
    }

    pub fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn eviction(&self, why: Evicted) {
        self.evictions[why as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// The Prometheus text exposition format, version 0.0.4.
    pub fn render(&self, g: &Gauges) -> String {
        let mut o = String::new();
        let mut family = |name: &str, kind: &str, help: &str, rows: Vec<(String, String)>| {
            o.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
            for (labels, value) in rows {
                o.push_str(&format!("{name}{labels} {value}\n"));
            }
        };
        family(
            "spared_build_info",
            "gauge",
            "The running spared version; the value is always 1.",
            vec![(format!("{{version=\"{}\"}}", g.version), "1".into())],
        );
        family(
            "spared_uptime_seconds",
            "gauge",
            "Seconds since the server started.",
            vec![(String::new(), g.uptime.as_secs().to_string())],
        );
        let mut rows = vec![];
        for l in [Listener::Public, Listener::Admin] {
            for class in 0..5 {
                let n = self.requests[l as usize][class].load(Ordering::Relaxed);
                rows.push((
                    format!("{{listener=\"{}\",status=\"{}xx\"}}", l.word(), class + 1),
                    n.to_string(),
                ));
            }
        }
        family(
            "spared_requests_total",
            "counter",
            "Requests answered, by listener and status class.",
            rows,
        );
        let rows = self
            .errors
            .lock()
            .unwrap()
            .iter()
            .map(|((l, kind), n)| {
                (
                    format!("{{listener=\"{}\",kind=\"{kind}\"}}", l.word()),
                    n.to_string(),
                )
            })
            .collect();
        family(
            "spared_errors_total",
            "counter",
            "Error responses produced by spared itself (not by an app), by listener and error kind.",
            rows,
        );
        family(
            "spared_wakes_total",
            "counter",
            "App instances started.",
            vec![(
                String::new(),
                self.wakes.load(Ordering::Relaxed).to_string(),
            )],
        );
        family(
            "spared_prefetch_bytes_total",
            "counter",
            "Bytes of stored components read into the page cache ahead of requests, at start and at wake.",
            vec![(
                String::new(),
                self.prefetch_bytes.load(Ordering::Relaxed).to_string(),
            )],
        );
        let rows = EVICT_REASONS
            .iter()
            .enumerate()
            .map(|(i, r)| {
                (
                    format!("{{reason=\"{r}\"}}"),
                    self.evictions[i].load(Ordering::Relaxed).to_string(),
                )
            })
            .collect();
        family(
            "spared_evictions_total",
            "counter",
            "Live app instances dropped, by reason.",
            rows,
        );
        family(
            "spared_apps",
            "gauge",
            "Apps on this server: deployed, and hot (an instance is loaded).",
            vec![
                ("{state=\"deployed\"}".into(), g.apps_deployed.to_string()),
                ("{state=\"hot\"}".into(), g.apps_hot.to_string()),
            ],
        );
        family(
            "spared_components_loaded",
            "gauge",
            "Distinct compiled components held in memory.",
            vec![(String::new(), g.components_loaded.to_string())],
        );
        if let Some(b) = g.resident_bytes {
            family(
                "process_resident_memory_bytes",
                "gauge",
                "Resident memory of the server process, in bytes.",
                vec![(String::new(), b.to_string())],
            );
        }
        o
    }
}

/// Resident memory of this process from `/proc/self/statm` (Linux; `None` elsewhere).
pub fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(page).ok().map(|p| pages * p)
}

/// Ids for requests: 16 hex digits, different on every request and unrelated to how many
/// requests the server has seen.
pub struct RequestIds {
    seed: u64,
    next: AtomicU64,
}

impl RequestIds {
    pub fn new() -> Self {
        let mut b = [0u8; 8];
        let from_os = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
            .is_ok();
        let seed = if from_os {
            u64::from_le_bytes(b)
        } else {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
        };
        RequestIds {
            seed,
            next: AtomicU64::new(0),
        }
    }

    pub fn next(&self) -> String {
        // splitmix64 over a counter: a permutation, so ids do not repeat.
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let mut z = self
            .seed
            .wrapping_add(n.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        format!("{:016x}", z ^ (z >> 31))
    }
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-11-10T04:05:06.123Z` for a time after 1970.
pub fn rfc3339_millis(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60,
        since.subsec_millis()
    )
}

pub struct AccessLine<'a> {
    pub time: SystemTime,
    pub listener: Listener,
    /// The app the request was routed to; `None` when it reached no app.
    pub app: Option<&'a str>,
    pub method: &'a str,
    /// The request path as received, without the query string.
    pub path: &'a str,
    pub status: u16,
    pub duration: Duration,
    pub id: &'a str,
    pub error: Option<ErrorKind>,
}

impl AccessLine<'_> {
    /// One JSON object. serde_json escapes every string, so a path cannot forge a second line.
    pub fn render(&self) -> String {
        let mut path: String = self.path.chars().take(MAX_LOGGED_PATH).collect();
        if path.len() < self.path.len() {
            path.push_str("...");
        }
        // Built by hand to keep the fields in reading order; every string goes through
        // serde_json, which escapes it.
        let q = |s: &str| serde_json::Value::from(s).to_string();
        let mut o = format!(
            "{{\"time\":{},\"listener\":{},\"app\":{},\"method\":{},\"path\":{},\"status\":{},\"duration_ms\":{},\"request_id\":{}",
            q(&rfc3339_millis(self.time)),
            q(self.listener.word()),
            self.app.map_or("null".to_string(), q),
            q(self.method),
            q(&path),
            self.status,
            (self.duration.as_secs_f64() * 1e6).round() / 1e3,
            q(self.id),
        );
        if let Some(ErrorKind(kind)) = self.error {
            o.push_str(&format!(",\"error\":{}", q(kind)));
        }
        o.push('}');
        o
    }

    /// Write the line to stdout. A closed or full stdout must not take a request down with it.
    pub fn write(&self) {
        let mut line = self.render();
        line.push('\n');
        let _ = std::io::stdout().lock().write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(path: &'a str, app: Option<&'a str>) -> AccessLine<'a> {
        AccessLine {
            time: UNIX_EPOCH + Duration::from_millis(1_794_283_506_123),
            listener: Listener::Public,
            app,
            method: "GET",
            path,
            status: 200,
            duration: Duration::from_micros(1234),
            id: "00000000deadbeef",
            error: None,
        }
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(rfc3339_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(951_782_400_007);
        assert_eq!(rfc3339_millis(t), "2000-02-29T00:00:00.007Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_794_283_506);
        assert_eq!(rfc3339_millis(t), "2026-11-10T04:05:06.000Z");
    }

    #[test]
    fn access_line_has_the_documented_fields_and_nothing_else() {
        let v: serde_json::Value =
            serde_json::from_str(&line("/api/notes", Some("notes")).render()).unwrap();
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "app",
                "duration_ms",
                "listener",
                "method",
                "path",
                "request_id",
                "status",
                "time"
            ]
        );
        assert_eq!(v["app"], "notes");
        assert_eq!(v["status"], 200);
        assert_eq!(v["duration_ms"], 1.234);
        assert_eq!(v["time"], "2026-11-10T04:05:06.123Z");
        let none: serde_json::Value = serde_json::from_str(&line("/", None).render()).unwrap();
        assert!(none["app"].is_null());
    }

    #[test]
    fn a_hostile_path_stays_on_one_line_and_long_paths_are_cut() {
        let l = line("/a\"b\n{\"status\":500}", None).render();
        assert_eq!(l.lines().count(), 1, "{l}");
        let v: serde_json::Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["status"], 200);
        let long = "/".to_string() + &"x".repeat(5000);
        let v: serde_json::Value = serde_json::from_str(&line(&long, None).render()).unwrap();
        assert_eq!(v["path"].as_str().unwrap().len(), MAX_LOGGED_PATH + 3);
    }

    #[test]
    fn an_error_kind_is_logged_when_spared_made_the_response() {
        let mut l = line("/", Some("notes"));
        l.status = 500;
        l.error = Some(ErrorKind("app_trap"));
        let v: serde_json::Value = serde_json::from_str(&l.render()).unwrap();
        assert_eq!(v["error"], "app_trap");
    }

    #[test]
    fn request_ids_differ_and_are_16_hex_digits() {
        let ids = RequestIds::new();
        let a: std::collections::HashSet<String> = (0..1000).map(|_| ids.next()).collect();
        assert_eq!(a.len(), 1000);
        assert!(
            a.iter()
                .all(|i| i.len() == 16 && i.bytes().all(|b| b.is_ascii_hexdigit()))
        );
    }

    #[test]
    fn metrics_render_as_prometheus_text() {
        let m = Metrics::default();
        m.request(Listener::Public, 200, None);
        m.request(Listener::Public, 200, None);
        m.request(Listener::Public, 500, Some(ErrorKind("app_trap")));
        m.request(Listener::Admin, 401, Some(ErrorKind("unauthorized")));
        m.wake();
        m.eviction(Evicted::Idle);
        m.eviction(Evicted::Error);
        let text = m.render(&Gauges {
            version: "1.2.3",
            uptime: Duration::from_secs(7),
            apps_deployed: 3,
            apps_hot: 1,
            components_loaded: 2,
            resident_bytes: Some(4096),
        });
        for want in [
            "# TYPE spared_requests_total counter",
            "spared_build_info{version=\"1.2.3\"} 1",
            "spared_uptime_seconds 7",
            "spared_requests_total{listener=\"public\",status=\"2xx\"} 2",
            "spared_requests_total{listener=\"public\",status=\"5xx\"} 1",
            "spared_requests_total{listener=\"admin\",status=\"4xx\"} 1",
            "spared_errors_total{listener=\"public\",kind=\"app_trap\"} 1",
            "spared_errors_total{listener=\"admin\",kind=\"unauthorized\"} 1",
            "spared_wakes_total 1",
            "spared_evictions_total{reason=\"idle\"} 1",
            "spared_evictions_total{reason=\"error\"} 1",
            "spared_evictions_total{reason=\"deploy\"} 0",
            "spared_apps{state=\"deployed\"} 3",
            "spared_apps{state=\"hot\"} 1",
            "spared_components_loaded 2",
            "process_resident_memory_bytes 4096",
        ] {
            assert!(
                text.lines().any(|l| l == want),
                "missing {want:?} in:\n{text}"
            );
        }
        // Every line is a comment or `name{labels} value`.
        for l in text.lines() {
            assert!(
                l.starts_with("# ")
                    || l.rsplit_once(' ')
                        .is_some_and(|(_, v)| v.parse::<f64>().is_ok()),
                "{l}"
            );
        }
    }
}
