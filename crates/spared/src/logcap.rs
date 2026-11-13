//! An app's log file, written through limits.
//!
//! An app's stdout and stderr both land in `app.log`. A guest can write gigabytes a second, so
//! the writer counts: each request may add at most `per_request` bytes, the app at most
//! `per_second` in any second, and the file rotates to `app.log.1` once it passes
//! `ROTATE_AT`, so a hot instance keeps at most two files of that size. Output past a limit is
//! dropped, not an error: the guest's `write` still succeeds. The first drop in a request adds a
//! `spared:` line to the log saying so.

use bytes::Bytes;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamResult};

/// The log file passes this size and the next write moves it to `app.log.1`.
pub const ROTATE_AT: u64 = 4 << 20;

#[derive(Debug, Clone, Copy)]
pub struct LogCaps {
    /// Bytes one request may write to the log.
    pub per_request: u64,
    /// Bytes an app may write to the log in one second.
    pub per_second: u64,
}

impl Default for LogCaps {
    fn default() -> Self {
        LogCaps {
            per_request: 1 << 20,
            per_second: 4 << 20,
        }
    }
}

/// Where a request's log allowance is kept when it is not this log's own: a wasip3-experimental
/// instance has several requests in flight and keeps one allowance per request.
pub trait RequestLog: Send + Sync {
    /// Bytes the running request may still write.
    fn left(&self) -> u64;
    fn spend(&self, n: u64);
    /// True the first time the running request has output dropped.
    fn first_drop(&self) -> bool;
}

struct State {
    /// Set for a wasip3-experimental instance; `request_left` and `noted` are then unused.
    request: Option<Arc<dyn RequestLog>>,
    file: std::fs::File,
    size: u64,
    request_left: u64,
    window_start: Instant,
    window_left: u64,
    dropped: u64,
    noted: bool,
}

/// The log of one app instance. Cloning shares the file and the counters.
#[derive(Clone)]
pub struct AppLog {
    path: PathBuf,
    caps: LogCaps,
    state: Arc<Mutex<State>>,
}

fn open_append(path: &PathBuf) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

impl AppLog {
    pub fn open(path: PathBuf, caps: LogCaps) -> std::io::Result<AppLog> {
        let file = open_append(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(AppLog {
            path,
            caps,
            state: Arc::new(Mutex::new(State {
                request: None,
                file,
                size,
                request_left: caps.per_request,
                window_start: Instant::now(),
                window_left: caps.per_second,
                dropped: 0,
                noted: false,
            })),
        })
    }

    /// Start a request's allowance over.
    pub fn start_request(&self) {
        let mut s = self.state.lock().unwrap();
        s.request_left = self.caps.per_request;
        s.noted = false;
    }

    /// Keep the per-request allowance in `request` from now on.
    pub fn attach(&self, request: Arc<dyn RequestLog>) {
        self.state.lock().unwrap().request = Some(request);
    }

    /// Bytes dropped over the life of this instance.
    #[cfg(test)]
    pub fn dropped(&self) -> u64 {
        self.state.lock().unwrap().dropped
    }

    fn append(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut s = self.state.lock().unwrap();
        if s.window_start.elapsed() >= Duration::from_secs(1) {
            s.window_start = Instant::now();
            s.window_left = self.caps.per_second;
        }
        let request = s.request.clone();
        let request_left = match &request {
            Some(r) => r.left(),
            None => s.request_left,
        };
        let allowed = (bytes.len() as u64).min(request_left).min(s.window_left);
        let kept = &bytes[..allowed as usize];
        if allowed < bytes.len() as u64 {
            s.dropped += bytes.len() as u64 - allowed;
            let first = match &request {
                Some(r) => r.first_drop(),
                None => !std::mem::replace(&mut s.noted, true),
            };
            if first {
                let why = if request_left == 0 || allowed == request_left {
                    "this request wrote more than the per-request log limit"
                } else {
                    "this app wrote more than the per-second log limit"
                };
                let note = format!("spared: log output dropped: {why}\n");
                let _ = Self::write_rotating(&mut s, &self.path, note.as_bytes());
            }
        }
        match &request {
            Some(r) => r.spend(allowed),
            None => s.request_left -= allowed,
        }
        s.window_left -= allowed;
        if kept.is_empty() {
            return Ok(());
        }
        Self::write_rotating(&mut s, &self.path, kept)
    }

    fn write_rotating(s: &mut State, path: &PathBuf, bytes: &[u8]) -> std::io::Result<()> {
        if s.size >= ROTATE_AT {
            let mut old = path.clone().into_os_string();
            old.push(".1");
            let _ = std::fs::rename(path, PathBuf::from(old));
            s.file = open_append(path)?;
            s.size = 0;
        }
        s.file.write_all(bytes)?;
        s.size += bytes.len() as u64;
        Ok(())
    }
}

impl IsTerminal for AppLog {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for AppLog {
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for AppLog {
    async fn ready(&mut self) {}
}

impl OutputStream for AppLog {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.append(&bytes).map_err(|e| {
            wasmtime_wasi::p2::StreamError::LastOperationFailed(wasmtime::format_err!(e))
        })
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(64 * 1024)
    }
}

impl tokio::io::AsyncWrite for AppLog {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(self.append(buf).map(|()| buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spared-logcap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("app.log")
    }

    fn text(p: &PathBuf) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn a_request_is_cut_at_its_allowance_and_the_next_starts_fresh() {
        let p = scratch("request");
        let caps = LogCaps {
            per_request: 100,
            per_second: 1 << 30,
        };
        let log = AppLog::open(p.clone(), caps).unwrap();
        for _ in 0..50 {
            log.append(&[b'x'; 40]).unwrap();
        }
        let body = text(&p);
        assert_eq!(body.matches('x').count(), 100);
        assert_eq!(
            body.matches("log output dropped").count(),
            1,
            "one note per request"
        );
        assert_eq!(log.dropped(), 50 * 40 - 100);
        log.start_request();
        log.append(b"again\n").unwrap();
        assert!(text(&p).ends_with("again\n"));
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_rate_limit_spans_requests() {
        let p = scratch("rate");
        let caps = LogCaps {
            per_request: 1 << 30,
            per_second: 64,
        };
        let log = AppLog::open(p.clone(), caps).unwrap();
        for _ in 0..10 {
            log.start_request();
            log.append(&[b'y'; 32]).unwrap();
        }
        assert_eq!(text(&p).matches('y').count(), 64);
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_big_log_rotates_instead_of_growing() {
        let p = scratch("rotate");
        let caps = LogCaps {
            per_request: u64::MAX,
            per_second: u64::MAX,
        };
        let log = AppLog::open(p.clone(), caps).unwrap();
        let chunk = vec![b'z'; 1 << 20];
        for _ in 0..10 {
            log.append(&chunk).unwrap();
        }
        let live = std::fs::metadata(&p).unwrap().len();
        let old = std::fs::metadata(p.with_extension("log.1")).unwrap().len();
        assert!(live <= ROTATE_AT + (1 << 20) && old <= ROTATE_AT + (1 << 20));
        assert!(live + old < 10 << 20);
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }
}
