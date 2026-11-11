mod backup;
mod cli;
mod compile;
mod datadir;
mod dirfd;
mod fsguard;
mod grants;
#[cfg(feature = "lab")]
mod lab;
mod logcap;
mod obs;
mod pinned_send;
mod prefetch;
mod sandbox;
mod server;
mod signals;
mod sqlite_cache;
use clap::{Parser, Subcommand};
use grants::Maxima;
use std::path::PathBuf;
use wasmtime::Result;

/// The release version, from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The version and, when the build knew it, the git commit: `0.1.0 (1a2b3c4d5e6f)`.
pub const VERSION_LONG: &str = env!("SPARED_VERSION_LONG");

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Parser)]
#[command(
    name = "spared",
    version = VERSION_LONG,
    about = "Host small Rust web apps, each with its own SQLite database, in its own Wasm sandbox."
)]
struct Cli {
    /// Machine-readable output; errors are {"error":{"kind","message","fix"}} on stdout.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(flatten)]
    User(cli::UserCmd),
    #[cfg(feature = "lab")]
    #[command(flatten)]
    Lab(lab::LabCmd),
    /// Serve apps: public listener routes <app>.localhost, admin listener takes deploys.
    Serve {
        #[arg(long, default_value = ".spared-data")]
        data_dir: PathBuf,
        /// Public listener. Default 127.0.0.1:8080; when $PORT is set (Railway, Fly) and this is
        /// not given, 0.0.0.0:$PORT.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Admin listener (deploys; needs the bearer token). Must be a loopback address.
        #[arg(long, default_value = "127.0.0.1:8081")]
        admin_listen: std::net::SocketAddr,
        /// Allow a non-loopback --admin-listen.
        #[arg(long)]
        admin_public_i_know: bool,
        /// Admin bearer token. Default: <data-dir>/admin-token, generated (mode 0600) on first start.
        #[arg(long, env = "SPARED_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
        /// Also serve every app at /_/<app>/ on any host. This puts all apps on one browser
        /// origin (no isolation between them in a visitor's browser): bundled examples only.
        #[arg(long, env = "SPARED_PATH_ROUTING", value_parser = clap::builder::BoolishValueParser::new())]
        allow_path_routing: bool,
        /// Highest limits.memory_mb an app may ask for.
        #[arg(long, env = "SPARED_MAX_MEMORY_MB", default_value_t = Maxima::default().memory_mb)]
        max_memory_mb: usize,
        /// Highest limits.cpu_ms (compute per request).
        #[arg(long, env = "SPARED_MAX_CPU_MS", default_value_t = Maxima::default().cpu_ms)]
        max_cpu_ms: u64,
        /// Highest limits.deadline_ms (wall clock per request).
        #[arg(long, env = "SPARED_MAX_DEADLINE_MS", default_value_t = Maxima::default().deadline_ms)]
        max_deadline_ms: u64,
        /// Highest limits.max_outbound (HTTP calls per request).
        #[arg(long, env = "SPARED_MAX_OUTBOUND", default_value_t = Maxima::default().max_outbound)]
        max_outbound: u32,
        /// Request body cap, in MB, for every app.
        #[arg(long, env = "SPARED_MAX_REQUEST_MB", default_value_t = Maxima::default().request_mb)]
        max_request_mb: usize,
        /// MB of request bodies the server holds in memory at once, over every connection and
        /// app. A request whose body would pass it gets 503 server_busy with Retry-After. Never
        /// below --max-request-mb, so one request of that size can always proceed alone.
        #[arg(long, env = "SPARED_MAX_BODY_BUFFER_MB", default_value_t = fsguard::AppCaps::default().body_buffer_bytes >> 20)]
        max_body_buffer_mb: u64,
        /// Response body cap, in MB, for every app.
        #[arg(long, env = "SPARED_MAX_RESPONSE_MB", default_value_t = Maxima::default().response_mb)]
        max_response_mb: usize,
        /// Seconds a client gets to send its request headers, and again its request body.
        #[arg(long, env = "SPARED_CLIENT_TIMEOUT_SECS", default_value_t = 10)]
        client_timeout_secs: u64,
        /// Open connections allowed on the public listener; more wait to be accepted.
        #[arg(long, env = "SPARED_MAX_CONNECTIONS", default_value_t = 256)]
        max_connections: usize,
        /// MB of stored components to read into the page cache at start, newest first, so the
        /// first request to an app after a restart does not wait for its code to come off the
        /// disk. 0 turns reading ahead off, here and at wake. Never more than half of the free
        /// memory. The pages stay ordinary page cache.
        #[arg(long, env = "SPARED_PREFETCH_MB", default_value_t = 1024)]
        prefetch_mb: u64,
        /// Backups kept per app; the oldest are removed after each new one.
        #[arg(long, env = "SPARED_MAX_BACKUPS", default_value_t = 20)]
        max_backups_per_app: usize,
        /// Files, directories and links one app may keep under /data. Also bounds what a single
        /// directory listing can pull into the server's memory.
        #[arg(long, env = "SPARED_MAX_APP_ENTRIES", default_value_t = fsguard::AppCaps::default().max_entries)]
        max_app_entries: u64,
        /// Files and directories one app's code may hold open at once (each is a descriptor of
        /// the server process).
        #[arg(long, env = "SPARED_MAX_APP_FDS", default_value_t = fsguard::AppCaps::default().max_open)]
        max_app_fds: usize,
        /// Bytes, in MB, one app may keep under /data (a sparse file counts at its full size).
        /// When it is reached, writes fail the way a full disk does.
        #[arg(long, env = "SPARED_MAX_APP_DATA_MB", default_value_t = fsguard::AppCaps::default().max_data_bytes >> 20)]
        max_app_data_mb: u64,
        /// KB one request may write to its app's log; the rest is dropped.
        #[arg(long, env = "SPARED_MAX_LOG_KB_PER_REQUEST", default_value_t = logcap::LogCaps::default().per_request >> 10)]
        max_log_kb_per_request: u64,
        /// KB one app may write to its log in a second; the rest is dropped.
        #[arg(long, env = "SPARED_MAX_LOG_KB_PER_SEC", default_value_t = logcap::LogCaps::default().per_second >> 10)]
        max_log_kb_per_sec: u64,
        /// Requests one app may have running or waiting at once; more get 503 app_busy with
        /// Retry-After, so a slow app cannot use up every connection.
        #[arg(long, env = "SPARED_MAX_APP_QUEUE", default_value_t = fsguard::AppCaps::default().max_queue)]
        max_app_queue: usize,
        /// Largest component upload, in MB.
        #[arg(long, env = "SPARED_MAX_COMPONENT_MB", default_value_t = compile::CompileLimits::default().max_component_bytes >> 20)]
        max_component_mb: usize,
        /// Largest single function in a component, in KB of wasm. Real apps stay far below this;
        /// the compiler's memory grows much faster than a function's size.
        #[arg(long, env = "SPARED_MAX_FUNCTION_KB", default_value_t = compile::CompileLimits::default().max_function_bytes >> 10)]
        max_function_kb: usize,
        /// Most locals one function may declare.
        #[arg(long, env = "SPARED_MAX_FUNCTION_LOCALS", default_value_t = compile::CompileLimits::default().max_function_locals)]
        max_function_locals: u64,
        /// Most functions one component may hold.
        #[arg(long, env = "SPARED_MAX_COMPONENT_FUNCTIONS", default_value_t = compile::CompileLimits::default().max_functions)]
        max_component_functions: u64,
        /// Address space, in MB, the compiler process may use; a deploy that needs more is refused.
        #[arg(long, env = "SPARED_COMPILE_MEMORY_MB", default_value_t = compile::CompileLimits::default().memory_bytes >> 20)]
        compile_memory_mb: u64,
        /// Seconds the compiler process may run before it is stopped and the deploy refused.
        #[arg(long, env = "SPARED_COMPILE_TIMEOUT_SECS", default_value_t = compile::CompileLimits::default().timeout.as_secs())]
        compile_timeout_secs: u64,
        /// Threads the compiler process may use (compiles run one at a time).
        #[arg(long, env = "SPARED_COMPILE_THREADS", default_value_t = compile::CompileLimits::default().threads)]
        compile_threads: usize,
        #[arg(long, default_value_t = 30)]
        idle_secs: u64,
        /// Also route <app>.<domain> to the app (in addition to <app>.localhost).
        #[arg(long, env = "SPARED_DOMAIN")]
        domain: Option<String>,
        /// At startup, deploy every <dir>/<name>/ holding app.wasm and spared.toml unless the app exists.
        #[arg(long, env = "SPARED_EXAMPLES")]
        examples_dir: Option<PathBuf>,
        /// One JSON line per request on stdout: time, listener, app, method, path, status,
        /// duration and request id. Never the query string, a header or a body.
        #[arg(long, env = "SPARED_ACCESS_LOG", value_enum, default_value = "on")]
        access_log: OnOff,
    },
    /// Compile the component on stdin and write the artifact to stdout; spawned by the server.
    #[command(hide = true)]
    CompileWorker {
        #[arg(long)]
        memory_mb: u64,
        #[arg(long)]
        cpu_secs: u64,
    },
}

const MIN_SERVER_WORKERS: usize = 4;

fn main() -> Result<()> {
    if sqlite_cache::is_wrapper_invocation() {
        sqlite_cache::wrapper_main();
    }
    let cli = Cli::parse();
    if let Cmd::CompileWorker {
        memory_mb,
        cpu_secs,
    } = cli.cmd
    {
        compile::worker_main(memory_mb, cpu_secs);
    }
    // The server runs apps on all cores so one app's compute cannot hold up the others or the
    // admin API. Benchmarks and one-shot commands keep the single-thread runtime they measured.
    // At least 4 workers even on one core: a guest's file call runs on the worker that polls it
    // (see docs/design.md), so one app's slow write must not stall the listeners.
    let mut rt = if matches!(cli.cmd, Cmd::Serve { .. } | Cmd::User(cli::UserCmd::Dev)) {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(cores.max(MIN_SERVER_WORKERS));
        b
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    rt.enable_all()
        .build()?
        .block_on(run(cli))
        .map(|never| match never {})
}

/// Handle SIGTERM and SIGINT: the first one resolves the returned future (the server then shuts
/// down gracefully), a second one while that runs exits at once.
fn shutdown_on_signal() -> Result<impl std::future::Future<Output = ()> + Send + 'static> {
    let signals = signals::install()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let first = signals.next().await;
        eprintln!("received {}, shutting down", signals::name(first));
        let _ = tx.send(());
        let second = signals.next().await;
        eprintln!("received {} again, exiting now", signals::name(second));
        std::process::exit(128 + second);
    });
    Ok(async move {
        if rx.await.is_err() {
            std::future::pending::<()>().await;
        }
    })
}

async fn run(cli: Cli) -> Result<std::convert::Infallible> {
    match cli.cmd {
        Cmd::User(u) => std::process::exit(cli::run(u, cli.json).await),
        #[cfg(feature = "lab")]
        Cmd::Lab(l) => {
            lab::run(l).await?;
            std::process::exit(0)
        }
        Cmd::Serve {
            data_dir,
            listen,
            admin_listen,
            admin_public_i_know,
            admin_token,
            allow_path_routing,
            max_memory_mb,
            max_cpu_ms,
            max_deadline_ms,
            max_outbound,
            max_request_mb,
            max_body_buffer_mb,
            max_response_mb,
            client_timeout_secs,
            max_connections,
            prefetch_mb,
            max_backups_per_app,
            max_app_entries,
            max_app_fds,
            max_app_data_mb,
            max_log_kb_per_request,
            max_log_kb_per_sec,
            max_app_queue,
            max_component_mb,
            max_function_kb,
            max_function_locals,
            max_component_functions,
            compile_memory_mb,
            compile_timeout_secs,
            compile_threads,
            idle_secs,
            domain,
            examples_dir,
            access_log,
        } => {
            // First, so a SIGTERM during a slow start (compiling the examples) is seen.
            let stop = shutdown_on_signal()?;
            if std::env::args().any(|a| a == "--admin-token" || a.starts_with("--admin-token=")) {
                eprintln!(
                    "warning: --admin-token on the command line is readable by every local user (/proc/<pid>/cmdline). Use SPARED_ADMIN_TOKEN, or let the server generate <data-dir>/admin-token"
                );
            }
            if let Err(e) = server::check_admin_bind(admin_listen, admin_public_i_know) {
                refuse(cli.json, &e);
            }
            let listen = match (listen, std::env::var("PORT")) {
                (Some(l), _) => l,
                (None, Ok(p)) if !p.is_empty() => {
                    let Ok(port) = p.parse::<u16>() else {
                        refuse(
                            cli.json,
                            &datadir::DataDirError {
                                kind: "invalid_port_env",
                                message: format!("$PORT is '{p}', not a port number"),
                                fix: "unset PORT or pass --listen ADDR:PORT".into(),
                            },
                        );
                    };
                    std::net::SocketAddr::from(([0, 0, 0, 0], port))
                }
                _ => "127.0.0.1:8080".parse().unwrap(),
            };
            // Before the token or anything else is written: refuse a data dir whose layout this
            // build does not read.
            if let Err(e) = datadir::check(&data_dir) {
                refuse(cli.json, &e);
            }
            // Then the ports and the data dir, so a server that cannot have them leaves no token
            // file behind, and two servers never run on one data dir.
            let listeners = match server::bind_listeners(listen, admin_listen) {
                Ok(l) => l,
                Err(e) => refuse(cli.json, &e),
            };
            let data_lock = match server::lock_data_dir(&data_dir) {
                Ok(l) => l,
                Err(e) => refuse(cli.json, &e),
            };
            let admin_token = match server::admin_token(&data_dir, admin_token) {
                Ok(t) => t,
                Err(e) => refuse(
                    cli.json,
                    &datadir::DataDirError {
                        kind: "admin_token_failed",
                        message: format!(
                            "cannot set up the admin token in {}: {e:#}",
                            data_dir.display()
                        ),
                        fix: "make the data dir writable, or pass --admin-token".into(),
                    },
                ),
            };
            server::share_token_with_cli(&admin_token);
            server::serve(
                server::Config {
                    data_dir,
                    listen,
                    admin: admin_listen,
                    idle: std::time::Duration::from_secs(idle_secs),
                    domain,
                    examples_dir,
                    path_routing: allow_path_routing,
                    max: Maxima {
                        memory_mb: max_memory_mb,
                        cpu_ms: max_cpu_ms,
                        deadline_ms: max_deadline_ms,
                        max_outbound,
                        request_mb: max_request_mb,
                        response_mb: max_response_mb,
                    },
                    admin_token,
                    client_timeout: std::time::Duration::from_secs(client_timeout_secs.max(1)),
                    max_connections,
                    prefetch_mb,
                    max_backups: max_backups_per_app.max(1),
                    access_log: access_log == OnOff::On,
                    caps: fsguard::AppCaps {
                        max_entries: max_app_entries.max(1),
                        max_open: max_app_fds.max(1),
                        max_queue: max_app_queue.max(1),
                        body_buffer_bytes: max_body_buffer_mb.saturating_mul(1 << 20),
                        max_data_bytes: max_app_data_mb.saturating_mul(1 << 20),
                        log: logcap::LogCaps {
                            per_request: max_log_kb_per_request.saturating_mul(1 << 10),
                            per_second: max_log_kb_per_sec.saturating_mul(1 << 10),
                        },
                        ..Default::default()
                    },
                    compile: compile::CompileLimits {
                        max_component_bytes: max_component_mb.max(1) << 20,
                        max_function_bytes: max_function_kb.max(1) << 10,
                        max_function_locals: max_function_locals.max(1),
                        max_functions: max_component_functions.max(1),
                        memory_bytes: compile_memory_mb.max(64) << 20,
                        timeout: std::time::Duration::from_secs(compile_timeout_secs.max(1)),
                        threads: compile_threads.max(1),
                        ..compile::CompileLimits::isolated()
                    },
                },
                sandbox::engine()?,
                stop,
                listeners,
                data_lock,
            )
            .await?;
            // Not a return: a copy still running on the blocking pool would hold the runtime's
            // shutdown past the container's kill timeout. `serve` has already stopped everything.
            std::process::exit(0);
        }
        Cmd::CompileWorker { .. } => unreachable!("handled before the runtime starts"),
    }
}

/// Print a start-up refusal the way every other error is printed (`error[kind]:` and `fix:`
/// lines, or the JSON error object with --json) and exit 1.
fn refuse(json: bool, e: &datadir::DataDirError) -> ! {
    if json {
        println!(
            "{}",
            serde_json::json!({"error": {"kind": e.kind, "message": e.message, "fix": e.fix}})
        );
    } else {
        eprintln!("error[{}]: {}", e.kind, e.message);
        eprintln!("fix: {}", e.fix);
    }
    std::process::exit(1)
}

#[cfg(test)]
mod engine_tests {
    fn accepted(wat: &str) -> Result<(), String> {
        let wasm = wat::parse_str(wat).map_err(|e| format!("test input: {e}"))?;
        crate::sandbox::engine()
            .unwrap()
            .precompile_component(&wasm)
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    }

    fn module(body: &str) -> String {
        format!("(component (core module {body}))")
    }

    #[test]
    fn what_a_rust_and_c_guest_uses_is_accepted() {
        for (what, wat) in [
            (
                "an ordinary function",
                module("(func (result i32) i32.const 1)"),
            ),
            (
                "bulk memory",
                module("(memory 1) (func (memory.fill (i32.const 0) (i32.const 0) (i32.const 8)))"),
            ),
            (
                "simd",
                module("(func (result v128) v128.const i32x4 1 2 3 4)"),
            ),
            (
                "multi-value",
                module("(func (result i32 i32) i32.const 1 i32.const 2)"),
            ),
            (
                "sign extension",
                module("(func (param i32) (result i32) local.get 0 i32.extend8_s)"),
            ),
            (
                "saturating float conversion",
                module("(func (param f32) (result i32) local.get 0 i32.trunc_sat_f32_s)"),
            ),
            (
                "reference types",
                module("(table 1 funcref) (func (result funcref) ref.null func)"),
            ),
        ] {
            assert_eq!(accepted(&wat), Ok(()), "{what}");
        }
    }

    #[test]
    fn features_no_guest_needs_are_refused() {
        let mut wrongly_accepted = Vec::new();
        for (what, wat) in [
            (
                "garbage collection",
                module(
                    "(type $s (struct (field i32))) (func (result (ref $s)) i32.const 1 struct.new $s)",
                ),
            ),
            ("function references", module("(func (param (ref func)))")),
            ("shared memory", module("(memory 1 1 shared)")),
            (
                "atomics",
                module("(memory 1 1 shared) (func (result i32) i32.const 0 i32.atomic.load)"),
            ),
            ("exception handling", module("(tag)")),
            ("memory64", module("(memory i64 1)")),
            ("multiple memories", module("(memory 1) (memory 1)")),
            ("tail calls", module("(func $f return_call $f)")),
            (
                "relaxed simd",
                module("(func (param v128) (result v128) local.get 0 i32x4.relaxed_trunc_f32x4_s)"),
            ),
            (
                "wide arithmetic",
                module(
                    "(func (param i64 i64 i64 i64) (result i64 i64) local.get 0 local.get 1 local.get 2 local.get 3 i64.add128)",
                ),
            ),
            (
                "extended constant expressions",
                module("(global i32 (i32.add (i32.const 1) (i32.const 2)))"),
            ),
            (
                "component model async",
                "(component (core func (canon waitable-set.new)))".to_string(),
            ),
        ] {
            match accepted(&wat) {
                Ok(()) => wrongly_accepted.push(what),
                Err(e) if e.starts_with("test input") => panic!("{what}: {e}"),
                Err(_) => {}
            }
        }
        assert!(
            wrongly_accepted.is_empty(),
            "accepted: {wrongly_accepted:?}"
        );
    }
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use clap::CommandFactory;

    fn serve_arg(name: &str) -> clap::Arg {
        Cli::command()
            .find_subcommand("serve")
            .unwrap()
            .get_arguments()
            .find(|a| a.get_id() == name)
            .unwrap()
            .clone()
    }

    fn prefetch_mb(args: &[&str]) -> Result<u64, clap::Error> {
        let mut argv = vec!["spared", "serve"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv)?.cmd {
            Cmd::Serve { prefetch_mb, .. } => Ok(prefetch_mb),
            _ => unreachable!("parsed serve"),
        }
    }

    #[test]
    fn prefetch_defaults_to_a_gigabyte() {
        assert_eq!(prefetch_mb(&[]).unwrap(), 1024);
    }

    #[test]
    fn prefetch_takes_a_size_and_zero_turns_it_off() {
        assert_eq!(prefetch_mb(&["--prefetch-mb", "64"]).unwrap(), 64);
        assert_eq!(prefetch_mb(&["--prefetch-mb=0"]).unwrap(), 0);
    }

    #[test]
    fn prefetch_refuses_what_is_not_a_size() {
        assert!(prefetch_mb(&["--prefetch-mb", "lots"]).is_err());
        assert!(prefetch_mb(&["--prefetch-mb", "-1"]).is_err());
    }

    #[test]
    fn prefetch_has_an_environment_twin_named_like_the_others() {
        let env = |n: &str| {
            serve_arg(n)
                .get_env()
                .map(|e| e.to_string_lossy().into_owned())
        };
        assert_eq!(env("prefetch_mb").as_deref(), Some("SPARED_PREFETCH_MB"));
        assert_eq!(
            env("max_connections").as_deref(),
            Some("SPARED_MAX_CONNECTIONS")
        );
    }
}
