use crate::grants::{Manifest, Maxima, valid_name};
use crate::sandbox::{CallError, Guarded, TrapKind, call_handler};
use crate::sqlite_cache;
use base64::Engine as _;
use bytes::Bytes;
use clap::Subcommand;
use http_body_util::{BodyExt, Full};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::p2::bindings::ProxyPre;

mod move_app;

const DEFAULT_ADMIN: &str = "http://127.0.0.1:8081";
const SDK_VERSION: &str = "25";

/// SHA-256 of each wasi-sdk 25 tarball `spared setup` downloads, in `sha256sum` format. One file
/// serves this check, the Dockerfile and the tests; CI pins the same x86_64 Linux value.
const SDK_SHA256: &str = include_str!("../../../scripts/wasi-sdk-sha256.txt");

/// C flags for the guest's SQLite. One file serves this build step, scripts/build-examples.sh and
/// bench/env.sh; it explains each flag (HAVE_PREAD/HAVE_PWRITE keep a page to one host call).
const GUEST_CFLAGS: &str = include_str!("../../../scripts/guest-cflags.txt");

/// `CFLAGS_wasm32_wasip2` for a guest build against the wasi-sdk in `sdk`.
fn guest_cflags(sdk: &str) -> String {
    let flags: Vec<&str> = GUEST_CFLAGS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    format!("--sysroot={sdk}/share/wasi-sysroot {}", flags.join(" "))
}

#[derive(Subcommand)]
pub enum UserCmd {
    /// Create a new app crate in ./<name>, from a template (see `spared templates`). Building the
    /// app later runs its build scripts on your machine, like any cargo build.
    New {
        name: String,
        /// Template to start from (default: notes). `spared templates` lists them.
        #[arg(long, short = 't')]
        template: Option<String>,
    },
    /// List the app templates `spared new --template` accepts.
    Templates,
    /// Check this machine for what building, testing and serving apps needs.
    Doctor,
    /// Install the wasm32-wasip2 target and wasi-sdk 25 into ~/.spared.
    Setup {
        /// Print what would be done without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Build the app and request it in-process in a temp data dir. Building runs the project's
    /// build scripts on your machine, like any cargo build.
    Test {
        /// Path to GET (repeatable, default /).
        #[arg(long = "path")]
        paths: Vec<String>,
    },
    /// Build the app and deploy it to a running server. Building runs the project's build scripts
    /// on your machine, like any cargo build.
    Deploy {
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// List deployed apps.
    Ps {
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Show an app's log (defaults to the app in ./spared.toml).
    Logs {
        name: Option<String>,
        #[arg(long, default_value_t = 50)]
        lines: usize,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Delete a deployed app and its data. Asks for --yes; backups stay.
    Rm {
        name: String,
        /// Confirm the deletion.
        #[arg(long)]
        yes: bool,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Run a local server, deploy the app, and redeploy on every change. Building runs the
    /// project's build scripts on your machine, like any cargo build.
    Dev,
    /// Copy an app's data directory to <data-dir>/backups/<name>/<timestamp>/ (data only, not code).
    /// When the server has --backup-to, also upload it, and wait for the upload.
    Backup {
        name: String,
        /// Return once the local backup exists; do not wait for the upload.
        #[arg(long)]
        no_wait: bool,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// List an app's backups, newest first.
    Backups {
        name: String,
        /// List the backups in the server's bucket instead (works without the app deployed).
        #[arg(long)]
        remote: bool,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Replace an app's data with a backup (the current data is kept as a pre-restore copy).
    Restore {
        name: String,
        /// Backup id from `spared backups <name>`, or remote:<id> from `spared backups <name>
        /// --remote` (fetched and verified into a new local backup first).
        #[arg(long = "from")]
        from: String,
        /// Confirm that the app's current data is replaced.
        #[arg(long)]
        yes: bool,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Keys for encrypted remote backups.
    #[command(subcommand)]
    BackupKey(BackupKeyCmd),
    /// App tokens: a token that works for one app only, within its scopes. Making, listing and
    /// revoking them needs the admin token.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Per-app limits the operator sets: data size, entries, a CPU budget over time and an
    /// outbound HTTP budget. Each can only lower the server's caps. Needs the admin token.
    #[command(subcommand)]
    Limits(LimitsCmd),
    /// Stop serving an app on this server and keep it stopped across restarts: public requests
    /// get 503 app_frozen. Admin commands (backup, restore, logs, rm) keep working; a deploy is
    /// refused until `spared thaw`.
    Freeze {
        name: String,
        /// Why, shown by `spared ps --json` (at most 200 characters).
        #[arg(long)]
        reason: Option<String>,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Serve a frozen app again on this server.
    Thaw {
        name: String,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Move an app to another spared server through the bucket both back up to: copy its code,
    /// freeze it here, back it up, restore that backup there, check it, and serve it there.
    /// Never changes DNS and never deletes the app here.
    Move {
        name: String,
        /// Admin URL of the target server, by IP (for example through `ssh -L`).
        #[arg(long, value_name = "URL")]
        to: String,
        /// File holding the target server's admin token. Default: $SPARED_TARGET_TOKEN. Never
        /// taken as an argument.
        #[arg(long, value_name = "PATH")]
        to_token_file: Option<PathBuf>,
        /// After the restore, GET / here with the app's Host header (<name>.localhost); a 5xx
        /// or no answer fails the move, for example http://10.0.0.2:8080.
        #[arg(long, value_name = "URL")]
        smoke_url: Option<String>,
        /// Check both servers and print the plan without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Admin URL of the source server. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Drop an app's live instance now; the next request wakes a fresh one.
    Evict {
        name: String,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Measure cold-wake and warm request latency (p50, p99, max) for a deployed app.
    Bench {
        name: String,
        /// Warm requests on one connection, after the 20 evict-then-request cold cycles.
        #[arg(long, default_value_t = 2000)]
        requests: usize,
        /// GET path to request.
        #[arg(long, default_value = "/")]
        path: String,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum TokenCmd {
    /// Make a token for one app and print it once, alone on stdout. The server keeps only its
    /// SHA-256, so it cannot be shown again.
    Create {
        /// The app the token is for. It need not exist yet: a token made for a free name may
        /// create that app with its first deploy.
        app: String,
        /// Comma-separated scopes: deploy, read, evict, backup, restore, freeze, rm. Default:
        /// deploy,read,evict.
        #[arg(long, value_name = "LIST")]
        scope: Option<String>,
        /// Lifetime, such as 30d, 12h or 45m. Default: it does not expire.
        #[arg(long, value_name = "DURATION")]
        expires: Option<String>,
        /// A label shown by `spared token list`, such as ci or user-42.
        #[arg(long, value_name = "LABEL")]
        name: Option<String>,
        /// Write the token to this new file (mode 0600) instead of printing it. An existing
        /// file is never written over.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// List tokens (ids and metadata, never a token), for every app or one.
    List {
        app: Option<String>,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Revoke a token by its id; it stops working at once. A whole token is accepted too and is
    /// turned into its id here, so the secret is never sent.
    Revoke {
        id: String,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum LimitsCmd {
    /// Set limits for one app. Flags not given keep the app's current override. The app need not
    /// exist yet, so a platform can set them before the first deploy. The server's caps still
    /// apply: the smaller number is in force.
    Set {
        app: String,
        /// MB the app may keep under /data (at most --max-app-data-mb).
        #[arg(long, value_name = "MB")]
        data_mb: Option<u64>,
        /// Files, directories and links the app may keep under /data (at most --max-app-entries).
        #[arg(long, value_name = "N")]
        entries: Option<u64>,
        /// CPU budget as a share of one core over time, 1 to 100.
        #[arg(long, value_name = "P", conflicts_with = "cpu_ms_per_min")]
        cpu_percent: Option<u64>,
        /// CPU budget in milliseconds of compute per minute (60000 is one core).
        #[arg(long, value_name = "MS")]
        cpu_ms_per_min: Option<u64>,
        /// Outbound HTTP budget: request and response body bytes per hour, in MB (fractions
        /// allowed, such as 0.5).
        #[arg(long, value_name = "MB")]
        egress_mb_per_hour: Option<f64>,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Show an app's override, the limits in force and what it has used since the server started.
    Show {
        app: String,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
    /// Remove an app's override: the server's caps apply again.
    Clear {
        app: String,
        /// Admin URL of the server to talk to. Default: $SPARED_URL, then http://127.0.0.1:8081.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum BackupKeyCmd {
    /// Print a new random key for --backup-key-file. Keep a copy off this machine: without it the
    /// encrypted backups cannot be restored.
    New {
        /// Write the key to this new file (mode 0600) instead of printing it.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

pub struct CliError {
    kind: String,
    message: String,
    fix: String,
    extra: Option<Value>,
}

type R<T> = std::result::Result<T, CliError>;

fn err(kind: &str, message: impl Into<String>, fix: impl Into<String>) -> CliError {
    CliError {
        kind: kind.into(),
        message: message.into(),
        fix: fix.into(),
        extra: None,
    }
}

fn report(e: &CliError, json: bool) {
    let fix = if e.fix.is_empty() {
        "run `spared --help` and see AGENTS.md"
    } else {
        e.fix.as_str()
    };
    if json {
        let mut v = json!({"error": {"kind": e.kind, "message": e.message, "fix": fix}});
        if let Some(Value::Object(extra)) = &e.extra {
            for (k, x) in extra {
                v[k] = x.clone();
            }
        }
        println!("{v}");
    } else {
        eprintln!("error[{}]: {}", e.kind, e.message);
        eprintln!("fix: {fix}");
    }
}

pub async fn run(cmd: UserCmd, json: bool) -> i32 {
    match dispatch(cmd, json).await {
        Ok(()) => 0,
        Err(e) => {
            report(&e, json);
            1
        }
    }
}

async fn dispatch(cmd: UserCmd, json: bool) -> R<()> {
    match cmd {
        UserCmd::New { name, template } => cmd_new(&name, template.as_deref(), json),
        UserCmd::Templates => cmd_templates(json),
        UserCmd::Doctor => cmd_doctor(json).await,
        UserCmd::Setup { dry_run } => cmd_setup(dry_run, json),
        UserCmd::Test { paths } => cmd_test(paths, json).await,
        UserCmd::Deploy { server } => cmd_deploy(server, json).await,
        UserCmd::Ps { server } => cmd_ps(server, json).await,
        UserCmd::Logs {
            name,
            lines,
            server,
        } => cmd_logs(name, lines, server, json).await,
        UserCmd::Rm { name, yes, server } => cmd_rm(&name, yes, server, json).await,
        UserCmd::Dev => cmd_dev(json).await,
        UserCmd::Backup {
            name,
            no_wait,
            server,
        } => cmd_backup(&name, no_wait, server, json).await,
        UserCmd::Backups {
            name,
            remote,
            server,
        } => {
            if remote {
                cmd_remote_backups(&name, server, json).await
            } else {
                cmd_backups(&name, server, json).await
            }
        }
        UserCmd::BackupKey(BackupKeyCmd::New { out }) => cmd_backup_key_new(out, json),
        UserCmd::Token(TokenCmd::Create {
            app,
            scope,
            expires,
            name,
            out,
            server,
        }) => cmd_token_create(&app, scope, expires, name, out, server, json).await,
        UserCmd::Token(TokenCmd::List { app, server }) => cmd_token_list(app, server, json).await,
        UserCmd::Token(TokenCmd::Revoke { id, server }) => {
            cmd_token_revoke(&id, server, json).await
        }
        UserCmd::Limits(LimitsCmd::Set {
            app,
            data_mb,
            entries,
            cpu_percent,
            cpu_ms_per_min,
            egress_mb_per_hour,
            server,
        }) => {
            let asked = LimitFlags {
                data_mb,
                entries,
                cpu_percent,
                cpu_ms_per_min,
                egress_mb_per_hour,
            };
            cmd_limits_set(&app, asked, server, json).await
        }
        UserCmd::Limits(LimitsCmd::Show { app, server }) => {
            cmd_limits_show(&app, server, json).await
        }
        UserCmd::Limits(LimitsCmd::Clear { app, server }) => {
            cmd_limits_clear(&app, server, json).await
        }
        UserCmd::Freeze {
            name,
            reason,
            server,
        } => cmd_freeze(&name, reason, server, json).await,
        UserCmd::Thaw { name, server } => cmd_thaw(&name, server, json).await,
        UserCmd::Move {
            name,
            to,
            to_token_file,
            smoke_url,
            dry_run,
            server,
        } => {
            move_app::cmd_move(
                move_app::Args {
                    name,
                    to,
                    to_token_file,
                    smoke_url,
                    dry_run,
                    server,
                },
                json,
            )
            .await
        }
        UserCmd::Restore {
            name,
            from,
            yes,
            server,
        } => cmd_restore(&name, &from, yes, server, json).await,
        UserCmd::Evict { name, server } => cmd_evict(&name, server, json).await,
        UserCmd::Bench {
            name,
            requests,
            path,
            server,
        } => cmd_bench(&name, requests, &path, server, json).await,
    }
}

fn home() -> R<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        err(
            "no_home",
            "$HOME is not set",
            "set HOME, or set WASI_SDK_PATH and CARGO_TARGET_DIR explicitly",
        )
    })
}

fn io_err(what: &str, e: std::io::Error) -> CliError {
    err(
        "io",
        format!("{what}: {e}"),
        "check the path and its permissions",
    )
}

fn out_json(v: Value) {
    println!("{v}");
}

fn read_manifest(dir: &Path) -> R<(Manifest, String)> {
    let p = dir.join("spared.toml");
    let text = std::fs::read_to_string(&p).map_err(|_| {
        err(
            "no_manifest",
            format!("no spared.toml in {}", dir.display()),
            "run this command inside an app directory, or create one with `spared new <name>`",
        )
    })?;
    let m = Manifest::parse(&text).map_err(|e| err(e.kind, e.message, e.fix))?;
    if !valid_name(&m.name) {
        return Err(err(
            "bad_name",
            format!("invalid app name '{}' in spared.toml", m.name),
            "use 1 to 40 lowercase letters, digits and dashes",
        ));
    }
    Ok((m, text))
}

/// The fix for an app that cannot start. A memory limit below the app's own minimum memory is the
/// likely cause of a failure that says so, and the error names the minimum in 64 KiB pages.
fn wake_failed_fix(msg: &str) -> String {
    let pages = msg
        .split_once("memory minimum size of ")
        .filter(|_| msg.contains("exceeds memory limits"))
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok());
    match pages {
        Some(pages) => {
            let mb = (pages * 65536).div_ceil(1 << 20);
            format!("raise limits.memory_mb in spared.toml (this app needs at least {mb})")
        }
        None => "check the manifest grants; the app log above may show why".to_string(),
    }
}

/// The one-line reason `spared test` prints for a trap, worded like the server's own error.
fn trap_message(kind: TrapKind, text: &str, limits: &crate::grants::Limits) -> String {
    match kind {
        TrapKind::MemoryLimit => format!("the app hit its memory limit ({} MB)", limits.memory_mb),
        TrapKind::CpuLimit => format!("request used more than {} ms of compute", limits.cpu_ms),
        TrapKind::AppTrap => {
            let first: String = text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(300)
                .collect();
            if first.ends_with("backtrace:") {
                "the app trapped (panic or abort)".to_string()
            } else {
                first
            }
        }
    }
}

/// Printed whenever the CLI runs or deploys a `runtime = "wasip3-experimental"` app.
const WASIP3_NOTE: &str = "note: runtime wasip3-experimental is EXPERIMENTAL: one instance serves several requests at once, so one crash or timeout fails every request in flight in it, and it runs on wasmtime code its authors mark not ready for production (docs/wasip3.md)";

fn trap_fix(kind: TrapKind) -> &'static str {
    match kind {
        TrapKind::CpuLimit => "reduce work per request or raise limits.cpu_ms in spared.toml",
        TrapKind::MemoryLimit => "use less memory or raise limits.memory_mb in spared.toml",
        TrapKind::AppTrap => {
            "the app crashed; read the app log printed above (panic message), or `spared logs` when deployed"
        }
    }
}

fn lock_for(name: &str) -> String {
    lock_from(include_str!("../templates/Cargo.lock.template"), name)
}

/// A template's pinned lock file with the app's name in it, the packages kept in name order.
fn lock_from(text: &str, name: &str) -> String {
    let (head, rest) = text.split_once("[[package]]").unwrap_or((text, ""));
    let mut blocks: Vec<String> = rest
        .split("[[package]]")
        .map(|b| b.replace("name = \"__NAME__\"", &format!("name = \"{name}\"")))
        .collect();
    let key = |b: &String| {
        b.lines()
            .find_map(|l| l.strip_prefix("name = "))
            .unwrap_or("")
            .to_string()
    };
    blocks.sort_by_key(key);
    let body: Vec<String> = blocks
        .iter()
        .map(|b| format!("[[package]]{}", b.trim_end()))
        .collect();
    format!("{head}{}\n", body.join("\n\n"))
}

fn sqlite_compiled(target_dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(target_dir.join("wasm32-wasip2/release/deps")) else {
        return false;
    };
    rd.flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .starts_with("libsqlite3_sys-")
    })
}

struct Template {
    name: &'static str,
    about: &'static str,
    routes: &'static [&'static str],
    /// `src/main.rs`, or `src/lib.rs` for a wasip3 template.
    main: &'static str,
    manifest: &'static str,
    /// An EXPERIMENTAL wasip3 template: its own Cargo.toml and lock, no `src/spared.rs`.
    wasip3: Option<Wasip3Files>,
}

struct Wasip3Files {
    cargo_toml: &'static str,
    lock: &'static str,
}

macro_rules! app_template {
    ($dir:literal) => {
        (
            include_str!(concat!("../templates/apps/", $dir, "/main.rs")),
            include_str!(concat!("../templates/apps/", $dir, "/spared.toml")),
        )
    };
}

const DEFAULT_TEMPLATE: &str = "notes";

/// Every template `spared new --template` accepts, in the order `spared templates` lists them.
const TEMPLATES: &[Template] = &[
    Template {
        name: "notes",
        about: "Notes page with add and delete, plus a JSON API",
        routes: &[
            "GET /",
            "GET /api/notes",
            "POST /api/notes",
            "DELETE /api/notes/<id>",
        ],
        main: include_str!("../templates/main.rs.tmpl"),
        manifest: include_str!("../templates/spared.toml.tmpl"),
        wasip3: None,
    },
    Template {
        name: "counter",
        about: "An atomic counter: a page with a button and a JSON API",
        routes: &["GET /", "GET /api/count", "POST /api/increment"],
        main: app_template!("counter").0,
        wasip3: None,
        manifest: app_template!("counter").1,
    },
    Template {
        name: "todo",
        about: "Todo list with checkboxes: create, read, update, delete",
        routes: &[
            "GET /",
            "GET /api/todos",
            "POST /api/todos",
            "PATCH /api/todos/<id>",
            "DELETE /api/todos/<id>",
        ],
        main: app_template!("todo").0,
        wasip3: None,
        manifest: app_template!("todo").1,
    },
    Template {
        name: "guestbook",
        about: "Guestbook with HTML-escaped messages, newest first",
        routes: &["GET /", "POST /api/messages"],
        main: app_template!("guestbook").0,
        wasip3: None,
        manifest: app_template!("guestbook").1,
    },
    Template {
        name: "kv",
        about: "Key-value store over HTTP: PUT, GET, DELETE, sorted listing",
        routes: &[
            "GET /",
            "GET /kv",
            "PUT /kv/<key>",
            "GET /kv/<key>",
            "DELETE /kv/<key>",
        ],
        main: app_template!("kv").0,
        wasip3: None,
        manifest: app_template!("kv").1,
    },
    Template {
        name: "shortener",
        about: "URL shortener: create a code, 302 redirect, hit counts",
        routes: &[
            "GET /",
            "POST /api/links",
            "GET /api/links",
            "GET /api/links/<code>",
            "GET /<code>",
        ],
        main: app_template!("shortener").0,
        wasip3: None,
        manifest: app_template!("shortener").1,
    },
    Template {
        name: "webhook",
        about: "Webhook receiver: stores any JSON with a timestamp, lists the last 50, shared-secret header",
        routes: &["GET /", "POST /hook", "GET /events"],
        main: app_template!("webhook").0,
        wasip3: None,
        manifest: app_template!("webhook").1,
    },
    Template {
        name: "fetcher",
        about: "Outbound HTTPS call to a host you grant in spared.toml, returns status and size",
        routes: &["GET /", "GET /fetch"],
        main: app_template!("fetcher").0,
        wasip3: None,
        manifest: app_template!("fetcher").1,
    },
    Template {
        name: "blog",
        about: "Blog: HTML list and post pages, bearer-token protected publish endpoint",
        routes: &["GET /", "GET /posts/<slug>", "POST /api/posts"],
        main: app_template!("blog").0,
        wasip3: None,
        manifest: app_template!("blog").1,
    },
    Template {
        name: "fetcher3",
        about: "EXPERIMENTAL wasip3: the fetcher, with several requests at once in one instance (needs a server with --experimental-wasip3)",
        routes: &["GET /", "GET /fetch"],
        main: include_str!("../templates/apps/fetcher3/lib.rs"),
        manifest: include_str!("../templates/apps/fetcher3/spared.toml"),
        wasip3: Some(Wasip3Files {
            cargo_toml: include_str!("../templates/apps/fetcher3/Cargo.toml.tmpl"),
            lock: include_str!("../templates/apps/fetcher3/Cargo.lock.template"),
        }),
    },
];

fn find_template(name: &str) -> R<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name).ok_or_else(|| {
        let names: Vec<&str> = TEMPLATES.iter().map(|t| t.name).collect();
        err(
            "unknown_template",
            format!("no template named '{name}'"),
            format!("pick one of: {} (see `spared templates`)", names.join(", ")),
        )
    })
}

const HELPER_TEMPLATE: &str = include_str!("../templates/spared.rs");

/// The version in a `// spared-helper: N` line near the top of a generated `src/spared.rs`.
fn helper_version(text: &str) -> Option<u32> {
    text.lines().take(5).find_map(|l| {
        let rest = l.trim().strip_prefix("// spared-helper:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// What this CLI writes: the version of the helper `spared new` generates.
fn current_helper_version() -> u32 {
    helper_version(HELPER_TEMPLATE).expect("templates/spared.rs carries a spared-helper marker")
}

/// A warning when the app in `dir` has a `src/spared.rs` older than the helper this CLI writes.
/// No `src/spared.rs` means an app that does not use the helper: nothing to say.
fn helper_warning(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("src/spared.rs")).ok()?;
    let ours = current_helper_version();
    match helper_version(&text) {
        None => Some(format!(
            "src/spared.rs has no version marker, so it is older than helper version {ours}. Compare it with the one `spared new` writes (run `spared new scratch` elsewhere and diff the two files), and copy over what you need"
        )),
        Some(v) if v < ours => Some(format!(
            "src/spared.rs is helper version {v}; this spared writes version {ours}. Compare it with the one `spared new` writes (run `spared new scratch` elsewhere and diff the two files), and copy over what you need"
        )),
        Some(v) if v > ours => Some(format!(
            "src/spared.rs is helper version {v}, newer than the {ours} this spared writes: it came from a newer spared. Upgrade spared"
        )),
        Some(_) => None,
    }
}

fn new_files(name: &str, t: &Template) -> Vec<(&'static str, String)> {
    let sub = |text: &str| text.replace("__NAME__", name);
    if let Some(p3) = &t.wasip3 {
        return vec![
            ("Cargo.toml", sub(p3.cargo_toml)),
            ("Cargo.lock", lock_from(p3.lock, name)),
            ("spared.toml", sub(t.manifest)),
            (
                ".gitignore",
                sub(include_str!("../templates/gitignore.tmpl")),
            ),
            ("src/lib.rs", sub(t.main)),
        ];
    }
    vec![
        (
            "Cargo.toml",
            sub(include_str!("../templates/Cargo.toml.tmpl")),
        ),
        ("Cargo.lock", lock_for(name)),
        ("spared.toml", sub(t.manifest)),
        (
            ".gitignore",
            sub(include_str!("../templates/gitignore.tmpl")),
        ),
        (
            "AGENTS.md",
            sub(include_str!("../templates/AGENTS.md.tmpl")),
        ),
        ("src/main.rs", sub(t.main)),
        ("src/spared.rs", HELPER_TEMPLATE.to_string()),
    ]
}

fn cmd_templates(json: bool) -> R<()> {
    if json {
        let list: Vec<Value> = TEMPLATES
            .iter()
            .map(|t| json!({"name": t.name, "description": t.about, "routes": t.routes, "default": t.name == DEFAULT_TEMPLATE}))
            .collect();
        out_json(json!({"templates": list}));
    } else {
        let width = TEMPLATES.iter().map(|t| t.name.len()).max().unwrap_or(0);
        for t in TEMPLATES {
            let mark = if t.name == DEFAULT_TEMPLATE {
                " (default)"
            } else {
                ""
            };
            println!("{:width$}  {}{mark}", t.name, t.about);
        }
        println!("start one with: spared new <name> --template <template>");
    }
    Ok(())
}

fn cmd_new(name: &str, template: Option<&str>, json: bool) -> R<()> {
    if !valid_name(name) {
        return Err(err(
            "bad_name",
            format!("invalid app name '{name}'"),
            "use 1 to 40 lowercase letters, digits and dashes, e.g. `spared new my-app`",
        ));
    }
    let t = find_template(template.unwrap_or(DEFAULT_TEMPLATE))?;
    let dir = PathBuf::from(name);
    if dir.exists() {
        return Err(err(
            "exists",
            format!("./{name} already exists"),
            "pick another name or remove the directory",
        ));
    }
    let mut written = vec![];
    for (rel, content) in new_files(name, t) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| io_err("create dir", e))?;
        std::fs::write(&p, content).map_err(|e| io_err(&format!("write {}", p.display()), e))?;
        written.push(rel);
    }
    let mut steps = next_steps(name);
    if t.wasip3.is_some() {
        steps[2] = "spared serve --data-dir ./data --experimental-wasip3   # in another terminal, once (EXPERIMENTAL, docs/wasip3.md)".to_string();
        if !json {
            eprintln!("{WASIP3_NOTE}");
        }
    }
    if json {
        out_json(
            json!({"created": name, "path": dir, "files": written, "template": t.name, "routes": t.routes, "next": steps}),
        );
    } else {
        println!(
            "created ./{name} from the {} template ({})",
            t.name,
            written.join(", ")
        );
        println!("routes:");
        for r in t.routes {
            println!("  {r}");
        }
        println!("next:");
        for step in &steps {
            println!("  {step}");
        }
    }
    Ok(())
}

fn next_steps(name: &str) -> Vec<String> {
    vec![
        format!("cd {name}"),
        "spared test --path /             # build it and request / in a sandbox".to_string(),
        "spared serve --data-dir ./data   # in another terminal, once; leave it running"
            .to_string(),
        format!("spared deploy                    # then open http://{name}.localhost:8080/"),
    ]
}

fn find_sdk() -> Option<PathBuf> {
    let ok = |p: &Path| p.join("bin/clang").exists();
    if let Some(p) = std::env::var_os("WASI_SDK_PATH").map(PathBuf::from) {
        if ok(&p) {
            return Some(p);
        }
    }
    let base = PathBuf::from(std::env::var_os("HOME")?).join(".spared/wasi-sdk");
    if ok(&base) {
        return Some(base);
    }
    let mut kids: Vec<_> = std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| ok(p))
        .collect();
    kids.sort();
    kids.pop()
}

fn target_installed() -> bool {
    let out = Command::new("rustc")
        .args(["--print", "target-libdir", "--target", "wasm32-wasip2"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let dir = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
            std::fs::read_dir(dir)
                .map(|mut d| d.any(|e| e.is_ok()))
                .unwrap_or(false)
        }
        _ => false,
    }
}

fn sdk_platform() -> R<String> {
    let run = |a: &str| {
        Command::new("uname")
            .arg(a)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let (os, arch) = (run("-s"), run("-m"));
    let arch = match arch.as_str() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "arm64",
        other => {
            return Err(err(
                "unsupported_platform",
                format!("no wasi-sdk build for architecture '{other}'"),
                "install wasi-sdk 25 manually and set WASI_SDK_PATH to its directory",
            ));
        }
    };
    let os = match os.as_str() {
        "Linux" => "linux",
        "Darwin" => "macos",
        other => {
            return Err(err(
                "unsupported_platform",
                format!("no wasi-sdk build for OS '{other}'"),
                "install wasi-sdk 25 manually and set WASI_SDK_PATH to its directory",
            ));
        }
    };
    Ok(format!("{arch}-{os}"))
}

fn sdk_expected_sha(table: &str, file: &str) -> Option<String> {
    table.lines().find_map(|l| {
        let (hash, name) = l.split_once("  ")?;
        (name.trim() == file).then(|| hash.trim().to_ascii_lowercase())
    })
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn verify_sdk_tarball(table: &str, file: &str, path: &Path) -> R<()> {
    let Some(want) = sdk_expected_sha(table, file) else {
        return Err(err(
            "checksum_missing",
            format!("no pinned SHA-256 for {file}"),
            "install wasi-sdk 25 manually and set WASI_SDK_PATH to its directory",
        ));
    };
    let got = sha256_hex(path).map_err(|e| io_err("read the downloaded wasi-sdk archive", e))?;
    if got != want {
        return Err(err(
            "checksum_mismatch",
            format!("{file} has SHA-256 {got}, expected {want}; it was not unpacked"),
            "rerun `spared setup` (the download may have been cut off or altered on the way), or install wasi-sdk 25 yourself and set WASI_SDK_PATH",
        ));
    }
    Ok(())
}

fn cmd_setup(dry_run: bool, json: bool) -> R<()> {
    let mut steps: Vec<Value> = vec![];
    let mut say = |step: &str, status: &str, detail: String| {
        if !json {
            println!(
                "{step}: {status}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                }
            );
        }
        steps.push(json!({"step": step, "status": status, "detail": detail}));
    };

    if target_installed() {
        say(
            "rust target wasm32-wasip2",
            "already installed",
            String::new(),
        );
    } else if dry_run {
        say(
            "rust target wasm32-wasip2",
            "would run",
            "rustup target add wasm32-wasip2".into(),
        );
    } else {
        let st = Command::new("rustup")
            .args(["target", "add", "wasm32-wasip2"])
            .stdout(if json {
                Stdio::null()
            } else {
                Stdio::inherit()
            })
            .status();
        match st {
            Ok(s) if s.success() => say(
                "rust target wasm32-wasip2",
                "installed",
                "rustup target add wasm32-wasip2".into(),
            ),
            Ok(_) => {
                return Err(err(
                    "rustup_failed",
                    "`rustup target add wasm32-wasip2` failed",
                    "run it by hand and read its output; install rustup from https://rustup.rs if it is missing",
                ));
            }
            Err(e) => {
                return Err(err(
                    "no_rustup",
                    format!("cannot run rustup: {e}"),
                    "install rustup from https://rustup.rs, then rerun `spared setup`",
                ));
            }
        }
    }

    if let Some(sdk) = find_sdk() {
        say("wasi-sdk", "already present", sdk.display().to_string());
    } else {
        let platform = sdk_platform()?;
        let url = format!(
            "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-{SDK_VERSION}/wasi-sdk-{SDK_VERSION}.0-{platform}.tar.gz"
        );
        let dest = home()?.join(".spared/wasi-sdk");
        if dry_run {
            say(
                "wasi-sdk",
                "would download",
                format!("{url} -> {}", dest.display()),
            );
        } else {
            let base = home()?.join(".spared");
            std::fs::create_dir_all(&base).map_err(|e| io_err("create ~/.spared", e))?;
            let tarball = base.join("wasi-sdk.tar.gz.part");
            let tmp = base.join("wasi-sdk.tmp");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).map_err(|e| io_err("create temp dir", e))?;
            if !json {
                println!("downloading {url}");
            }
            let dl = Command::new("curl")
                .args([
                    "--proto",
                    "=https",
                    "--tlsv1.2",
                    "-fL",
                    "--retry",
                    "3",
                    "-o",
                ])
                .arg(&tarball)
                .arg(&url)
                .stdout(Stdio::null())
                .status();
            match dl {
                Ok(s) if s.success() => {}
                _ => {
                    let _ = std::fs::remove_file(&tarball);
                    return Err(err(
                        "download_failed",
                        format!("could not download {url}"),
                        "check network access (curl must reach github.com), or install wasi-sdk 25 yourself and set WASI_SDK_PATH",
                    ));
                }
            }
            let file = format!("wasi-sdk-{SDK_VERSION}.0-{platform}.tar.gz");
            if let Err(e) = verify_sdk_tarball(SDK_SHA256, &file, &tarball) {
                let _ = std::fs::remove_file(&tarball);
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
            let ex = Command::new("tar")
                .arg("xzf")
                .arg(&tarball)
                .arg("--strip-components=1")
                .arg("-C")
                .arg(&tmp)
                .status();
            let _ = std::fs::remove_file(&tarball);
            if !matches!(ex, Ok(s) if s.success()) || !tmp.join("bin/clang").exists() {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(err(
                    "extract_failed",
                    "could not unpack the wasi-sdk archive",
                    "make sure `tar` is installed and ~/.spared has free space, then rerun `spared setup`",
                ));
            }
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::rename(&tmp, &dest).map_err(|e| io_err("move wasi-sdk into place", e))?;
            say("wasi-sdk", "installed", dest.display().to_string());
        }
    }
    let step = "sqlite precompile";
    if sqlite_cache::disabled() {
        say(
            step,
            "skipped",
            format!("{} is set", sqlite_cache::ENV_DISABLE),
        );
    } else if let Some(sdk) = find_sdk() {
        if sqlite_cache_is_warm(&sdk) {
            say(step, "already built", lib_label());
        } else if dry_run {
            say(
                step,
                "would run",
                format!(
                    "compile SQLite once into {} (about a minute) so a first app build skips it",
                    lib_label()
                ),
            );
        } else {
            if !json {
                println!(
                    "compiling SQLite once into {} (about a minute)",
                    lib_label()
                );
            }
            match precompile_sqlite(&sdk, json) {
                Ok(()) if sqlite_cache_is_warm(&sdk) => say(step, "built", lib_label()),
                Ok(()) => say(
                    step,
                    "skipped",
                    "no object was cached; the first app build compiles SQLite".into(),
                ),
                Err(e) => say(
                    step,
                    "skipped",
                    format!("{}; the first app build compiles SQLite instead", e.message),
                ),
            }
        }
    } else if dry_run {
        say(
            step,
            "would run",
            format!(
                "compile SQLite once into {} after wasi-sdk is installed",
                lib_label()
            ),
        );
    }
    if json {
        out_json(json!({"dry_run": dry_run, "steps": steps}));
    }
    Ok(())
}

fn no_rust_tool(tool: &str, e: &std::io::Error) -> CliError {
    err(
        "no_cargo",
        format!("cannot run {tool}: {e}"),
        "install Rust from https://rustup.rs, then run `spared setup`",
    )
}

fn package_name(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    Some(v.get("package")?.get("name")?.as_str()?.to_string())
}

/// Every app builds into one target directory, and cargo names the final unit after the crate
/// alone, so two app directories with the same crate name share a unit: cargo sees the other
/// directory's sources as unchanged and keeps the other app's .wasm. The directory that last built
/// each crate name is recorded next to the build; a different directory drops that one unit first.
fn unit_owner_stamp(target_dir: &Path, name: &str) -> PathBuf {
    target_dir.join("spared-unit-owner").join(name)
}

fn unit_owned_by_other(target_dir: &Path, name: &str, canonical_dir: &Path) -> bool {
    match std::fs::read_to_string(unit_owner_stamp(target_dir, name)) {
        Ok(owner) => Path::new(owner.trim_end_matches('\n')) != canonical_dir,
        Err(_) => true,
    }
}

fn release_unit_from_other_dir(dir: &Path, target_dir: &Path) -> R<()> {
    let (Some(name), Ok(canonical)) = (package_name(dir), std::fs::canonicalize(dir)) else {
        return Ok(());
    };
    if !unit_owned_by_other(target_dir, &name, &canonical) {
        return Ok(());
    }
    let out = Command::new("cargo")
        .args([
            "clean",
            "--release",
            "--target",
            "wasm32-wasip2",
            "-p",
            &name,
        ])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .output()
        .map_err(|e| no_rust_tool("cargo", &e))?;
    if !out.status.success() {
        return Err(err(
            "build_failed",
            format!(
                "cargo clean -p {name} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            "remove the build directory (~/.spared/target, or $CARGO_TARGET_DIR) and rerun",
        ));
    }
    let stamp = unit_owner_stamp(target_dir, &name);
    if let Some(parent) = stamp.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err("record the build directory", e))?;
    }
    std::fs::write(&stamp, canonical.display().to_string())
        .map_err(|e| io_err("record the build directory", e))?;
    Ok(())
}

/// `cargo` set up to build guest code: wasi-sdk clang for C (through the SQLite cache when it can
/// be set up), its archiver and the shared C flags, into `target_dir`. A build script never sees
/// the admin token.
fn guest_cargo(sdk: &Path, target_dir: &Path) -> Command {
    let sdk_s = sdk.display().to_string();
    let real_cc = sdk.join("bin/clang");
    let cc = match home() {
        Ok(h) => sqlite_cache::guest_cc(&real_cc, &sqlite_cache::lib_dir(&h)),
        Err(_) => sqlite_cache::GuestCc {
            cc: real_cc,
            env: vec![],
        },
    };
    let mut cmd = Command::new("cargo");
    cmd.env("CC_wasm32_wasip2", &cc.cc)
        .env("AR_wasm32_wasip2", format!("{sdk_s}/bin/llvm-ar"))
        .env("CFLAGS_wasm32_wasip2", guest_cflags(&sdk_s))
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("SPARED_TOKEN")
        .env_remove("SPARED_ADMIN_TOKEN");
    for (k, v) in cc.env {
        cmd.env(k, v);
    }
    cmd
}

/// Where the SQLite cache lives, for messages: `~/.spared/lib` unless `$SPARED_LIB_DIR` moves it.
fn lib_label() -> String {
    match std::env::var_os(sqlite_cache::ENV_LIB_DIR) {
        Some(d) if !d.is_empty() => PathBuf::from(d).display().to_string(),
        _ => "~/.spared/lib".to_string(),
    }
}

/// The SQLite objects cached for this wasi-sdk, or none when HOME or the compiler is unreadable.
fn sqlite_cache_entries(sdk: &Path) -> Vec<sqlite_cache::Entry> {
    let (Ok(h), Some(id)) = (home(), sqlite_cache::sdk_id(&sdk.join("bin/clang"))) else {
        return vec![];
    };
    sqlite_cache::entries_for_sdk(&sqlite_cache::lib_dir(&h), &id)
}

fn sqlite_cache_has_entry(sdk: &Path) -> bool {
    !sqlite_cache::disabled() && !sqlite_cache_entries(sdk).is_empty()
}

/// The libsqlite3-sys directory a new app locks (`libsqlite3-sys-0.39.1`).
fn template_sqlite_crate() -> String {
    sqlite_cache::locked_crate_dir(include_str!("../templates/Cargo.lock.template"))
        .unwrap_or_default()
}

fn sqlite_cache_is_warm(sdk: &Path) -> bool {
    let (Ok(h), Some(id)) = (home(), sqlite_cache::sdk_id(&sdk.join("bin/clang"))) else {
        return false;
    };
    sqlite_cache::is_warm(&sqlite_cache::lib_dir(&h), &id, &template_sqlite_crate())
}

/// Compile SQLite into the cache by building libsqlite3-sys alone in a scratch project that has
/// the same manifest, lock file and release profile as a new app, so the compile command (and so
/// the cache key) is the one an app build issues. The scratch directory is removed afterwards.
fn precompile_sqlite(sdk: &Path, json: bool) -> R<()> {
    let lib = sqlite_cache::lib_dir(&home()?);
    let dir = lib.join(format!("warm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = || -> R<()> {
        std::fs::create_dir_all(dir.join("src"))
            .map_err(|e| io_err("create the scratch project", e))?;
        let name = "spared-warm";
        for (file, text) in [
            (
                "Cargo.toml",
                include_str!("../templates/Cargo.toml.tmpl").replace("__NAME__", name),
            ),
            ("Cargo.lock", lock_for(name)),
            ("src/main.rs", "fn main() {}\n".to_string()),
        ] {
            std::fs::write(dir.join(file), text)
                .map_err(|e| io_err("write the scratch project", e))?;
        }
        let st = guest_cargo(sdk, &dir.join("target"))
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "libsqlite3-sys",
                "--locked",
            ])
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(if json {
                Stdio::null()
            } else {
                Stdio::inherit()
            })
            .status()
            .map_err(|e| no_rust_tool("cargo", &e))?;
        if st.success() {
            Ok(())
        } else {
            Err(err(
                "build_failed",
                "cargo could not build libsqlite3-sys",
                "check network access to crates.io, then rerun `spared setup`",
            ))
        }
    };
    let result = run();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn build(dir: &Path, json: bool) -> R<PathBuf> {
    if !dir.join("Cargo.toml").exists() {
        return Err(err(
            "not_an_app",
            format!("no Cargo.toml in {}", dir.display()),
            "run this inside an app directory created by `spared new <name>`",
        ));
    }
    for tool in ["cargo", "rustc"] {
        if let Err(e) = Command::new(tool).arg("--version").output() {
            return Err(no_rust_tool(tool, &e));
        }
    }
    let sdk = find_sdk().ok_or_else(|| {
        err(
            "no_sdk",
            "wasi-sdk not found (needed to compile SQLite for wasm)",
            "run `spared setup`, or set WASI_SDK_PATH to a wasi-sdk 25 directory",
        )
    })?;
    if !target_installed() {
        return Err(err(
            "no_target",
            "the wasm32-wasip2 Rust target is not installed",
            "run `spared setup`",
        ));
    }
    let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(t) => PathBuf::from(t),
        None => home()?.join(".spared/target"),
    };
    release_unit_from_other_dir(dir, &target_dir)?;
    if !json {
        let uses_sqlite =
            std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|t| t.contains("rusqlite"));
        if !uses_sqlite || sqlite_compiled(&target_dir) || sqlite_cache_has_entry(&sdk) {
            eprintln!("building {}", dir.display());
        } else {
            eprintln!(
                "building {} (first build compiles SQLite and takes a minute; `spared setup` precompiles it)",
                dir.display()
            );
        }
    }
    let out = guest_cargo(&sdk, &target_dir)
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(if json {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .output()
        .map_err(|e| no_rust_tool("cargo", &e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(4000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let msg = if json {
            format!("cargo build failed:\n{tail}")
        } else {
            "cargo build failed (compiler output above)".to_string()
        };
        return Err(err(
            "build_failed",
            msg,
            "fix the compiler errors in src/ and rerun; each error names the file and line",
        ));
    }
    let mut wasm = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-artifact" {
            continue;
        }
        if let Some(files) = v["filenames"].as_array() {
            for f in files.iter().filter_map(|f| f.as_str()) {
                if f.ends_with(".wasm") && !f.contains("/deps/") {
                    wasm = Some(PathBuf::from(f));
                }
            }
        }
    }
    wasm.ok_or_else(|| {
        err(
            "no_wasm",
            "cargo produced no .wasm file",
            "the package must be a binary crate (src/main.rs) that builds for wasm32-wasip2",
        )
    })
}

struct Response {
    status: u16,
    headers: hyper::HeaderMap,
    body: Bytes,
}

fn parse_base(base: &str) -> R<String> {
    let rest = base.strip_prefix("http://").ok_or_else(|| {
        err(
            "bad_server_url",
            format!("server URL '{base}' must start with http://"),
            "pass --server http://127.0.0.1:8081",
        )
    })?;
    let hp = rest.split('/').next().unwrap_or("");
    if hp.is_empty() {
        return Err(err(
            "bad_server_url",
            format!("server URL '{base}' has no host"),
            "pass --server http://127.0.0.1:8081",
        ));
    }
    Ok(if hp.contains(':') {
        hp.to_string()
    } else {
        format!("{hp}:80")
    })
}

/// How long a refused connection is retried, so a command run right after `spared serve &` waits for the listener.
const CONNECT_RETRY: Duration = Duration::from_millis(2000);

async fn request(
    base: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Bytes,
) -> R<Response> {
    request_retrying(base, method, path, headers, body, CONNECT_RETRY).await
}

async fn request_retrying(
    base: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Bytes,
    connect_retry: Duration,
) -> R<Response> {
    let addr = parse_base(base)?;
    let unreachable = |why: String| {
        err(
            "server_unreachable",
            format!("cannot reach the spared server at {base}: {why}"),
            "start it with `spared serve` (or `spared dev` for a local loop), or point at another server with --server URL or $SPARED_URL",
        )
    };
    let retry_until = tokio::time::Instant::now() + connect_retry;
    let stream = loop {
        match tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(s)) => break s,
            Ok(Err(e))
                if e.kind() == std::io::ErrorKind::ConnectionRefused
                    && tokio::time::Instant::now() < retry_until =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(Err(e)) => return Err(unreachable(e.to_string())),
            Err(_) => return Err(unreachable("connection timed out".into())),
        }
    };
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| unreachable(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut b = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.as_str());
    for (k, v) in headers {
        b = b.header(*k, v.as_str());
    }
    let req = b
        .body(Full::new(body))
        .map_err(|e| err("bad_request", e.to_string(), "report this"))?;
    let resp = tokio::time::timeout(Duration::from_secs(120), sender.send_request(req))
        .await
        .map_err(|_| {
            err(
                "server_timeout",
                "the server did not answer within 120 s",
                "check `spared logs`, then retry",
            )
        })?
        .map_err(|e| unreachable(e.to_string()))?;
    let status = resp.status().as_u16();
    let (parts, body) = resp.into_parts();
    let body = body
        .collect()
        .await
        .map_err(|e| unreachable(e.to_string()))?
        .to_bytes();
    Ok(Response {
        status,
        headers: parts.headers,
        body,
    })
}

async fn admin(
    base: &str,
    token: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Bytes,
) -> R<Response> {
    let mut headers = headers.to_vec();
    headers.push(("authorization", format!("Bearer {token}")));
    let r = request(base, method, path, &headers, body).await?;
    if (200..300).contains(&r.status) {
        return Ok(r);
    }
    match serde_json::from_slice::<Value>(&r.body) {
        Ok(v) if v["error"]["kind"].is_string() => {
            let e = &v["error"];
            let kind = e["kind"].as_str().unwrap();
            let fix = if kind == "unauthorized" {
                UNAUTHORIZED_FIX
            } else {
                e["fix"].as_str().unwrap_or("")
            };
            Err(err(kind, e["message"].as_str().unwrap_or(""), fix))
        }
        _ => Err(err(
            &format!("http_{}", r.status),
            format!(
                "server answered {}: {}",
                r.status,
                String::from_utf8_lossy(&r.body)
                    .chars()
                    .take(300)
                    .collect::<String>()
                    .trim()
            ),
            "check that the --server URL is the admin listener (default 127.0.0.1:8081), not the app listener",
        )),
    }
}

/// `~/.spared/token` holds the token of the first server started on this machine, so a second
/// server (another data dir, `spared dev` in an app directory) is refused until the CLI is given its token.
const UNAUTHORIZED_FIX: &str = "this server has a different token than the CLI sent: run `export SPARED_TOKEN=$(cat <data-dir>/admin-token)`, where <data-dir> is the directory you started this server with (`./.spared` for `spared dev`); ~/.spared/token holds only the first server's token";

fn token_export_line(data_dir: &Path) -> String {
    format!(
        "export SPARED_TOKEN=$(cat '{}')",
        data_dir.join("admin-token").display()
    )
}

/// The admin token: $SPARED_TOKEN, then ~/.spared/token (written by `spared serve`).
fn token() -> R<String> {
    let nonempty = |t: String| Some(t.trim().to_string()).filter(|t| !t.is_empty());
    let from_file = || {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".spared/token");
        std::fs::read_to_string(p).ok().and_then(nonempty)
    };
    std::env::var("SPARED_TOKEN")
        .ok()
        .and_then(nonempty)
        .or_else(from_file)
        .ok_or_else(|| {
            err(
                "no_token",
                "no admin token: $SPARED_TOKEN is not set and ~/.spared/token does not exist",
                "start `spared serve` on this machine once (it writes ~/.spared/token), or set SPARED_TOKEN to the contents of <data-dir>/admin-token on the server",
            )
        })
}

fn token_present() -> R<()> {
    let set = std::env::var("SPARED_TOKEN").is_ok_and(|t| !t.trim().is_empty());
    let file =
        std::env::var_os("HOME").is_some_and(|h| PathBuf::from(h).join(".spared/token").is_file());
    if set || file {
        Ok(())
    } else {
        token().map(|_| ())
    }
}

fn server_url(flag: Option<String>) -> String {
    flag.or_else(|| std::env::var("SPARED_URL").ok())
        .unwrap_or_else(|| DEFAULT_ADMIN.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn json_body(r: &Response) -> R<Value> {
    serde_json::from_slice(&r.body).map_err(|e| {
        err(
            "bad_response",
            format!("server sent invalid JSON: {e}"),
            "check that the server and CLI are the same spared version",
        )
    })
}

async fn push(server: &str, token: &str, dir: &Path, wasm: &Path) -> R<Value> {
    let (m, text) = read_manifest(dir)?;
    let bytes = std::fs::read(wasm).map_err(|e| io_err("read the built .wasm", e))?;
    let header = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let r = admin(
        server,
        token,
        "PUT",
        &format!("/v1/apps/{}", m.name),
        &[("x-spared-manifest", header)],
        Bytes::from(bytes),
    )
    .await?;
    json_body(&r)
}

fn print_deployed(v: &Value) {
    let name = v["name"].as_str().unwrap_or("");
    let url = v["url"].as_str().unwrap_or("");
    println!("deployed {name} (version {})", v["version"]);
    if let Some(note) = v["note"].as_str() {
        println!("note: {note}");
    }
    println!("url: {url}");
    println!("try: curl -i {url}");
}

fn deploy_hints(name: &str) -> Vec<String> {
    vec![format!("spared logs {name}"), "spared ps".to_string()]
}

async fn cmd_deploy(server: Option<String>, json: bool) -> R<()> {
    let server = server_url(server);
    let dir = PathBuf::from(".");
    if read_manifest(&dir)?.0.runtime.is_p3() && !json {
        eprintln!("{WASIP3_NOTE}");
    }
    match admin(&server, "", "GET", "/v1/apps", &[], Bytes::new()).await {
        Err(e) if e.kind != "unauthorized" => return Err(e),
        _ => {}
    }
    token_present()?;
    let wasm = build(&dir, json)?;
    let token = token()?;
    let mut v = match push(&server, &token, &dir, &wasm).await {
        Err(mut e) if e.kind == "wrong_scope" => {
            let name = read_manifest(&dir).map(|m| m.0.name).unwrap_or_default();
            e.fix = format!(
                "the CLI's token is an app token that cannot deploy '{name}', the name in spared.toml. Use a token made for '{name}' (the operator runs `spared token create {name}`) in SPARED_TOKEN, or the admin token; {}",
                e.fix
            );
            return Err(e);
        }
        r => r?,
    };
    let hints = deploy_hints(v["name"].as_str().unwrap_or(""));
    let warning = helper_warning(&dir);
    if json {
        v["next"] = json!(hints);
        if let Some(w) = &warning {
            v["warnings"] = json!([w]);
        }
        out_json(v);
    } else {
        print_deployed(&v);
        if let Some(w) = &warning {
            eprintln!("warning: {w}");
        }
        println!("logs: {}", hints[0]);
        println!("apps: {}", hints[1]);
    }
    Ok(())
}

async fn cmd_ps(server: Option<String>, json: bool) -> R<()> {
    let r = admin(
        &server_url(server),
        &token()?,
        "GET",
        "/v1/apps",
        &[],
        Bytes::new(),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
        return Ok(());
    }
    let apps = v["apps"].as_array().cloned().unwrap_or_default();
    if apps.is_empty() {
        println!("no apps deployed; run `spared deploy` in an app directory");
        return Ok(());
    }
    println!(
        "{:<20} {:<6} {:>4} {:>8} {:>6} {:>6} {:>10}  URL",
        "NAME", "STATE", "VER", "REQUESTS", "WAKES", "ERRORS", "DATA"
    );
    let mut experimental = false;
    for a in apps {
        let p3 = a["runtime"] == "wasip3-experimental";
        experimental |= p3;
        println!(
            "{:<20} {:<6} {:>4} {:>8} {:>6} {:>6} {:>10}  {}{}",
            a["name"].as_str().unwrap_or(""),
            a["state"].as_str().unwrap_or(""),
            a["version"].to_string(),
            a["requests"].to_string(),
            a["wakes"].to_string(),
            a["errors"].to_string(),
            format!("{} B", a["data_bytes"]),
            a["url"].as_str().unwrap_or(""),
            if p3 { "  EXPERIMENTAL wasip3" } else { "" }
        );
    }
    if experimental {
        println!(
            "EXPERIMENTAL wasip3: one crash or timeout fails every request in flight in that instance, compute in one request holds up the others, and wasmtime marks its wasip3 support not ready for production (docs/wasip3.md)"
        );
    }
    Ok(())
}

const NO_LOG_YET: &str = "has written nothing to stdout or stderr yet. An app that prints nothing logs nothing; panics and limit hits are recorded here when they happen";

async fn cmd_logs(name: Option<String>, lines: usize, server: Option<String>, json: bool) -> R<()> {
    let name = match name {
        Some(n) => n,
        None => {
            read_manifest(Path::new("."))
                .map_err(|_| {
                    err(
                        "no_name",
                        "no app name given and no spared.toml here",
                        "run `spared logs <name>` (list names with `spared ps`)",
                    )
                })?
                .0
                .name
        }
    };
    let r = admin(
        &server_url(server),
        &token()?,
        "GET",
        &format!("/v1/apps/{name}/logs?lines={lines}"),
        &[],
        Bytes::new(),
    )
    .await?;
    let text = String::from_utf8_lossy(&r.body).to_string();
    let empty = text.trim().is_empty();
    if json {
        let mut v = json!({"name": name, "lines": text.lines().collect::<Vec<_>>()});
        if empty {
            v["lines"] = json!([]);
            v["note"] = NO_LOG_YET.into();
        }
        out_json(v);
    } else if empty {
        // On stderr, so stdout holds only what the app wrote.
        eprintln!("no output yet: app '{name}' {NO_LOG_YET}.");
    } else {
        print!("{text}");
    }
    Ok(())
}

async fn cmd_rm(name: &str, yes: bool, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    if !yes {
        return Err(err(
            "confirm_required",
            format!("removing {name} deletes its code and its data"),
            format!(
                "rerun with --yes: `spared rm {name} --yes`. To keep a copy first, run `spared backup {name}`; backups stay after the app is removed"
            ),
        ));
    }
    let r = admin(
        &server_url(server),
        &token()?,
        "DELETE",
        &format!("/v1/apps/{name}"),
        &[],
        Bytes::new(),
    )
    .await?;
    let answer = serde_json::from_slice::<Value>(&r.body).unwrap_or_default();
    let kept = answer["backups_kept"].as_u64().unwrap_or(0);
    let revoked = answer["tokens_revoked"].as_u64().unwrap_or(0);
    let limits_kept = answer["limits_kept"] == true;
    if json {
        let mut v = json!({"deleted": name});
        if limits_kept {
            v["limits_kept"] = true.into();
        }
        if kept > 0 {
            v["backups_kept"] = kept.into();
        }
        if revoked > 0 {
            v["tokens_revoked"] = revoked.into();
        }
        if answer["tokens_revoke_failed"] == true {
            v["tokens_revoke_failed"] = true.into();
        }
        out_json(v);
    } else {
        println!("deleted {name} and its data");
        if revoked > 0 {
            println!("revoked its {revoked} app token(s)");
        }
        if answer["tokens_revoke_failed"] == true {
            eprintln!(
                "warning: writing the revocation of {name}'s tokens failed; run `spared token list {name}` and revoke the active ones"
            );
        }
        if kept > 0 {
            println!(
                "note: {kept} backup(s) of {name} remain under <data-dir>/backups/{name}/; deploy the name again and `spared restore` brings them back"
            );
        }
        if limits_kept {
            println!(
                "note: the limits set for {name} stay and apply to an app deployed under this name again. `spared limits clear {name}` removes them"
            );
        }
    }
    Ok(())
}

/// What `spared limits set` was given.
struct LimitFlags {
    data_mb: Option<u64>,
    entries: Option<u64>,
    cpu_percent: Option<u64>,
    cpu_ms_per_min: Option<u64>,
    egress_mb_per_hour: Option<f64>,
}

const LIMITS_FIX: &str = "pass at least one of --data-mb, --entries, --cpu-percent (1 to 100) or --cpu-ms-per-min, --egress-mb-per-hour, each above 0";

impl LimitFlags {
    /// The fields to send, as the server names them, checked here so a typo never reaches it.
    fn fields(&self) -> R<serde_json::Map<String, Value>> {
        let bad = |m: String| err("bad_limits", m, LIMITS_FIX);
        let mut m = serde_json::Map::new();
        for (k, v) in [("data_mb", self.data_mb), ("entries", self.entries)] {
            match v {
                Some(0) => {
                    return Err(bad(format!("--{} must be at least 1", k.replace('_', "-"))));
                }
                Some(n) => {
                    m.insert(k.into(), n.into());
                }
                None => {}
            }
        }
        let cpu = match (self.cpu_percent, self.cpu_ms_per_min) {
            (Some(p), _) if !(1..=100).contains(&p) => {
                return Err(bad(format!("--cpu-percent is {p}, and it takes 1 to 100")));
            }
            (Some(p), _) => Some(p * 600),
            (None, Some(0)) => return Err(bad("--cpu-ms-per-min must be at least 1".into())),
            (None, ms) => ms,
        };
        if let Some(c) = cpu {
            m.insert("cpu_ms_per_min".into(), c.into());
        }
        if let Some(mb) = self.egress_mb_per_hour {
            let bytes = (mb * 1_048_576.0).round();
            if !mb.is_finite() || bytes < 1.0 {
                return Err(bad(format!(
                    "--egress-mb-per-hour is {mb}, and it takes a number above 0, such as 50 or 0.5"
                )));
            }
            m.insert("egress_bytes_per_hour".into(), (bytes as u64).into());
        }
        if m.is_empty() {
            return Err(bad("no limit given".into()));
        }
        Ok(m)
    }
}

/// The server's answer for an app's limits. A server without the route predates them.
async fn limits_call(base: &str, method: &str, name: &str, body: Option<Value>) -> R<Value> {
    let (headers, bytes) = match &body {
        Some(b) => (
            vec![("content-type", "application/json".to_string())],
            Bytes::from(b.to_string()),
        ),
        None => (vec![], Bytes::new()),
    };
    let r = admin(
        base,
        &token()?,
        method,
        &format!("/v1/apps/{name}/limits"),
        &headers,
        bytes,
    )
    .await
    .map_err(|mut e| {
        if e.kind == "no_route" {
            e = err(
                "limits_unsupported",
                format!("the server at {base} is a spared version without per-app limits"),
                "upgrade spared on that server to this CLI's version",
            );
        } else if e.kind == "wrong_scope" {
            e.fix = "per-app limits take the admin token: an app token can never read or change them. Run this with the server's <data-dir>/admin-token in SPARED_TOKEN".into();
        }
        e
    })?;
    json_body(&r)
}

async fn cmd_limits_set(
    name: &str,
    asked: LimitFlags,
    server: Option<String>,
    json: bool,
) -> R<()> {
    check_app_name(name)?;
    let fields = asked.fields()?;
    let base = server_url(server);
    // A PUT replaces the whole override, so start from the one stored and change what was given.
    let now = limits_call(&base, "GET", name, None).await?;
    let mut body = match &now["override"] {
        Value::Object(o) => o.clone(),
        _ => serde_json::Map::new(),
    };
    body.extend(fields);
    let v = limits_call(&base, "PUT", name, Some(Value::Object(body))).await?;
    if json {
        out_json(v);
    } else {
        println!("set limits for {name}");
        print_limits(&v);
    }
    Ok(())
}

async fn cmd_limits_show(name: &str, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    let v = limits_call(&server_url(server), "GET", name, None).await?;
    if json {
        out_json(v);
    } else {
        print_limits(&v);
    }
    Ok(())
}

async fn cmd_limits_clear(name: &str, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    let v = limits_call(&server_url(server), "DELETE", name, None).await?;
    if json {
        out_json(v);
    } else {
        if v["cleared"] == true {
            println!("cleared the limits of {name}: the server's caps apply again");
        } else {
            println!("{name} had no limits of its own: the server's caps apply");
        }
        print_limits(&v);
    }
    Ok(())
}

fn print_limits(v: &Value) {
    let name = v["name"].as_str().unwrap_or("");
    let eff = &v["effective"];
    let caps = &v["server_caps"];
    let u = &v["usage"];
    let set: Vec<&str> = v["overridden"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let clamped: Vec<&str> = v["clamped"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let whence = |k: &str, cap: &Value| {
        if clamped.contains(&k) {
            format!("the override is above the server's cap of {cap}, so the cap")
        } else if set.contains(&k) {
            format!("override, server cap {cap}")
        } else {
            "the server's cap".to_string()
        }
    };
    println!(
        "{name}: {}",
        if v["app_exists"] == true {
            "deployed"
        } else {
            "no app deployed under this name"
        }
    );
    if let Some(d) = v["damaged"].as_str() {
        println!(
            "  the stored override cannot be read ({d}). The app is not served until it is set or cleared"
        );
    }
    match v["set_at"].as_str() {
        Some(at) => println!("  override set at {at}"),
        None => println!("  no override: the server's caps apply"),
    }
    let used_data = match (u["data_bytes"].as_u64(), u["data_entries"].as_u64()) {
        (Some(b), Some(e)) => format!(", used {} and {e} entries", size_text(b)),
        _ => String::new(),
    };
    println!(
        "  data      {} MB ({}){used_data}",
        eff["data_mb"],
        whence("data_mb", &caps["data_mb"])
    );
    println!(
        "  entries   {} ({})",
        eff["entries"],
        whence("entries", &caps["entries"])
    );
    match eff["cpu_ms_per_min"].as_u64() {
        Some(c) => println!(
            "  cpu       {c} ms per minute, {:.1}% of a core ({}), used {} ms of the {} s bucket, {} ms left, {} requests refused, {} ms since start",
            c as f64 / 600.0,
            whence("cpu_ms_per_min", &caps["cpu_ms_per_min"]),
            u["cpu_ms_in_window"],
            u["cpu_window_secs"],
            u["cpu_ms_left"],
            u["cpu_refused"],
            u["cpu_ms_total"]
        ),
        None => println!(
            "  cpu       no budget over time (each request is still held to its cpu_ms), {} ms since start",
            u["cpu_ms_total"]
        ),
    }
    match eff["egress_bytes_per_hour"].as_u64() {
        Some(b) => println!(
            "  egress    {} per hour, used {} of it, {} calls or bodies refused, {} since start",
            size_text(b),
            size_text(u["egress_bytes_in_window"].as_u64().unwrap_or(0)),
            u["egress_refused"],
            size_text(u["egress_bytes_total"].as_u64().unwrap_or(0))
        ),
        None => println!(
            "  egress    no budget, {} since start",
            size_text(u["egress_bytes_total"].as_u64().unwrap_or(0))
        ),
    }
    println!("  counters start over when the server restarts or the app is removed");
}

/// A byte count for a person: B, KiB, MiB or GiB.
fn size_text(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

fn check_app_name(name: &str) -> R<()> {
    if valid_name(name) {
        return Ok(());
    }
    Err(err(
        "bad_name",
        format!("invalid app name '{name}'"),
        "use 1 to 40 lowercase letters, digits and dashes; list apps with `spared ps`",
    ))
}

const BACKUP_SCOPE: &str = "a backup holds the app's data only (data/, so /data/app.db), not its code; redeploy to restore code";

/// How often `spared backup` and `spared restore --from remote:` ask the server how a remote
/// job is going. Each ask is its own short request, so a long upload never meets the 120 s
/// request timeout.
const POLL: Duration = Duration::from_millis(500);

/// Ask `path` until `pending` says the job is no longer running; the last answer.
async fn poll_job(base: &str, token: &str, path: &str, pending: &str) -> R<Value> {
    loop {
        let r = admin(base, token, "GET", path, &[], Bytes::new()).await?;
        let v = json_body(&r)?;
        if v["status"].as_str() != Some(pending) {
            return Ok(v);
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn cmd_backup(name: &str, no_wait: bool, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    let (base, token) = (server_url(server), token()?);
    let r = admin(
        &base,
        &token,
        "POST",
        &format!("/v1/apps/{name}/backup"),
        &[],
        Bytes::new(),
    )
    .await?;
    let mut v = json_body(&r)?;
    let id = v["backup_id"].as_str().unwrap_or("").to_string();
    if !json {
        print_local_backup(name, &v);
    }
    if v["remote"]["status"].as_str() == Some("uploading") && !no_wait {
        if !json {
            eprintln!(
                "uploading {id} to {} as {} ...",
                v["remote"]["target"].as_str().unwrap_or(""),
                v["remote"]["remote_id"].as_str().unwrap_or("")
            );
        }
        let path = format!("/v1/apps/{name}/backups/{id}/remote");
        v["remote"] = poll_job(&base, &token, &path, "uploading").await?;
    }
    let remote = &v["remote"];
    if remote["status"].as_str() == Some("failed") {
        let cause = &remote["error"];
        let mut e = err(
            "remote_upload_failed",
            format!(
                "the local backup {id} of {name} was taken, but uploading it failed: {}: {}",
                cause["kind"].as_str().unwrap_or("unknown"),
                cause["message"].as_str().unwrap_or("")
            ),
            format!(
                "{} The local backup stands. Restore it with `spared restore {name} --from {id} --yes`",
                cause["fix"].as_str().unwrap_or("")
            ),
        );
        e.extra = Some(json!({"backup_id": id, "remote": remote}));
        return Err(e);
    }
    if json {
        out_json(v);
        return Ok(());
    }
    match remote["status"].as_str() {
        Some("uploaded") => println!(
            "uploaded {id} to {} as {} ({} bytes, {})",
            remote["target"].as_str().unwrap_or(""),
            remote["remote_id"].as_str().unwrap_or(""),
            remote["archive_bytes"],
            if remote["encrypted"].as_bool() == Some(true) {
                "encrypted"
            } else {
                "NOT encrypted"
            }
        ),
        Some("uploading") => println!(
            "uploading in the background as {}. `spared backups {name}` shows when it is done",
            remote["remote_id"].as_str().unwrap_or("")
        ),
        _ => {}
    }
    Ok(())
}

fn print_local_backup(name: &str, v: &Value) {
    let id = v["backup_id"].as_str().unwrap_or("");
    println!(
        "backed up {name} as {id} ({} bytes in {} files, integrity {})",
        v["bytes"],
        v["files"],
        v["integrity"].as_str().unwrap_or("")
    );
    println!("note: {BACKUP_SCOPE}");
    if let Some(n) = v["note"].as_str() {
        println!("note: {n}");
    }
    if let Some(old) = v["removed_old"].as_array() {
        println!(
            "removed {} old backup(s) over the retention limit",
            old.len()
        );
    }
    println!("restore with: spared restore {name} --from {id} --yes");
}

async fn cmd_remote_backups(name: &str, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    let r = admin(
        &server_url(server),
        &token()?,
        "GET",
        &format!("/v1/apps/{name}/remote-backups"),
        &[],
        Bytes::new(),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
        return Ok(());
    }
    let target = v["target"].as_str().unwrap_or("");
    let items = v["remote_backups"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("no backups of {name} in {target}");
        return Ok(());
    }
    println!(
        "{:<34} {:<21} {:>12} {:<13} {:>5}",
        "REMOTE ID", "CREATED", "BYTES", "NODE", "EPOCH"
    );
    for b in &items {
        println!(
            "{:<34} {:<21} {:>12} {:<13} {:>5}",
            b["remote_id"].as_str().unwrap_or(""),
            b["created_at"].as_str().unwrap_or(""),
            b["bytes"].to_string(),
            format!(
                "{}{}",
                b["node"].as_str().unwrap_or(""),
                if b["this_node"].as_bool() == Some(true) {
                    "*"
                } else {
                    ""
                }
            ),
            b["epoch"].to_string()
        );
    }
    println!("in {target}. * marks backups this server uploaded");
    println!("restore with: spared restore {name} --from remote:<REMOTE ID> --yes");
    Ok(())
}

/// Create `path` as a new file with mode 0600; an existing file is never opened.
fn open_new_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn cmd_backup_key_new(out: Option<PathBuf>, json: bool) -> R<()> {
    let key = crate::remote::seal::BackupKey::generate()
        .map_err(|e| err("random_failed", e.to_string(), "report this"))?;
    let note = "keep a copy of this key off this machine (a password manager, a secret store): the encrypted backups cannot be restored without it, and a key that lives only on the server's disk is lost with that disk";
    if let Some(p) = &out {
        use std::io::Write;
        let written = open_new_private(p).and_then(|mut f| {
            f.write_all(format!("{}\n", key.to_hex()).as_bytes())?;
            f.sync_all()
        });
        if let Err(e) = written {
            return Err(err(
                "write_failed",
                format!("cannot create {}: {e}", p.display()),
                "pick a path that does not exist yet. The key is never written over another file",
            ));
        }
    }
    if json {
        let mut v = json!({"key_id": key.key_id(), "note": note});
        match &out {
            Some(p) => v["file"] = p.display().to_string().into(),
            None => v["key"] = key.to_hex().into(),
        }
        out_json(v);
        return Ok(());
    }
    match &out {
        Some(p) => println!(
            "wrote a new backup key to {} (key id {})",
            p.display(),
            key.key_id()
        ),
        None => println!("{}", key.to_hex()),
    }
    eprintln!("key id {}: {note}", key.key_id());
    eprintln!(
        "then: spared serve --backup-to s3://<bucket>/<prefix> --backup-key-file <file with the key>"
    );
    Ok(())
}

async fn cmd_backups(name: &str, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    let r = admin(
        &server_url(server),
        &token()?,
        "GET",
        &format!("/v1/apps/{name}/backups"),
        &[],
        Bytes::new(),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
        return Ok(());
    }
    let items = v["backups"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("no backups of {name}; run `spared backup {name}`");
        return Ok(());
    }
    println!(
        "{:<30} {:<12} {:<21} {:>10} {:>5}  REMOTE",
        "ID", "KIND", "CREATED", "BYTES", "FILES"
    );
    for b in items {
        println!(
            "{:<30} {:<12} {:<21} {:>10} {:>5}  {}",
            b["backup_id"].as_str().unwrap_or(""),
            b["kind"].as_str().unwrap_or(""),
            b["created_at"].as_str().unwrap_or(""),
            b["bytes"].to_string(),
            b["files"].to_string(),
            match b["remote"]["status"].as_str() {
                Some(st) => format!("{st} {}", b["remote"]["remote_id"].as_str().unwrap_or("")),
                None => "-".into(),
            }
        );
    }
    println!("keeping the newest {} of each kind", v["max_backups"]);
    Ok(())
}

async fn cmd_restore(
    name: &str,
    from: &str,
    yes: bool,
    server: Option<String>,
    json: bool,
) -> R<()> {
    check_app_name(name)?;
    if !yes {
        return Err(err(
            "confirm_required",
            format!("restoring {name} from {from} replaces its current data"),
            format!(
                "rerun with --yes: `spared restore {name} --from {from} --yes`. The current data is first kept as a pre-restore copy, so the restore can be undone"
            ),
        ));
    }
    let (base, token) = (server_url(server), token()?);
    let mut fetched = None;
    let local = match from.strip_prefix("remote:") {
        Some(rid) => {
            let f = fetch_remote(&base, &token, name, rid, json).await?;
            let id = f["local_id"].as_str().unwrap_or("").to_string();
            fetched = Some(f);
            id
        }
        None => from.to_string(),
    };
    let r = admin(
        &base,
        &token,
        "POST",
        &format!("/v1/apps/{name}/restore"),
        &[("content-type", "application/json".to_string())],
        Bytes::from(json!({"backup_id": local}).to_string()),
    )
    .await?;
    let mut v = json_body(&r)?;
    if json {
        if let Some(f) = fetched {
            v["fetched"] = f;
        }
        out_json(v);
        return Ok(());
    }
    println!(
        "restored {name} from {local} ({} bytes in {} files)",
        v["bytes"], v["files"]
    );
    println!("note: {BACKUP_SCOPE}");
    if let Some(u) = v["undo"].as_str() {
        println!(
            "the data it replaced is kept as {}; undo with: {u}",
            v["pre_restore_id"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// Phase one of a remote restore: have the server download, verify and unpack the remote backup
/// into a new local backup, and wait for it. The app keeps serving meanwhile.
async fn fetch_remote(base: &str, token: &str, name: &str, rid: &str, json: bool) -> R<Value> {
    // The restore that follows needs the app; say so before a long download. An app token
    // without the read scope cannot ask, and the restore says it instead.
    match admin(
        base,
        token,
        "GET",
        &format!("/v1/apps/{name}"),
        &[],
        Bytes::new(),
    )
    .await
    {
        Err(e) if e.kind != "wrong_scope" => return Err(e),
        _ => {}
    }
    let r = admin(
        base,
        token,
        "POST",
        &format!("/v1/apps/{name}/fetch"),
        &[("content-type", "application/json".to_string())],
        Bytes::from(json!({"remote_id": rid}).to_string()),
    )
    .await?;
    let mut v = json_body(&r)?;
    if v["status"].as_str() == Some("fetching") {
        if !json {
            eprintln!(
                "fetching {rid} from {} and checking it ...",
                v["target"].as_str().unwrap_or("")
            );
        }
        v = poll_job(
            base,
            token,
            &format!("/v1/apps/{name}/fetch/{rid}"),
            "fetching",
        )
        .await?;
    }
    if v["status"].as_str() != Some("fetched") {
        let e = &v["error"];
        let mut ce = err(
            e["kind"].as_str().unwrap_or("remote_fetch_failed"),
            format!(
                "{} (nothing was restored, and the app's data is unchanged)",
                e["message"].as_str().unwrap_or("the fetch failed")
            ),
            e["fix"].as_str().unwrap_or(""),
        );
        ce.extra = Some(json!({"remote_id": rid}));
        return Err(ce);
    }
    if !json {
        println!(
            "fetched {rid} as local backup {} ({} bytes in {} files, every hash and seal checked)",
            v["local_id"].as_str().unwrap_or(""),
            v["bytes"],
            v["files"]
        );
    }
    Ok(v)
}

async fn cmd_token_create(
    app: &str,
    scope: Option<String>,
    expires: Option<String>,
    label: Option<String>,
    out: Option<PathBuf>,
    server: Option<String>,
    json: bool,
) -> R<()> {
    check_app_name(app)?;
    let mut body = json!({ "app": app });
    if let Some(s) = scope {
        body["scopes"] = json!(s.split(',').map(str::trim).collect::<Vec<_>>());
    }
    if let Some(e) = expires {
        let secs = crate::tokens::parse_duration(&e).ok_or_else(|| {
            err(
                "bad_expiry",
                format!("--expires '{e}' is not a duration"),
                "pass a whole number with s, m, h or d, such as --expires 30d",
            )
        })?;
        body["expires_in_secs"] = secs.into();
    }
    if let Some(l) = label {
        body["label"] = l.into();
    }
    // The file is created before the token is, so a path that cannot be used mints nothing.
    let mut file = match &out {
        Some(p) => Some(open_new_private(p).map_err(|e| {
            err(
                "write_failed",
                format!("cannot create {}: {e}", p.display()),
                "pick a path that does not exist yet. A token is never written over another file",
            )
        })?),
        None => None,
    };
    let sent = admin(
        &server_url(server),
        &token()?,
        "POST",
        "/v1/tokens",
        &[("content-type", "application/json".to_string())],
        Bytes::from(body.to_string()),
    )
    .await
    .and_then(|r| json_body(&r));
    let mut v = match sent {
        Ok(v) => v,
        Err(e) => {
            if let Some(p) = &out {
                let _ = std::fs::remove_file(p);
            }
            return Err(e);
        }
    };
    if let (Some(f), Some(p)) = (file.as_mut(), &out) {
        use std::io::Write;
        let written = f
            .write_all(format!("{}\n", v["token"].as_str().unwrap_or("")).as_bytes())
            .and_then(|_| f.sync_all());
        if let Err(e) = written {
            return Err(err(
                "write_failed",
                format!(
                    "token {} was made but cannot be written to {}: {e}",
                    v["id"].as_str().unwrap_or(""),
                    p.display()
                ),
                "revoke it with `spared token revoke <id>` and make another",
            ));
        }
        if let Some(o) = v.as_object_mut() {
            o.remove("token");
        }
        v["file"] = p.display().to_string().into();
    }
    if json {
        out_json(v);
        return Ok(());
    }
    match &out {
        // `--out` keeps the token off stdout, so it never reaches a terminal log or a pipe.
        Some(p) => println!("wrote the token to {} (mode 0600)", p.display()),
        // The token alone on stdout. Redirecting it makes a file the umask decides the mode of:
        // use `--out`, or `(umask 077; spared token create my-app > FILE)`.
        None => println!("{}", v["token"].as_str().unwrap_or("")),
    }
    eprintln!(
        "token {} for app {} (scopes: {}; expires: {})",
        v["id"].as_str().unwrap_or(""),
        v["app"].as_str().unwrap_or(""),
        scope_words(&v),
        v["expires_at"].as_str().unwrap_or("never")
    );
    eprintln!(
        "this is the only time it is shown: store it in a secret store now. Use it as SPARED_TOKEN; revoke it with `spared token revoke {}`",
        v["id"].as_str().unwrap_or("")
    );
    Ok(())
}

fn scope_words(v: &Value) -> String {
    v["scopes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

async fn cmd_token_list(app: Option<String>, server: Option<String>, json: bool) -> R<()> {
    let path = match &app {
        Some(a) => {
            check_app_name(a)?;
            format!("/v1/tokens?app={a}")
        }
        None => "/v1/tokens".to_string(),
    };
    let r = admin(
        &server_url(server),
        &token()?,
        "GET",
        &path,
        &[],
        Bytes::new(),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
        return Ok(());
    }
    let list = v["tokens"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no tokens; make one with `spared token create <app>`");
        return Ok(());
    }
    println!(
        "{:<12}  {:<20} {:<8} {:<22} {:<20} {:<20} {:<20} LABEL",
        "ID", "APP", "STATUS", "SCOPES", "CREATED", "EXPIRES", "LAST USED"
    );
    for t in list {
        let s = |k: &str, none: &'static str| t[k].as_str().unwrap_or(none).to_string();
        println!(
            "{:<12}  {:<20} {:<8} {:<22} {:<20} {:<20} {:<20} {}",
            s("id", ""),
            s("app", ""),
            s("status", ""),
            scope_words(&t),
            s("created_at", ""),
            s("expires_at", "never"),
            s("last_used_at", "never"),
            s("label", "")
        );
    }
    Ok(())
}

async fn cmd_token_revoke(id: &str, server: Option<String>, json: bool) -> R<()> {
    // A token pasted for its id would go into the URL, and from there into the access log.
    let id = &if crate::tokens::looks_like_token(id) {
        crate::tokens::id_of(id)
    } else {
        id.to_string()
    };
    if !crate::tokens::valid_id(id) {
        return Err(err(
            "no_such_token",
            "that is not a token id: an id is 12 lowercase hex digits".to_string(),
            "list tokens with `spared token list` and pass an id from it",
        ));
    }
    let r = admin(
        &server_url(server),
        &token()?,
        "DELETE",
        &format!("/v1/tokens/{id}"),
        &[],
        Bytes::new(),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
    } else if v["already_revoked"].as_bool() == Some(true) {
        println!(
            "token {id} (app {}) was already revoked",
            v["app"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "revoked token {id} (app {}); it stops working now",
            v["app"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

async fn cmd_evict(name: &str, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    let r = admin(
        &server_url(server),
        &token()?,
        "POST",
        &format!("/v1/apps/{name}/evict"),
        &[],
        Bytes::new(),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
    } else {
        println!(
            "evicted {name} (state: {}); the next request wakes a fresh instance",
            v["state"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

async fn cmd_freeze(
    name: &str,
    reason: Option<String>,
    server: Option<String>,
    json: bool,
) -> R<()> {
    check_app_name(name)?;
    let body = match &reason {
        Some(r) => json!({"reason": r}),
        None => json!({}),
    };
    let r = admin(
        &server_url(server),
        &token()?,
        "POST",
        &format!("/v1/apps/{name}/freeze"),
        &[("content-type", "application/json".to_string())],
        Bytes::from(body.to_string()),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
        return Ok(());
    }
    let f = &v["freeze"];
    if v["already_frozen"].as_bool() == Some(true) {
        println!(
            "{name} was already frozen ({}: {}), and is left as it was",
            f["kind"].as_str().unwrap_or(""),
            f["reason"].as_str().unwrap_or("")
        );
    } else {
        println!("froze {name}: public requests get 503 app_frozen, here and after a restart");
    }
    println!(
        "admin commands (backup, restore, logs, rm) still work. Serve it again with: spared thaw {name}"
    );
    Ok(())
}

async fn cmd_thaw(name: &str, server: Option<String>, json: bool) -> R<()> {
    check_app_name(name)?;
    let r = admin(
        &server_url(server),
        &token()?,
        "POST",
        &format!("/v1/apps/{name}/thaw"),
        &[("content-type", "application/json".to_string())],
        Bytes::from("{}"),
    )
    .await?;
    let v = json_body(&r)?;
    if json {
        out_json(v);
        return Ok(());
    }
    if v["was_frozen"].as_bool() == Some(true) {
        println!("thawed {name}: it serves on this server again");
    } else {
        println!("{name} was not frozen");
    }
    if let Some(n) = v["note"].as_str() {
        println!("note: {n}");
    }
    Ok(())
}

type Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

async fn connect(addr: &str) -> R<Sender> {
    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
        err(
            "app_unreachable",
            format!("cannot connect to the app listener at {addr}: {e}"),
            "check that `spared serve` is running and its public listener is reachable from here",
        )
    })?;
    let _ = stream.set_nodelay(true);
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| err("app_unreachable", e.to_string(), "retry"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

/// One GET: the time until the response head arrives, then the body is drained untimed.
async fn timed_get(sender: &mut Sender, host: &str, path: &str) -> R<(u16, Duration)> {
    let lost = |e: String| {
        err(
            "bench_failed",
            format!("request to {host}{path} failed: {e}"),
            "check `spared logs <name>` and that the server is still running",
        )
    };
    sender.ready().await.map_err(|e| lost(e.to_string()))?;
    let req = hyper::Request::builder()
        .uri(path)
        .header("host", host)
        .body(Full::new(Bytes::new()))
        .map_err(|e| err("bad_path", e.to_string(), "pass --path like /api/notes"))?;
    let t = std::time::Instant::now();
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| lost(e.to_string()))?;
    let took = t.elapsed();
    let status = resp.status().as_u16();
    resp.into_body()
        .collect()
        .await
        .map_err(|e| lost(e.to_string()))?;
    Ok((status, took))
}

/// Nearest-rank percentile of a sorted, non-empty slice.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1_000_000.0).round() / 1000.0
}

fn summarize(mut v: Vec<Duration>) -> Value {
    v.sort();
    json!({
        "samples": v.len(),
        "p50_ms": ms(percentile(&v, 50.0)),
        "p99_ms": ms(percentile(&v, 99.0)),
        "max_ms": ms(*v.last().unwrap()),
    })
}

const COLD_CYCLES: usize = 20;

async fn cmd_bench(
    name: &str,
    requests: usize,
    path: &str,
    server: Option<String>,
    json: bool,
) -> R<()> {
    check_app_name(name)?;
    if !path.starts_with('/') {
        return Err(err(
            "bad_path",
            format!("--path '{path}' must start with /"),
            "pass --path /api/notes",
        ));
    }
    if requests == 0 {
        return Err(err(
            "bad_requests",
            "--requests must be at least 1",
            "pass --requests 2000",
        ));
    }
    let server = server_url(server);
    let token = token()?;
    let app = json_body(
        &admin(
            &server,
            &token,
            "GET",
            &format!("/v1/apps/{name}"),
            &[],
            Bytes::new(),
        )
        .await?,
    )?;
    let url = app["url"].as_str().unwrap_or("");
    let authority = url
        .strip_prefix("http://")
        .and_then(|r| r.split('/').next())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            err(
                "bad_response",
                format!("the server reported the app url '{url}'"),
                "check that the server and CLI are the same spared version",
            )
        })?;
    let port = authority.rsplit_once(':').map_or("80", |(_, p)| p);
    let admin_host = parse_base(&server)?;
    let admin_host = admin_host.rsplit_once(':').map_or("", |(h, _)| h);
    let ip = if admin_host == "localhost" {
        "127.0.0.1"
    } else {
        admin_host
    };
    let addr = format!("{ip}:{port}");
    let evict_path = format!("/v1/apps/{name}/evict");
    let evict = || admin(&server, &token, "POST", &evict_path, &[], Bytes::new());
    let mut status = 0;
    let mut cold = Vec::with_capacity(COLD_CYCLES);
    for _ in 0..COLD_CYCLES {
        let mut conn = connect(&addr).await?;
        evict().await?;
        let (st, took) = timed_get(&mut conn, authority, path).await?;
        status = st;
        if st >= 400 {
            return Err(err(
                "bad_status",
                format!("GET {path} answered {st}, so its latency says little"),
                "pass --path of a route that answers 2xx, like --path /",
            ));
        }
        cold.push(took);
    }
    let mut conn = connect(&addr).await?;
    let mut warm = Vec::with_capacity(requests);
    for _ in 0..requests {
        warm.push(timed_get(&mut conn, authority, path).await?.1);
    }
    let (cold, warm) = (summarize(cold), summarize(warm));
    if json {
        out_json(json!({
            "app": name,
            "path": path,
            "status": status,
            "unit": "ms",
            "measure": "time from sending a GET to receiving the response head (first byte)",
            "cold": cold,
            "warm": warm,
        }));
        return Ok(());
    }
    println!("{name} GET {path}: time to first byte, ms (HTTP {status})");
    println!(
        "{:<28} {:>8} {:>9} {:>9} {:>9}",
        "", "samples", "p50", "p99", "max"
    );
    for (label, v) in [
        ("cold wake (evict, request)", &cold),
        ("warm (one connection)", &warm),
    ] {
        println!(
            "{:<28} {:>8} {:>9.3} {:>9.3} {:>9.3}",
            label,
            v["samples"],
            v["p50_ms"].as_f64().unwrap_or(0.0),
            v["p99_ms"].as_f64().unwrap_or(0.0),
            v["max_ms"].as_f64().unwrap_or(0.0)
        );
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

struct Check {
    name: &'static str,
    level: Level,
    detail: String,
    fix: String,
}

fn check(
    name: &'static str,
    level: Level,
    detail: impl Into<String>,
    fix: impl Into<String>,
) -> Check {
    Check {
        name,
        level,
        detail: detail.into(),
        fix: fix.into(),
    }
}

/// The first `major.minor.patch` in a tool's `--version` output, like `rustc 1.97.0 (abc 2026-07-07)`.
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split_whitespace().find_map(|w| {
        if !w.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let mut it = w.split(|c: char| !c.is_ascii_digit());
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some((major, minor, patch))
    })
}

/// The oldest Rust that builds a scaffolded app: edition 2024 and the wasm32-wasip2 target.
const MIN_RUST: (u32, u32) = (1, 85);

fn toolchain_level(v: Option<(u32, u32, u32)>) -> Level {
    match v {
        Some((major, minor, _)) if (major, minor) >= MIN_RUST => Level::Ok,
        _ => Level::Fail,
    }
}

const MIB: u64 = 1 << 20;

/// A cold build of an app's dependencies writes about a gigabyte.
fn disk_level(free_bytes: u64) -> Level {
    if free_bytes < 500 * MIB {
        Level::Fail
    } else if free_bytes < 2048 * MIB {
        Level::Warn
    } else {
        Level::Ok
    }
}

fn human_bytes(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MiB", b / MIB)
    }
}

fn all_loopback(addrs: &[std::net::IpAddr]) -> bool {
    !addrs.is_empty() && addrs.iter().all(|a| a.is_loopback())
}

fn readable_file(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => std::fs::File::open(path)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Ok(_) => Err("not a file".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn summarize_checks(checks: &[Check]) -> (usize, usize, usize) {
    let n = |l: Level| checks.iter().filter(|c| c.level == l).count();
    (n(Level::Ok), n(Level::Warn), n(Level::Fail))
}

fn doctor_json(checks: &[Check]) -> Value {
    let (ok, warn, fail) = summarize_checks(checks);
    json!({
        "ok": fail == 0,
        "summary": {"ok": ok, "warn": warn, "fail": fail},
        "checks": checks.iter().map(|c| json!({
            "name": c.name, "status": c.level.word(), "detail": c.detail, "fix": c.fix,
        })).collect::<Vec<_>>(),
    })
}

fn tool_version(tool: &str) -> Result<String, String> {
    let out = Command::new(tool)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("`{tool} --version` exited {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// In an app directory: whether `src/spared.rs` is as new as the helper this CLI writes.
fn check_helper(dir: &Path) -> Option<Check> {
    if !dir.join("spared.toml").is_file() || !dir.join("src/spared.rs").is_file() {
        return None;
    }
    Some(match helper_warning(dir) {
        Some(w) => check(
            "helper",
            Level::Warn,
            w.clone(),
            "update src/spared.rs as the detail says",
        ),
        None => check(
            "helper",
            Level::Ok,
            format!(
                "src/spared.rs is helper version {}",
                current_helper_version()
            ),
            "",
        ),
    })
}

fn check_tool(name: &'static str, tool: &str) -> Check {
    match tool_version(tool) {
        Ok(text) => {
            let level = toolchain_level(parse_version(&text));
            let fix = if level == Level::Ok {
                String::new()
            } else {
                format!(
                    "update Rust to 1.{}.0 or newer: `rustup update stable`",
                    MIN_RUST.1
                )
            };
            check(name, level, text, fix)
        }
        Err(e) => check(
            name,
            Level::Fail,
            format!("{tool} not runnable: {e}"),
            "install Rust from https://rustup.rs and open a new shell so ~/.cargo/bin is on PATH",
        ),
    }
}

fn nearest_existing(p: &Path) -> Option<PathBuf> {
    let mut cur = p.to_path_buf();
    loop {
        if cur.exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a zeroed statvfs the call fills in.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

fn check_target_dir() -> Vec<Check> {
    let (dir, source) = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(t) => (PathBuf::from(t), "CARGO_TARGET_DIR"),
        None => match home() {
            Ok(h) => (h.join(".spared/target"), "~/.spared/target"),
            Err(e) => return vec![check("build-dir", Level::Fail, e.message, e.fix)],
        },
    };
    let Some(base) = nearest_existing(&dir) else {
        return vec![check(
            "build-dir",
            Level::Fail,
            format!("{} has no existing parent directory", dir.display()),
            "set CARGO_TARGET_DIR to a directory you can write, or unset it",
        )];
    };
    let probe = base.join(format!(".spared-doctor-{}", std::process::id()));
    let writable = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    let mut out = vec![if writable {
        check(
            "build-dir",
            Level::Ok,
            format!("{} ({source}) is writable", dir.display()),
            "",
        )
    } else {
        check(
            "build-dir",
            Level::Fail,
            format!("cannot write to {} ({source})", base.display()),
            format!(
                "make {} writable (chmod u+w), or set CARGO_TARGET_DIR to another directory",
                base.display()
            ),
        )
    }];
    out.push(match free_bytes(&base) {
        Some(b) => check(
            "disk-space",
            disk_level(b),
            format!("{} free where builds go", human_bytes(b)),
            if disk_level(b) == Level::Ok {
                String::new()
            } else {
                format!("free space on {}, or point CARGO_TARGET_DIR at a bigger disk (a first build needs about 1 GiB)", base.display())
            },
        ),
        None => check("disk-space", Level::Warn, format!("could not read free space of {}", base.display()), "check `df` by hand: a first build needs about 1 GiB"),
    });
    out
}

fn check_sqlite_cache() -> Option<Check> {
    if sqlite_cache::disabled() {
        return Some(check(
            "sqlite-cache",
            Level::Ok,
            format!("off ({} is set)", sqlite_cache::ENV_DISABLE),
            "",
        ));
    }
    let sdk = find_sdk()?;
    if sqlite_cache_is_warm(&sdk) {
        let versions: Vec<String> = sqlite_cache_entries(&sdk)
            .into_iter()
            .map(|e| e.sqlite)
            .collect();
        return Some(check(
            "sqlite-cache",
            Level::Ok,
            format!(
                "SQLite {} is precompiled in {}",
                versions.join(", "),
                lib_label()
            ),
            "",
        ));
    }
    Some(check(
        "sqlite-cache",
        Level::Warn,
        "SQLite is not precompiled for this wasi-sdk, so the first app build compiles it (about a minute)",
        "run `spared setup`",
    ))
}

fn check_sdk() -> Check {
    match find_sdk() {
        Some(p) => check(
            "wasi-sdk",
            Level::Ok,
            format!("found at {}", p.display()),
            "",
        ),
        None => check(
            "wasi-sdk",
            Level::Fail,
            "not found in $WASI_SDK_PATH or ~/.spared/wasi-sdk",
            "run `spared setup`, or set WASI_SDK_PATH to a wasi-sdk 25 directory",
        ),
    }
}

async fn is_spared(base: &str) -> bool {
    match request_retrying(base, "GET", "/", &[], Bytes::new(), Duration::ZERO).await {
        Ok(r) => serde_json::from_slice::<Value>(&r.body)
            .map(|v| v["error"]["kind"].is_string())
            .unwrap_or(false),
        Err(_) => false,
    }
}

async fn check_port(name: &'static str, port: u16, role: &str) -> Check {
    let bind = std::net::TcpListener::bind(("127.0.0.1", port));
    if bind.is_ok() {
        return check(name, Level::Ok, format!("port {port} is free ({role})"), "");
    }
    drop(bind);
    if is_spared(&format!("http://127.0.0.1:{port}")).await {
        return check(
            name,
            Level::Ok,
            format!("port {port} is held by a running spared server ({role})"),
            "",
        );
    }
    check(
        name,
        Level::Warn,
        format!("port {port} ({role}) is in use by something that is not a spared server"),
        format!(
            "stop that program, or start spared on other ports: `spared serve --listen 127.0.0.1:18080 --admin-listen 127.0.0.1:18081` and deploy with `--server http://127.0.0.1:18081` (`spared dev` needs {port} free)"
        ),
    )
}

async fn check_admin_token() -> Option<Check> {
    let base = DEFAULT_ADMIN;
    if !is_spared(base).await {
        return None;
    }
    Some(match token() {
        Err(e) => check("admin-token", Level::Warn, e.message, e.fix),
        Ok(t) => match admin(base, &t, "GET", "/v1/apps", &[], Bytes::new()).await {
            Ok(_) if t.starts_with(crate::tokens::PREFIX) => check(
                "admin-token",
                Level::Ok,
                "the CLI's token is an app token the server on 8081 accepts; it works for one app only",
                "",
            ),
            Ok(_) => check(
                "admin-token",
                Level::Ok,
                "the CLI's admin token is accepted by the server on 8081",
                "",
            ),
            Err(e) if e.kind == "wrong_scope" => check(
                "admin-token",
                Level::Ok,
                "the CLI's token is an app token the server on 8081 accepts; it works for one app only",
                "",
            ),
            Err(e) => check(
                "admin-token",
                Level::Warn,
                format!("the server on 8081 rejected the CLI's token: {}", e.message),
                "export SPARED_TOKEN=$(cat <data-dir>/admin-token) for the server you deploy to",
            ),
        },
    })
}

fn check_ca_files() -> Check {
    let mut set = vec![];
    for var in ["SSL_CERT_FILE", "SPARED_EXTRA_CA_FILE"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            set.push((var, PathBuf::from(v)));
        }
    }
    if set.is_empty() {
        return check(
            "ca-bundle",
            Level::Ok,
            "SSL_CERT_FILE and SPARED_EXTRA_CA_FILE are not set (system trust is used)",
            "",
        );
    }
    for (var, path) in &set {
        if let Err(why) = readable_file(path) {
            return check(
                "ca-bundle",
                Level::Fail,
                format!("{var}={} cannot be read: {why}", path.display()),
                format!("point {var} at a readable PEM bundle, or unset it"),
            );
        }
    }
    let names: Vec<&str> = set.iter().map(|(v, _)| *v).collect();
    check(
        "ca-bundle",
        Level::Ok,
        format!("{} readable", names.join(" and ")),
        "",
    )
}

async fn check_localhost() -> Check {
    let lookup = tokio::task::spawn_blocking(|| {
        use std::net::ToSocketAddrs;
        ("spared-doctor.localhost", 8080)
            .to_socket_addrs()
            .map(|it| it.map(|a| a.ip()).collect::<Vec<_>>())
            .map_err(|e| e.to_string())
    });
    let addrs = match tokio::time::timeout(Duration::from_secs(3), lookup).await {
        Ok(Ok(Ok(a))) => a,
        _ => vec![],
    };
    if all_loopback(&addrs) {
        check(
            "localhost",
            Level::Ok,
            "*.localhost resolves to loopback",
            "",
        )
    } else {
        check(
            "localhost",
            Level::Warn,
            "the system resolver does not map <app>.localhost to loopback (browsers and curl still reach it; tools that use the system resolver do not)",
            "for such a tool, add `127.0.0.1 <app>.localhost` to /etc/hosts, or give it the address, as in `curl --resolve <app>.localhost:8080:127.0.0.1 http://<app>.localhost:8080/`",
        )
    }
}

async fn cmd_doctor(json: bool) -> R<()> {
    let mut checks = vec![check_tool("rustc", "rustc"), check_tool("cargo", "cargo")];
    checks.push(if target_installed() {
        check("wasm-target", Level::Ok, "wasm32-wasip2 is installed", "")
    } else {
        check(
            "wasm-target",
            Level::Fail,
            "the wasm32-wasip2 Rust target is not installed",
            "run `spared setup` (or `rustup target add wasm32-wasip2`)",
        )
    });
    checks.push(check_sdk());
    checks.extend(check_sqlite_cache());
    checks.extend(check_target_dir());
    checks.push(check_port("port-8080", 8080, "apps").await);
    checks.push(check_port("port-8081", 8081, "admin").await);
    checks.extend(check_admin_token().await);
    checks.push(check_ca_files());
    checks.push(check_localhost().await);
    checks.extend(check_helper(Path::new(".")));

    let (_, warn, fail) = summarize_checks(&checks);
    let report_json = doctor_json(&checks);
    if !json {
        for c in &checks {
            println!(
                "{:<4} {}: {}",
                c.level.word().to_uppercase(),
                c.name,
                c.detail
            );
            if c.level != Level::Ok && !c.fix.is_empty() {
                println!("     fix: {}", c.fix);
            }
        }
        println!("{fail} failed, {warn} warnings");
    }
    if fail == 0 {
        if json {
            out_json(report_json);
        }
        return Ok(());
    }
    let mut e = err(
        "doctor_failed",
        format!("{fail} check(s) failed"),
        "apply the fix lines for the FAIL checks, then run `spared doctor` again",
    );
    e.extra = Some(report_json);
    Err(e)
}

struct PathResult {
    path: String,
    status: Option<u16>,
    body: String,
    failure: Option<(String, String, String)>,
}

/// What `spared test` runs: a wasip2 component, or an EXPERIMENTAL wasip3 one.
enum TestPre {
    P2(ProxyPre<Guarded>),
    P3(wasmtime_wasi_http::p3::bindings::ServicePre<Guarded>),
}

enum TestLive {
    P2(
        wasmtime::Store<Guarded>,
        wasmtime_wasi_http::p2::bindings::Proxy,
    ),
    P3(std::sync::Arc<crate::p3::Instance>),
}

async fn request_once(
    engine: &Engine,
    pre: &TestPre,
    m: &Manifest,
    data: &Path,
    path: &str,
    log: &MemoryOutputPipe,
) -> PathResult {
    let mut res = PathResult {
        path: path.to_string(),
        status: None,
        body: String::new(),
        failure: None,
    };
    let fail = |res: &mut PathResult, kind: &str, msg: String, fix: &str| {
        res.failure = Some((kind.into(), msg, fix.into()))
    };
    let wake = |res: &mut PathResult, e: wasmtime::Error| {
        let msg = format!("{e:#}");
        let fix = wake_failed_fix(&msg);
        res.failure = Some(("wake_failed".into(), msg, fix));
    };
    let live = match pre {
        TestPre::P2(pre) => {
            let mut store = match crate::sandbox::app_store(
                engine,
                m,
                data,
                log,
                &crate::fsguard::AppCaps::default(),
                None,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    fail(
                        &mut res,
                        "wake_failed",
                        format!("{e:#}"),
                        "check the temp dir is writable",
                    );
                    return res;
                }
            };
            match pre.instantiate_async(&mut store).await {
                Ok(p) => TestLive::P2(store, p),
                Err(e) => {
                    wake(&mut res, e);
                    return res;
                }
            }
        }
        TestPre::P3(pre) => {
            let wasi = match crate::sandbox::wasi_builder(m, data, log) {
                Ok(mut b) => b.build(),
                Err(e) => {
                    wake(&mut res, e);
                    return res;
                }
            };
            let store = crate::p3::store(engine, m, wasi, None, None, u64::MAX);
            let deadline = Duration::from_millis(m.limits.deadline_ms);
            match crate::p3::Instance::start(pre, store, 1, deadline, None).await {
                Ok(i) => TestLive::P3(i),
                Err(e) => {
                    wake(&mut res, e);
                    return res;
                }
            }
        }
    };
    let uri = format!(
        "http://{}.localhost{}",
        m.name,
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    );
    let req = match http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(Bytes::new())
    {
        Ok(r) => r,
        Err(e) => {
            fail(
                &mut res,
                "bad_path",
                format!("invalid path '{path}': {e}"),
                "pass a path like --path /api/items",
            );
            return res;
        }
    };
    let deadline = Duration::from_millis(m.limits.deadline_ms);
    let max = Maxima::default().response_mb << 20;
    let outcome = match live {
        TestLive::P2(mut store, proxy) => {
            call_handler(&mut store, &proxy, req, deadline, max).await
        }
        TestLive::P3(instance) => {
            let r = instance.call(req, deadline, max).await;
            instance.drain().await;
            r
        }
    };
    match outcome {
        Ok(resp) => {
            res.status = Some(resp.status().as_u16());
            let bytes = resp.body();
            res.body = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).to_string();
        }
        Err(CallError::BadRequest(msg)) => fail(
            &mut res,
            "bad_request",
            msg,
            "pass a path like --path /api/items",
        ),
        Err(CallError::Deadline) => fail(
            &mut res,
            "deadline",
            format!("request exceeded {} ms", m.limits.deadline_ms),
            "make the handler faster or raise limits.deadline_ms in spared.toml",
        ),
        Err(CallError::Trap { kind, text }) => {
            let fix = trap_fix(kind);
            let first = trap_message(kind, &text, &m.limits);
            fail(&mut res, kind.as_str(), first, fix);
        }
        Err(CallError::ResponseTooLarge) => fail(
            &mut res,
            "response_too_large",
            format!("response body over {} MB", Maxima::default().response_mb),
            "send a smaller response",
        ),
        Err(CallError::ResponseBody(msg)) => fail(
            &mut res,
            "response_body",
            msg,
            "check the handler's response body",
        ),
        Err(CallError::NoResponse) => fail(
            &mut res,
            "no_response",
            "the handler returned without sending a response".into(),
            "the handler failed before answering (an Err returned with `?` sends nothing); turn errors into a 500 response with spared::text_response, and read the app log",
        ),
    }
    res
}

async fn cmd_test(paths: Vec<String>, json: bool) -> R<()> {
    let dir = PathBuf::from(".");
    let (mut m, _) = read_manifest(&dir)?;
    if let Some((field, asked, max, _)) = m.limits.over(&Maxima::default()) {
        return Err(err(
            "limit_too_high",
            format!("limits.{field} = {asked} is above the default server maximum of {max}"),
            format!("set limits.{field} to at most {max} in spared.toml"),
        ));
    }
    m.limits.clamp(&Maxima::default());
    let wasm = build(&dir, json)?;
    let engine = crate::sandbox::engine_for(m.runtime)
        .map_err(|e| err("engine", format!("{e:#}"), "report this"))?;
    crate::sandbox::start_epoch_ticker(&engine);
    if m.runtime.is_p3() && !json {
        eprintln!("{WASIP3_NOTE}");
    }
    let comp = Component::from_file(&engine, &wasm).map_err(|e| {
        err(
            "invalid_component",
            format!("{e:#}"),
            "build for wasm32-wasip2 as a component: use `#[wstd::http_server]` and a bin crate",
        )
    })?;
    crate::sandbox::check_component(&engine, &comp, &m.grants, m.runtime)
        .map_err(|r| err(r.kind, r.message, r.fix))?;
    let pre = if m.runtime.is_p3() {
        TestPre::P3(
            crate::p3::link(&engine, &comp)
                .map_err(|e| err("link", format!("{e:#}"), "report this"))?,
        )
    } else {
        TestPre::P2(
            crate::sandbox::link(&engine, &comp)
                .map_err(|e| err("link", format!("{e:#}"), "report this"))?,
        )
    };

    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("spared-test-{}-{nanos}", std::process::id()));
    let data = tmp.join("data");
    std::fs::create_dir_all(&data).map_err(|e| io_err("create temp data dir", e))?;

    let paths = if paths.is_empty() {
        vec!["/".to_string()]
    } else {
        paths
    };
    let log = MemoryOutputPipe::new(1 << 16);
    let mut results = vec![];
    for p in &paths {
        results.push(request_once(&engine, &pre, &m, &data, p, &log).await);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let app_log = String::from_utf8_lossy(&log.contents()).to_string();

    let failed = results
        .iter()
        .find(|r| r.failure.is_some() || r.status.is_some_and(|s| s >= 500));
    let results_json: Vec<Value> = results
        .iter()
        .map(|r| json!({"path": r.path, "status": r.status, "body": r.body, "error": r.failure.as_ref().map(|(k, msg, _)| json!({"kind": k, "message": msg}))}))
        .collect();
    if !json {
        for r in &results {
            match (&r.failure, r.status) {
                (Some((kind, msg, _)), _) => println!("GET {} -> FAILED {kind}: {msg}", r.path),
                (None, Some(s)) => println!("GET {} -> {s}\n{}", r.path, r.body),
                _ => {}
            }
        }
        if !app_log.trim().is_empty() {
            println!("--- app log ---\n{}", app_log.trim_end());
        }
    }
    match failed {
        None => {
            if json {
                out_json(
                    json!({"ok": true, "results": results_json, "app_log": app_log, "next": "spared deploy"}),
                );
            } else {
                println!("next: spared deploy");
            }
            Ok(())
        }
        Some(r) => {
            let mut e = match &r.failure {
                Some((k, msg, fix)) => err(k, format!("GET {}: {msg}", r.path), fix.clone()),
                None => err(
                    &format!("http_{}", r.status.unwrap_or(500)),
                    format!("GET {} answered {}", r.path, r.status.unwrap_or(500)),
                    "the handler returned a 5xx; read the response body and app log above, then fix src/main.rs",
                ),
            };
            e.extra = Some(json!({"ok": false, "results": results_json, "app_log": app_log}));
            Err(e)
        }
    }
}

fn newest_mtime(dir: &Path) -> SystemTime {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut stack = vec![dir.join("src")];
    let mut files = vec![dir.join("spared.toml"), dir.join("Cargo.toml")];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    files.push(p);
                }
            }
        }
    }
    for f in files {
        if let Ok(t) = std::fs::metadata(&f).and_then(|m| m.modified()) {
            newest = newest.max(t);
        }
    }
    newest
}

async fn cmd_dev(json: bool) -> R<()> {
    let dir = PathBuf::from(".");
    let dev_wasip3 = read_manifest(&dir)?.0.runtime.is_p3();
    if dev_wasip3 && !json {
        eprintln!("{WASIP3_NOTE}");
    }
    let data_dir = std::fs::canonicalize(".")
        .map_err(|e| io_err("resolve .", e))?
        .join(".spared");
    let engine =
        crate::sandbox::engine().map_err(|e| err("engine", format!("{e:#}"), "report this"))?;
    crate::datadir::check(&data_dir).map_err(|e| err(e.kind, e.message, e.fix))?;
    let (listen, admin) = (
        "127.0.0.1:8080".parse().unwrap(),
        "127.0.0.1:8081".parse().unwrap(),
    );
    let listeners =
        crate::server::bind_listeners(listen, admin).map_err(|e| err(e.kind, e.message, e.fix))?;
    let data_lock =
        crate::server::lock_data_dir(&data_dir).map_err(|e| err(e.kind, e.message, e.fix))?;
    let token = crate::server::admin_token(&data_dir, None).map_err(|e| {
        err(
            "io",
            format!(
                "cannot set up the admin token in {}: {e:#}",
                data_dir.display()
            ),
            "make the app directory writable",
        )
    })?;
    crate::server::share_token_with_cli(&token);
    let export_line = token_export_line(&data_dir);
    let cfg = crate::server::Config {
        data_dir,
        listen,
        admin,
        domain: None,
        examples_dir: None,
        idle: Duration::from_secs(30),
        path_routing: false,
        max: Maxima::default(),
        admin_token: token.clone(),
        client_timeout: Duration::from_secs(10),
        max_connections: 256,
        prefetch_mb: 0,
        max_backups: 20,
        // `spared dev` prints its own build and deploy lines (JSON with --json) on stdout.
        access_log: false,
        caps: Default::default(),
        compile: crate::compile::CompileLimits::isolated(),
        remote: None,
        // `spared dev` runs a wasip3-experimental app when its spared.toml asks for one: the
        // developer chose it for their own app on their own machine.
        wasip3: dev_wasip3,
        #[cfg(test)]
        test_loopback: false,
    };
    let server = tokio::spawn(crate::server::serve(
        cfg,
        engine,
        std::future::pending(),
        listeners,
        data_lock,
    ));
    let base = DEFAULT_ADMIN.to_string();
    let mut up = false;
    for _ in 0..50 {
        if server.is_finished() {
            let why = match server.await {
                Ok(Err(e)) => format!("{e:#}"),
                _ => "server task exited".into(),
            };
            return Err(err(
                "serve_failed",
                format!("could not start the dev server: {why}"),
                "ports 8080 and 8081 must be free: stop any other `spared serve` or `spared dev`",
            ));
        }
        if request_retrying(&base, "GET", "/v1/apps", &[], Bytes::new(), Duration::ZERO)
            .await
            .is_ok()
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !up {
        return Err(err(
            "serve_failed",
            "the dev server did not come up within 5 s",
            "ports 8080 and 8081 must be free: stop any other `spared serve` or `spared dev`",
        ));
    }

    if !json {
        eprintln!(
            "to run spared commands against this dev server from another shell, first run: {export_line}"
        );
    }
    let mut seen = SystemTime::UNIX_EPOCH;
    loop {
        let now = newest_mtime(&dir);
        if now != seen {
            let d = dir.clone();
            let built = tokio::task::spawn_blocking(move || build(&d, json))
                .await
                .map_err(|e| err("build_failed", e.to_string(), "report this"))?;
            let result = match built {
                Ok(wasm) => push(&base, &token, &dir, &wasm).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(v) => {
                    if json {
                        out_json(json!({"event": "deployed", "app": v}));
                    } else {
                        print_deployed(&v);
                        println!(
                            "watching src/ and spared.toml; edit and save to redeploy (ctrl-c to stop)"
                        );
                    }
                }
                Err(e) => {
                    report(&e, json);
                    if !json {
                        eprintln!("waiting for the next change");
                    }
                }
            }
            seen = newest_mtime(&dir);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod sdk_checksum_tests {
    use super::*;

    const TABLE: &str = "\
52640dde13599bf127a95499e61d6d640256119456d1af8897ab6725bcf3d89c  wasi-sdk-25.0-x86_64-linux.tar.gz
";

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("spared-sdk-test-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn tarball_must_match_the_pinned_digest() {
        let p = temp_file("bad", b"not a wasi-sdk");
        let Err(e) = verify_sdk_tarball(TABLE, "wasi-sdk-25.0-x86_64-linux.tar.gz", &p) else {
            panic!("accepted a wrong file")
        };
        assert_eq!(e.kind, "checksum_mismatch");
        assert!(e.fix.contains("WASI_SDK_PATH"), "{}", e.fix);
        let Err(e) = verify_sdk_tarball(TABLE, "wasi-sdk-25.0-riscv-linux.tar.gz", &p) else {
            panic!("accepted an unpinned file")
        };
        assert_eq!(e.kind, "checksum_missing");
        let table = format!(
            "{}  wasi-sdk-25.0-x86_64-linux.tar.gz\n",
            sha256_hex(&p).unwrap()
        );
        assert!(verify_sdk_tarball(&table, "wasi-sdk-25.0-x86_64-linux.tar.gz", &p).is_ok());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn pinned_table_covers_every_platform_setup_downloads() {
        for platform in ["x86_64-linux", "arm64-linux", "arm64-macos", "x86_64-macos"] {
            let file = format!("wasi-sdk-{SDK_VERSION}.0-{platform}.tar.gz");
            let h = sdk_expected_sha(SDK_SHA256, &file).unwrap_or_else(|| panic!("{file}"));
            assert_eq!(h.len(), 64, "{file}");
            assert!(h.bytes().all(|b| b.is_ascii_hexdigit()), "{file}");
        }
    }
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    #[test]
    fn versions_parse_from_tool_output() {
        assert_eq!(
            parse_version("rustc 1.97.0 (2d8144b78 2026-07-07)"),
            Some((1, 97, 0))
        );
        assert_eq!(
            parse_version("cargo 1.85.1 (abc 2025-03-01)"),
            Some((1, 85, 1))
        );
        assert_eq!(parse_version("rustc 1.99.0-nightly (x)"), Some((1, 99, 0)));
        assert_eq!(parse_version("no numbers here"), None);
    }

    #[test]
    fn old_or_unknown_toolchains_fail() {
        assert_eq!(toolchain_level(Some((1, 85, 0))), Level::Ok);
        assert_eq!(toolchain_level(Some((2, 0, 0))), Level::Ok);
        assert_eq!(toolchain_level(Some((1, 84, 9))), Level::Fail);
        assert_eq!(toolchain_level(None), Level::Fail);
    }

    #[test]
    fn disk_thresholds() {
        assert_eq!(disk_level(100 * MIB), Level::Fail);
        assert_eq!(disk_level(1000 * MIB), Level::Warn);
        assert_eq!(disk_level(5000 * MIB), Level::Ok);
        assert_eq!(human_bytes(3 << 30), "3.0 GiB");
        assert_eq!(human_bytes(700 * MIB), "700 MiB");
    }

    #[test]
    fn loopback_needs_every_address_loopback() {
        let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: std::net::IpAddr = "::1".parse().unwrap();
        let other: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        assert!(all_loopback(&[lo, lo6]));
        assert!(!all_loopback(&[lo, other]));
        assert!(!all_loopback(&[]));
    }

    #[test]
    fn unreadable_ca_files_are_reported() {
        let dir = std::env::temp_dir().join(format!("spared-doctor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ca.pem");
        std::fs::write(&file, "x").unwrap();
        assert!(readable_file(&file).is_ok());
        assert!(readable_file(&dir.join("missing.pem")).is_err());
        assert!(readable_file(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_report_names_status_and_fail_count() {
        let checks = vec![
            check("a", Level::Ok, "fine", ""),
            check("b", Level::Warn, "hmm", "do x"),
            check("c", Level::Fail, "bad", "do y"),
        ];
        let v = doctor_json(&checks);
        assert_eq!(v["ok"], false);
        assert_eq!(v["summary"]["fail"], 1);
        assert_eq!(v["checks"][1]["status"], "warn");
        assert_eq!(v["checks"][2]["fix"], "do y");
    }

    #[test]
    fn nearest_existing_walks_up() {
        let tmp = std::env::temp_dir();
        assert_eq!(nearest_existing(&tmp.join("no/such/deep/dir")), Some(tmp));
    }
}

#[cfg(test)]
mod template_tests {
    use super::*;

    #[test]
    fn every_template_has_a_valid_manifest_and_entry_point() {
        let mut names = std::collections::HashSet::new();
        for t in TEMPLATES {
            assert!(names.insert(t.name), "duplicate template {}", t.name);
            assert!(valid_name(t.name));
            assert!(!t.routes.is_empty());
            let m: Manifest = toml::from_str(&t.manifest.replace("__NAME__", "demo"))
                .unwrap_or_else(|e| panic!("{}: {e}", t.name));
            assert_eq!(m.name, "demo");
            if t.wasip3.is_some() {
                let m = Manifest::parse(&t.manifest.replace("__NAME__", "demo"))
                    .unwrap_or_else(|e| panic!("{}: {e}", t.name));
                assert!(m.runtime.is_p3() && !m.grants.data, "{}", t.name);
                assert!(
                    t.main.contains("wasip3::http::service::export!"),
                    "{} has no handler",
                    t.name
                );
                let files = new_files("demo", t);
                assert!(files.iter().all(|(p, _)| *p != "src/spared.rs"));
                let lock = &files.iter().find(|(p, _)| *p == "Cargo.lock").unwrap().1;
                assert!(lock.contains("name = \"demo\"") && lock.contains("name = \"wasip3\""));
                continue;
            }
            assert!(t.main.contains("const SCHEMA"), "{} has no SCHEMA", t.name);
            assert!(
                t.main.contains("#[wstd::http_server]"),
                "{} has no handler",
                t.name
            );
            assert!(
                t.main.lines().count() <= 150,
                "{} is over 150 lines",
                t.name
            );
        }
        assert!(find_template(DEFAULT_TEMPLATE).is_ok());
    }

    #[test]
    fn unknown_template_names_the_choices() {
        let e = find_template("nope").err().unwrap();
        assert_eq!(e.kind, "unknown_template");
        assert!(e.fix.contains("counter") && e.fix.contains("spared templates"));
    }
}

#[cfg(test)]
mod scaffold_tests {
    use super::*;

    fn app_dir(tag: &str, spared_rs: Option<&str>) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spared-helper-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("spared.toml"), "name = \"x\"\n").unwrap();
        if let Some(t) = spared_rs {
            std::fs::write(d.join("src/spared.rs"), t).unwrap();
        }
        d
    }

    #[test]
    fn a_new_app_carries_the_helper_marker_and_gets_no_warning() {
        let files = new_files("x", &TEMPLATES[0]);
        let (_, text) = files.iter().find(|(p, _)| *p == "src/spared.rs").unwrap();
        assert_eq!(helper_version(text), Some(current_helper_version()));
        let d = app_dir("fresh", Some(text));
        assert_eq!(helper_warning(&d), None);
        let c = check_helper(&d).unwrap();
        assert_eq!(c.level, Level::Ok);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_older_helper_or_one_with_no_marker_is_warned_about() {
        let ours = current_helper_version();
        let older = format!(
            "// spared-helper: {}\nfn x() {{}}\n",
            ours.saturating_sub(1)
        );
        for (tag, text) in [("none", "fn x() {}\n".to_string()), ("old", older)] {
            let d = app_dir(tag, Some(&text));
            let w = helper_warning(&d).unwrap_or_else(|| panic!("{tag}: no warning"));
            assert!(w.contains("spared new"), "{w}");
            assert_eq!(check_helper(&d).unwrap().level, Level::Warn, "{tag}");
            std::fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn a_newer_helper_says_to_upgrade_spared() {
        let text = format!("// spared-helper: {}\n", current_helper_version() + 1);
        let d = app_dir("newer", Some(&text));
        assert!(helper_warning(&d).unwrap().contains("Upgrade spared"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_directory_without_a_helper_is_not_checked() {
        let d = app_dir("nohelper", None);
        assert_eq!(helper_warning(&d), None);
        assert!(check_helper(&d).is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn scaffold_lock_names_the_app_and_stays_sorted() {
        for name in ["aaa", "my-app", "zzz"] {
            let v: toml::Value = toml::from_str(&lock_for(name)).unwrap();
            let pkgs = v["package"].as_array().unwrap();
            let names: Vec<&str> = pkgs.iter().map(|p| p["name"].as_str().unwrap()).collect();
            assert_eq!(names.iter().filter(|n| **n == name).count(), 1);
            assert!(!names.contains(&"__NAME__"));
            let root = pkgs
                .iter()
                .find(|p| p["name"].as_str() == Some(name))
                .unwrap();
            assert_eq!(root["version"].as_str(), Some("0.1.0"));
            assert!(root.get("source").is_none());
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted);
        }
    }
}

#[cfg(test)]
mod bench_tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let v: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&v, 50.0), Duration::from_millis(50));
        assert_eq!(percentile(&v, 99.0), Duration::from_millis(99));
        let one = [Duration::from_millis(7)];
        assert_eq!(percentile(&one, 50.0), Duration::from_millis(7));
        assert_eq!(percentile(&one, 99.0), Duration::from_millis(7));
        let s = summarize(vec![
            Duration::from_micros(1500),
            Duration::from_micros(500),
        ]);
        assert_eq!(s["samples"], 2);
        assert_eq!(s["p50_ms"], 0.5);
        assert_eq!(s["max_ms"], 1.5);
    }
}

#[cfg(test)]
mod guest_cflags_tests {
    use super::*;

    #[test]
    fn guest_sqlite_uses_pread_and_pwrite() {
        // Without these SQLite on WASI reads and writes every page through a WASI stream, which
        // wasmtime-wasi runs on a blocking-pool thread: the SQLite microbenchmark went from 1.8x to 3.2x native.
        let f = guest_cflags("/sdk");
        let words: Vec<&str> = f.split_whitespace().collect();
        for flag in [
            "-DHAVE_PREAD=1",
            "-DHAVE_PWRITE=1",
            "-DSQLITE_THREADSAFE=0",
            "-DSQLITE_OMIT_LOAD_EXTENSION=1",
            "-O3",
        ] {
            assert!(words.contains(&flag), "{flag} missing from {f}");
        }
        assert_eq!(words[0], "--sysroot=/sdk/share/wasi-sysroot");
        assert!(!f.contains('#') && !f.contains('\n'), "{f}");
    }
}

#[cfg(test)]
mod shared_target_tests {
    use super::*;

    #[test]
    fn a_unit_belongs_to_the_directory_that_last_built_it() {
        let t = std::env::temp_dir().join(format!("spared-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let a = Path::new("/work/a/my-app");
        let b = Path::new("/work/b/my-app");
        assert!(unit_owned_by_other(&t, "my-app", a), "no stamp yet");
        let stamp = unit_owner_stamp(&t, "my-app");
        std::fs::create_dir_all(stamp.parent().unwrap()).unwrap();
        std::fs::write(&stamp, a.display().to_string()).unwrap();
        assert!(!unit_owned_by_other(&t, "my-app", a));
        assert!(unit_owned_by_other(&t, "my-app", b));
        assert!(unit_owned_by_other(&t, "other-app", a));
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn the_package_name_comes_from_cargo_toml() {
        let d = std::env::temp_dir().join(format!("spared-pkgname-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(package_name(&d), None);
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = \"my-app\"\n").unwrap();
        assert_eq!(package_name(&d).as_deref(), Some("my-app"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}

#[cfg(test)]
mod connect_retry_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn a_server_that_starts_late_is_waited_for() {
        let port = free_port();
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let r = request(
            &format!("http://127.0.0.1:{port}"),
            "GET",
            "/",
            &[],
            Bytes::new(),
        )
        .await;
        let Ok(r) = r else {
            panic!("the request should wait for the listener")
        };
        assert_eq!(r.status, 200);
        late.await.unwrap();
    }

    #[tokio::test]
    async fn a_server_that_never_starts_fails_after_about_two_seconds() {
        let port = free_port();
        let t = std::time::Instant::now();
        let e = request(
            &format!("http://127.0.0.1:{port}"),
            "GET",
            "/",
            &[],
            Bytes::new(),
        )
        .await;
        let Err(e) = e else { panic!("nothing listens") };
        assert_eq!(e.kind, "server_unreachable");
        let took = t.elapsed();
        assert!(
            took >= Duration::from_millis(1900) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }
}

#[cfg(test)]
mod message_tests {
    use super::*;
    use crate::grants::Limits;

    #[test]
    fn a_memory_limit_below_the_apps_minimum_names_the_floor() {
        let msg = "memory minimum size of 19 pages exceeds memory limits";
        assert_eq!(
            wake_failed_fix(msg),
            "raise limits.memory_mb in spared.toml (this app needs at least 2)"
        );
        let big = "memory minimum size of 1025 pages exceeds memory limits";
        assert!(
            wake_failed_fix(big).contains("at least 65)"),
            "{}",
            wake_failed_fix(big)
        );
        assert!(wake_failed_fix("something else").contains("manifest grants"));
        assert!(
            wake_failed_fix("memory minimum size of x pages exceeds memory limits")
                .contains("manifest grants")
        );
    }

    #[test]
    fn a_cpu_trap_uses_the_servers_wording() {
        let limits = Limits {
            cpu_ms: 1000,
            ..Limits::default()
        };
        assert_eq!(
            trap_message(
                TrapKind::CpuLimit,
                "error while executing\nbacktrace:",
                &limits
            ),
            "request used more than 1000 ms of compute"
        );
        assert_eq!(
            trap_message(TrapKind::MemoryLimit, "", &limits),
            "the app hit its memory limit (64 MB)"
        );
        assert_eq!(
            trap_message(
                TrapKind::AppTrap,
                "wasm trap: unreachable\nbacktrace:",
                &limits
            ),
            "wasm trap: unreachable"
        );
        assert_eq!(
            trap_message(TrapKind::AppTrap, "backtrace:", &limits),
            "the app trapped (panic or abort)"
        );
    }

    #[test]
    fn the_dev_server_prints_the_export_line_for_its_own_token() {
        let line = token_export_line(Path::new("/work/my-app/.spared"));
        assert_eq!(
            line,
            "export SPARED_TOKEN=$(cat '/work/my-app/.spared/admin-token')"
        );
        assert!(
            UNAUTHORIZED_FIX.contains("admin-token")
                && UNAUTHORIZED_FIX.contains("~/.spared/token")
        );
    }
}
