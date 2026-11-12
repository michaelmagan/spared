//! Lifecycle tests that drive a real `Server` (real files, a real engine, real components) without
//! its listeners: what an app's directory and the app table look like after admin operations.

use super::*;
use std::sync::atomic::AtomicUsize;

/// A component with the HTTP handler export that links and does nothing when called.
pub(super) const HANDLER: &str = r#"(component
  (import "wasi:http/types@0.2.4" (instance $types
    (export "incoming-request" (type (sub resource)))
    (export "response-outparam" (type (sub resource)))))
  (alias export $types "incoming-request" (type $ireq))
  (alias export $types "response-outparam" (type $rout))
  (core module $m (func (export "handle") (param i32 i32)))
  (core instance $i (instantiate $m))
  (func $handle (param "request" (own $ireq)) (param "response-out" (own $rout))
    (canon lift (core func $i "handle")))
  (instance $h (export "handle" (func $handle)))
  (export "wasi:http/incoming-handler@0.2.4" (instance $h)))"#;

/// The same handler built differently: another hash.
pub(super) fn handler_v2() -> String {
    HANDLER.replace(
        "(func (export \"handle\") (param i32 i32))",
        "(func (export \"handle\") (param i32 i32) (nop))",
    )
}

/// Passes the name-based import and handler checks and fails only when the host links it: the
/// handler instance has no `handle` function.
pub(super) const LINK_FAILS: &str = r#"(component
  (instance $h)
  (export "wasi:http/incoming-handler@0.2.0" (instance $h)))"#;

pub(super) fn manifest(name: &str) -> String {
    format!("name = \"{name}\"\n[grants]\ndata = true\n")
}

pub(super) fn wasm(wat: &str) -> Vec<u8> {
    wat::parse_str(wat).unwrap()
}

pub(super) struct Fixture {
    pub dir: PathBuf,
    pub server: Arc<Server>,
    prefetch_mb: u64,
}

impl Fixture {
    pub fn new() -> Fixture {
        Self::with_prefetch(0)
    }

    pub fn with_prefetch(prefetch_mb: u64) -> Fixture {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "spared-lifecycle-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let server = Self::open(&dir, prefetch_mb);
        Fixture {
            dir,
            server,
            prefetch_mb,
        }
    }

    /// A new server on the same data directory: what a restart sees.
    pub fn reopen(&self) -> Arc<Server> {
        Self::open(&self.dir, self.prefetch_mb)
    }

    fn config(dir: &Path, prefetch_mb: u64) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            listen: "127.0.0.1:0".parse().unwrap(),
            admin: "127.0.0.1:0".parse().unwrap(),
            idle: Duration::from_secs(30),
            domain: None,
            examples_dir: None,
            path_routing: false,
            max: Maxima::default(),
            admin_token: "test-token-0123456789".into(),
            client_timeout: Duration::from_secs(10),
            max_connections: 16,
            prefetch_mb,
            max_backups: 4,
            access_log: false,
            caps: Default::default(),
            compile: Default::default(),
            remote: None,
        }
    }

    fn open(dir: &Path, prefetch_mb: u64) -> Arc<Server> {
        Server::new(
            Self::config(dir, prefetch_mb),
            crate::sandbox::engine().unwrap(),
        )
        .unwrap()
    }

    pub fn app(&self, name: &str) -> Option<Arc<App>> {
        self.server.apps.read().unwrap().get(name).cloned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_idle_gives_up_on_a_retired_app_still_in_the_table() {
    let fx = Fixture::new();
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    fx.app("a").unwrap().retired.store(true, Ordering::Release);
    let r = tokio::time::timeout(Duration::from_secs(2), fx.server.lock_idle("a"))
        .await
        .expect("lock_idle spun on a retired app that no newer app replaces");
    let Err(resp) = r else {
        panic!("took the lock of a retired app")
    };
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Every file of an app directory outside `data/` and the log, with its bytes, and the component
/// store's file names.
fn disk_state(fx: &Fixture, name: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = vec![];
    for root in [fx.dir.join("apps").join(name), fx.dir.join("components")] {
        for e in std::fs::read_dir(&root).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n == "data" || n == "app.log" {
                continue;
            }
            let bytes = std::fs::read(e.path()).unwrap();
            out.push((
                format!("{}/{n}", root.file_name().unwrap().to_string_lossy()),
                bytes,
            ));
        }
    }
    out.sort();
    out
}

fn listed(server: &Server, name: &str) -> Option<(u64, String)> {
    let apps = server.apps.read().unwrap();
    apps.get(name).map(|a| (a.version, a.shared.hash.clone()))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_component_that_does_not_link_changes_nothing() {
    let fx = Fixture::new();
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    let (before, listed_before) = (disk_state(&fx, "a"), listed(&fx.server, "a"));
    for bytes in [wasm(LINK_FAILS), vec![0u8; 53]] {
        let e = fx
            .server
            .install("a", &manifest("a"), &bytes, false, &Creator::Admin)
            .await
            .expect_err("a component that does not link was installed");
        assert_eq!(e.kind, "invalid_component", "{}", e.message);
        assert_eq!(disk_state(&fx, "a"), before);
        assert_eq!(listed(&fx.server, "a"), listed_before);
    }
    let restarted = fx.reopen();
    assert_eq!(listed(&restarted, "a"), listed_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_deploy_that_does_not_link_leaves_no_app_directory() {
    let fx = Fixture::new();
    let e = fx
        .server
        .install(
            "new",
            &manifest("new"),
            &wasm(LINK_FAILS),
            false,
            &Creator::Admin,
        )
        .await
        .expect_err("installed");
    assert_eq!(e.kind, "invalid_component");
    assert!(!fx.dir.join("apps/new").exists());
    assert!(listed(&fx.server, "new").is_none());
    assert_eq!(
        std::fs::read_dir(fx.dir.join("components"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redeploy_replaces_the_whole_release_and_leaves_no_staged_file() {
    let fx = Fixture::new();
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    let v1 = listed(&fx.server, "a").unwrap();
    fx.server
        .install(
            "a",
            &manifest("a"),
            &wasm(&handler_v2()),
            false,
            &Creator::Admin,
        )
        .await
        .unwrap();
    let v2 = listed(&fx.server, "a").unwrap();
    assert_eq!(v2.0, 2);
    assert_ne!(v1.1, v2.1);
    let names: Vec<String> = disk_state(&fx, "a").into_iter().map(|(n, _)| n).collect();
    assert!(
        names
            .iter()
            .all(|n| !n.ends_with(".tmp") && !n.ends_with("release.pending")),
        "{names:?}"
    );
    assert!(
        !fx.dir
            .join("components")
            .join(format!("{}.cwasm", v1.1))
            .exists()
    );
    assert_eq!(listed(&fx.reopen(), "a"), Some(v2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deploy_that_stopped_before_its_commit_leaves_the_old_release() {
    let fx = Fixture::new();
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    let v1 = listed(&fx.server, "a").unwrap();
    let v2 = wasm(&handler_v2());
    fx.server
        .stage_release(
            &fx.dir.join("apps/a"),
            &sha256_hex(&v2),
            &v2,
            None,
            &manifest("a"),
            2,
        )
        .unwrap();
    assert_eq!(listed(&fx.reopen(), "a"), Some(v1));
    assert!(!fx.dir.join("apps/a/component.sha256.tmp").exists());
    assert!(!fx.dir.join("apps/a/manifest.toml.tmp").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deploy_that_stopped_after_its_commit_is_finished_at_start() {
    let fx = Fixture::new();
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    let v2 = wasm(&handler_v2());
    let hash = sha256_hex(&v2);
    let dir = fx.dir.join("apps/a");
    fx.server
        .stage_release(&dir, &hash, &v2, None, &manifest("a"), 2)
        .unwrap();
    std::fs::write(dir.join(RELEASE_PENDING), b"").unwrap();
    let restarted = fx.reopen();
    assert_eq!(listed(&restarted, "a"), Some((2, hash.clone())));
    assert_eq!(
        std::fs::read_to_string(dir.join("component.sha256")).unwrap(),
        hash
    );
    assert!(!dir.join(RELEASE_PENDING).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_ahead_after_a_restart_covers_each_distinct_component_once() {
    let fx = Fixture::with_prefetch(1024);
    for (name, wat) in [
        ("a", HANDLER.to_string()),
        ("b", HANDLER.to_string()),
        ("c", handler_v2()),
    ] {
        fx.server
            .install(name, &manifest(name), &wasm(&wat), false, &Creator::Admin)
            .await
            .unwrap();
    }
    let restarted = fx.reopen();
    let warmed = |n: &str| {
        let a = restarted.apps.read().unwrap().get(n).cloned().unwrap();
        a.shared.warmed.load(Ordering::Acquire)
    };
    assert!(
        !warmed("a") && !warmed("c"),
        "nothing is read before the pass runs"
    );
    let report = restarted.start_prefetch().unwrap().finish();
    assert_eq!(report.components, 2);
    assert_eq!(report.missing, 0);
    let stored: u64 = ["a", "c"]
        .iter()
        .map(|n| {
            let a = restarted.apps.read().unwrap().get(*n).cloned().unwrap();
            std::fs::metadata(restarted.component_path(&a.shared.hash))
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(report.bytes, stored);
    assert_eq!(
        restarted.metrics.prefetch_bytes.load(Ordering::Relaxed),
        stored
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_budget_turns_reading_ahead_off_at_start_and_at_wake() {
    let fx = Fixture::with_prefetch(0);
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    assert!(fx.server.start_prefetch().is_none());
    let shared = fx.app("a").unwrap().shared.clone();
    fx.server.hint_cold_component(&shared);
    assert!(!shared.warmed.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_wake_of_a_component_checks_it_and_later_wakes_do_not() {
    let fx = Fixture::with_prefetch(1024);
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    let shared = fx.app("a").unwrap().shared.clone();
    shared.warmed.store(false, Ordering::Release);
    fx.server.hint_cold_component(&shared);
    assert!(shared.warmed.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_stops_promptly_while_the_prefetch_pass_is_running() {
    let fx = Fixture::with_prefetch(1024);
    for i in 0..5 {
        let wat = HANDLER.replace(
            "(func (export \"handle\") (param i32 i32))",
            &format!(
                "(func (export \"handle\") (param i32 i32) {})",
                "(nop) ".repeat(i + 1)
            ),
        );
        fx.server
            .install(
                &format!("app{i}"),
                &manifest(&format!("app{i}")),
                &wasm(&wat),
                false,
                &Creator::Admin,
            )
            .await
            .unwrap();
    }
    let mut cfg = Fixture::config(&fx.dir, 1024);
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    cfg.admin = "127.0.0.1:0".parse().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let engine = crate::sandbox::engine().unwrap();
    let listeners = super::bind_listeners(cfg.listen, cfg.admin).unwrap();
    let lock = super::lock_data_dir(&cfg.data_dir).unwrap();
    let serving = tokio::spawn(super::serve(
        cfg,
        engine,
        async move {
            let _ = stopped.await;
        },
        listeners,
        lock,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asked = Instant::now();
    let _ = stop.send(());
    let done = tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .expect("serve did not return within 3 s of the stop signal");
    assert!(done.unwrap().is_ok());
    assert!(asked.elapsed() < Duration::from_secs(3));
}

/// A fixture whose component store is on another file system (`/dev/shm`), where hard links
/// fail and every app directory gets its own copy. `None` where no such directory exists.
fn fixture_without_links() -> Option<(Fixture, PathBuf)> {
    use std::os::unix::fs::MetadataExt;
    let shm = Path::new("/dev/shm");
    let fx = Fixture::new();
    if std::fs::metadata(shm).ok()?.dev() == std::fs::metadata(&fx.dir).ok()?.dev() {
        return None;
    }
    let store = shm.join(format!(
        "spared-components-{}",
        fx.dir.file_name()?.to_str()?
    ));
    std::fs::create_dir_all(&store).ok()?;
    let components = fx.dir.join("components");
    std::fs::remove_dir_all(&components).ok()?;
    std::os::unix::fs::symlink(&store, &components).ok()?;
    Some((fx, store))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_that_cannot_hard_link_keeps_its_components_across_restarts() {
    let Some((fx, store)) = fixture_without_links() else {
        eprintln!("skipped: no second file system to put the component store on");
        return;
    };
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    let hash = fx.app("a").unwrap().shared.hash.clone();
    let stored = store.join(format!("{hash}.cwasm"));
    assert_eq!(std::fs::metadata(&stored).unwrap().nlink(), 1);
    drop(fx.reopen());
    assert!(
        stored.is_file(),
        "the first restart swept a component an app runs"
    );
    let second = fx.reopen();
    assert_eq!(
        listed(&second, "a"),
        Some((1, hash.clone())),
        "the app vanished"
    );
    std::fs::remove_file(&stored).unwrap();
    std::fs::remove_file(stored.with_extension("wasm")).unwrap();
    let third = fx.reopen();
    assert_eq!(
        listed(&third, "a"),
        Some((1, hash.clone())),
        "a store file that is gone is put back from the app directory's copy"
    );
    assert!(stored.is_file());
    third.delete("a").await;
    assert!(
        !stored.exists(),
        "deleting the last app that runs it still removes the file"
    );
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_component_two_apps_share_stays_when_one_goes_even_without_hard_links() {
    let Some((fx, store)) = fixture_without_links() else {
        eprintln!("skipped: no second file system to put the component store on");
        return;
    };
    for n in ["a", "b"] {
        fx.server
            .install(n, &manifest(n), &wasm(HANDLER), false, &Creator::Admin)
            .await
            .unwrap();
    }
    let hash = fx.app("a").unwrap().shared.hash.clone();
    let stored = store.join(format!("{hash}.cwasm"));
    fx.server.delete("a").await;
    assert!(stored.is_file(), "b still runs it");
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_stored_with_a_fetch_entry_this_version_refuses_still_loads() {
    let fx = Fixture::new();
    fx.server
        .install("a", &manifest("a"), &wasm(HANDLER), false, &Creator::Admin)
        .await
        .unwrap();
    std::fs::write(
        fx.dir.join("apps/a/manifest.toml"),
        "name = \"a\"\n[grants]\ndata = true\nfetch = [\"https://example.com/x\", \"example.org\"]\n",
    )
    .unwrap();
    let restarted = fx.reopen();
    let app = restarted
        .apps
        .read()
        .unwrap()
        .get("a")
        .cloned()
        .expect("the app was skipped");
    assert_eq!(app.manifest.grants.fetch, vec!["example.org".to_string()]);
}
