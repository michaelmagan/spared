//! A cache for the compiled SQLite object every guest build needs.
//!
//! An app links `libsqlite3-sys` with its `bundled` feature, whose build script compiles
//! `sqlite3.c` with wasi-sdk clang. That one compile is most of a first build. The CLI points the
//! build's C compiler at a symlink to this binary (`spared-cc`). The wrapper forwards every
//! command to the real clang. When the command compiles `sqlite3.c` it first looks for an object
//! built from the same command, and on a miss it runs clang and keeps the object.
//!
//! The libsqlite3-sys build script still runs unchanged: it picks the flags, writes the bindings
//! and archives the object. Nothing in an app's `Cargo.toml` or `Cargo.lock` changes, and the
//! flags cannot drift from what a build without the cache uses, because the cache key is that
//! command itself.

use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// File name the wrapper symlink carries; `main` runs the wrapper when argv[0] has this name.
pub const WRAPPER_NAME: &str = "spared-cc";
/// Real compiler the wrapper forwards to.
const ENV_REAL: &str = "SPARED_CC_REAL";
/// Directory the wrapper keeps entries in.
const ENV_CACHE: &str = "SPARED_CC_CACHE";
/// Overrides `~/.spared/lib`.
pub const ENV_LIB_DIR: &str = "SPARED_LIB_DIR";
/// Set to anything but empty or `0` to build without the wrapper.
pub const ENV_DISABLE: &str = "SPARED_NO_SQLITE_CACHE";
/// Bumped when the key or the entry layout changes, so an old entry is never read as a new one.
const FORMAT: &str = "spared-sqlite-cache-1";
const OBJECT: &str = "sqlite3.o";
const KEY_FILE: &str = "key.txt";
/// Environment variables that change what clang compiles without appearing in its arguments.
const COMPILE_ENV: &[&str] = &["CPATH", "C_INCLUDE_PATH", "CCC_OVERRIDE_OPTIONS", "SDKROOT"];

/// Where the cache and the wrapper symlink live: `$SPARED_LIB_DIR`, else `~/.spared/lib`.
pub fn lib_dir(home: &Path) -> PathBuf {
    match std::env::var_os(ENV_LIB_DIR) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home.join(".spared/lib"),
    }
}

pub fn disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some_and(|v| !v.is_empty() && v != "0")
}

/// The C compiler a guest build should use and the environment that goes with it.
pub struct GuestCc {
    pub cc: PathBuf,
    pub env: Vec<(&'static str, OsString)>,
}

/// The wrapper when it can be set up, else the real compiler itself, so a build never fails
/// because the cache could not be prepared.
pub fn guest_cc(real_cc: &Path, lib: &Path) -> GuestCc {
    let plain = GuestCc {
        cc: real_cc.to_path_buf(),
        env: vec![],
    };
    if disabled() {
        return plain;
    }
    let Ok(exe) = std::env::current_exe() else {
        return plain;
    };
    if !exe.is_file() {
        return plain;
    }
    let bin = lib.join("bin");
    let link = bin.join(WRAPPER_NAME);
    if std::fs::create_dir_all(&bin).is_err() || !link_to(&link, &exe) {
        return plain;
    }
    GuestCc {
        cc: link,
        env: vec![
            (ENV_REAL, real_cc.as_os_str().to_owned()),
            (ENV_CACHE, lib.as_os_str().to_owned()),
        ],
    }
}

fn link_to(link: &Path, target: &Path) -> bool {
    if std::fs::read_link(link).is_ok_and(|t| t == target) {
        return true;
    }
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// True when this process was started as the compiler wrapper.
pub fn is_wrapper_invocation() -> bool {
    std::env::args_os()
        .next()
        .is_some_and(|a| Path::new(&a).file_name() == Some(OsStr::new(WRAPPER_NAME)))
}

/// Run as the compiler wrapper and exit.
pub fn wrapper_main() -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(real) = std::env::var_os(ENV_REAL).map(PathBuf::from) else {
        eprintln!("{WRAPPER_NAME}: {ENV_REAL} is not set; run builds through `spared`");
        std::process::exit(127);
    };
    let cache = std::env::var_os(ENV_CACHE).map(PathBuf::from);
    if let (Some(cache), Some(compile)) = (cache.as_deref(), parse_compile(&args)) {
        std::process::exit(compile_cached(&real, cache, &compile, &args));
    }
    forward(&real, &args)
}

fn forward(real: &Path, args: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = Command::new(real).args(args).exec();
    eprintln!("{WRAPPER_NAME}: cannot run {}: {err}", real.display());
    std::process::exit(127);
}

fn run_real(real: &Path, args: &[OsString]) -> i32 {
    match Command::new(real).args(args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("{WRAPPER_NAME}: cannot run {}: {e}", real.display());
            127
        }
    }
}

/// A command that compiles SQLite's amalgamation to one object file.
#[derive(Debug, PartialEq)]
pub struct Compile {
    pub source: PathBuf,
    pub out: PathBuf,
    /// Every argument except the source and `-o <out>`, in order.
    pub flags: Vec<OsString>,
}

/// `Some` only for `clang <flags> -o <out> -c <dir>/sqlite3.c`; every other command is forwarded.
pub fn parse_compile(args: &[OsString]) -> Option<Compile> {
    let mut out = None;
    let mut source = None;
    let mut flags = Vec::new();
    let mut compile_only = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-o" {
            if out.is_some() {
                return None;
            }
            out = Some(PathBuf::from(it.next()?));
        } else if a == "-c" {
            compile_only = true;
            flags.push(a.clone());
        } else if !a.to_string_lossy().starts_with('-')
            && Path::new(a).file_name() == Some(OsStr::new("sqlite3.c"))
        {
            if source.is_some() {
                return None;
            }
            source = Some(PathBuf::from(a));
        } else {
            flags.push(a.clone());
        }
    }
    if !compile_only {
        return None;
    }
    Some(Compile {
        source: source?,
        out: out?,
        flags,
    })
}

fn sha_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Identity of the compiler install: clang's version banner (without the directory it runs from,
/// so moving the SDK keeps the cache) and wasi-sdk's `VERSION` file (the pinned wasi-libc, llvm and
/// config commits). `None` when clang does not answer.
pub fn sdk_id(real_cc: &Path) -> Option<String> {
    let out = Command::new(real_cc).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let version_file = real_cc
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("VERSION"))
        .and_then(|p| std::fs::read(p).ok())
        .unwrap_or_default();
    let banner: String = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with("InstalledDir:"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(sha_hex(&[banner.as_bytes(), &version_file[..]])[..16].to_string())
}

/// The full key: what is compiled (the source bytes), how (every flag), and with what (the SDK).
/// The output path and the source's location do not matter.
pub fn cache_key(sdk: &str, flags: &[OsString], source: &[u8], env: &[(String, String)]) -> String {
    let mut parts: Vec<Vec<u8>> = vec![FORMAT.into(), sdk.into(), source.to_vec()];
    for f in flags {
        parts.push(f.as_encoded_bytes().to_vec());
    }
    parts.push(b"env".to_vec());
    for (k, v) in env {
        parts.push(format!("{k}={v}").into_bytes());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    sha_hex(&refs)
}

/// `3.51.1` from the `#define SQLITE_VERSION "3.51.1"` line of the amalgamation.
pub fn sqlite_version(source: &[u8]) -> String {
    let marker = b"#define SQLITE_VERSION ";
    let found = source
        .windows(marker.len())
        .position(|w| w == marker)
        .and_then(|i| {
            let rest = &source[i + marker.len()..];
            let rest = &rest[rest.iter().position(|&b| b == b'"')? + 1..];
            let end = rest.iter().position(|&b| b == b'"')?;
            String::from_utf8(rest[..end].to_vec()).ok()
        })
        .filter(|v| {
            !v.is_empty()
                && v.len() < 24
                && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
        });
    found.unwrap_or_else(|| "unknown".to_string())
}

fn entry_name(version: &str, key: &str) -> String {
    format!("sqlite-{version}-{}", &key[..16])
}

fn compile_env() -> Vec<(String, String)> {
    COMPILE_ENV
        .iter()
        .filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?)))
        .collect()
}

/// Compile through the cache and return clang's exit code. Any trouble with the cache itself
/// (no readable source, a damaged entry, a directory that cannot be written) falls back to the
/// plain compile.
pub fn compile_cached(real: &Path, cache: &Path, compile: &Compile, args: &[OsString]) -> i32 {
    let (Some(sdk), Ok(source)) = (sdk_id(real), std::fs::read(&compile.source)) else {
        return run_real(real, args);
    };
    let key = cache_key(&sdk, &compile.flags, &source, &compile_env());
    let dir = cache.join(entry_name(&sqlite_version(&source), &key));
    if restore(&dir, &key, &compile.out) {
        return 0;
    }
    let code = run_real(real, args);
    if code == 0 {
        let note = Note {
            key: &key,
            sdk: &sdk,
            sqlite: &sqlite_version(&source),
            krate: &crate_dir_name(),
            flags: &compile.flags,
        };
        let _ = store(cache, &dir, &note, &compile.out);
    }
    code
}

fn crate_dir_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

struct Note<'a> {
    key: &'a str,
    sdk: &'a str,
    sqlite: &'a str,
    krate: &'a str,
    flags: &'a [OsString],
}

fn read_object(path: &Path) -> Option<(Vec<u8>, String)> {
    let bytes = std::fs::read(path).ok()?;
    let sha = sha_hex(&[&bytes[..]]);
    Some((bytes, sha))
}

fn restore(dir: &Path, key: &str, out: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(KEY_FILE)) else {
        return false;
    };
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
    };
    if text.lines().next() != Some(FORMAT) || field("key") != Some(key) {
        return false;
    }
    let Some((bytes, sha)) = read_object(&dir.join(OBJECT)) else {
        return false;
    };
    if bytes.is_empty() || field("object_sha256") != Some(sha.as_str()) {
        return false;
    }
    std::fs::write(out, bytes).is_ok()
}

fn store(cache: &Path, dir: &Path, note: &Note, out: &Path) -> std::io::Result<()> {
    let (bytes, sha) = read_object(out)
        .filter(|(b, _)| !b.is_empty())
        .ok_or_else(|| std::io::Error::other("no object to keep"))?;
    std::fs::create_dir_all(cache)?;
    let tmp = cache.join(format!(".tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let flags: Vec<String> = note
        .flags
        .iter()
        .map(|f| f.to_string_lossy().into_owned())
        .collect();
    let text = format!(
        "{FORMAT}\nkey={}\nsdk={}\nsqlite={}\ncrate={}\nobject_sha256={sha}\nflags={}\n",
        note.key,
        note.sdk,
        note.sqlite,
        note.krate,
        flags.join(" ")
    );
    let written = std::fs::write(tmp.join(OBJECT), &bytes)
        .and_then(|_| std::fs::write(tmp.join(KEY_FILE), text))
        .and_then(|_| {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::rename(&tmp, dir)
        });
    if written.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    written
}

/// One cached object, as `key.txt` describes it.
#[derive(Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub sdk: String,
    pub sqlite: String,
    pub krate: String,
}

/// The entries under `cache`, oldest name first. Directories without a readable `key.txt` are skipped.
pub fn entries(cache: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(cache) else {
        return vec![];
    };
    let mut out: Vec<Entry> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("sqlite-") {
                return None;
            }
            let text = std::fs::read_to_string(e.path().join(KEY_FILE)).ok()?;
            if text.lines().next() != Some(FORMAT) || !e.path().join(OBJECT).is_file() {
                return None;
            }
            let field = |n: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(n)?.strip_prefix('='))
                    .unwrap_or("")
                    .to_string()
            };
            Some(Entry {
                name,
                sdk: field("sdk"),
                sqlite: field("sqlite"),
                krate: field("crate"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Entries built with the compiler install `sdk` names.
pub fn entries_for_sdk(cache: &Path, sdk: &str) -> Vec<Entry> {
    entries(cache)
        .into_iter()
        .filter(|e| e.sdk == sdk)
        .collect()
}

/// Whether `spared setup` has nothing to compile: an entry for this SDK that came from the
/// libsqlite3-sys directory (`libsqlite3-sys-0.39.1`) the app template locks.
pub fn is_warm(cache: &Path, sdk: &str, krate: &str) -> bool {
    entries_for_sdk(cache, sdk).iter().any(|e| e.krate == krate)
}

/// `libsqlite3-sys-0.39.1`, read from the `[[package]]` block of a Cargo.lock text.
pub fn locked_crate_dir(lock: &str) -> Option<String> {
    let at = lock.find("name = \"libsqlite3-sys\"")?;
    let version = lock[at..]
        .lines()
        .nth(1)?
        .strip_prefix("version = \"")?
        .strip_suffix('"')?;
    Some(format!("libsqlite3-sys-{version}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let d =
                std::env::temp_dir().join(format!("spared-sqlcache-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Scratch(d)
        }
        fn path(&self, p: &str) -> PathBuf {
            self.0.join(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    /// A stand-in for clang: `--version` prints a banner, a compile writes "object:<n>" to `-o`
    /// and appends a line to `calls`, and a file named `fail` next to `calls` makes it fail.
    fn fake_clang(s: &Scratch, banner: &str) -> PathBuf {
        let bin = s.path("sdk/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "{banner}"; exit 0; fi
echo "$@" >> "{calls}"
[ -e "{fail}" ] && exit 1
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then out="$2"; fi
  shift
done
echo "object:$(wc -l < "{calls}")" > "$out"
"#,
            calls = s.path("calls").display(),
            fail = s.path("fail").display()
        );
        let p = bin.join("clang");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn calls(s: &Scratch) -> usize {
        std::fs::read_to_string(s.path("calls")).map_or(0, |t| t.lines().count())
    }

    fn compile_args(s: &Scratch, out: &str, extra: &[&str]) -> (Vec<OsString>, Compile) {
        let mut a = os(&["-O2", "-DSQLITE_THREADSAFE=0"]);
        a.extend(os(extra));
        a.extend(os(&[
            "-o",
            s.path(out).to_str().unwrap(),
            "-c",
            s.path("src/sqlite3.c").to_str().unwrap(),
        ]));
        let c = parse_compile(&a).expect("a sqlite3.c compile");
        (a, c)
    }

    fn with_source(s: &Scratch, text: &str) {
        std::fs::create_dir_all(s.path("src")).unwrap();
        std::fs::write(s.path("src/sqlite3.c"), text).unwrap();
    }

    const SOURCE: &str = "#define SQLITE_VERSION        \"3.51.1\"\nint x;\n";

    #[test]
    fn only_a_sqlite_compile_is_parsed() {
        let c = parse_compile(&os(&["-O2", "-o", "/o/a.o", "-c", "sqlite3/sqlite3.c"])).unwrap();
        assert_eq!(c.out, PathBuf::from("/o/a.o"));
        assert_eq!(c.source, PathBuf::from("sqlite3/sqlite3.c"));
        assert_eq!(c.flags, os(&["-O2", "-c"]));
        assert!(parse_compile(&os(&["-E", "/o/detect_compiler_family.c"])).is_none());
        assert!(parse_compile(&os(&["--version"])).is_none());
        assert!(parse_compile(&os(&["-o", "/o/a.o", "-c", "other.c"])).is_none());
        assert!(
            parse_compile(&os(&["-o", "/o/a", "sqlite3.c"])).is_none(),
            "a link, not -c"
        );
        assert!(parse_compile(&os(&["-c", "sqlite3.c"])).is_none(), "no -o");
        assert!(parse_compile(&os(&["-o", "a", "-o", "b", "-c", "sqlite3.c"])).is_none());
        assert!(parse_compile(&os(&["-c", "-o", "a.o", "sqlite3.c", "other/sqlite3.c"])).is_none());
    }

    #[test]
    fn key_follows_source_flags_sdk_and_env_but_not_paths() {
        let flags = os(&["-O2", "-DA=1", "-c"]);
        let base = cache_key("sdk1", &flags, b"src", &[]);
        assert_eq!(base, cache_key("sdk1", &flags, b"src", &[]));
        assert_eq!(base.len(), 64);
        assert_ne!(base, cache_key("sdk2", &flags, b"src", &[]));
        assert_ne!(base, cache_key("sdk1", &flags, b"src2", &[]));
        assert_ne!(
            base,
            cache_key("sdk1", &os(&["-O3", "-DA=1", "-c"]), b"src", &[])
        );
        assert_ne!(
            base,
            cache_key("sdk1", &os(&["-O2", "-c", "-DA=1"]), b"src", &[])
        );
        assert_ne!(
            base,
            cache_key("sdk1", &os(&["-O2", "-DA=1", "-DB", "-c"]), b"src", &[])
        );
        assert_ne!(
            base,
            cache_key("sdk1", &flags, b"src", &[("CPATH".into(), "/x".into())])
        );
        let a = parse_compile(&os(&["-O2", "-o", "/one/x.o", "-c", "/reg1/sqlite3.c"])).unwrap();
        let b = parse_compile(&os(&["-O2", "-o", "/two/y.o", "-c", "/reg2/sqlite3.c"])).unwrap();
        assert_eq!(
            cache_key("s", &a.flags, b"src", &[]),
            cache_key("s", &b.flags, b"src", &[])
        );
    }

    #[test]
    fn field_boundaries_are_part_of_the_key() {
        assert_ne!(
            cache_key("s", &os(&["ab", "c"]), b"", &[]),
            cache_key("s", &os(&["a", "bc"]), b"", &[])
        );
    }

    #[test]
    fn reads_the_sqlite_version_and_distrusts_odd_ones() {
        assert_eq!(sqlite_version(SOURCE.as_bytes()), "3.51.1");
        assert_eq!(sqlite_version(b"nothing here"), "unknown");
        assert_eq!(
            sqlite_version(b"#define SQLITE_VERSION \"../../etc\"\n"),
            "unknown"
        );
    }

    #[test]
    fn locked_crate_dir_reads_the_template_lock() {
        let lock = include_str!("../templates/Cargo.lock.template");
        let dir = locked_crate_dir(lock).expect("the template locks libsqlite3-sys");
        assert!(dir.starts_with("libsqlite3-sys-0."), "{dir}");
        assert_eq!(locked_crate_dir("[[package]]\nname = \"other\"\n"), None);
    }

    #[test]
    fn second_compile_comes_from_the_cache_and_matches_the_first() {
        let s = Scratch::new("hit");
        let clang = fake_clang(&s, "clang A");
        with_source(&s, SOURCE);
        let cache = s.path("lib");
        let (args, c) = compile_args(&s, "one.o", &[]);
        assert_eq!(compile_cached(&clang, &cache, &c, &args), 0);
        assert_eq!(calls(&s), 1);
        let (args, c) = compile_args(&s, "two.o", &[]);
        assert_eq!(compile_cached(&clang, &cache, &c, &args), 0);
        assert_eq!(calls(&s), 1, "the second compile must not run clang");
        assert_eq!(
            std::fs::read(s.path("one.o")).unwrap(),
            std::fs::read(s.path("two.o")).unwrap()
        );
        let found = entries(&cache);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].name.starts_with("sqlite-3.51.1-"),
            "{}",
            found[0].name
        );
        assert_eq!(found[0].sqlite, "3.51.1");
        assert!(!found[0].sdk.is_empty());
        assert_eq!(entries_for_sdk(&cache, &found[0].sdk).len(), 1);
        assert!(entries_for_sdk(&cache, "other").is_empty());
    }

    #[test]
    fn a_changed_flag_sdk_or_source_misses_and_compiles() {
        let s = Scratch::new("miss");
        let clang = fake_clang(&s, "clang A");
        with_source(&s, SOURCE);
        let cache = s.path("lib");
        let (args, c) = compile_args(&s, "a.o", &[]);
        compile_cached(&clang, &cache, &c, &args);
        let (args, c) = compile_args(&s, "b.o", &["-DSQLITE_OMIT_SHARED_CACHE"]);
        compile_cached(&clang, &cache, &c, &args);
        assert_eq!(calls(&s), 2, "an added flag is a different object");
        with_source(&s, "#define SQLITE_VERSION \"3.52.0\"\n");
        let (args, c) = compile_args(&s, "c.o", &[]);
        compile_cached(&clang, &cache, &c, &args);
        assert_eq!(calls(&s), 3, "a different sqlite is a different object");
        fake_clang(&s, "clang B");
        with_source(&s, SOURCE);
        let (args, c) = compile_args(&s, "d.o", &[]);
        compile_cached(&clang, &cache, &c, &args);
        assert_eq!(calls(&s), 4, "a different compiler is a different object");
        assert_eq!(entries(&cache).len(), 4);
        let (args, c) = compile_args(&s, "e.o", &[]);
        compile_cached(&clang, &cache, &c, &args);
        assert_eq!(calls(&s), 4, "and each is kept");
    }

    #[test]
    fn a_damaged_entry_is_rebuilt_not_trusted() {
        let s = Scratch::new("damaged");
        let clang = fake_clang(&s, "clang A");
        with_source(&s, SOURCE);
        let cache = s.path("lib");
        let (args, c) = compile_args(&s, "a.o", &[]);
        compile_cached(&clang, &cache, &c, &args);
        let entry = cache.join(&entries(&cache)[0].name);
        std::fs::write(entry.join(OBJECT), b"corrupt").unwrap();
        let (args, c) = compile_args(&s, "b.o", &[]);
        assert_eq!(compile_cached(&clang, &cache, &c, &args), 0);
        assert_eq!(calls(&s), 2, "a damaged object forces a compile");
        assert_eq!(
            std::fs::read_to_string(s.path("b.o")).unwrap(),
            "object:2\n"
        );
        let (args, c) = compile_args(&s, "c.o", &[]);
        compile_cached(&clang, &cache, &c, &args);
        assert_eq!(calls(&s), 2, "the rebuilt entry is good again");
        std::fs::remove_file(entry.join(OBJECT)).unwrap();
        let (args, c) = compile_args(&s, "d.o", &[]);
        assert_eq!(compile_cached(&clang, &cache, &c, &args), 0);
        assert_eq!(calls(&s), 3);
        std::fs::write(entry.join(KEY_FILE), "garbage").unwrap();
        let (args, c) = compile_args(&s, "e.o", &[]);
        assert_eq!(compile_cached(&clang, &cache, &c, &args), 0);
        assert_eq!(
            calls(&s),
            4,
            "an unreadable key file is a miss, not an error"
        );
    }

    #[test]
    fn a_missing_or_unwritable_cache_still_compiles() {
        let s = Scratch::new("nowrite");
        let clang = fake_clang(&s, "clang A");
        with_source(&s, SOURCE);
        std::fs::write(
            s.path("blocker"),
            b"a file where the cache directory should be",
        )
        .unwrap();
        let cache = s.path("blocker/lib");
        let (args, c) = compile_args(&s, "a.o", &[]);
        assert_eq!(compile_cached(&clang, &cache, &c, &args), 0);
        assert_eq!(
            std::fs::read_to_string(s.path("a.o")).unwrap(),
            "object:1\n"
        );
        assert!(entries(&cache).is_empty());
        let (args, c) = compile_args(&s, "b.o", &[]);
        assert_eq!(compile_cached(&clang, &s.path("never-made"), &c, &args), 0);
    }

    #[test]
    fn a_failed_compile_is_reported_and_not_kept() {
        let s = Scratch::new("fail");
        let clang = fake_clang(&s, "clang A");
        with_source(&s, SOURCE);
        let cache = s.path("lib");
        let (args, c) = compile_args(&s, "a.o", &[]);
        std::fs::write(s.path("fail"), b"").unwrap();
        assert_eq!(compile_cached(&clang, &cache, &c, &args), 1);
        assert!(entries(&cache).is_empty());
    }

    #[test]
    fn an_unreadable_source_or_compiler_falls_back_to_the_plain_command() {
        let s = Scratch::new("nosrc");
        let clang = fake_clang(&s, "clang A");
        let (args, c) = compile_args(&s, "a.o", &[]);
        assert_eq!(
            compile_cached(&clang, &s.path("lib"), &c, &args),
            0,
            "no source file: clang decides"
        );
        assert_eq!(calls(&s), 1);
        with_source(&s, SOURCE);
        let missing = s.path("sdk/bin/nope");
        assert_eq!(compile_cached(&missing, &s.path("lib"), &c, &args), 127);
    }

    #[test]
    fn warm_means_this_sdk_and_this_libsqlite3_sys() {
        let s = Scratch::new("warm");
        let cache = s.path("lib");
        let dir = cache.join("sqlite-3.51.1-0123456789abcdef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(OBJECT), b"o").unwrap();
        std::fs::write(
            dir.join(KEY_FILE),
            format!("{FORMAT}\nkey=k\nsdk=abc\nsqlite=3.51.1\ncrate=libsqlite3-sys-0.39.1\n"),
        )
        .unwrap();
        assert!(is_warm(&cache, "abc", "libsqlite3-sys-0.39.1"));
        assert!(!is_warm(&cache, "abc", "libsqlite3-sys-0.40.0"));
        assert!(!is_warm(&cache, "xyz", "libsqlite3-sys-0.39.1"));
        assert!(!is_warm(&s.path("empty"), "abc", "libsqlite3-sys-0.39.1"));
        std::fs::write(dir.join(KEY_FILE), "FORMAT 0\n").unwrap();
        assert!(
            entries(&cache).is_empty(),
            "an entry from another format version is ignored"
        );
    }

    #[test]
    fn sdk_id_changes_with_the_compiler_banner() {
        let s = Scratch::new("sdkid");
        let a = sdk_id(&fake_clang(&s, "clang A")).unwrap();
        assert_eq!(a, sdk_id(&s.path("sdk/bin/clang")).unwrap());
        let b = sdk_id(&fake_clang(&s, "clang B")).unwrap();
        assert_ne!(a, b);
        std::fs::write(s.path("sdk/VERSION"), "25.0\nwasi-libc: x\n").unwrap();
        assert_ne!(b, sdk_id(&s.path("sdk/bin/clang")).unwrap());
        assert!(sdk_id(&s.path("sdk/bin/nope")).is_none());
        let moved = fake_clang(&s, "clang B\nInstalledDir: /one");
        let first = sdk_id(&moved).unwrap();
        let moved = fake_clang(&s, "clang B\nInstalledDir: /two");
        assert_eq!(
            first,
            sdk_id(&moved).unwrap(),
            "the install directory is not part of the identity"
        );
    }

    #[test]
    fn the_wrapper_link_is_replaced_when_the_binary_moves() {
        let s = Scratch::new("link");
        let link = s.path("bin/spared-cc");
        std::fs::create_dir_all(s.path("bin")).unwrap();
        let (a, b) = (s.path("a"), s.path("b"));
        assert!(link_to(&link, &a));
        assert!(link_to(&link, &a));
        assert!(link_to(&link, &b));
        assert_eq!(std::fs::read_link(&link).unwrap(), b);
    }
}
