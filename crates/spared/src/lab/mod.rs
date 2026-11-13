//! Test-lab commands, built only with `--features lab`. They exist to reproduce the numbers in
//! docs/benchmarks.md and to run the sandbox escape suite; a normal build contains none of this
//! code, none of its environment switches and no native SQLite.
//!
//! The escape suite builds its sandbox with the same functions the server uses (`sandbox::linker`,
//! `wasi_builder`, `store_for`). The wake, density and request-cost harnesses deliberately keep a
//! bare store with no limits: they reproduce numbers that were taken that way.
//!
//! A lab binary is not for serving apps: `spared serve` in a lab build still uses the same engine
//! configuration as a release build and ignores every switch read here.

mod bench3b;
mod density;
mod escape;
mod wake;

use clap::Subcommand;
use std::path::PathBuf;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Result, Store};
use wasmtime_wasi::p2::bindings::Command;
use wasmtime_wasi::p2::bindings::sync::Command as SyncCommand;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

#[derive(Subcommand)]
pub enum LabCmd {
    /// Run a command-style component against a granted data directory.
    #[command(hide = true)]
    RunProbe {
        component: PathBuf,
        #[arg(long)]
        data: PathBuf,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Wake an app repeatedly and time instantiate plus first query.
    #[command(hide = true)]
    BenchWake {
        component: PathBuf,
        #[arg(long)]
        data: PathBuf,
        #[arg(long, default_value_t = 2000)]
        iters: usize,
        #[arg(long, default_value_t = 100)]
        seed: usize,
    },
    /// Deploy many apps, keep some hot, report memory and latency.
    #[command(hide = true)]
    BenchDensity {
        component: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 10000)]
        deployed: usize,
        #[arg(long, default_value_t = 500)]
        hot: usize,
        #[arg(long, default_value_t = 20)]
        rows: usize,
    },
    /// One escape case in a process of its own, so it runs with the environment the suite gives
    /// it: the debug text of a failed stream, and whether this process captures host backtraces.
    #[command(hide = true)]
    DebugTextProbe {
        #[arg(long)]
        probes: PathBuf,
        #[arg(long)]
        data: PathBuf,
    },
    /// Run the sandbox escape suite against the probe guests.
    #[command(hide = true)]
    EscapeTest {
        #[arg(long)]
        probes: PathBuf,
        #[arg(long)]
        root: PathBuf,
    },
    /// Request-cost benchmark: cost per realistic request against a live instance.
    #[command(name = "bench-3b", hide = true)]
    Bench3b {
        component: PathBuf,
        #[arg(long)]
        variant: bench3b::Variant,
        #[arg(long)]
        data: PathBuf,
        #[arg(long, default_value_t = 10000)]
        requests: usize,
        #[arg(long, default_value_t = 500)]
        warmup: usize,
        #[arg(long, default_value_t = 1)]
        run: u32,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Precompile a component to a .cwasm artifact.
    #[command(hide = true)]
    Compile { component: PathBuf, out: PathBuf },
}

/// The server's engine configuration, plus `SPARED_NO_EPOCH` for measuring what epoch
/// interruption costs. The compiled artifact only loads into an engine with the same setting.
fn engine() -> Result<Engine> {
    let mut c = crate::sandbox::engine_config();
    if std::env::var_os("SPARED_NO_EPOCH").is_some() {
        c.epoch_interruption(false);
    }
    Engine::new(&c)
}

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

pub async fn run(cmd: LabCmd) -> Result<()> {
    match cmd {
        LabCmd::BenchDensity {
            component,
            root,
            deployed,
            hot,
            rows,
        } => density::run(engine()?, component, root, deployed, hot, rows).await?,
        LabCmd::EscapeTest { probes, root } => escape::run(probes, root).await?,
        LabCmd::DebugTextProbe { probes, data } => escape::debug_text_child(probes, data).await?,
        LabCmd::BenchWake {
            component,
            data,
            iters,
            seed,
        } => wake::bench(engine()?, component, data, iters, seed).await?,
        LabCmd::Bench3b {
            component,
            variant,
            data,
            requests,
            warmup,
            run,
            out,
        } => {
            bench3b::run(
                engine()?,
                component,
                variant,
                data,
                requests,
                warmup,
                run,
                out,
            )
            .await?
        }
        LabCmd::Compile { component, out } => {
            let bytes = std::fs::read(&component)?;
            std::fs::write(out, engine()?.precompile_component(&bytes)?)?;
        }
        LabCmd::RunProbe {
            component,
            data,
            args,
        } => run_probe(component, data, args).await?,
    }
    Ok(())
}

async fn run_probe(component: PathBuf, data: PathBuf, args: Vec<String>) -> Result<()> {
    let engine = engine()?;
    let comp = if component.extension().is_some_and(|e| e == "cwasm") {
        unsafe { Component::deserialize_file(&engine, &component)? }
    } else {
        Component::from_file(&engine, &component)?
    };
    let mut b = WasiCtx::builder();
    b.inherit_stdout().inherit_stderr();
    b.args(
        &std::iter::once("probe".to_string())
            .chain(args)
            .collect::<Vec<_>>(),
    );
    b.preopened_dir(&data, "/data", FsPerms::ReadWrite)?;
    // The default is the server's host path: the async WASI linker with
    // allow_blocking_current_thread, so positional file calls (pread, pwrite, fsync) run on
    // the calling thread. Stream reads and writes still go through tokio's blocking pool
    // in wasmtime-wasi 50 (docs/sqlite-in-wasm-speedup.md). SPARED_PROBE_LEGACY_IO=1 reproduces
    // the harness the first SQLite microbenchmark numbers were taken with: the sync linker on a plain
    // thread, where every file call crosses to a blocking-pool thread.
    let legacy = std::env::var_os("SPARED_PROBE_LEGACY_IO").is_some();
    b.allow_blocking_current_thread(!legacy);
    let mut store = Store::new(
        &engine,
        Host {
            ctx: b.build(),
            table: ResourceTable::new(),
        },
    );
    store.set_epoch_deadline(u64::MAX / 2);
    let mut linker: Linker<Host> = Linker::new(&engine);
    let failed = if legacy {
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        std::thread::scope(|sc| {
            sc.spawn(|| -> Result<bool> {
                let cmd = SyncCommand::instantiate(&mut store, &comp, &linker)?;
                Ok(cmd.wasi_cli_run().call_run(&mut store)?.is_err())
            })
            .join()
            .expect("probe thread")
        })?
    } else {
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        let cmd = Command::instantiate_async(&mut store, &comp, &linker).await?;
        cmd.wasi_cli_run().call_run(&mut store).await?.is_err()
    };
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
