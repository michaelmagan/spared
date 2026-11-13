use crate::fsguard::{AppCaps, FsGuard};
use crate::grants::{Grants, Manifest, host_allowed, ip_denied, norm_host, ungranted_imports};
use crate::quota::{AppQuota, EGRESS_REFUSED};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder, Trap, UpdateDeadline,
};
use wasmtime_wasi::cli::StdoutStream;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::bindings::{Proxy, ProxyPre};
use wasmtime_wasi_http::{
    Error as HttpError, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks,
    WasiHttpView,
};

/// The wasmtime engine configuration every part of spared uses: the server, `spared test`, the
/// compile process and the lab commands. Epoch interruption is always on (it is the CPU limit),
/// and the wasm features no guest uses are off.
pub fn engine_config() -> Config {
    let mut c = Config::new();
    c.epoch_interruption(true);
    c.cranelift_opt_level(wasmtime::OptLevel::Speed);
    c.wasm_threads(false)
        .wasm_shared_everything_threads(false)
        .wasm_gc(false)
        .wasm_function_references(false)
        .wasm_exceptions(false)
        .wasm_memory64(false)
        .wasm_multi_memory(false)
        .wasm_tail_call(false)
        .wasm_relaxed_simd(false)
        .wasm_wide_arithmetic(false)
        .wasm_extended_const(false)
        .wasm_stack_switching(false)
        .wasm_custom_page_sizes(false)
        .wasm_component_model_async(false)
        .wasm_component_model_more_async_builtins(false)
        .wasm_component_model_async_stackful(false)
        .wasm_component_model_threading(false)
        .wasm_component_model_error_context(false)
        .wasm_component_model_gc(false)
        .wasm_component_model_map(false)
        .wasm_component_model_memory64(false)
        .wasm_component_model_fixed_length_lists(false);
    c
}

pub fn engine() -> wasmtime::Result<Engine> {
    Engine::new(&engine_config())
}

/// Advance the engine's epoch about every millisecond, on a thread of its own. The epoch is the
/// clock behind `cpu_ms` and the yield points of [`Guarded::store`]; an engine that runs guests
/// needs one of these.
pub fn start_epoch_ticker(engine: &Engine) {
    let engine = engine.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(1));
            engine.increment_epoch();
        }
    });
}

/// The linker for every app instance: wasmtime-wasi with the data-directory books on top of
/// `wasi:filesystem`, and the WASI HTTP interfaces. Deploying, running (`spared serve`) and
/// `spared test` all link through here, so a component links the same way in each.
pub fn linker(engine: &Engine) -> wasmtime::Result<Linker<Guarded>> {
    let mut linker: Linker<Guarded> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    linker.allow_shadowing(true);
    crate::fsguard::shadow_filesystem(&mut linker)?;
    linker.allow_shadowing(false);
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    Ok(linker)
}

/// Link `component` against [`linker`]: the step that rejects a component whose imports or
/// exports do not match what the host provides.
pub fn link(engine: &Engine, component: &Component) -> wasmtime::Result<ProxyPre<Guarded>> {
    ProxyPre::new(linker(engine)?.instantiate_pre(component)?)
}

/// Most random bytes one `wasi:random` call may ask for.
const MAX_RANDOM_BYTES: u64 = 1 << 20;

/// The WASI context of an app: no sockets and no name lookups (outbound HTTP goes through the
/// policy hooks instead), `out` for stdout and stderr, and `/data` mapped to `data` when the
/// manifest grants it.
pub fn wasi_builder<O>(m: &Manifest, data: &Path, out: &O) -> wasmtime::Result<WasiCtxBuilder>
where
    O: StdoutStream + Clone + 'static,
{
    let mut b = WasiCtx::builder();
    b.allow_tcp(false)
        .allow_udp(false)
        .allow_ip_name_lookup(false)
        .allow_blocking_current_thread(true)
        .max_random_size(MAX_RANDOM_BYTES);
    b.stdout(out.clone()).stderr(out.clone());
    if m.grants.data {
        b.preopened_dir(data, "/data", FsPerms::ReadWrite)?;
    }
    Ok(b)
}

/// A store for one app instance from a finished WASI context: the manifest's limits, the books for
/// the data directory when the app has one, and the app's budgets (`quota`, which the server keeps
/// per app across instances, or `None` where nothing keeps them, as in `spared test`).
pub async fn store_for(
    engine: &Engine,
    m: &Manifest,
    data: &Path,
    wasi: WasiCtx,
    caps: &AppCaps,
    quota: Option<Arc<AppQuota>>,
) -> Store<Guarded> {
    let mut store = Guarded::store(engine, wasi, m, quota);
    if m.grants.data {
        store.data_mut().fsguard = Some(FsGuard::open(data.to_path_buf(), caps.clone()).await);
    }
    store
}

/// The store the server gives an app, built from the manifest. `spared test` uses it too.
pub async fn app_store<O>(
    engine: &Engine,
    m: &Manifest,
    data: &Path,
    out: &O,
    caps: &AppCaps,
    quota: Option<Arc<AppQuota>>,
) -> wasmtime::Result<Store<Guarded>>
where
    O: StdoutStream + Clone + 'static,
{
    let wasi = wasi_builder(m, data, out)?.build();
    Ok(store_for(engine, m, data, wasi, caps, quota).await)
}

/// Why a component cannot be an app, in the shape the CLI and the server both report.
pub struct Refusal {
    pub kind: &'static str,
    pub message: String,
    pub fix: String,
}

/// The manifest line or change that would fix an ungranted import.
pub fn import_fix(bad: &[String]) -> String {
    let mut fix = String::new();
    if bad
        .iter()
        .any(|b| b.starts_with("wasi:http/outgoing-handler"))
    {
        fix.push_str("the app makes outbound HTTP calls: add `fetch = [\"api.example.com\"]` under [grants] in spared.toml. ");
    }
    if bad.iter().any(|b| b.starts_with("wasi:sockets")) {
        fix.push_str(
            "raw sockets cannot be granted: use HTTP through the wstd client instead of std::net. ",
        );
    }
    if bad.iter().any(|b| b.starts_with("wasi:filesystem")) {
        fix.push_str("the app uses files: set `data = true` under [grants]. ");
    }
    if fix.is_empty() {
        fix = "remove the dependency that needs this interface".into();
    }
    fix.trim().to_string()
}

/// Whether `component` may run under `grants`: every import is granted, and it exports an HTTP
/// handler.
pub fn check_component(
    engine: &Engine,
    component: &Component,
    grants: &Grants,
) -> Result<(), Refusal> {
    let bad = ungranted_imports(engine, component, grants);
    if !bad.is_empty() {
        return Err(Refusal {
            kind: "ungranted_import",
            message: format!(
                "component imports interfaces not granted by spared.toml: {}",
                bad.join(", ")
            ),
            fix: import_fix(&bad),
        });
    }
    if !component
        .component_type()
        .exports(engine)
        .any(|(n, _)| n.starts_with("wasi:http/incoming-handler"))
    {
        return Err(Refusal {
            kind: "no_http_handler",
            message: "component does not export wasi:http/incoming-handler".into(),
            fix: "write the entry point with #[wstd::http_server] async fn main(req: Request<Body>) -> Result<Response<Body>, Error>".into(),
        });
    }
    Ok(())
}

pub struct PolicyHooks {
    pub allow: Vec<String>,
    /// Outbound calls left in the current request; reset to `per_request` at each request.
    pub remaining: u32,
    pub per_request: u32,
    /// The app's egress budget and byte count, kept by the server outside the instance.
    pub quota: Option<Arc<AppQuota>>,
    /// Unit tests only: reach a loopback test server, which [`ip_denied`] always refuses.
    #[cfg(test)]
    pub test_loopback: bool,
}

impl PolicyHooks {
    #[cfg(test)]
    fn loopback_allowed(&self) -> bool {
        self.test_loopback
    }

    #[cfg(not(test))]
    fn loopback_allowed(&self) -> bool {
        false
    }
}

fn egress_refused() -> HttpError {
    HttpError::InternalError(Some(EGRESS_REFUSED.into()))
}

/// An outbound request or response body whose data bytes are charged to the app's egress budget
/// as they pass. The frame that spends the budget is replaced by an error, and so is every later
/// poll, so the guest sees the call fail and the body stops there.
struct Metered {
    inner: WasiBody,
    quota: Arc<AppQuota>,
    cut: bool,
}

fn metered(inner: WasiBody, quota: &Arc<AppQuota>) -> WasiBody {
    Metered {
        inner,
        quota: quota.clone(),
        cut: false,
    }
    .boxed_unsync()
}

impl http_body::Body for Metered {
    type Data = Bytes;
    type Error = HttpError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, HttpError>>> {
        if self.cut {
            return Poll::Ready(Some(Err(egress_refused())));
        }
        let r = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(f))) = &r
            && let Some(d) = f.data_ref()
            && !self.quota.egress_charge(d.len() as u64)
        {
            self.cut = true;
            return Poll::Ready(Some(Err(egress_refused())));
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        !self.cut && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Why an outbound request is refused before any lookup, or None if its URL may proceed.
/// Userinfo (`user@host`) would let TLS and the Host header name a different site than the one
/// the allowlist checked; a Host header that differs from the URI host does the same over HTTP.
fn url_refused(request: &http::Request<WasiBody>) -> Option<HttpError> {
    let uri = request.uri();
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Some(HttpError::HttpRequestUriInvalid);
    }
    let host = norm_host(uri.host().unwrap_or(""));
    if let Some(h) = request.headers().get(http::header::HOST) {
        let h = h.to_str().unwrap_or("");
        if h.contains('@') || norm_host(h) != host {
            return Some(HttpError::HttpRequestDenied);
        }
    }
    None
}

impl WasiHttpHooks for PolicyHooks {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = std::result::Result<(), HttpError>> + Send>,
    ) -> Box<
        dyn Future<
                Output = std::result::Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = std::result::Result<(), HttpError>> + Send>,
                    ),
                    HttpError,
                >,
            > + Send,
    > {
        _ = fut;
        let refused = url_refused(&request);
        let host = norm_host(request.uri().host().unwrap_or(""));
        let port =
            request
                .uri()
                .port_u16()
                .unwrap_or(if request.uri().scheme_str() == Some("http") {
                    80
                } else {
                    443
                });
        let listed = host_allowed(&self.allow, &host);
        let budget = self.remaining > 0;
        self.remaining = self.remaining.saturating_sub(1);
        let loopback_ok = self.loopback_allowed();
        let quota = self.quota.clone();
        // After the policy checks, so a call they refuse is not counted against the egress budget.
        let egress_open = refused.is_none()
            && budget
            && listed
            && quota.as_ref().is_none_or(|q| q.egress_admit());
        let request = match &quota {
            Some(q) => request.map(|b| metered(b, q)),
            None => request,
        };
        Box::new(async move {
            if let Some(e) = refused {
                return Err(e);
            }
            if !budget || !listed {
                return Err(HttpError::HttpRequestDenied);
            }
            if !egress_open {
                return Err(egress_refused());
            }
            let bare = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            let addrs: Vec<_> = match tokio::net::lookup_host((bare.as_str(), port)).await {
                Ok(a) => a.collect(),
                Err(_) => return Err(HttpError::DestinationNotFound),
            };
            let denied = |ip: std::net::IpAddr| ip_denied(ip) && !(loopback_ok && ip.is_loopback());
            if addrs.is_empty() || addrs.iter().any(|a| denied(a.ip())) {
                return Err(HttpError::DestinationIpProhibited);
            }
            let (res, io) =
                crate::pinned_send::pinned_send_request(addrs[0], request, options).await?;
            let res = res.map(BodyExt::boxed_unsync);
            Ok((
                match &quota {
                    Some(q) => res.map(|b| metered(b, q)),
                    None => res,
                },
                Box::new(io) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

/// The store's resource limits (`StoreLimits`), plus a record of whether the guest was refused
/// memory by `memory_mb` during the current request. A Rust guest whose allocation fails aborts
/// with a plain `unreachable` trap, so that record is the only way to tell "ran out of memory"
/// from "panicked".
pub struct Limiter {
    inner: StoreLimits,
    memory_cap: usize,
    memory_denied: bool,
}

impl Limiter {
    fn new(inner: StoreLimits, memory_cap: usize) -> Self {
        Limiter {
            inner,
            memory_cap,
            memory_denied: false,
        }
    }

    /// Whether a memory growth past `memory_mb` was refused since the request started.
    pub fn memory_denied(&self) -> bool {
        self.memory_denied
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        if !allowed && desired > self.memory_cap {
            self.memory_denied = true;
        }
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.inner.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.inner.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

/// Wall-clock time the guest's future spends being polled, which is the time a server thread is
/// busy for it: its own code and every host call it makes inside a poll (file I/O, directory
/// listings). Time spent waiting, for the network or for another app's turn, is not in it.
pub struct PollClock {
    base: std::time::Instant,
    spent_ns: AtomicU64,
    polling_since_ns: AtomicU64,
}

const NOT_POLLING: u64 = u64::MAX;

impl PollClock {
    pub fn new() -> Self {
        PollClock {
            base: std::time::Instant::now(),
            spent_ns: AtomicU64::new(0),
            polling_since_ns: AtomicU64::new(NOT_POLLING),
        }
    }

    fn now_ns(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }

    pub fn reset(&self) {
        self.spent_ns.store(0, Ordering::Relaxed);
    }

    /// Nanoseconds polled since the last reset, the poll in progress included.
    pub fn used_ns(&self) -> u64 {
        let since = self.polling_since_ns.load(Ordering::Relaxed);
        let live = if since == NOT_POLLING {
            0
        } else {
            self.now_ns().saturating_sub(since)
        };
        self.spent_ns.load(Ordering::Relaxed) + live
    }

    pub fn enter(&self) {
        self.polling_since_ns
            .store(self.now_ns(), Ordering::Relaxed);
    }

    pub fn leave(&self) {
        let since = self.polling_since_ns.swap(NOT_POLLING, Ordering::Relaxed);
        if since != NOT_POLLING {
            self.spent_ns
                .fetch_add(self.now_ns().saturating_sub(since), Ordering::Relaxed);
        }
    }

    pub fn used_ms(&self) -> u64 {
        self.used_ns() / 1_000_000
    }
}

impl Default for PollClock {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Guarded {
    pub wasi: WasiCtx,
    pub http: WasiHttpCtx,
    pub table: ResourceTable,
    pub hooks: PolicyHooks,
    pub limits: Limiter,
    /// The books for this instance's data directory; `None` for a harness that has no limits.
    pub fsguard: Option<std::sync::Arc<crate::fsguard::FsGuard>>,
    /// The app's log, whose per-request allowance starts over with each request.
    pub log: Option<crate::logcap::AppLog>,
    /// Epoch ticks (about 1 ms each) this request may run before it is interrupted.
    pub cpu_budget: u64,
    pub cpu_used: u64,
    /// Time the guest's calls have been polled this request; [`call_handler`] drives it.
    pub clock: Arc<PollClock>,
}

/// Host-side caps that wasmtime does not count against `memory_mb`.
const MAX_RESOURCES: usize = 4096;
const MAX_FIELDS_BYTES: usize = 64 << 10;
const MAX_TABLE_ELEMENTS: usize = 20_000;

impl Guarded {
    /// A store for one app instance, with the manifest's limits (already capped by the server).
    ///
    /// Compute: the store yields to the async executor at every epoch tick, so a guest that
    /// computes never holds a runtime thread for more than about a millisecond, and it is
    /// interrupted once it has run for `cpu_ms` ticks in one request (see [`start_request`]).
    pub fn store(
        engine: &Engine,
        wasi: WasiCtx,
        m: &Manifest,
        quota: Option<Arc<AppQuota>>,
    ) -> Store<Guarded> {
        let memory_cap = m.limits.memory_mb.saturating_mul(1 << 20);
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_cap)
            .memories(1)
            .tables(4)
            .table_elements(MAX_TABLE_ELEMENTS)
            .instances(16)
            .trap_on_grow_failure(false)
            .build();
        let limits = Limiter::new(limits, memory_cap);
        let mut http = WasiHttpCtx::new();
        http.set_field_size_limit(MAX_FIELDS_BYTES);
        let mut table = ResourceTable::new();
        table.set_max_capacity(MAX_RESOURCES);
        let mut store = Store::new(
            engine,
            Guarded {
                wasi,
                http,
                table,
                hooks: PolicyHooks {
                    allow: m.grants.fetch.clone(),
                    remaining: m.limits.max_outbound,
                    per_request: m.limits.max_outbound,
                    quota,
                    #[cfg(test)]
                    test_loopback: false,
                },
                limits,
                fsguard: None,
                log: None,
                cpu_budget: m.limits.cpu_ms,
                cpu_used: 0,
                clock: Arc::new(PollClock::new()),
            },
        );
        store.limiter(|s| &mut s.limits);
        store.epoch_deadline_callback(|mut s| {
            let d = s.data_mut();
            d.cpu_used += 1;
            if d.cpu_used.max(d.clock.used_ms()) >= d.cpu_budget {
                return Err(Trap::Interrupt.into());
            }
            Ok(UpdateDeadline::Yield(1))
        });
        store.set_epoch_deadline(1);
        store
    }
}

/// Reset the per-request compute budget and outbound call count.
pub fn start_request(store: &mut Store<Guarded>) {
    let d = store.data_mut();
    d.limits.memory_denied = false;
    d.cpu_used = 0;
    d.clock.reset();
    if let Some(log) = &d.log {
        log.start_request();
    }
    d.hooks.remaining = d.hooks.per_request;
    store.set_epoch_deadline(1);
}

pub enum CallError {
    BadRequest(String),
    Deadline,
    Trap { kind: TrapKind, text: String },
    NoResponse,
    ResponseTooLarge,
    ResponseBody(String),
}

/// Why the guest stopped with a trap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKind {
    /// The epoch callback interrupted it: `cpu_ms` was used up.
    CpuLimit,
    /// It was refused memory by `memory_mb` and then trapped (a failed allocation aborts).
    MemoryLimit,
    /// Anything else: a panic, an abort, an out-of-bounds access, a stack overflow.
    AppTrap,
}

impl TrapKind {
    /// The error kind clients and logs see.
    pub fn as_str(self) -> &'static str {
        match self {
            TrapKind::CpuLimit => "cpu_limit",
            TrapKind::MemoryLimit => "memory_limit",
            TrapKind::AppTrap => "app_trap",
        }
    }
}

/// Classify a trap from the error's type, not its text: `Trap::Interrupt` is the CPU budget, and
/// a trap after `memory_mb` refused a growth (`memory_denied`) is the memory limit.
fn trap_kind(error: &wasmtime::Error, memory_denied: bool) -> TrapKind {
    if error.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
        TrapKind::CpuLimit
    } else if memory_denied {
        TrapKind::MemoryLimit
    } else {
        TrapKind::AppTrap
    }
}

/// Run one request through the app's handler and collect the response, all within `deadline`.
///
/// The response body is read while the handler runs: a guest writing a body larger than the pipe
/// buffer blocks until the host reads, so waiting for the handler to return first would stall
/// until the deadline. A body over `max_response` bytes stops the request. After any error the
/// caller must drop the instance.
pub async fn call_handler(
    store: &mut Store<Guarded>,
    proxy: &Proxy,
    req: http::Request<Bytes>,
    deadline: Duration,
    max_response: usize,
) -> Result<http::Response<Bytes>, CallError> {
    start_request(store);
    let req = req.map(|b| {
        Full::new(b).map_err(|e: std::convert::Infallible| -> hyper::Error { match e {} })
    });
    let (tx, rx) = tokio::sync::oneshot::channel();
    let prep = (|| -> wasmtime::Result<_> {
        let r = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, req)?;
        let o = store.data_mut().http().new_response_outparam(tx)?;
        Ok((r, o))
    })();
    let (r, o) = prep.map_err(|e| CallError::BadRequest(format!("{e:#}")))?;
    let clock = store.data().clock.clone();
    let handler = proxy
        .wasi_http_incoming_handler()
        .call_handle(&mut *store, r, o);

    let trap = |e: wasmtime::Error| CallError::Trap {
        kind: trap_kind(&e, false),
        text: format!("{e:#}"),
    };
    let run = async {
        tokio::pin!(handler);
        let call = std::future::poll_fn(|cx| {
            clock.enter();
            let r = handler.as_mut().poll(cx);
            clock.leave();
            r
        });
        tokio::pin!(call);
        let mut rx = rx;
        let mut called = false;
        let resp = loop {
            tokio::select! {
                r = &mut rx => break r,
                c = &mut call, if !called => {
                    c.map_err(trap)?;
                    called = true;
                }
            }
        };
        let Ok(Ok(resp)) = resp else {
            return Err(CallError::NoResponse);
        };
        let (parts, body) = resp.into_parts();
        let collect = Limited::new(body, max_response).collect();
        tokio::pin!(collect);
        let mut bytes = None;
        while !called || bytes.is_none() {
            tokio::select! {
                c = &mut call, if !called => {
                    c.map_err(trap)?;
                    called = true;
                }
                b = &mut collect, if bytes.is_none() => match b {
                    Ok(b) => bytes = Some(b.to_bytes()),
                    Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
                        return Err(CallError::ResponseTooLarge);
                    }
                    Err(e) => {
                        if !called {
                            (&mut call).await.map_err(trap)?;
                        }
                        return Err(CallError::ResponseBody(format!("{e}")));
                    }
                },
            }
        }
        Ok(http::Response::from_parts(parts, bytes.unwrap_or_default()))
    };
    let result = tokio::time::timeout(deadline, run)
        .await
        .unwrap_or(Err(CallError::Deadline));
    match result {
        Err(CallError::Trap {
            kind: TrapKind::AppTrap,
            text,
        }) if store.data().limits.memory_denied() => Err(CallError::Trap {
            kind: TrapKind::MemoryLimit,
            text,
        }),
        other => other,
    }
}

impl WasiView for Guarded {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for Guarded {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(uri: &str, host: Option<&str>) -> http::Request<WasiBody> {
        let mut b = http::Request::builder().uri(uri);
        if let Some(h) = host {
            b = b.header(http::header::HOST, h);
        }
        b.body(
            http_body_util::Empty::new()
                .map_err(|_| unreachable!())
                .boxed_unsync(),
        )
        .unwrap()
    }

    fn refusal(wat: &str, grants: &Grants) -> Option<Refusal> {
        let engine = engine().unwrap();
        let wasm = wat::parse_str(wat).unwrap();
        let comp = Component::new(&engine, wasm).unwrap();
        check_component(&engine, &comp, grants).err()
    }

    fn grants(data: bool, fetch: &[&str]) -> Grants {
        Grants {
            data,
            fetch: fetch.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_component_with_an_ungranted_import_is_refused_with_the_fix() {
        let wat = r#"(component (import "wasi:sockets/tcp@0.2.0" (instance)))"#;
        let r = refusal(wat, &grants(false, &[])).expect("refused");
        assert_eq!(r.kind, "ungranted_import");
        assert!(
            r.message.contains("wasi:sockets/tcp@0.2.0"),
            "{}",
            r.message
        );
        assert!(r.fix.contains("raw sockets cannot be granted"), "{}", r.fix);

        let wat = r#"(component (import "wasi:filesystem/types@0.2.0" (instance)))"#;
        let r = refusal(wat, &grants(false, &[])).expect("refused without data");
        assert!(r.fix.contains("data = true"), "{}", r.fix);
    }

    #[test]
    fn a_component_without_an_http_handler_is_refused() {
        let r = refusal("(component)", &grants(false, &[])).expect("refused");
        assert_eq!(r.kind, "no_http_handler");
    }

    #[test]
    fn refuses_userinfo_and_mismatched_host() {
        assert!(url_refused(&req("https://api.example.com/", Some("api.example.com"))).is_none());
        assert!(
            url_refused(&req(
                "https://API.example.com./x",
                Some("api.example.com:443")
            ))
            .is_none()
        );
        assert!(url_refused(&req("http://[::1]:80/", Some("[::1]:80"))).is_none());
        assert!(url_refused(&req("https://evil.example:x@api.example.com:443/", None)).is_some());
        assert!(url_refused(&req("https://u@api.example.com/", None)).is_some());
        assert!(url_refused(&req("https://api.example.com/", Some("evil.example"))).is_some());
        assert!(
            url_refused(&req(
                "https://api.example.com/",
                Some("evil@api.example.com")
            ))
            .is_some()
        );
    }

    #[test]
    fn trap_kinds_come_from_the_error_type() {
        let interrupt = wasmtime::Error::from(Trap::Interrupt);
        assert_eq!(trap_kind(&interrupt, false), TrapKind::CpuLimit);
        assert_eq!(trap_kind(&interrupt, true), TrapKind::CpuLimit);
        let unreachable = wasmtime::Error::from(Trap::UnreachableCodeReached);
        assert_eq!(trap_kind(&unreachable, false), TrapKind::AppTrap);
        assert_eq!(trap_kind(&unreachable, true), TrapKind::MemoryLimit);
        let oob = wasmtime::Error::from(Trap::MemoryOutOfBounds);
        assert_eq!(trap_kind(&oob, false), TrapKind::AppTrap);
    }

    #[test]
    fn error_text_never_decides_the_kind() {
        for text in [
            "wasm trap: interrupt",
            "memory allocation failed",
            "hit the limit",
            "out of bounds memory access",
        ] {
            let e = wasmtime::Error::msg(text);
            assert_eq!(trap_kind(&e, false), TrapKind::AppTrap, "{text}");
        }
    }

    #[test]
    fn limiter_notes_a_refused_growth_past_the_memory_cap() {
        let cap = 1 << 20;
        let inner = StoreLimitsBuilder::new()
            .memory_size(cap)
            .trap_on_grow_failure(false)
            .build();
        let mut l = Limiter::new(inner, cap);
        assert!(l.memory_growing(0, cap, None).unwrap());
        assert!(!l.memory_denied());
        assert!(!l.memory_growing(cap, cap + (64 << 10), None).unwrap());
        assert!(l.memory_denied());
    }

    #[test]
    fn limiter_ignores_a_growth_past_the_modules_own_maximum() {
        let cap = 1 << 20;
        let inner = StoreLimitsBuilder::new()
            .memory_size(cap)
            .trap_on_grow_failure(false)
            .build();
        let mut l = Limiter::new(inner, cap);
        assert!(!l.memory_growing(0, cap / 2, Some(cap / 4)).unwrap());
        assert!(!l.memory_denied());
    }

    /// A local HTTP server that answers every request with `len` bytes and adds up the request
    /// body bytes it received.
    async fn origin(len: usize) -> (u16, Arc<AtomicU64>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(AtomicU64::new(0));
        let total = seen.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let total = total.clone();
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(
                        move |req: http::Request<hyper::body::Incoming>| {
                            let total = total.clone();
                            async move {
                                let n = req
                                    .into_body()
                                    .collect()
                                    .await
                                    .map_or(0, |c| c.to_bytes().len());
                                total.fetch_add(n as u64, Ordering::Relaxed);
                                Ok::<_, std::convert::Infallible>(http::Response::new(Full::new(
                                    Bytes::from(vec![b'x'; len]),
                                )))
                            }
                        },
                    );
                    let io = wasmtime_wasi_http::io::TokioIo::new(sock);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
        (port, seen)
    }

    fn hooks_for(quota: &Arc<AppQuota>, allow: &str) -> PolicyHooks {
        PolicyHooks {
            allow: vec![allow.into()],
            remaining: 100,
            per_request: 100,
            quota: Some(quota.clone()),
            test_loopback: true,
        }
    }

    /// One outbound POST of `body` through the hooks, as a guest's call goes: the response body's
    /// length, or the error the guest would see.
    async fn post(h: &mut PolicyHooks, port: u16, body: usize) -> Result<usize, HttpError> {
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("http://127.0.0.1:{port}/"))
            .body(
                Full::new(Bytes::from(vec![b'r'; body]))
                    .map_err(|e: std::convert::Infallible| match e {})
                    .boxed_unsync(),
            )
            .unwrap();
        let (resp, io) =
            Box::into_pin(h.send_request(req, None, Box::new(async { Ok(()) }))).await?;
        let io = tokio::spawn(Box::into_pin(io));
        let got = resp.into_body().collect().await.map(|c| c.to_bytes().len());
        let _ = io.await;
        got
    }

    fn is_egress_refusal(e: &HttpError) -> bool {
        matches!(e, HttpError::InternalError(Some(m)) if m.starts_with("egress_budget_exhausted"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn outbound_calls_run_until_the_egress_budget_is_spent_and_neighbors_keep_theirs() {
        let (port, seen) = origin(3000).await;
        let q = Arc::new(AppQuota::new(Some(crate::quota::Override {
            egress_bytes_per_hour: Some(10_000),
            ..Default::default()
        })));
        let mut h = hooks_for(&q, "127.0.0.1");
        // 500 bytes up and 3000 down per call: two fit in 10,000 bytes.
        assert_eq!(post(&mut h, port, 500).await.unwrap(), 3000);
        assert_eq!(post(&mut h, port, 500).await.unwrap(), 3000);
        // The third starts with 3000 left and is cut once its response spends them.
        let third = post(&mut h, port, 500).await.unwrap_err();
        assert!(is_egress_refusal(&third), "{third}");
        // From here every call is refused before it connects.
        let before = seen.load(Ordering::Relaxed);
        for _ in 0..3 {
            let e = post(&mut h, port, 500).await.unwrap_err();
            assert!(is_egress_refusal(&e), "{e}");
        }
        assert_eq!(
            seen.load(Ordering::Relaxed),
            before,
            "a refused call sends nothing"
        );
        let u = q.usage_json();
        assert_eq!(u["egress_refused"], 4, "{u}");
        assert!(u["egress_bytes_total"].as_u64().unwrap() >= 10_000, "{u}");
        assert_eq!(u["egress_bytes_left"], 0, "{u}");

        // A neighbor without a budget, and one with its own, are not touched by the first.
        let free = Arc::new(AppQuota::new(None));
        let mut h2 = hooks_for(&free, "127.0.0.1");
        for _ in 0..5 {
            assert_eq!(post(&mut h2, port, 500).await.unwrap(), 3000);
        }
        assert_eq!(free.usage_json()["egress_bytes_total"], 5 * 3500);
        let own = Arc::new(AppQuota::new(Some(crate::quota::Override {
            egress_bytes_per_hour: Some(1 << 20),
            ..Default::default()
        })));
        let mut h3 = hooks_for(&own, "127.0.0.1");
        assert_eq!(post(&mut h3, port, 500).await.unwrap(), 3000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_large_request_body_is_cut_and_a_call_the_policy_refuses_costs_nothing() {
        let (port, seen) = origin(10).await;
        let q = Arc::new(AppQuota::new(Some(crate::quota::Override {
            egress_bytes_per_hour: Some(4096),
            ..Default::default()
        })));
        let mut h = hooks_for(&q, "example.invalid");
        let e = post(&mut h, port, 100_000).await.unwrap_err();
        assert!(matches!(e, HttpError::HttpRequestDenied), "{e}");
        assert_eq!(q.usage_json()["egress_bytes_total"], 0);
        let mut h = hooks_for(&q, "127.0.0.1");
        assert!(post(&mut h, port, 100_000).await.is_err());
        assert!(
            seen.load(Ordering::Relaxed) < 100_000,
            "the body stopped at the budget ({} bytes arrived)",
            seen.load(Ordering::Relaxed)
        );
        assert!(!q.egress_admit());
    }

    #[test]
    fn the_poll_clock_counts_time_inside_polls_and_nothing_else() {
        let c = PollClock::new();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.used_ms(), 0, "time before any poll is not counted");
        c.enter();
        std::thread::sleep(Duration::from_millis(30));
        assert!(c.used_ms() >= 30, "time inside a poll shows while it runs");
        c.leave();
        let after_one = c.used_ms();
        assert!((30..300).contains(&after_one), "{after_one}");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            c.used_ms(),
            after_one,
            "waiting between polls is not counted"
        );
        c.enter();
        std::thread::sleep(Duration::from_millis(20));
        c.leave();
        assert!(c.used_ms() >= after_one + 20);
        c.reset();
        assert_eq!(c.used_ms(), 0);
    }
}
