//! EXPERIMENTAL: `runtime = "wasip3-experimental"`, apps whose one instance serves several
//! requests at once and interleaves them at their await points.
//!
//! An instance is one store driven by one `run_concurrent` loop on a task of its own. Requests
//! reach it over a channel, at most `concurrency` at a time. Each takes a semaphore permit that
//! it keeps until its whole task group has finished, which can be after its response, and each
//! answer comes back on its own oneshot channel. What this gives up against wasip2, because of
//! what wasmtime 50 can do, is written in docs/wasip3.md:
//!
//! - a trap stops the store, so every request in flight on the instance fails with it,
//! - guest code that computes holds the store, so the other requests wait while it runs,
//! - one request cannot be cancelled. At its deadline the client gets 504 and the instance takes
//!   no more requests. A task group still running `deadline_ms` after its request started has the
//!   same effect, and the store is dropped, which stops every task in it, `deadline_ms` later.
//!
//! Per-request budgets (`cpu_ms`, `max_outbound` and the log allowance) are kept per wasmtime task
//! group. A task group is the root task of one host-to-guest call, which here is one request, plus
//! every subtask it starts. The `task-group-hook` feature tells [`Attribution`] which group runs.

use crate::grants::Manifest;
use crate::logcap::{AppLog, RequestLog};
use crate::quota::AppQuota;
use crate::sandbox::{CallError, Guarded, TrapKind, trap_kind};
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use wasmtime::component::{Accessor, Component, TaskGroupHook, TaskGroupId};
use wasmtime::{Engine, Store, Trap, UpdateDeadline};
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi_http::WasiHttpView;
use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::p3::bindings::{Service, ServicePre};
use wasmtime_wasi_http::p3::{Request as P3Request, Response as P3Response};

/// The name of the component's `wasi:http/handler@0.3.x` export, if it has one.
pub fn handler_export(engine: &Engine, component: &Component) -> Option<String> {
    component
        .component_type()
        .exports(engine)
        .map(|(n, _)| n.to_string())
        .find(|n| n.starts_with("wasi:http/handler@0.3."))
}

/// Link `component` against [`crate::sandbox::linker_p3`]: refuses a component whose imports or
/// handler export do not match what the host provides.
pub fn link(engine: &Engine, component: &Component) -> wasmtime::Result<ServicePre<Guarded>> {
    ServicePre::new(crate::sandbox::linker_p3(engine)?.instantiate_pre(component)?)
}

/// The budgets every request starts with.
#[derive(Clone, Copy, Debug)]
struct Budget {
    cpu_ticks: u64,
    outbound: u32,
    log_bytes: u64,
}

/// What one task group (one request) has left.
#[derive(Clone, Copy, Debug)]
struct Ledger {
    ticks: u64,
    outbound_left: u32,
    log_left: u64,
    noted: bool,
}

impl Ledger {
    fn new(b: Budget) -> Ledger {
        Ledger {
            ticks: 0,
            outbound_left: b.outbound,
            log_left: b.log_bytes,
            noted: false,
        }
    }
}

struct Ledgers {
    budget: Budget,
    /// The task group whose code runs now, as the hook last reported it.
    current: Option<TaskGroupId>,
    groups: HashMap<TaskGroupId, Ledger>,
    /// What each request keeps until its task group has finished, the response notwithstanding.
    holds: HashMap<TaskGroupId, Hold>,
    /// Holds of finished groups, released by the instance's loop between polls (see
    /// [`Attribution::release_finished`]).
    finished: Vec<Hold>,
    /// Ticks seen while no group was entered, since a group last was. Bounded by `cpu_ticks` too.
    loose_ticks: u64,
    /// Log bytes written while no group was entered, as one request's allowance over the instance.
    loose_log: Ledger,
}

/// Per-request budgets of one wasip3 instance, kept by task group.
pub struct Attribution(Mutex<Ledgers>);

impl Attribution {
    fn new(budget: Budget) -> Attribution {
        Attribution(Mutex::new(Ledgers {
            budget,
            current: None,
            groups: HashMap::new(),
            holds: HashMap::new(),
            finished: Vec::new(),
            loose_ticks: 0,
            loose_log: Ledger::new(budget),
        }))
    }

    /// One epoch tick (about a millisecond) of guest code, charged to the group that runs. True
    /// when that group, or code outside any group, has used up `cpu_ms`.
    fn tick(&self) -> bool {
        let mut l = self.0.lock().unwrap();
        let budget = l.budget;
        match l.current {
            Some(g) => {
                let e = l.groups.entry(g).or_insert_with(|| Ledger::new(budget));
                e.ticks += 1;
                e.ticks >= budget.cpu_ticks
            }
            None => {
                l.loose_ticks += 1;
                l.loose_ticks >= budget.cpu_ticks
            }
        }
    }

    /// Keep `hold` until task group `id` finishes. A group that already has (it ended before
    /// this ran) releases it at once.
    fn hold(&self, id: TaskGroupId, hold: Hold) {
        let mut l = self.0.lock().unwrap();
        if l.groups.contains_key(&id) {
            let _released = l.holds.insert(id, hold);
        }
    }

    /// Let go of the holds of the groups that finished. The loop does it between polls, and after
    /// it has recorded how it ended, so that when a trap stops every group at once, no request
    /// gets a place on the instance before the instance says it is closed.
    fn release_finished(&self) {
        let finished = std::mem::take(&mut self.0.lock().unwrap().finished);
        drop(finished);
    }

    /// Take one outbound call from the running request's `max_outbound`. False when it has none
    /// left, or when no request's code runs (the call cannot be charged, so it is refused).
    pub fn take_outbound(&self) -> bool {
        let mut l = self.0.lock().unwrap();
        let budget = l.budget;
        let Some(g) = l.current else {
            return false;
        };
        let e = l.groups.entry(g).or_insert_with(|| Ledger::new(budget));
        if e.outbound_left == 0 {
            return false;
        }
        e.outbound_left -= 1;
        true
    }

    fn with_log<T>(&self, f: impl FnOnce(&mut Ledger) -> T) -> T {
        let mut l = self.0.lock().unwrap();
        let budget = l.budget;
        match l.current {
            Some(g) => f(l.groups.entry(g).or_insert_with(|| Ledger::new(budget))),
            None => f(&mut l.loose_log),
        }
    }

    #[cfg(test)]
    fn groups(&self) -> usize {
        self.0.lock().unwrap().groups.len()
    }
}

impl RequestLog for Attribution {
    fn left(&self) -> u64 {
        self.with_log(|e| e.log_left)
    }

    fn spend(&self, n: u64) {
        self.with_log(|e| e.log_left = e.log_left.saturating_sub(n));
    }

    fn first_drop(&self) -> bool {
        self.with_log(|e| !std::mem::replace(&mut e.noted, true))
    }
}

/// What a request holds until its whole task group is done: its place among the instance's
/// `concurrency`, and the signal the deadline watcher waits on.
struct Hold {
    _slot: OwnedSemaphorePermit,
    _finished: oneshot::Sender<()>,
}

/// Tells [`Attribution`] which task group runs.
struct Hook(Arc<Attribution>);

impl TaskGroupHook for Hook {
    fn handle_start(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        let mut l = self.0.0.lock().unwrap();
        let fresh = Ledger::new(l.budget);
        l.groups.insert(id, fresh);
        Ok(())
    }

    fn handle_enter(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        let mut l = self.0.0.lock().unwrap();
        l.current = Some(id);
        l.loose_ticks = 0;
        Ok(())
    }

    fn handle_exit(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        let mut l = self.0.0.lock().unwrap();
        if l.current == Some(id) {
            l.current = None;
        }
        Ok(())
    }

    fn handle_finish(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        let mut l = self.0.0.lock().unwrap();
        l.groups.remove(&id);
        if let Some(hold) = l.holds.remove(&id) {
            l.finished.push(hold);
        }
        Ok(())
    }
}

/// The most stream, future, waitable and subtask handles one instance may hold. wasmtime keeps
/// about 1 KB of host memory per handle, outside `memory_mb`, and its default is 100,000. A guest
/// over the cap traps like any other, so one app cannot grow the host's memory by leaking them.
const MAX_COMPONENT_HANDLES: usize = 8192;

/// The store of a wasip3 instance: the manifest's limits as for wasip2, with the per-request
/// budgets kept by task group. `log_per_request` is the server's per-request log allowance.
pub fn store(
    engine: &Engine,
    m: &Manifest,
    wasi: WasiCtx,
    quota: Option<Arc<AppQuota>>,
    log: Option<AppLog>,
    log_per_request: u64,
) -> Store<Guarded> {
    let mut store = Guarded::store(engine, wasi, m, quota);
    store.set_max_component_handles(MAX_COMPONENT_HANDLES);
    let attr = Arc::new(Attribution::new(Budget {
        cpu_ticks: m.limits.cpu_ms.max(1),
        outbound: m.limits.max_outbound,
        log_bytes: log_per_request,
    }));
    if let Some(log) = &log {
        log.attach(attr.clone());
    }
    let d = store.data_mut();
    d.log = log;
    d.hooks.p3 = Some(attr.clone());
    d.p3 = Some(attr.clone());
    store.epoch_deadline_callback(|s| {
        if s.data().p3.as_ref().is_some_and(|a| a.tick()) {
            return Err(Trap::Interrupt.into());
        }
        Ok(UpdateDeadline::Yield(1))
    });
    store.set_epoch_deadline(1);
    store.task_group_hook(Hook(attr));
    store
}

struct Job {
    hold: Hold,
    req: http::Request<Bytes>,
    max_response: usize,
    reply: oneshot::Sender<Result<http::Response<Bytes>, CallError>>,
}

/// How an instance's loop ended.
#[derive(Clone, Debug)]
enum Ended {
    /// No more requests were sent and every one in flight finished.
    Drained,
    /// The guest trapped (or a budget stopped it): every request in flight failed.
    Trapped { kind: TrapKind, text: String },
}

/// A request handed to an instance, waiting for its answer.
pub struct Pending {
    rx: oneshot::Receiver<Result<http::Response<Bytes>, CallError>>,
}

/// One wasip3 app instance.
pub struct Instance {
    jobs: Mutex<Option<mpsc::UnboundedSender<Job>>>,
    /// One permit per request until its task group finishes, `concurrency` in all.
    slots: Arc<Semaphore>,
    concurrency: u32,
    /// Cleared once the instance may take no more requests: a deadline passed, or it was drained.
    open: AtomicBool,
    ended: watch::Receiver<Option<Ended>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    abort: tokio::task::AbortHandle,
    /// The app's `deadline_ms`: the longest a drain waits for the requests in flight.
    deadline: Duration,
}

impl Instance {
    /// Instantiate the component in `store` and start its loop.
    pub async fn start(
        pre: &ServicePre<Guarded>,
        mut store: Store<Guarded>,
        concurrency: u32,
        deadline: Duration,
        quota: Option<Arc<AppQuota>>,
    ) -> wasmtime::Result<Arc<Instance>> {
        let svc = pre.instantiate_async(&mut store).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (end_tx, end_rx) = watch::channel(None);
        let task = tokio::spawn(run_loop(store, svc, rx, end_tx, quota));
        Ok(Arc::new(Instance {
            jobs: Mutex::new(Some(tx)),
            slots: Arc::new(Semaphore::new(concurrency as usize)),
            concurrency,
            open: AtomicBool::new(true),
            ended: end_rx,
            abort: task.abort_handle(),
            task: Mutex::new(Some(task)),
            deadline,
        }))
    }

    /// Whether the instance takes new requests: not past a deadline, not draining, not ended.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire) && self.ended.borrow().is_none()
    }

    /// Requests in flight.
    pub fn in_flight(&self) -> usize {
        (self.concurrency as usize).saturating_sub(self.slots.available_permits())
    }

    /// A place for one request, waiting while `concurrency` are in flight. None when the
    /// instance closes while it waits: the request belongs on another instance then.
    pub async fn slot(&self) -> Option<OwnedSemaphorePermit> {
        self.slots.clone().acquire_owned().await.ok()
    }

    /// Run one request in a place of its own. See [`Instance::send`] and [`Instance::wait`].
    pub async fn call(
        self: &Arc<Self>,
        req: http::Request<Bytes>,
        deadline: Duration,
        max_response: usize,
    ) -> Result<http::Response<Bytes>, CallError> {
        let Some(slot) = self.slot().await else {
            return Err(CallError::NoResponse);
        };
        let pending = self.send(req, slot, deadline, max_response);
        self.wait(pending, deadline).await
    }

    /// Hand one request to the instance, which the caller does while it still holds whatever
    /// keeps the instance from being replaced. The request keeps `slot` until its task group has
    /// finished, which can be after its response: a task it left running still counts toward
    /// `concurrency` and `max_outbound`. If that has not happened `deadline` from now, the
    /// instance takes no more requests and is dropped `deadline_ms` later, stopping the task.
    pub fn send(
        self: &Arc<Self>,
        req: http::Request<Bytes>,
        slot: OwnedSemaphorePermit,
        deadline: Duration,
        max_response: usize,
    ) -> Pending {
        let (reply, rx) = oneshot::channel();
        let (finished, group_done) = oneshot::channel();
        let job = Job {
            hold: Hold {
                _slot: slot,
                _finished: finished,
            },
            req,
            max_response,
            reply,
        };
        let sent = match self.jobs.lock().unwrap().as_ref() {
            Some(jobs) => jobs.send(job).is_ok(),
            None => false,
        };
        if sent {
            let instance = self.clone();
            tokio::spawn(async move {
                if tokio::time::timeout(deadline, group_done).await.is_err() {
                    instance.close_after_deadline();
                }
            });
        }
        Pending { rx }
    }

    /// The answer to a request sent with [`Instance::send`]. At `deadline` the client gets
    /// [`CallError::Deadline`] and the instance takes no more requests.
    pub async fn wait(
        &self,
        pending: Pending,
        deadline: Duration,
    ) -> Result<http::Response<Bytes>, CallError> {
        match tokio::time::timeout(deadline, pending.rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(self.failure().await),
            Err(_) => {
                self.close_after_deadline();
                Err(CallError::Deadline)
            }
        }
    }

    /// Why a request got no answer: the loop's end, once it is known.
    async fn failure(&self) -> CallError {
        let mut ended = self.ended.clone();
        let end = tokio::time::timeout(Duration::from_secs(1), ended.wait_for(Option::is_some))
            .await
            .ok()
            .and_then(|r| r.ok().and_then(|e| e.clone()));
        match end {
            Some(Ended::Trapped { kind, text }) => CallError::Trap { kind, text },
            _ => CallError::NoResponse,
        }
    }

    /// Stop sending requests here. The loop ends once the requests in flight are done.
    fn close(&self) {
        self.open.store(false, Ordering::Release);
        self.slots.close();
        self.jobs.lock().unwrap().take();
    }

    /// After a request, or a task it left running, passed its deadline: take no more requests,
    /// and drop the store `deadline_ms` from now, which stops every task in it. The requests
    /// still in flight get that long to finish.
    fn close_after_deadline(&self) {
        if !self.open.swap(false, Ordering::AcqRel) {
            return;
        }
        self.close();
        let (abort, grace) = (self.abort.clone(), self.deadline);
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            abort.abort();
        });
    }

    /// Take no more requests, wait for the ones in flight (at most `deadline_ms`), then drop the
    /// store. Returns when the store is gone.
    pub async fn drain(&self) {
        self.close();
        let task = self.task.lock().unwrap().take();
        let Some(mut task) = task else {
            return;
        };
        if tokio::time::timeout(self.deadline, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// The instance's event loop: start each request as it arrives, until the channel closes and
/// the requests in flight are done, or the guest traps.
async fn run_loop(
    mut store: Store<Guarded>,
    svc: Service,
    mut rx: mpsc::UnboundedReceiver<Job>,
    end: watch::Sender<Option<Ended>>,
    quota: Option<Arc<AppQuota>>,
) {
    let attr = store.data().p3.clone();
    let result = {
        let svc = &svc;
        let rx = &mut rx;
        let fut = store.run_concurrent(async move |acc| {
            let mut tasks = FuturesUnordered::new();
            let mut open = true;
            loop {
                tokio::select! {
                    job = rx.recv(), if open => match job {
                        Some(job) => tasks.push(one(acc, svc, job)),
                        None => open = false,
                    },
                    Some(()) = tasks.next(), if !tasks.is_empty() => {}
                }
                if !open && tasks.is_empty() {
                    break;
                }
            }
        });
        tokio::pin!(fut);
        // Wall-clock time inside the loop's polls is the CPU the app's budget pays for, as the
        // poll clock measures it for a wasip2 request.
        std::future::poll_fn(|cx| {
            let t = Instant::now();
            let r = fut.as_mut().poll(cx);
            if let Some(q) = &quota {
                q.cpu_charge(t.elapsed().as_nanos() as u64);
            }
            if r.is_pending()
                && let Some(a) = &attr
            {
                a.release_finished();
            }
            r
        })
        .await
    };
    let ended = match result {
        Ok(()) => Ended::Drained,
        Err(e) => Ended::Trapped {
            kind: trap_kind(&e, store.data().limits.memory_denied()),
            text: format!("{e:#}"),
        },
    };
    let _ = end.send(Some(ended));
    if let Some(a) = &attr {
        a.release_finished();
    }
}

async fn one(acc: &Accessor<Guarded>, svc: &Service, job: Job) {
    let r = serve_one(acc, svc, job.hold, job.req, job.max_response).await;
    let _ = job.reply.send(r);
}

async fn serve_one(
    acc: &Accessor<Guarded>,
    svc: &Service,
    hold: Hold,
    req: http::Request<Bytes>,
    max_response: usize,
) -> Result<http::Response<Bytes>, CallError> {
    let bad = |e: wasmtime::Error| CallError::BadRequest(format!("{e:#}"));
    let req = req
        .map(|b| Full::new(b).map_err(|e: std::convert::Infallible| -> ErrorCode { match e {} }));
    let func = svc.wasi_http_handler().func_handle();
    let call = acc
        .with(|mut s| -> wasmtime::Result<_> {
            let attr = s.data_mut().p3.clone();
            let (r, _io) = P3Request::from_http(s.data_mut().http().hooks, req);
            let r = s.data_mut().http().table.push(r)?;
            let call = func.start_call_concurrent(s, (r,))?;
            if let Some(attr) = attr {
                attr.hold(call.group(), hold);
            }
            Ok(call)
        })
        .map_err(bad)?;
    let out = func
        .finish_call_concurrent(acc, call)
        .await
        .map_err(|e| CallError::Trap {
            kind: TrapKind::AppTrap,
            text: format!("{e:#}"),
        })?;
    let resp = match out.0 {
        Ok(r) => r,
        Err(_) => return Err(CallError::NoResponse),
    };
    let resp = acc
        .with(|mut s| -> wasmtime::Result<_> {
            let r: P3Response = s.data_mut().http().table.delete(resp)?;
            r.into_http(s, async { Ok(()) })
        })
        .map_err(|e| CallError::ResponseBody(format!("{e:#}")))?;
    let (parts, body) = resp.into_parts();
    match Limited::new(body, max_response).collect().await {
        Ok(b) => Ok(http::Response::from_parts(parts, b.to_bytes())),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            Err(CallError::ResponseTooLarge)
        }
        Err(e) => Err(CallError::ResponseBody(format!("{e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribution() -> Attribution {
        Attribution::new(Budget {
            cpu_ticks: 3,
            outbound: 1,
            log_bytes: 10,
        })
    }

    #[test]
    fn work_outside_any_request_is_bounded_and_cannot_call_out() {
        let a = attribution();
        assert!(
            !a.take_outbound(),
            "no request runs: nothing to charge it to"
        );
        assert!(!a.tick() && !a.tick());
        assert!(
            a.tick(),
            "code outside any request is bounded by cpu_ms too"
        );
        assert_eq!(a.left(), 10);
        a.spend(8);
        assert_eq!(a.left(), 2);
        assert!(a.first_drop() && !a.first_drop());
        assert_eq!(a.groups(), 0);
    }
}
