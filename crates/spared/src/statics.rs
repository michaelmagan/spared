//! Static files an app deploys with its component (`[static]` in spared.toml), served by the host
//! for GET and HEAD without waking the app.
//!
//! Upload: the CLI packs the project's static directory into a zstd-compressed tar of regular
//! files ([`pack`]) and sends it after the component in the deploy body. The server reads it with
//! the strict tar reader of the backups and writes each file into a new directory
//! `<app-dir>/static-<id>/` under the SHA-256 of its bytes, beside `index.json`, which maps each
//! name to its hash and size ([`unpack`]). Nothing in that directory is named after anything the
//! upload chose, so a name in the archive never reaches the filesystem. The guest sees only its
//! `data/` directory, so it can neither read nor write these files.
//!
//! Serving: the index is held in memory ([`Site`]). A request path is percent-decoded once and
//! must then be a name the index holds exactly ([`lookup_key`]); the only file ever opened is a
//! hash from the index, relative to a handle on the release directory, without following links.

use crate::dirfd::Dir;
use crate::remote::archive;
use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// The largest one static file may be. A response reads or maps the whole file.
pub const MAX_FILE_BYTES: u64 = 16 << 20;
/// The longest name (path inside the static directory) in bytes, and the most parts it may have.
pub const MAX_NAME_BYTES: usize = 1024;
pub const MAX_DEPTH: usize = 32;
/// The longest request path (before decoding) that is looked up at all.
const MAX_RAW_PATH: usize = 3 * MAX_NAME_BYTES + 1;
const INDEX: &str = "index.json";
const INDEX_FORMAT: &str = "spared-static-v1";
const MAX_INDEX_BYTES: u64 = 8 << 20;
/// Files up to this size are read for each response; larger ones are mapped, so many
/// concurrent responses of one large file share the page cache instead of a copy each.
const MAP_ABOVE: u64 = 64 << 10;
/// The prefix of a release directory inside the app directory.
pub const DIR_PREFIX: &str = "static-";
/// The app directory's pointer to its current release directory (empty: none). Part of a
/// release, staged and committed with the manifest and the component.
pub const POINTER: &str = "static.current";
const CACHE_REVALIDATE: &str = "no-cache";
const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Operator caps on one app's static files (`--max-static-mb`, `--max-static-files`).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_files: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: 64 << 20,
            max_files: 5000,
        }
    }
}

impl Limits {
    /// The most bytes a stream within these limits decompresses to: the files plus the tar
    /// framing of each, plus the end marker. More is a decompression bomb.
    pub fn max_stream(&self) -> u64 {
        self.max_bytes
            .saturating_add(
                self.max_files
                    .saturating_add(1)
                    .saturating_mul(archive::PER_FILE_OVERHEAD),
            )
            .saturating_add(16 << 10)
    }

    /// The most compressed bytes an upload may send: incompressible data grows a little under
    /// zstd.
    pub fn max_upload(&self) -> u64 {
        let s = self.max_stream();
        s.saturating_add(s / 64).saturating_add(64 << 10)
    }
}

/// Why static files cannot be packed, uploaded or stored.
#[derive(Debug)]
pub struct StaticError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
    pub fix: String,
}

fn refuse(kind: &'static str, message: String, fix: &str) -> StaticError {
    let status = match kind {
        "static_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "write_failed" | "io" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    StaticError {
        status,
        kind,
        message,
        fix: fix.into(),
    }
}

/// A name for printing: at most 200 characters, control characters escaped.
fn shown(name: &str) -> String {
    format!("{:?}", name.chars().take(200).collect::<String>())
}

const BAD_PATH_FIX: &str = "static file names are relative UTF-8 paths of at most 1024 bytes and 32 parts, with no empty, '.' or '..' part, no part starting with '.', and no control character or backslash; rename the file";

/// A name a static file may have: a relative UTF-8 path of at most [`MAX_NAME_BYTES`] bytes and
/// [`MAX_DEPTH`] parts, no empty part (so no leading, trailing or double `/`), no part that starts
/// with `.` (so no `.`, `..` or hidden file), and no control character (NUL included) or
/// backslash anywhere.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.split('/').count() <= MAX_DEPTH
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.chars().any(|c| c.is_control() || c == '\\')
        })
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode `%XX` escapes once. `None` for a malformed escape or a result that is not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = hex_val(*b.get(i + 1)?)?;
            let lo = hex_val(*b.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The static file name a request path asks for, if it can name one: the path is decoded once,
/// `/` means `index.html` and a path ending in `/` means `index.html` inside it. A path that
/// decodes to anything [`valid_name`] refuses (`..`, `.`, a hidden part, NUL, a backslash, a
/// double slash, too long) names no static file, and the request goes to the app as it came.
pub fn lookup_key(raw_path: &str) -> Option<String> {
    if raw_path.len() > MAX_RAW_PATH {
        return None;
    }
    let decoded = percent_decode(raw_path.strip_prefix('/')?)?;
    let key = if decoded.is_empty() || decoded.ends_with('/') {
        decoded + "index.html"
    } else {
        decoded
    };
    valid_name(&key).then_some(key)
}

/// The Content-Type for a name, from its extension (any case). Unknown: application/octet-stream.
pub fn content_type(name: &str) -> &'static str {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    let ext = match leaf.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => return "application/octet-stream",
    };
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "xml" => "application/xml",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// Whether a name carries a content hash, so its bytes never change under that name: the file
/// name without its extension has a part (split at `.`, `-` and `_`) of 8 or more characters of
/// `0-9a-f` with at least one digit and one letter, as in `app.3f9a8b2c.js` or `chunk-1d2e3f4a.css`.
/// Such files are cached for a year; every other file is revalidated with its ETag each time.
pub fn immutable(name: &str) -> bool {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    let stem = leaf.rsplit_once('.').map_or(leaf, |(s, _)| s);
    stem.split(['.', '-', '_']).any(|p| {
        p.len() >= 8
            && p.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && p.bytes().any(|b| b.is_ascii_digit())
            && p.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `name` can be a release directory: `static-` and then digits and dashes.
pub fn valid_release_name(name: &str) -> bool {
    name.strip_prefix(DIR_PREFIX).is_some_and(|rest| {
        !rest.is_empty() && rest.len() <= 64 && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexFile {
    format: String,
    files: Vec<IndexEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    path: String,
    sha256: String,
    size: u64,
}

// ---------------------------------------------------------------- packing (the CLI)

/// What [`pack`] produced.
pub struct Packed {
    /// The upload: a zstd-compressed tar of the files.
    pub archive: Vec<u8>,
    /// Each file's name and size, in the order packed.
    pub files: Vec<(String, u64)>,
    pub bytes: u64,
    /// Hidden files and directories (names starting with `.`) that were left out.
    pub skipped: Vec<String>,
}

/// Pack the static directory `root` of an app project. Hidden entries (a name starting with `.`,
/// such as `.git/` or `.DS_Store`) are left out and listed in `skipped`. A symlink, a special
/// file, a name that is not UTF-8 or that [`valid_name`] refuses, and a file over
/// [`MAX_FILE_BYTES`] are refused; so are a missing directory and one with no files.
pub fn pack(root: &Path, shown_dir: &str) -> Result<Packed, StaticError> {
    let missing = |why: &str| {
        refuse(
            "no_static_files",
            format!("[static] dir '{shown_dir}' {why}"),
            "create the directory and put the web client's files in it (index.html is served at /), or remove [static] from spared.toml",
        )
    };
    match std::fs::metadata(root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(missing("is not a directory")),
        Err(_) => return Err(missing("does not exist")),
    }
    let mut w = archive::FileTarWriter::new(Vec::new())
        .map_err(|e| refuse("io", format!("packing failed: {e}"), "report this"))?;
    let (mut files, mut skipped, mut bytes) = (vec![], vec![], 0u64);
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| {
            refuse(
                "io",
                format!("cannot read {}: {e}", dir.display()),
                "check the directory's permissions",
            )
        })?;
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by(|a, b| a.file_name().as_bytes().cmp(b.file_name().as_bytes()));
        let mut subdirs = vec![];
        for e in entries {
            let os_name = e.file_name();
            let Some(leaf) = os_name.to_str() else {
                return Err(refuse(
                    "static_bad_path",
                    format!(
                        "{shown_dir}/{prefix}{} is not a UTF-8 name",
                        os_name.to_string_lossy()
                    ),
                    BAD_PATH_FIX,
                ));
            };
            let name = format!("{prefix}{leaf}");
            let ft = e.file_type().map_err(|err| {
                refuse(
                    "io",
                    format!("cannot read {shown_dir}/{name}: {err}"),
                    "check the file's permissions",
                )
            })?;
            if leaf.starts_with('.') {
                skipped.push(if ft.is_dir() { format!("{name}/") } else { name });
                continue;
            }
            if ft.is_symlink() {
                return Err(refuse(
                    "static_bad_path",
                    format!("{shown_dir}/{name} is a symlink; static files must be regular files"),
                    "replace the link with a copy of the file, or move the file into the static directory",
                ));
            }
            if ft.is_dir() {
                subdirs.push((e.path(), format!("{name}/")));
                continue;
            }
            if !ft.is_file() {
                return Err(refuse(
                    "static_bad_path",
                    format!(
                        "{shown_dir}/{name} is not a regular file (a device, socket or fifo)"
                    ),
                    "remove it from the static directory",
                ));
            }
            if !valid_name(&name) {
                return Err(refuse(
                    "static_bad_path",
                    format!("{shown_dir}/{} is not a name spared serves", shown(&name)),
                    BAD_PATH_FIX,
                ));
            }
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(e.path())
                .map_err(|err| {
                    refuse(
                        "io",
                        format!("cannot read {shown_dir}/{name}: {err}"),
                        "check the file's permissions",
                    )
                })?;
            let mut data = vec![];
            (&mut f)
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut data)
                .map_err(|err| {
                    refuse(
                        "io",
                        format!("cannot read {shown_dir}/{name}: {err}"),
                        "check the file's permissions",
                    )
                })?;
            if data.len() as u64 > MAX_FILE_BYTES {
                return Err(refuse(
                    "static_too_large",
                    format!(
                        "{shown_dir}/{name} is over {} MiB, the most one static file may be",
                        MAX_FILE_BYTES >> 20
                    ),
                    "serve large files from the app or another host, or make the file smaller",
                ));
            }
            w.add(&name, &data)
                .map_err(|err| refuse("io", format!("packing failed: {err}"), "report this"))?;
            bytes += data.len() as u64;
            files.push((name, data.len() as u64));
        }
        // Depth first, in name order: the stack takes the last pushed first.
        stack.extend(subdirs.into_iter().rev());
    }
    if files.is_empty() {
        return Err(missing("has no files to serve"));
    }
    let archive = w
        .finish()
        .map_err(|e| refuse("io", format!("packing failed: {e}"), "report this"))?;
    Ok(Packed {
        archive,
        files,
        bytes,
        skipped,
    })
}

// ---------------------------------------------------------------- storing (the server)

/// Read the upload `compressed` and store its files in the new directory `dest` (which must not
/// exist): one file per distinct content, named by its SHA-256, then `index.json`, all synced.
/// Refuses, with nothing kept that the caller has to trust, an archive that is not a strict tar
/// of regular files, a name [`valid_name`] refuses or that appears twice, and more files or bytes
/// than `limits` (or one file over [`MAX_FILE_BYTES`]). The caller removes `dest` on an error.
pub fn unpack(compressed: &[u8], dest: &Path, limits: &Limits) -> Result<Site, StaticError> {
    let disk = |e: std::io::Error| {
        refuse(
            "write_failed",
            format!("storing the static files failed: {e}"),
            "check disk space and permissions of the data dir",
        )
    };
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dest)
        .map_err(disk)?;
    let dir = Dir::open(dest).map_err(disk)?;
    let too_large_fix = "send fewer or smaller files; the operator can raise --max-static-mb or --max-static-files";
    let mut r = archive::FileTarReader::new(
        compressed,
        limits.max_stream(),
        "the static files decompress to more than this server's caps allow",
    )
    .map_err(|e| bad_archive(&e))?;
    let (mut index, mut names, mut stored) = (vec![], HashSet::new(), HashSet::new());
    let mut total = 0u64;
    loop {
        let (name, size) = match r.next() {
            Ok(Some(x)) => x,
            Ok(None) => break,
            Err(e) if archive::is_cap_reached(&e) => {
                return Err(refuse("static_too_large", e.to_string(), too_large_fix));
            }
            Err(e) => return Err(bad_archive(&e)),
        };
        if index.len() as u64 >= limits.max_files {
            return Err(refuse(
                "static_too_large",
                format!(
                    "more than {} static files; this server's cap is --max-static-files {}",
                    limits.max_files, limits.max_files
                ),
                too_large_fix,
            ));
        }
        if !valid_name(&name) {
            return Err(refuse(
                "static_bad_path",
                format!("the static files hold the name {}, which spared does not serve", shown(&name)),
                BAD_PATH_FIX,
            ));
        }
        if size > MAX_FILE_BYTES {
            return Err(refuse(
                "static_too_large",
                format!(
                    "{} is {size} bytes, over the {} MiB one static file may be",
                    shown(&name),
                    MAX_FILE_BYTES >> 20
                ),
                too_large_fix,
            ));
        }
        total = total.saturating_add(size);
        if total > limits.max_bytes {
            return Err(refuse(
                "static_too_large",
                format!(
                    "the static files are over {} MB; this server's cap is --max-static-mb {}",
                    limits.max_bytes >> 20,
                    limits.max_bytes >> 20
                ),
                too_large_fix,
            ));
        }
        if !names.insert(name.clone()) {
            return Err(refuse(
                "static_bad_path",
                format!("the static files hold {} twice", shown(&name)),
                BAD_PATH_FIX,
            ));
        }
        let data = match r.read_file(size) {
            Ok(d) => d,
            Err(e) if archive::is_cap_reached(&e) => {
                return Err(refuse("static_too_large", e.to_string(), too_large_fix));
            }
            Err(e) => return Err(bad_archive(&e)),
        };
        let sha = archive::hex(&Sha256::digest(&data));
        if stored.insert(sha.clone()) {
            let mut f = dir.create_file(OsStr::new(&sha)).map_err(disk)?;
            f.write_all(&data).map_err(disk)?;
            f.sync_all().map_err(disk)?;
        }
        index.push(IndexEntry {
            path: name,
            sha256: sha,
            size,
        });
    }
    let text = serde_json::to_vec(&IndexFile {
        format: INDEX_FORMAT.into(),
        files: index,
    })
    .map_err(|e| disk(std::io::Error::other(e)))?;
    let mut f = dir.create_file(OsStr::new(INDEX)).map_err(disk)?;
    f.write_all(&text).map_err(disk)?;
    f.sync_all().map_err(disk)?;
    dir.sync().map_err(disk)?;
    Site::load(dest).map_err(disk)
}

fn bad_archive(e: &std::io::Error) -> StaticError {
    refuse(
        "static_bad_archive",
        format!("the static files are not an archive spared accepts: {e}"),
        "deploy with `spared deploy`, which packs [static] dir as a zstd-compressed tar of regular files",
    )
}

// ---------------------------------------------------------------- serving

/// One stored file.
#[derive(Clone, Debug)]
pub struct Entry {
    pub sha256: String,
    pub size: u64,
}

/// The static files of one release, as loaded from its `index.json`, and a handle on their
/// directory.
pub struct Site {
    dir: File,
    files: HashMap<String, Entry>,
    bytes: u64,
}

impl std::fmt::Debug for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Site({} files, {} bytes)", self.files.len(), self.bytes)
    }
}

fn open_nofollow(dirfd: i32, name: &str, dir: bool) -> std::io::Result<File> {
    let c = std::ffi::CString::new(name).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if dir {
        flags |= libc::O_DIRECTORY;
    } else {
        flags |= libc::O_NONBLOCK;
    }
    let fd = unsafe { libc::openat(dirfd, c.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

impl Site {
    /// Load the release directory `dir`: its index, checked again (names, hashes, no repeats),
    /// and a handle on the directory that every later open is relative to.
    pub fn load(dir: &Path) -> std::io::Result<Site> {
        let bad = |what: String| std::io::Error::new(std::io::ErrorKind::InvalidData, what);
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let handle = unsafe { File::from_raw_fd(fd) };
        let f = open_nofollow(handle.as_raw_fd(), INDEX, false)?;
        let mut text = vec![];
        f.take(MAX_INDEX_BYTES + 1).read_to_end(&mut text)?;
        if text.len() as u64 > MAX_INDEX_BYTES {
            return Err(bad(format!("{INDEX} is over {MAX_INDEX_BYTES} bytes")));
        }
        let index: IndexFile = serde_json::from_slice(&text)
            .map_err(|e| bad(format!("{INDEX} is not valid: {e}")))?;
        if index.format != INDEX_FORMAT {
            return Err(bad(format!("{INDEX} has the unknown format {:?}", index.format)));
        }
        let mut files = HashMap::with_capacity(index.files.len());
        let mut bytes = 0u64;
        for e in index.files {
            if !valid_name(&e.path) || !is_sha256_hex(&e.sha256) || e.size > MAX_FILE_BYTES {
                return Err(bad(format!("{INDEX} lists a bad entry {}", shown(&e.path))));
            }
            bytes = bytes.saturating_add(e.size);
            let entry = Entry {
                sha256: e.sha256,
                size: e.size,
            };
            if files.insert(e.path.clone(), entry).is_some() {
                return Err(bad(format!("{INDEX} lists {} twice", shown(&e.path))));
            }
        }
        Ok(Site {
            dir: handle,
            files,
            bytes,
        })
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.files.get(name)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The bytes of `e`, read or mapped. `NotFound` when a redeploy removed the release's
    /// directory since this site was looked up; `InvalidData` when the file is not the regular
    /// file of the recorded size.
    pub fn read(&self, e: &Entry) -> std::io::Result<Bytes> {
        let f = open_nofollow(self.dir.as_raw_fd(), &e.sha256, false)?;
        let m = f.metadata()?;
        if !m.is_file() || m.len() != e.size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("static file {} is not the file the index records", e.sha256),
            ));
        }
        if e.size == 0 {
            return Ok(Bytes::new());
        }
        if e.size <= MAP_ABOVE {
            let mut v = Vec::with_capacity(e.size as usize);
            (&f).take(e.size).read_to_end(&mut v)?;
            if v.len() as u64 != e.size {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            return Ok(Bytes::from(v));
        }
        Ok(Bytes::from_owner(Mapped::new(&f, e.size as usize)?))
    }
}

/// A read-only shared mapping of a whole stored file. The host writes a stored file once, before
/// its release is committed, and never changes it again (a redeploy writes a new directory and
/// unlinks the old one, which leaves an existing mapping intact), so the mapping cannot shrink
/// under a response. The guest cannot reach these files.
struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

impl Mapped {
    fn new(f: &File, len: usize) -> std::io::Result<Mapped> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Mapped { ptr, len })
    }
}

impl AsRef<[u8]> for Mapped {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// What a GET or HEAD for a static file gets, decided from the request headers alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// `If-None-Match` matched: 304, no body.
    NotModified,
    /// The whole file: 200.
    Whole,
    /// One byte range, `start..=end`: 206.
    Part(u64, u64),
    /// A range that starts past the end: 416.
    Unsatisfiable,
}

impl Plan {
    pub fn needs_body(self) -> bool {
        matches!(self, Plan::Whole | Plan::Part(..))
    }
}

pub fn etag(e: &Entry) -> String {
    format!("\"{}\"", e.sha256)
}

fn inm_matches(value: &str, tag: &str) -> bool {
    value.split(',').map(str::trim).any(|t| {
        t == "*" || t.strip_prefix("W/").unwrap_or(t) == tag
    })
}

/// One range of `bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `size` bytes. `None`
/// means the header is ignored (malformed, or several ranges, which are served whole).
fn parse_range(value: &str, size: u64) -> Option<Plan> {
    let spec = value.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let num = |s: &str| {
        let s = s.trim();
        (!s.is_empty() && s.len() <= 19 && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, true) => None,
        (true, false) => {
            let n = num(b)?;
            if n == 0 || size == 0 {
                return Some(Plan::Unsatisfiable);
            }
            Some(Plan::Part(size.saturating_sub(n), size - 1))
        }
        (false, empty_end) => {
            let start = num(a)?;
            let end = if empty_end { u64::MAX } else { num(b)? };
            if end < start {
                return None;
            }
            if start >= size {
                return Some(Plan::Unsatisfiable);
            }
            Some(Plan::Part(start, end.min(size - 1)))
        }
    }
}

/// Decide the answer for `e`: `If-None-Match` first, then a single `Range` on a GET (honored
/// only when `If-Range`, if sent, is this ETag), else the whole file.
pub fn plan(e: &Entry, head: bool, headers: &HeaderMap) -> Plan {
    let tag = etag(e);
    if let Some(v) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if inm_matches(v, &tag) {
            return Plan::NotModified;
        }
    }
    if head {
        return Plan::Whole;
    }
    let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Plan::Whole;
    };
    if let Some(ir) = headers.get(header::IF_RANGE) {
        if ir.to_str().ok().map(str::trim) != Some(tag.as_str()) {
            return Plan::Whole;
        }
    }
    parse_range(range, e.size).unwrap_or(Plan::Whole)
}

/// The response for a static file. `body` holds the file's bytes when `plan` needs them and the
/// request is not a HEAD.
pub fn response(name: &str, e: &Entry, plan: Plan, head: bool, body: Option<Bytes>) -> Response<Full<Bytes>> {
    let cache = if immutable(name) {
        CACHE_IMMUTABLE
    } else {
        CACHE_REVALIDATE
    };
    let mut r = Response::builder()
        .header(header::ETAG, etag(e))
        .header(header::CACHE_CONTROL, cache)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    let (status, len, bytes) = match plan {
        Plan::NotModified => (StatusCode::NOT_MODIFIED, None, Bytes::new()),
        Plan::Unsatisfiable => {
            r = r.header(header::CONTENT_RANGE, format!("bytes */{}", e.size));
            (StatusCode::RANGE_NOT_SATISFIABLE, None, Bytes::new())
        }
        Plan::Whole => {
            r = r
                .header(header::CONTENT_TYPE, content_type(name))
                .header(header::ACCEPT_RANGES, "bytes");
            (StatusCode::OK, Some(e.size), body.unwrap_or_default())
        }
        Plan::Part(start, end) => {
            r = r
                .header(header::CONTENT_TYPE, content_type(name))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", e.size),
                );
            let part = body
                .map(|b| b.slice(start as usize..=end as usize))
                .unwrap_or_default();
            (StatusCode::PARTIAL_CONTENT, Some(end - start + 1), part)
        }
    };
    if head {
        if let Some(n) = len {
            r = r.header(header::CONTENT_LENGTH, HeaderValue::from(n));
        }
        return r.status(status).body(Full::new(Bytes::new())).unwrap();
    }
    r.status(status).body(Full::new(bytes)).unwrap()
}

// ---------------------------------------------------------------- the app directory

/// The release directory the app directory `app_dir` points at, if any.
pub fn current(app_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(app_dir.join(POINTER)).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Load the app's current static files. `Ok(None)` when it has none.
pub fn load_current(app_dir: &Path) -> std::io::Result<Option<Site>> {
    let Some(name) = current(app_dir) else {
        return Ok(None);
    };
    if !valid_release_name(&name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{POINTER} names {}, which is not a static release directory", shown(&name)),
        ));
    }
    Site::load(&app_dir.join(&name)).map(Some)
}

/// Remove every release directory in `app_dir` except `keep`: left by a deploy that did not
/// commit, or by one whose old release a crash kept from being removed. Runs at start.
pub fn sweep(app_dir: &Path, keep: Option<&str>) {
    for e in std::fs::read_dir(app_dir).into_iter().flatten().flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(DIR_PREFIX) && Some(name) != keep {
            let _ = crate::backup::remove_tree(&e.path());
        }
    }
}

/// A new, unused release directory name.
pub fn new_release_name(unique: &str) -> String {
    format!("{DIR_PREFIX}{unique}")
}

#[cfg(test)]
mod tests;
