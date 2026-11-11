use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wasmparser::{Parser, Payload};
use wasmtime::Engine;

const WORKER_FAILED: i32 = 2;
const STDERR_KEPT: usize = 8 << 10;

#[derive(Clone, Debug)]
pub struct CompileLimits {
    pub max_component_bytes: usize,
    pub max_function_bytes: usize,
    pub max_function_locals: u64,
    pub max_functions: u64,
    pub memory_bytes: u64,
    pub timeout: Duration,
    pub threads: usize,
    pub exe: Option<PathBuf>,
}

impl Default for CompileLimits {
    fn default() -> Self {
        CompileLimits {
            max_component_bytes: 16 << 20,
            max_function_bytes: 1 << 20,
            max_function_locals: 2000,
            max_functions: 50_000,
            memory_bytes: 1024 << 20,
            timeout: Duration::from_secs(60),
            threads: std::thread::available_parallelism().map_or(1, |n| n.get().min(4)),
            exe: None,
        }
    }
}

impl CompileLimits {
    pub fn isolated() -> Self {
        CompileLimits {
            exe: std::env::current_exe().ok(),
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub enum CompileError {
    Invalid(String),
    TooComplex(String),
    Failed(String),
}

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

pub fn prescan(wasm: &[u8], l: &CompileLimits) -> Result<(), CompileError> {
    let mut functions = 0u64;
    for payload in Parser::new(0).parse_all(wasm) {
        let Ok(payload) = payload else {
            return Ok(());
        };
        match payload {
            Payload::CodeSectionStart { count, .. } => {
                functions = functions.saturating_add(u64::from(count));
                if functions > l.max_functions {
                    return Err(CompileError::TooComplex(format!(
                        "the component has more than {} functions",
                        l.max_functions
                    )));
                }
            }
            Payload::CodeSectionEntry(body) => {
                let range = body.range();
                let bytes = (range.end - range.start) as usize;
                if bytes > l.max_function_bytes {
                    return Err(CompileError::TooComplex(format!(
                        "a function in the component is {} KB, over the limit of {} KB",
                        bytes.div_ceil(1024),
                        l.max_function_bytes / 1024
                    )));
                }
                let Ok(reader) = body.get_locals_reader() else {
                    return Ok(());
                };
                let mut locals = 0u64;
                for group in reader {
                    let Ok((count, _)) = group else {
                        return Ok(());
                    };
                    locals = locals.saturating_add(u64::from(count));
                }
                if locals > l.max_function_locals {
                    return Err(CompileError::TooComplex(format!(
                        "a function in the component has {locals} locals, over the limit of {}",
                        l.max_function_locals
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn compile(
    engine: &Engine,
    l: &CompileLimits,
    wasm: &[u8],
    scan: bool,
) -> Result<Vec<u8>, CompileError> {
    if scan {
        prescan(wasm, l)?;
    }
    match &l.exe {
        Some(exe) => isolated(exe, l, wasm),
        None => engine
            .precompile_component(wasm)
            .map_err(|e| CompileError::Invalid(format!("{e:#}"))),
    }
}

fn isolated(exe: &PathBuf, l: &CompileLimits, wasm: &[u8]) -> Result<Vec<u8>, CompileError> {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let failed = |what: &str, e: std::io::Error| CompileError::Failed(format!("{what}: {e}"));
    let mut cmd = Command::new(exe);
    cmd.arg("compile-worker")
        .arg("--memory-mb")
        .arg((l.memory_bytes >> 20).to_string())
        .arg("--cpu-secs")
        .arg((l.timeout.as_secs() * l.threads.max(1) as u64 + 5).to_string())
        .env_clear()
        .env("RAYON_NUM_THREADS", l.threads.max(1).to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| failed("cannot start the compiler process", e))?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let started = Instant::now();
    let (out, err, status, timed_out) = std::thread::scope(|sc| {
        sc.spawn(move || {
            let _ = stdin.write_all(wasm);
        });
        let out = sc.spawn(move || {
            let mut v = Vec::new();
            let _ = stdout.read_to_end(&mut v);
            v
        });
        let err = sc.spawn(move || {
            let mut v = Vec::new();
            let _ = (&mut stderr).take(STDERR_KEPT as u64).read_to_end(&mut v);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            v
        });
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if started.elapsed() >= l.timeout => {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break None,
            }
        };
        (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
            status,
            timed_out,
        )
    });
    if timed_out {
        return Err(CompileError::TooComplex(format!(
            "compiling took longer than {} s and was stopped",
            l.timeout.as_secs()
        )));
    }
    match status.and_then(|s| s.code()) {
        Some(0) => Ok(out),
        Some(WORKER_FAILED) => Err(CompileError::Invalid(
            String::from_utf8_lossy(&err).trim().to_string(),
        )),
        _ => Err(CompileError::TooComplex(format!(
            "compiling needed more than {} MB of memory or was otherwise cut short",
            l.memory_bytes >> 20
        ))),
    }
}

pub fn worker_main(memory_mb: u64, cpu_secs: u64) -> ! {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
    let limit = |resource, value: u64| {
        let r = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: value as libc::rlim_t,
        };
        unsafe { libc::setrlimit(resource, &r) };
    };
    limit(libc::RLIMIT_AS, memory_mb.saturating_mul(1 << 20));
    limit(libc::RLIMIT_CPU, cpu_secs);
    limit(libc::RLIMIT_NOFILE, 16);
    let mut wasm = Vec::new();
    let result = std::io::stdin()
        .read_to_end(&mut wasm)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            crate::sandbox::engine()
                .and_then(|e| e.precompile_component(&wasm))
                .map_err(|e| format!("{e:#}"))
        });
    match result {
        Ok(cwasm) => {
            let mut out = std::io::stdout().lock();
            let wrote = out.write_all(&cwasm).and_then(|_| out.flush());
            std::process::exit(if wrote.is_ok() { 0 } else { 1 })
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(WORKER_FAILED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(locals: u32, nops: usize) -> Vec<u8> {
        let body = "nop ".repeat(nops);
        wat::parse_str(format!(
            "(component (core module (func (local {}) {body})))",
            "i32 ".repeat(locals as usize)
        ))
        .unwrap()
    }

    #[test]
    fn a_function_with_few_locals_passes() {
        assert!(prescan(&function(10, 10), &CompileLimits::default()).is_ok());
    }

    #[test]
    fn too_many_locals_are_refused() {
        let l = CompileLimits {
            max_function_locals: 100,
            ..Default::default()
        };
        let Err(CompileError::TooComplex(m)) = prescan(&function(101, 1), &l) else {
            panic!("expected TooComplex");
        };
        assert!(m.contains("101 locals"), "{m}");
        assert!(prescan(&function(100, 1), &l).is_ok());
    }

    #[test]
    fn a_large_function_is_refused() {
        let l = CompileLimits {
            max_function_bytes: 2048,
            ..Default::default()
        };
        assert!(matches!(
            prescan(&function(1, 4000), &l),
            Err(CompileError::TooComplex(m)) if m.contains("KB")
        ));
        assert!(prescan(&function(1, 100), &l).is_ok());
    }

    #[test]
    fn too_many_functions_are_refused() {
        let wat = format!("(component (core module {}))", "(func nop) ".repeat(20));
        let wasm = wat::parse_str(wat).unwrap();
        let l = CompileLimits {
            max_functions: 19,
            ..Default::default()
        };
        assert!(matches!(
            prescan(&wasm, &l),
            Err(CompileError::TooComplex(_))
        ));
        let l = CompileLimits {
            max_functions: 20,
            ..Default::default()
        };
        assert!(prescan(&wasm, &l).is_ok());
    }

    #[test]
    fn modules_nested_in_components_are_scanned() {
        let wasm = wat::parse_str(format!(
            "(component (component (core module (func (local {}) nop))))",
            "i32 ".repeat(50)
        ))
        .unwrap();
        let l = CompileLimits {
            max_function_locals: 10,
            ..Default::default()
        };
        assert!(prescan(&wasm, &l).is_err());
    }

    #[test]
    fn bytes_that_are_not_wasm_are_left_to_the_compiler() {
        assert!(prescan(b"not wasm at all", &CompileLimits::default()).is_ok());
    }

    #[test]
    fn without_a_worker_process_the_engine_compiles_in_place() {
        let engine = crate::sandbox::engine().unwrap();
        let wasm = function(3, 3);
        let cwasm = compile(&engine, &CompileLimits::default(), &wasm, true).unwrap();
        assert!(!cwasm.is_empty());
        assert!(matches!(
            compile(&engine, &CompileLimits::default(), b"junk", true),
            Err(CompileError::Invalid(_))
        ));
    }

    #[test]
    fn a_worker_that_cannot_start_is_a_failure_not_a_verdict() {
        let l = CompileLimits {
            exe: Some(PathBuf::from("/nonexistent/spared")),
            ..Default::default()
        };
        let engine = crate::sandbox::engine().unwrap();
        assert!(matches!(
            compile(&engine, &l, &function(1, 1), true),
            Err(CompileError::Failed(_))
        ));
    }
}
