//! Page-cache warming for the compiled components in `<data-dir>/components`.
//!
//! wasmtime maps a compiled component from its file and reads only the headers when it loads
//! it. The code pages arrive later, as major faults on the first requests, one read at a time.
//! When the file is not in the page cache (after a restart or a reboot, or once the kernel has
//! reclaimed it) the first wake of every app pays for that. Reading the files ahead of the first
//! request moves the cost to a background thread. Nothing here pins memory: the pages stay
//! ordinary page cache that the kernel can reclaim.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::thread::JoinHandle;
use std::thread::Thread;
use std::time::{Duration, Instant, SystemTime};

/// Bytes read per call. The thread looks at its stop flag between calls, so this is also how much
/// work a shutdown waits for.
const CHUNK: usize = 1 << 20;

/// Fraction of a component's pages that must be in the page cache for it to count as warm.
const WARM_FRACTION: f64 = 0.9;

/// A stored component that may be worth reading ahead.
pub struct Candidate {
    pub path: PathBuf,
    /// When the apps using it were last deployed or written to. Newer is read first.
    pub recency: SystemTime,
}

/// How the thread paces itself. The default is as fast as the disk allows; tests slow it down.
#[derive(Clone, Copy)]
pub struct Pace {
    pub chunk: usize,
    pub pause: Duration,
}

impl Default for Pace {
    fn default() -> Self {
        Pace {
            chunk: CHUNK,
            pause: Duration::ZERO,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    /// Components read in full.
    pub components: u64,
    pub bytes: u64,
    /// Files that were gone or unreadable when their turn came.
    pub missing: u64,
    /// Files left out because they did not fit in what remained of the budget.
    pub over_budget: u64,
    pub cancelled: bool,
    pub elapsed: Duration,
    /// CPU time the thread used.
    pub cpu: Duration,
    #[cfg(test)]
    pub read: Vec<PathBuf>,
}

struct Control {
    stop: AtomicBool,
    bytes: AtomicU64,
}

/// A running read-ahead pass. Dropping it asks the thread to stop and does not wait for it.
pub struct Prefetch {
    control: Arc<Control>,
    wake: Option<Thread>,
    #[cfg(test)]
    thread: Option<JoinHandle<Report>>,
}

impl Prefetch {
    /// Start a thread that builds its list with `plan`, orders it newest first and reads files
    /// until `budget` bytes are used. Every byte read is also added to `counter`.
    pub fn start(
        budget: u64,
        pace: Pace,
        counter: Arc<AtomicU64>,
        plan: impl FnOnce() -> Vec<Candidate> + Send + 'static,
    ) -> Prefetch {
        let control = Arc::new(Control {
            stop: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
        });
        let c = control.clone();
        let thread = std::thread::Builder::new()
            .name("spared-prefetch".into())
            .spawn(move || run(&c, budget, pace, &counter, plan))
            .ok();
        Prefetch {
            control,
            wake: thread.as_ref().map(|t| t.thread().clone()),
            #[cfg(test)]
            thread,
        }
    }

    /// Ask the thread to stop after the call it is in. A pause between calls ends at once.
    pub fn cancel(&self) {
        self.control.stop.store(true, Ordering::Release);
        if let Some(t) = &self.wake {
            t.unpark();
        }
    }

    #[cfg(test)]
    /// Bytes read so far.
    pub fn bytes_read(&self) -> u64 {
        self.control.bytes.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    #[cfg(test)]
    /// Wait for the thread and return what it did.
    pub fn finish(mut self) -> Report {
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The candidates for a set of apps given as (component hash, app directory): one
/// per distinct component, ranked by its busiest app. An app counts as recent by the latest of
/// its deploy, its log and its database file; a file that is not there counts as never.
pub fn plan(apps: Vec<(String, PathBuf)>) -> Vec<Candidate> {
    let mtime = |p: PathBuf| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    let mut best: std::collections::HashMap<String, Candidate> = Default::default();
    for (hash, dir) in apps {
        let recency = [
            dir.join("component.sha256"),
            dir.join("app.log"),
            dir.join("data").join("app.db"),
        ]
        .into_iter()
        .map(mtime)
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH);
        let c = Candidate {
            path: dir.join("component.cwasm"),
            recency,
        };
        match best.get(&hash) {
            Some(old) if old.recency >= recency => {}
            _ => {
                best.insert(hash, c);
            }
        }
    }
    best.into_values().collect()
}

/// The part of `flag_bytes` the thread may read, given how much memory the kernel says is free
/// for caching: half of it at most, so reading ahead never competes with the apps for memory.
/// `None` means the amount is unknown and the flag stands.
pub fn cap_budget(flag_bytes: u64, headroom: Option<u64>) -> u64 {
    match headroom {
        Some(h) => flag_bytes.min(h / 2),
        None => flag_bytes,
    }
}

/// Memory available for page cache: the kernel's `MemAvailable`, lowered to what is left under a
/// cgroup v2 limit when there is one.
pub fn memory_headroom() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    let host = kb.saturating_mul(1024);
    let read = |f: &str| {
        std::fs::read_to_string(format!("/sys/fs/cgroup/{f}"))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    };
    match (read("memory.max"), read("memory.current")) {
        (Some(max), Some(used)) => Some(host.min(max.saturating_sub(used))),
        _ => Some(host),
    }
}

fn run(
    control: &Control,
    budget: u64,
    pace: Pace,
    counter: &AtomicU64,
    plan: impl FnOnce() -> Vec<Candidate>,
) -> Report {
    lower_priority();
    let started = Instant::now();
    let mut plan = plan();
    plan.sort_by_key(|c| std::cmp::Reverse(c.recency));
    let mut report = Report::default();
    let mut left = budget;
    let mut reader = Reader::new(pace.chunk.max(4096));
    for c in plan {
        if control.stop.load(Ordering::Acquire) {
            report.cancelled = true;
            break;
        }
        let Ok(file) = File::open(&c.path) else {
            report.missing += 1;
            continue;
        };
        let Ok(size) = file.metadata().map(|m| m.len()) else {
            report.missing += 1;
            continue;
        };
        if size > left {
            report.over_budget += 1;
            continue;
        }
        match reader.read_all(&file, size, pace.pause, control, counter) {
            Outcome::Done => {
                left -= size;
                report.components += 1;
                report.bytes += size;
                #[cfg(test)]
                report.read.push(c.path.clone());
            }
            Outcome::Stopped => {
                report.cancelled = true;
                break;
            }
            Outcome::Failed => report.missing += 1,
        }
    }
    report.elapsed = started.elapsed();
    report.cpu = thread_cpu_time();
    if !report.cancelled && (report.components > 0 || report.over_budget > 0) {
        let left_out = if report.over_budget > 0 {
            format!(
                "; {} not read because the budget (--prefetch-mb) ran out",
                report.over_budget
            )
        } else {
            String::new()
        };
        eprintln!(
            "prefetch: {} components, {} MB read into the page cache in {} ms ({} ms of CPU){left_out}",
            report.components,
            report.bytes >> 20,
            report.elapsed.as_millis(),
            report.cpu.as_millis()
        );
    }
    report
}

enum Outcome {
    Done,
    Stopped,
    Failed,
}

/// Reads a file to the end and throws the bytes away. On Linux the kernel moves them to
/// `/dev/null` itself (`sendfile`), so no copy through user space is made.
struct Reader {
    chunk: usize,
    sink: Option<File>,
    buf: Vec<u8>,
}

impl Reader {
    fn new(chunk: usize) -> Reader {
        let sink = if cfg!(target_os = "linux") {
            File::options().write(true).open("/dev/null").ok()
        } else {
            None
        };
        Reader {
            chunk,
            sink,
            buf: Vec::new(),
        }
    }

    fn read_all(
        &mut self,
        file: &File,
        size: u64,
        pause: Duration,
        control: &Control,
        counter: &AtomicU64,
    ) -> Outcome {
        let mut off = 0u64;
        while off < size {
            if control.stop.load(Ordering::Acquire) {
                return Outcome::Stopped;
            }
            let want = (size - off).min(self.chunk as u64) as usize;
            let n = match self.pull(file, off, want) {
                Ok(0) | Err(_) => return Outcome::Failed,
                Ok(n) => n as u64,
            };
            off += n;
            control.bytes.fetch_add(n, Ordering::Relaxed);
            counter.fetch_add(n, Ordering::Relaxed);
            if !pause.is_zero() {
                std::thread::park_timeout(pause);
            }
        }
        Outcome::Done
    }

    fn pull(&mut self, file: &File, off: u64, want: usize) -> std::io::Result<usize> {
        #[cfg(target_os = "linux")]
        if let Some(sink) = &self.sink {
            use std::os::fd::AsRawFd;
            let mut at = off as libc::off_t;
            loop {
                let n =
                    unsafe { libc::sendfile(sink.as_raw_fd(), file.as_raw_fd(), &mut at, want) };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let e = std::io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EINVAL | libc::ENOSYS) => {
                        self.sink = None;
                        break;
                    }
                    _ => return Err(e),
                }
            }
        }
        use std::os::unix::fs::FileExt;
        if self.buf.len() < self.chunk {
            self.buf.resize(self.chunk, 0);
        }
        file.read_at(&mut self.buf[..want], off)
    }
}

/// Nice 19 and the lowest best-effort I/O priority for the calling thread, so reading ahead
/// yields to the requests it is meant to speed up.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_BE_LOWEST: libc::c_long = (2 << 13) | 7;
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_BE_LOWEST,
        );
    }
}

fn thread_cpu_time() -> Duration {
    #[cfg(target_os = "linux")]
    {
        let mut t = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) } == 0 {
            return Duration::new(t.tv_sec as u64, t.tv_nsec as u32);
        }
    }
    Duration::ZERO
}

/// Pages of the mapped range `[addr, addr + len)` that are in the page cache, and the total.
/// `None` where the question cannot be asked (not Linux, or the range is not mapped).
pub fn resident_pages(addr: usize, len: usize) -> Option<(usize, usize)> {
    #[cfg(target_os = "linux")]
    {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as usize;
        let start = addr & !(page - 1);
        let len = len + (addr - start);
        let pages = len.div_ceil(page);
        let mut vec = vec![0u8; pages];
        let r = unsafe { libc::mincore(start as *mut libc::c_void, len, vec.as_mut_ptr()) };
        (r == 0).then(|| (vec.iter().filter(|b| **b & 1 == 1).count(), pages))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (addr, len);
        None
    }
}

/// Spots checked by [`looks_cold`] and the pages looked at in each. Eight small `mincore` calls
/// cost a few microseconds, against about forty for the whole of a 7 MB component.
const SAMPLE_SPOTS: usize = 8;
const SAMPLE_PAGES: usize = 16;

/// Whether the mapped component at `[addr, addr + len)` is mostly missing from the page cache,
/// judged from a few spread-out spots rather than every page. `false` where the question cannot
/// be asked.
pub fn looks_cold(addr: usize, len: usize) -> bool {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as usize;
    let pages = len.div_ceil(page);
    let (mut resident, mut seen) = (0, 0);
    if pages <= SAMPLE_SPOTS * SAMPLE_PAGES {
        let Some((r, n)) = resident_pages(addr, len) else {
            return false;
        };
        (resident, seen) = (r, n);
    } else {
        for i in 0..SAMPLE_SPOTS {
            let first = i * (pages - SAMPLE_PAGES) / (SAMPLE_SPOTS - 1);
            let Some((r, n)) = resident_pages(addr + first * page, SAMPLE_PAGES * page) else {
                return false;
            };
            resident += r;
            seen += n;
        }
    }
    seen > 0 && (resident as f64) < seen as f64 * WARM_FRACTION
}

/// Ask the kernel to read the file at `path` into the page cache in the background and return its
/// size. This is a hint: the call does not wait for the disk.
pub fn read_ahead(path: &Path) -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let Ok(file) = File::open(path) else {
            return 0;
        };
        let Ok(size) = file.metadata().map(|m| m.len()) else {
            return 0;
        };
        let mut off = 0u64;
        while off < size {
            let n = (size - off).min(CHUNK as u64);
            unsafe {
                libc::posix_fadvise(
                    file.as_raw_fd(),
                    off as libc::off_t,
                    n as libc::off_t,
                    libc::POSIX_FADV_WILLNEED,
                );
            }
            off += n;
        }
        size
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let d =
                std::env::temp_dir().join(format!("spared-prefetch-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }

        fn file(&self, name: &str, size: usize) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, vec![7u8; size]).unwrap();
            p
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn candidate(path: PathBuf, age_secs: u64) -> Candidate {
        Candidate {
            path,
            recency: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
        }
    }

    fn run_to_end(budget: u64, plan: Vec<Candidate>) -> (Report, u64) {
        let counter = Arc::new(AtomicU64::new(0));
        let p = Prefetch::start(budget, Pace::default(), counter.clone(), move || plan);
        let r = p.finish();
        (r, counter.load(Ordering::Relaxed))
    }

    #[test]
    fn reads_every_file_and_counts_the_bytes() {
        let d = Dir::new("all");
        let a = candidate(d.file("a", 100_000), 1);
        let b = candidate(d.file("b", 3_000_000), 2);
        let (r, counted) = run_to_end(u64::MAX, vec![a, b]);
        assert_eq!((r.components, r.bytes, r.missing), (2, 3_100_000, 0));
        assert_eq!(counted, 3_100_000);
    }

    #[test]
    fn the_budget_is_kept_and_the_newest_files_go_first() {
        let d = Dir::new("budget");
        let oldest = candidate(d.file("old", 100_000), 300);
        let newest = candidate(d.file("new", 100_000), 1);
        let middle = candidate(d.file("mid", 100_000), 100);
        let (r, counted) = run_to_end(250_000, vec![oldest, newest, middle]);
        assert_eq!((r.components, r.bytes, r.over_budget), (2, 200_000, 1));
        assert_eq!(counted, 200_000);
        assert_eq!(r.read, vec![d.0.join("new"), d.0.join("mid")]);
    }

    #[test]
    fn a_file_too_big_for_what_is_left_does_not_stop_smaller_ones() {
        let d = Dir::new("fit");
        let big = candidate(d.file("big", 500_000), 1);
        let small = candidate(d.file("small", 10_000), 2);
        let (r, _) = run_to_end(100_000, vec![big, small]);
        assert_eq!((r.components, r.over_budget), (1, 1));
        assert_eq!(r.read, vec![d.0.join("small")]);
    }

    #[test]
    fn a_zero_budget_reads_nothing() {
        let d = Dir::new("zero");
        let a = candidate(d.file("a", 10_000), 1);
        let (r, counted) = run_to_end(0, vec![a]);
        assert_eq!((r.components, r.bytes, counted), (0, 0, 0));
    }

    #[test]
    fn a_file_that_disappeared_is_skipped_and_the_rest_are_read() {
        let d = Dir::new("gone");
        let gone = candidate(d.0.join("never-there"), 1);
        let here = candidate(d.file("here", 50_000), 2);
        let (r, _) = run_to_end(u64::MAX, vec![gone, here]);
        assert_eq!((r.components, r.missing), (1, 1));
        assert_eq!(r.read, vec![d.0.join("here")]);
    }

    #[test]
    fn cancel_stops_a_slow_pass_within_a_moment() {
        let d = Dir::new("cancel");
        let plan: Vec<Candidate> = (0..4)
            .map(|i| candidate(d.file(&format!("f{i}"), 1 << 20), i))
            .collect();
        let pace = Pace {
            chunk: 4096,
            pause: Duration::from_millis(50),
        };
        let p = Prefetch::start(u64::MAX, pace, Arc::default(), move || plan);
        let waited = Instant::now();
        while p.bytes_read() == 0 {
            assert!(
                waited.elapsed() < Duration::from_secs(5),
                "the pass never began"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let cancelled_at = Instant::now();
        p.cancel();
        while !p.is_finished() {
            assert!(
                cancelled_at.elapsed() < Duration::from_secs(1),
                "the pass kept running after cancel"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let r = p.finish();
        assert!(r.cancelled);
        assert!(r.bytes < 4 << 20, "read {} bytes after cancel", r.bytes);
    }

    #[test]
    fn dropping_the_handle_stops_the_pass() {
        let d = Dir::new("drop");
        let a = candidate(d.file("a", 1 << 20), 1);
        let counter = Arc::new(AtomicU64::new(0));
        let pace = Pace {
            chunk: 4096,
            pause: Duration::from_millis(50),
        };
        let p = Prefetch::start(u64::MAX, pace, counter.clone(), move || vec![a]);
        let waited = Instant::now();
        while counter.load(Ordering::Relaxed) == 0 {
            assert!(
                waited.elapsed() < Duration::from_secs(5),
                "the pass never began"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(p);
        std::thread::sleep(Duration::from_millis(300));
        let after_stop = counter.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(counter.load(Ordering::Relaxed), after_stop);
        assert!(after_stop < 1 << 20);
    }

    #[test]
    fn the_budget_follows_the_free_memory() {
        assert_eq!(cap_budget(1024 << 20, None), 1024 << 20);
        assert_eq!(cap_budget(1024 << 20, Some(600 << 20)), 300 << 20);
        assert_eq!(cap_budget(100 << 20, Some(600 << 20)), 100 << 20);
        assert_eq!(cap_budget(0, Some(600 << 20)), 0);
        assert_eq!(cap_budget(1024 << 20, Some(0)), 0);
    }

    #[cfg(target_os = "linux")]
    fn map(file: &File, len: usize) -> usize {
        use std::os::fd::AsRawFd;
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        p as usize
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_component_in_the_page_cache_does_not_look_cold() {
        let d = Dir::new("warm");
        let path = d.file("c", 1 << 20);
        let _ = std::fs::read(&path).unwrap();
        let file = File::open(&path).unwrap();
        let addr = map(&file, 1 << 20);
        let (resident, pages) = resident_pages(addr, 1 << 20).unwrap();
        assert_eq!(resident, pages);
        assert!(!looks_cold(addr, 1 << 20));
        unsafe { libc::munmap(addr as *mut libc::c_void, 1 << 20) };
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_component_missing_from_the_page_cache_looks_cold_and_is_read_ahead() {
        use std::os::fd::AsRawFd;
        let d = Dir::new("cold");
        let path = d.file("c", 1 << 20);
        let file = File::open(&path).unwrap();
        file.sync_all().unwrap();
        unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        let addr = map(&file, 1 << 20);
        let (resident, pages) = resident_pages(addr, 1 << 20).unwrap();
        if resident == pages {
            unsafe { libc::munmap(addr as *mut libc::c_void, 1 << 20) };
            return;
        }
        assert!(looks_cold(addr, 1 << 20));
        assert_eq!(read_ahead(&path), 1 << 20);
        unsafe { libc::munmap(addr as *mut libc::c_void, 1 << 20) };
    }
}
