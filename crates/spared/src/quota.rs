//! Per-app limits the operator sets with `spared limits set`, and what each app has used.
//!
//! An override lives in `<data-dir>/limits/<name>.json` (directory 0700, file 0600, written to a
//! temporary name, synced and renamed). It is server state, not app state: it sits outside the
//! app's directory, the guest never sees it, only the admin token reaches its routes, and it stays
//! when the app is redeployed, removed or moved away. The number that counts for each resource is
//! the smallest of the override and the operator-wide cap (`--max-app-data-mb` and so on), so an
//! override can lower a cap and never raise it. A manifest has no key for these resources.
//!
//! Three resources are enforced from here:
//! - Disk: the app's data bytes and entries. [`AppQuota::caps`] gives the books
//!   ([`crate::fsguard::FsGuard`]) the per-app numbers at each wake, and the books work as before.
//! - CPU over time: a token bucket of guest poll time, the clock behind `cpu_ms`. It refills at
//!   the override's rate and holds [`CPU_BURST`] worth of it. A request is let in while the bucket
//!   holds anything and charged what it used when it ends, so a request may overdraw it by up to its
//!   own `cpu_ms`. While the bucket is empty new requests get 503 `cpu_budget_exhausted`.
//! - Egress: request and response body bytes of the app's outbound HTTP calls, in a bucket that
//!   holds one hour's allowance and refills continuously. Once empty, outbound calls fail and a body
//!   in flight is cut.
//!
//! The counters live here, one [`AppQuota`] per app name, outside the app's instance: idle
//! eviction, a trap and a redeploy keep them, and an app has one instance, so its requests share
//! one bucket. They are in memory only and start over when the server restarts. Removing the app
//! keeps them while an override is stored, so removing and deploying again does not refill a
//! budget. Without one they go with the app.

use crate::fsguard::AppCaps;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The directory under the data dir that holds the overrides.
pub const DIR: &str = "limits";
/// What a CPU bucket holds when full: this much time's worth of its rate.
pub const CPU_BURST: Duration = Duration::from_secs(10);
/// The CPU rate a budget may name at most: all of one core. One app runs one request at a time.
pub const CPU_MS_PER_MIN_MAX: u64 = 60_000;
/// The window an egress budget is stated for, and what its bucket holds when full.
pub const EGRESS_WINDOW: Duration = Duration::from_secs(3600);
/// What the guest sees when an outbound call is refused or cut for its egress budget.
pub const EGRESS_REFUSED: &str =
    "egress_budget_exhausted: this app's outbound HTTP budget is used up and refills over the hour";

/// The values an operator set for one app. A field left out is not overridden.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ms_per_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_bytes_per_hour: Option<u64>,
}

/// The numbers in force for one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    pub data_bytes: u64,
    pub entries: u64,
    pub cpu_ms_per_min: Option<u64>,
    pub egress_bytes_per_hour: Option<u64>,
}

impl Override {
    /// Parse and check a request body: a JSON object with at least one known field, each a whole
    /// number of at least 1. The message names what is wrong.
    pub fn from_request(body: &[u8]) -> Result<Override, String> {
        let o: Override = serde_json::from_slice(body).map_err(|e| {
            format!(
                "the body must be a JSON object with data_mb, entries, cpu_ms_per_min or egress_bytes_per_hour, each a whole number: {e}"
            )
        })?;
        if o == Override::default() {
            return Err("give at least one of data_mb, entries, cpu_ms_per_min, egress_bytes_per_hour. To remove every override use DELETE (spared limits clear)".into());
        }
        for (field, v) in o.fields() {
            if v == Some(0) {
                return Err(format!("{field} must be at least 1"));
            }
        }
        if o.data_mb.is_some_and(|v| v > u64::MAX >> 20) {
            return Err("data_mb is too large".into());
        }
        Ok(o)
    }

    fn fields(&self) -> [(&'static str, Option<u64>); 4] {
        [
            ("data_mb", self.data_mb),
            ("entries", self.entries),
            ("cpu_ms_per_min", self.cpu_ms_per_min),
            ("egress_bytes_per_hour", self.egress_bytes_per_hour),
        ]
    }

    /// What is in force under `caps`, and the fields of this override the caps lowered.
    pub fn effective(o: Option<&Override>, caps: &AppCaps) -> (Effective, Vec<&'static str>) {
        let o = o.cloned().unwrap_or_default();
        let mut clamped = vec![];
        let data = o.data_mb.map(|mb| mb << 20);
        if data.is_some_and(|d| d > caps.max_data_bytes) {
            clamped.push("data_mb");
        }
        if o.entries.is_some_and(|e| e > caps.max_entries) {
            clamped.push("entries");
        }
        if o.cpu_ms_per_min.is_some_and(|c| c > CPU_MS_PER_MIN_MAX) {
            clamped.push("cpu_ms_per_min");
        }
        let eff = Effective {
            data_bytes: data.map_or(caps.max_data_bytes, |d| d.min(caps.max_data_bytes)),
            entries: o
                .entries
                .map_or(caps.max_entries, |e| e.min(caps.max_entries)),
            cpu_ms_per_min: o.cpu_ms_per_min.map(|c| c.min(CPU_MS_PER_MIN_MAX)),
            egress_bytes_per_hour: o.egress_bytes_per_hour,
        };
        (eff, clamped)
    }
}

fn file(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join(DIR).join(format!("{name}.json"))
}

/// The override stored for `name`: `Ok(None)` when there is none, an error when the file is there
/// but cannot be read or does not parse.
pub fn read(data_dir: &Path, name: &str) -> Result<Option<Override>, String> {
    let p = file(data_dir, name);
    let text = match std::fs::read(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
    };
    let v: serde_json::Value =
        serde_json::from_slice(&text).map_err(|e| format!("{} is not JSON: {e}", p.display()))?;
    let limits = v
        .get("limits")
        .ok_or_else(|| format!("{} has no limits object", p.display()))?;
    serde_json::from_value::<Override>(limits.clone())
        .map(Some)
        .map_err(|e| {
            format!(
                "{} holds limits this version does not read: {e}",
                p.display()
            )
        })
}

/// Store `o` for `name`: a temporary file, synced, renamed into place, then the directory synced.
pub fn write(data_dir: &Path, name: &str, o: &Override, at: &str) -> std::io::Result<()> {
    let dir = data_dir.join(DIR);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let body = serde_json::json!({"app": name, "limits": o, "set_at": at});
    let (path, tmp) = (file(data_dir, name), dir.join(format!(".{name}.json.tmp")));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(format!("{body}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    std::fs::File::open(&dir)?.sync_all()
}

/// When the stored override was set, for `spared limits show`.
pub fn set_at(data_dir: &Path, name: &str) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file(data_dir, name)).ok()?).ok()?;
    v["set_at"].as_str().map(str::to_string)
}

/// Remove the override for `name`. True when there was one.
pub fn remove(data_dir: &Path, name: &str) -> std::io::Result<bool> {
    match std::fs::remove_file(file(data_dir, name)) {
        Ok(()) => {
            let _ = std::fs::File::open(data_dir.join(DIR)).and_then(|d| d.sync_all());
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The names that have an override file, whether or not it parses. A missing directory is no
/// names; a directory or an entry that cannot be read is an error, never an empty list, because
/// an empty list would drop every limit the operator set.
pub fn stored_names(data_dir: &Path) -> std::io::Result<Vec<String>> {
    let rd = match std::fs::read_dir(data_dir.join(DIR)) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut v = vec![];
    for e in rd {
        let n = e?.file_name().to_string_lossy().to_string();
        if let Some(name) = n.strip_suffix(".json")
            && crate::grants::valid_name(name)
        {
            v.push(name.to_string());
        }
    }
    v.sort();
    Ok(v)
}

/// The start-up refusal for a `limits/` directory that cannot be read. Starting anyway would
/// serve every app without the limits its operator set, and stamp the data dir as if it had none.
pub fn unreadable(data_dir: &Path, e: &std::io::Error) -> crate::datadir::DataDirError {
    let dir = data_dir.join(DIR);
    crate::datadir::DataDirError {
        kind: "limits_unreadable",
        message: format!(
            "cannot read the limits directory {}: {e}. Starting would serve apps without the limits set for them. Nothing in the data dir was changed",
            dir.display()
        ),
        fix: format!(
            "make {} a directory the server's user can read and list, or restore it from a copy of the data dir",
            dir.display()
        ),
    }
}

/// A token bucket: it holds up to `cap` units and gains `per_window` units every `window`. Its
/// level may go below zero, when a charge was bigger than what it held.
#[derive(Debug, Clone)]
struct Bucket {
    cap: i64,
    per_window: u64,
    window_ns: u64,
    level: i64,
    at: Instant,
}

impl Bucket {
    fn new(cap: u64, per_window: u64, window: Duration, now: Instant) -> Bucket {
        let cap = cap.clamp(1, i64::MAX as u64) as i64;
        Bucket {
            cap,
            per_window: per_window.max(1),
            window_ns: (window.as_nanos() as u64).max(1),
            level: cap,
            at: now,
        }
    }

    /// The same rate with a new size, keeping what was used: a lower limit takes effect at once,
    /// and changing a limit does not hand out a full bucket.
    fn resized(&self, cap: u64, per_window: u64, window: Duration, now: Instant) -> Bucket {
        let mut old = self.clone();
        old.refill(now);
        let used = old.cap.saturating_sub(old.level);
        let mut b = Bucket::new(cap, per_window, window, now);
        b.level = b.cap.saturating_sub(used);
        b
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.at).as_nanos();
        let add = dt * self.per_window as u128 / self.window_ns as u128;
        if add == 0 {
            return;
        }
        self.level = (self.level as i128 + add as i128).min(self.cap as i128) as i64;
        self.at = now;
    }

    fn take(&mut self, n: u64, now: Instant) {
        self.refill(now);
        self.level = self.level.saturating_sub(n.min(i64::MAX as u64) as i64);
    }

    /// Whole seconds until the level is above zero again (at least 1), or `None` while it is.
    fn wait(&mut self, now: Instant) -> Option<u64> {
        self.refill(now);
        if self.level > 0 {
            return None;
        }
        let need = (1 - self.level) as u128;
        let ns = need * self.window_ns as u128 / self.per_window as u128;
        Some((ns.div_ceil(1_000_000_000) as u64).max(1))
    }

    /// Units taken out and not yet given back: what was used within about the bucket's span.
    fn used(&mut self, now: Instant) -> u64 {
        self.refill(now);
        (self.cap as i128 - self.level as i128).max(0) as u64
    }
}

struct State {
    over: Option<Override>,
    cpu: Option<Bucket>,
    egress: Option<Bucket>,
    /// The stored override did not parse at start: the app is not loaded until an operator sets
    /// or clears its limits.
    damaged: Option<String>,
}

/// One app's override, buckets and counters. Shared by every instance and deploy of the app.
pub struct AppQuota {
    state: Mutex<State>,
    cpu_on: AtomicBool,
    egress_on: AtomicBool,
    cpu_ns_total: AtomicU64,
    egress_bytes_total: AtomicU64,
    cpu_refused: AtomicU64,
    egress_refused: AtomicU64,
}

impl AppQuota {
    pub fn new(over: Option<Override>) -> AppQuota {
        let q = AppQuota {
            state: Mutex::new(State {
                over: None,
                cpu: None,
                egress: None,
                damaged: None,
            }),
            cpu_on: AtomicBool::new(false),
            egress_on: AtomicBool::new(false),
            cpu_ns_total: AtomicU64::new(0),
            egress_bytes_total: AtomicU64::new(0),
            cpu_refused: AtomicU64::new(0),
            egress_refused: AtomicU64::new(0),
        };
        q.set(over);
        q
    }

    /// One whose stored override cannot be read: `why` says so.
    pub fn damaged(why: String) -> AppQuota {
        let q = AppQuota::new(None);
        q.state.lock().unwrap().damaged = Some(why);
        q
    }

    pub fn damage(&self) -> Option<String> {
        self.state.lock().unwrap().damaged.clone()
    }

    pub fn override_now(&self) -> Option<Override> {
        self.state.lock().unwrap().over.clone()
    }

    /// Put `over` in force. The buckets keep what was used, so changing a budget never refills it.
    pub fn set(&self, over: Option<Override>) {
        let now = Instant::now();
        let mut s = self.state.lock().unwrap();
        let cpu = over
            .as_ref()
            .and_then(|o| o.cpu_ms_per_min)
            .map(|c| c.min(CPU_MS_PER_MIN_MAX));
        s.cpu = cpu.map(|per_min| {
            let per_window = per_min.saturating_mul(1_000_000);
            let cap = (per_window as u128 * CPU_BURST.as_nanos() / 60_000_000_000u128) as u64;
            let window = Duration::from_secs(60);
            match &s.cpu {
                Some(b) => b.resized(cap, per_window, window, now),
                None => Bucket::new(cap, per_window, window, now),
            }
        });
        let egress = over.as_ref().and_then(|o| o.egress_bytes_per_hour);
        s.egress = egress.map(|per_hour| match &s.egress {
            Some(b) => b.resized(per_hour, per_hour, EGRESS_WINDOW, now),
            None => Bucket::new(per_hour, per_hour, EGRESS_WINDOW, now),
        });
        self.cpu_on.store(s.cpu.is_some(), Ordering::Release);
        self.egress_on.store(s.egress.is_some(), Ordering::Release);
        s.over = over;
        s.damaged = None;
    }

    /// The filesystem caps for the app's next instance: the server's, with the override's data
    /// and entry numbers where they are lower.
    pub fn caps(&self, server: &AppCaps) -> AppCaps {
        let over = self.state.lock().unwrap().over.clone();
        let (eff, _) = Override::effective(over.as_ref(), server);
        AppCaps {
            max_data_bytes: eff.data_bytes,
            max_entries: eff.entries,
            ..server.clone()
        }
    }

    /// `Err(seconds)` while the CPU budget is used up: how long until a request may run again.
    pub fn cpu_admit(&self) -> Result<(), u64> {
        if !self.cpu_on.load(Ordering::Acquire) {
            return Ok(());
        }
        let wait = match self.state.lock().unwrap().cpu.as_mut() {
            Some(b) => b.wait(Instant::now()),
            None => None,
        };
        match wait {
            None => Ok(()),
            Some(secs) => {
                self.cpu_refused.fetch_add(1, Ordering::Relaxed);
                Err(secs)
            }
        }
    }

    /// Charge the poll time one request used.
    pub fn cpu_charge(&self, ns: u64) {
        self.cpu_ns_total.fetch_add(ns, Ordering::Relaxed);
        if self.cpu_on.load(Ordering::Acquire)
            && let Some(b) = self.state.lock().unwrap().cpu.as_mut()
        {
            b.take(ns, Instant::now());
        }
    }

    /// Whether an outbound call may start: false, counted as refused, once the budget is spent.
    pub fn egress_admit(&self) -> bool {
        if !self.egress_on.load(Ordering::Acquire) {
            return true;
        }
        let open = match self.state.lock().unwrap().egress.as_mut() {
            Some(b) => b.wait(Instant::now()).is_none(),
            None => true,
        };
        if !open {
            self.egress_refused.fetch_add(1, Ordering::Relaxed);
        }
        open
    }

    /// Charge `bytes` of an outbound body. False when that spent the budget: the body is cut.
    pub fn egress_charge(&self, bytes: u64) -> bool {
        self.egress_bytes_total.fetch_add(bytes, Ordering::Relaxed);
        if !self.egress_on.load(Ordering::Acquire) {
            return true;
        }
        let mut s = self.state.lock().unwrap();
        let Some(b) = s.egress.as_mut() else {
            return true;
        };
        b.take(bytes, Instant::now());
        let open = b.level >= 0;
        drop(s);
        if !open {
            self.egress_refused.fetch_add(1, Ordering::Relaxed);
        }
        open
    }

    /// What the app has used, for `spared ps --json` and `spared limits show`.
    pub fn usage_json(&self) -> serde_json::Value {
        let now = Instant::now();
        let mut s = self.state.lock().unwrap();
        let cpu_window = s.cpu.as_mut().map(|b| b.used(now) / 1_000_000);
        let cpu_left = s.cpu.as_mut().map(|b| b.level.max(0) as u64 / 1_000_000);
        let egress_window = s.egress.as_mut().map(|b| b.used(now));
        let egress_left = s.egress.as_mut().map(|b| b.level.max(0) as u64);
        drop(s);
        serde_json::json!({
            "cpu_ms_total": self.cpu_ns_total.load(Ordering::Relaxed) / 1_000_000,
            "cpu_ms_in_window": cpu_window,
            "cpu_ms_left": cpu_left,
            "cpu_window_secs": CPU_BURST.as_secs(),
            "cpu_refused": self.cpu_refused.load(Ordering::Relaxed),
            "egress_bytes_total": self.egress_bytes_total.load(Ordering::Relaxed),
            "egress_bytes_in_window": egress_window,
            "egress_bytes_left": egress_left,
            "egress_window_secs": EGRESS_WINDOW.as_secs(),
            "egress_refused": self.egress_refused.load(Ordering::Relaxed),
            "since": "server start",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(data_mb: u64, entries: u64) -> AppCaps {
        AppCaps {
            max_data_bytes: data_mb << 20,
            max_entries: entries,
            ..Default::default()
        }
    }

    #[test]
    fn an_override_lowers_a_cap_and_never_raises_it() {
        let o = Override {
            data_mb: Some(100),
            entries: Some(90_000),
            cpu_ms_per_min: Some(90_000),
            egress_bytes_per_hour: Some(5),
        };
        let (eff, clamped) = Override::effective(Some(&o), &caps(1024, 50_000));
        assert_eq!(eff.data_bytes, 100 << 20);
        assert_eq!(eff.entries, 50_000);
        assert_eq!(eff.cpu_ms_per_min, Some(CPU_MS_PER_MIN_MAX));
        assert_eq!(eff.egress_bytes_per_hour, Some(5));
        assert_eq!(clamped, vec!["entries", "cpu_ms_per_min"]);
        let (eff, clamped) = Override::effective(None, &caps(8, 100));
        assert_eq!((eff.data_bytes, eff.entries), (8 << 20, 100));
        assert_eq!(
            (eff.cpu_ms_per_min, eff.egress_bytes_per_hour),
            (None, None)
        );
        assert!(clamped.is_empty());
    }

    #[test]
    fn a_request_body_needs_a_known_field_with_a_value_of_at_least_one() {
        assert!(Override::from_request(br#"{"data_mb": 10}"#).is_ok());
        for bad in [
            &br#"{}"#[..],
            br#"{"data_mb": 0}"#,
            br#"{"data_mb": -1}"#,
            br#"{"data_mb": 1.5}"#,
            br#"{"memory_mb": 10}"#,
            br#"[]"#,
            b"not json",
        ] {
            assert!(
                Override::from_request(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn the_store_round_trips_and_a_damaged_file_is_an_error() {
        let d = std::env::temp_dir().join(format!("spared-quota-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(read(&d, "a").unwrap(), None);
        let o = Override {
            data_mb: Some(3),
            ..Default::default()
        };
        write(&d, "a", &o, "2026-11-13T00:00:00Z").unwrap();
        assert_eq!(read(&d, "a").unwrap(), Some(o));
        assert_eq!(stored_names(&d).unwrap(), vec!["a".to_string()]);
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d.join(DIR)), 0o700);
        assert_eq!(mode(&d.join(DIR).join("a.json")), 0o600);
        std::fs::write(d.join(DIR).join("b.json"), "{").unwrap();
        assert!(read(&d, "b").is_err());
        std::fs::write(d.join(DIR).join("b.json"), r#"{"limits":{"memory_mb":1}}"#).unwrap();
        assert!(read(&d, "b").is_err());
        assert!(remove(&d, "a").unwrap());
        assert!(!remove(&d, "a").unwrap());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_bucket_lets_in_while_it_holds_anything_and_refills_at_its_rate() {
        let t0 = Instant::now();
        let mut b = Bucket::new(1000, 100, Duration::from_secs(1), t0);
        assert_eq!(b.wait(t0), None);
        b.take(1200, t0);
        assert_eq!(b.level, -200);
        assert_eq!(b.wait(t0), Some(3), "201 units at 100 a second");
        assert_eq!(b.wait(t0 + Duration::from_millis(2500)), None);
        assert_eq!(b.used(t0 + Duration::from_secs(100)), 0, "never above full");
        b.take(400, t0 + Duration::from_secs(100));
        assert_eq!(b.used(t0 + Duration::from_secs(100)), 400);
    }

    #[test]
    fn a_tiny_rate_still_refills_over_many_small_steps() {
        let t0 = Instant::now();
        let mut b = Bucket::new(10, 10, Duration::from_secs(3600), t0);
        b.take(10, t0);
        let mut t = t0;
        for _ in 0..400 {
            t += Duration::from_secs(1);
            b.wait(t);
        }
        assert_eq!(b.level, 1, "one unit every 360 s");
    }

    #[test]
    fn a_cpu_budget_turns_requests_away_until_it_refills() {
        let q = AppQuota::new(Some(Override {
            cpu_ms_per_min: Some(6000),
            ..Default::default()
        }));
        assert_eq!(q.cpu_admit(), Ok(()));
        q.cpu_charge(1_500_000_000);
        let secs = q.cpu_admit().unwrap_err();
        assert!(
            (5..=6).contains(&secs),
            "500 ms of debt at 100 ms a second: {secs}"
        );
        assert_eq!(q.usage_json()["cpu_refused"], 1);
        assert_eq!(q.usage_json()["cpu_ms_total"], 1500);
        q.set(None);
        assert_eq!(
            q.cpu_admit(),
            Ok(()),
            "clearing the budget lets requests in"
        );
    }

    #[test]
    fn changing_a_budget_keeps_what_was_used() {
        let q = AppQuota::new(Some(Override {
            egress_bytes_per_hour: Some(1000),
            ..Default::default()
        }));
        assert!(q.egress_charge(900));
        q.set(Some(Override {
            egress_bytes_per_hour: Some(2000),
            ..Default::default()
        }));
        assert_eq!(q.usage_json()["egress_bytes_left"], 1100);
        assert!(
            !q.egress_charge(1200),
            "the charge that spends it cuts the body"
        );
        assert!(!q.egress_admit());
        assert_eq!(q.usage_json()["egress_refused"], 2);
    }

    #[test]
    fn without_an_override_nothing_is_refused_and_use_is_still_counted() {
        let q = AppQuota::new(None);
        q.cpu_charge(u64::MAX / 2);
        assert_eq!(q.cpu_admit(), Ok(()));
        assert!(q.egress_charge(u64::MAX / 2));
        assert!(q.egress_admit());
        assert_eq!(q.usage_json()["cpu_ms_in_window"], serde_json::Value::Null);
    }

    #[test]
    fn an_unreadable_limits_directory_is_an_error_not_an_empty_list() {
        let d =
            std::env::temp_dir().join(format!("spared-quota-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(
            stored_names(&d).unwrap(),
            Vec::<String>::new(),
            "no directory: no names"
        );
        std::fs::write(d.join(DIR), "").unwrap();
        let e = stored_names(&d).unwrap_err();
        let r = unreadable(&d, &e);
        assert_eq!(r.kind, "limits_unreadable");
        assert!(r.message.contains("limits"), "{}", r.message);
        let _ = std::fs::remove_dir_all(&d);
    }
}
