//! The archive of one remote backup: a POSIX ustar stream (pax headers for long names) of a local
//! backup directory, compressed with zstd level 3 and then, unless encryption is off, sealed
//! (`seal.rs`). The first tar entry is `.spared-manifest.json`, which lists every entry in order
//! with each file's size and SHA-256.
//!
//! Reading is strict, because the bytes come from a bucket: only regular files and directories,
//! names relative with no `..`, `.` or empty parts, at most [`crate::backup::MAX_DEPTH`] levels and
//! [`crate::backup::MAX_ENTRIES`] entries, sizes within `--max-app-data-mb`, entries exactly in the
//! order of the inner manifest, and every file's hash checked. Extraction is relative to directory
//! handles, never follows a link, and creates every name new. Nothing here opens `app.db` as a
//! database: files are bytes.

use super::seal::{BackupKey, Opener, Sealer};
use crate::dirfd::{Dir, Kind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, BufWriter, Error, ErrorKind, Read, Result, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub const INNER_MANIFEST: &str = ".spared-manifest.json";
pub const CONTENTS_FORMAT: &str = "spared-backup-contents-v1";
const BLOCK: usize = 512;
const MAX_INNER_MANIFEST: u64 = 32 << 20;
/// The largest value the 11 octal digits of a ustar size field hold, the most [`parse_octal`] reads.
const MAX_TAR_SIZE: u64 = 0o77_777_777_777;
const MAX_PAX: u64 = 16 << 10;
const MAX_PATH: usize = 4096;
const ZSTD_LEVEL: i32 = 3;
/// The largest zstd window a reader accepts (16 MiB). Level 3 writes 2 MiB at most, so a frame
/// asking for more did not come from spared and is refused before memory is set aside for it.
const ZSTD_WINDOW_LOG_MAX: u32 = 24;
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    File,
    Dir,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: ItemKind,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The inner manifest, the archive's first entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contents {
    pub format: String,
    pub app: String,
    pub remote_id: String,
    pub local_id: String,
    pub spared_version: String,
    pub journal_clean: bool,
    pub files: u64,
    pub bytes: u64,
    pub entries: Vec<Item>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: u64,
    pub max_depth: usize,
}

impl Limits {
    pub fn new(max_bytes: u64) -> Limits {
        Limits {
            max_bytes,
            max_entries: crate::backup::MAX_ENTRIES,
            max_depth: crate::backup::MAX_DEPTH,
        }
    }

    /// The most decompressed bytes an archive within these limits can hold: the files, plus a
    /// header, a pax header and padding per entry, plus the inner manifest.
    pub fn max_stream(&self) -> u64 {
        self.max_bytes
            .saturating_add(
                (self.max_entries + 2).saturating_mul(3 * BLOCK as u64 + MAX_PATH as u64),
            )
            .saturating_add(MAX_INNER_MANIFEST + 4 * BLOCK as u64)
    }
}

fn corrupt(what: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, what.into())
}

/// A relative path with no empty, `.` or `..` part, no NUL and at most `max_depth + 1` parts.
pub fn valid_path(p: &str, max_depth: usize) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PATH
        && !p.starts_with('/')
        && !p.contains('\0')
        && p.split('/').count() <= max_depth + 1
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// List the local backup at `root` in the order the archive holds it: depth first, names sorted
/// by their bytes, each file hashed. Walks with directory handles and never follows a link; a
/// link or special file (a local backup has none) is left out.
pub fn scan(root: &Path, limits: &Limits) -> Result<Vec<Item>> {
    fn sorted(d: &mut Dir) -> Result<Vec<crate::dirfd::Entry>> {
        let mut v = vec![];
        while let Some(e) = d.read_entry()? {
            v.push(e);
        }
        v.sort_by(|a, b| b.name.as_encoded_bytes().cmp(a.name.as_encoded_bytes()));
        Ok(v)
    }
    let mut top = Dir::open(root)?;
    let first = sorted(&mut top)?;
    let mut stack = vec![(top, String::new(), first)];
    let mut items = vec![];
    loop {
        let depth = stack.len();
        let Some((dir, prefix, pending)) = stack.last_mut() else {
            break;
        };
        let Some(e) = pending.pop() else {
            stack.pop();
            continue;
        };
        let name = e
            .name
            .to_str()
            .ok_or_else(|| Error::other(format!("{prefix}{:?} is not a UTF-8 name", e.name)))?;
        let path = format!("{prefix}{name}");
        if items.len() as u64 >= limits.max_entries {
            return Err(Error::other(format!(
                "more than {} files and directories",
                limits.max_entries
            )));
        }
        match e.kind {
            Kind::Dir => {
                if depth > limits.max_depth {
                    return Err(Error::other(format!(
                        "directories are nested more than {} levels deep",
                        limits.max_depth
                    )));
                }
                let mut child = dir.open_dir(&e.name)?;
                let pending = sorted(&mut child)?;
                items.push(Item {
                    path: path.clone(),
                    kind: ItemKind::Dir,
                    size: 0,
                    sha256: None,
                });
                stack.push((child, format!("{path}/"), pending));
            }
            Kind::File => {
                if e.len > MAX_TAR_SIZE {
                    return Err(oversize(format!(
                        "{path} is {} bytes, over the {MAX_TAR_SIZE} a tar entry holds",
                        e.len
                    )));
                }
                let mut f = dir.open_file(&e.name)?;
                let (size, digest) = hash_reader(&mut f)?;
                if size > MAX_TAR_SIZE {
                    return Err(oversize(format!(
                        "{path} is {size} bytes, over the {MAX_TAR_SIZE} a tar entry holds"
                    )));
                }
                items.push(Item {
                    path,
                    kind: ItemKind::File,
                    size,
                    sha256: Some(hex(&digest)),
                });
            }
            Kind::Other => {}
        }
    }
    Ok(items)
}

fn hash_reader(r: &mut impl Read) -> Result<(u64, [u8; 32])> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut n = 0u64;
    loop {
        let k = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((n, h.finalize().into()))
}

/// Open the directory that holds `path` (relative to `root`), one part at a time, never
/// following a link. `None` means `root` itself.
fn open_parent(root: &Dir, path: &str) -> Result<(Option<Dir>, String)> {
    let mut parts: Vec<&str> = path.split('/').collect();
    let leaf = parts.pop().unwrap_or_default().to_string();
    let mut cur: Option<Dir> = None;
    for p in parts {
        let next = match &cur {
            None => root.open_dir(OsStr::new(p))?,
            Some(d) => d.open_dir(OsStr::new(p))?,
        };
        cur = Some(next);
    }
    Ok((cur, leaf))
}

// ---------------------------------------------------------------- writing

/// Counts and hashes what is written; refuses to pass `max` bytes with [`TooBig`].
struct HashCount<W: Write> {
    inner: W,
    hash: Sha256,
    n: u64,
    max: u64,
}

/// The stored object would be larger than allowed.
#[derive(Debug)]
pub struct TooBig(pub u64);

impl std::fmt::Display for TooBig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the archive is larger than {} bytes", self.0)
    }
}

impl std::error::Error for TooBig {}

/// Something the archive format cannot hold: a file over the tar size field, a manifest over
/// the reader's cap.
#[derive(Debug)]
struct Oversize(String);

impl std::fmt::Display for Oversize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Oversize {}

fn oversize(what: impl Into<String>) -> Error {
    Error::other(Oversize(what.into()))
}

pub fn is_too_big(e: &Error) -> bool {
    e.get_ref()
        .is_some_and(|i| i.is::<TooBig>() || i.is::<Oversize>())
}

impl<W: Write> Write for HashCount<W> {
    fn write(&mut self, b: &[u8]) -> Result<usize> {
        if self.n + b.len() as u64 > self.max {
            return Err(Error::other(TooBig(self.max)));
        }
        let k = self.inner.write(b)?;
        self.hash.update(&b[..k]);
        self.n += k as u64;
        Ok(k)
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

enum Sink<W: Write> {
    Plain(W),
    Sealed(Sealer<W>),
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, b: &[u8]) -> Result<usize> {
        match self {
            Sink::Plain(w) => w.write(b),
            Sink::Sealed(s) => s.write(b),
        }
    }

    fn flush(&mut self) -> Result<()> {
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Sealed(s) => s.flush(),
        }
    }
}

impl<W: Write> Sink<W> {
    fn finish(self) -> Result<W> {
        match self {
            Sink::Plain(w) => Ok(w),
            Sink::Sealed(s) => s.finish(),
        }
    }
}

fn octal(field: &mut [u8], v: u64) -> Result<()> {
    let w = field.len() - 1;
    let s = format!("{v:o}");
    if s.len() > w {
        return Err(oversize(format!(
            "{v} does not fit a {w}-digit tar octal field"
        )));
    }
    let s = format!("{v:0w$o}");
    field[..w].copy_from_slice(s.as_bytes());
    field[w] = 0;
    Ok(())
}

fn tar_header(name: &[u8], kind: u8, size: u64, mode: u64) -> Result<[u8; BLOCK]> {
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name);
    octal(&mut h[100..108], mode)?;
    octal(&mut h[108..116], 0)?;
    octal(&mut h[116..124], 0)?;
    octal(&mut h[124..136], size)?;
    octal(&mut h[136..148], 0)?;
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    Ok(h)
}

fn pad(w: &mut impl Write, n: u64) -> Result<()> {
    let r = (n % BLOCK as u64) as usize;
    if r != 0 {
        w.write_all(&[0u8; BLOCK][..BLOCK - r])?;
    }
    Ok(())
}

/// One pax record: `<len> <key>=<value>\n`, where `<len>` counts the whole record, itself included.
fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while len.to_string().len() + body.len() != len {
        len += 1;
    }
    format!("{len}{body}")
}

fn write_entry(w: &mut impl Write, path: &str, kind: ItemKind, size: u64) -> Result<()> {
    let name = match kind {
        ItemKind::Dir => format!("{path}/"),
        ItemKind::File => path.to_string(),
    };
    let short = if name.len() <= 100 {
        name.as_bytes()
    } else {
        let rec = pax_record("path", &name);
        w.write_all(&tar_header(
            b"././@PaxHeader",
            b'x',
            rec.len() as u64,
            0o600,
        )?)?;
        w.write_all(rec.as_bytes())?;
        pad(w, rec.len() as u64)?;
        let mut cut = 100;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        &name.as_bytes()[..cut]
    };
    let (flag, mode) = match kind {
        ItemKind::Dir => (b'5', 0o700),
        ItemKind::File => (b'0', 0o600),
    };
    w.write_all(&tar_header(short, flag, size, mode)?)
}

/// What [`build`] wrote.
pub struct Built {
    /// The stored object: its length and SHA-256.
    pub bytes: u64,
    pub sha256: [u8; 32],
    pub contents: Contents,
    /// SHA-256 of the inner manifest's bytes.
    pub digest: [u8; 32],
}

/// Identity of an archive: written into the inner manifest and checked when it is read.
pub struct Names<'a> {
    pub app: &'a str,
    pub remote_id: &'a str,
    pub local_id: &'a str,
}

/// Write the archive of the local backup at `src` to `out`. With `seal`, the compressed stream is
/// sealed with that key and salt for `names`. Stops with a [`TooBig`] error once the object
/// passes `max_object` bytes.
pub fn build(
    src: &Path,
    out: File,
    names: &Names,
    seal: Option<(&BackupKey, &[u8; super::seal::SALT_LEN])>,
    limits: &Limits,
    max_object: u64,
) -> Result<Built> {
    let entries = scan(src, limits)?;
    let files = entries.iter().filter(|i| i.kind == ItemKind::File).count() as u64;
    let bytes = entries.iter().map(|i| i.size).sum();
    let contents = Contents {
        format: CONTENTS_FORMAT.into(),
        app: names.app.into(),
        remote_id: names.remote_id.into(),
        local_id: names.local_id.into(),
        spared_version: crate::VERSION.into(),
        journal_clean: crate::backup::journal_clean(src),
        files,
        bytes,
        entries,
    };
    let manifest = serde_json::to_vec(&contents).map_err(Error::other)?;
    if manifest.len() as u64 > MAX_INNER_MANIFEST {
        return Err(oversize(format!(
            "the inner manifest is {} bytes, over the {MAX_INNER_MANIFEST} the reader accepts",
            manifest.len()
        )));
    }
    check_contents(&contents, names, limits)?;
    let digest: [u8; 32] = Sha256::digest(&manifest).into();
    let counted = HashCount {
        inner: BufWriter::with_capacity(1 << 20, out),
        hash: Sha256::new(),
        n: 0,
        max: max_object,
    };
    let sink = match seal {
        None => Sink::Plain(counted),
        Some((key, salt)) => Sink::Sealed(Sealer::new(
            key,
            salt,
            super::seal::aad(names.app, names.remote_id),
            counted,
        )?),
    };
    let mut z = zstd::stream::write::Encoder::new(sink, ZSTD_LEVEL)?;
    z.include_checksum(true)?;
    write_entry(
        &mut z,
        INNER_MANIFEST,
        ItemKind::File,
        manifest.len() as u64,
    )?;
    z.write_all(&manifest)?;
    pad(&mut z, manifest.len() as u64)?;
    let root = Dir::open(src)?;
    let mut buf = vec![0u8; CHUNK];
    for item in &contents.entries {
        write_entry(&mut z, &item.path, item.kind, item.size)?;
        if item.kind == ItemKind::Dir {
            continue;
        }
        let (parent, leaf) = open_parent(&root, &item.path)?;
        let f = parent
            .as_ref()
            .unwrap_or(&root)
            .open_file(OsStr::new(&leaf))?;
        let mut h = Sha256::new();
        let mut at = 0u64;
        while at < item.size {
            let n = ((item.size - at) as usize).min(CHUNK);
            f.read_exact_at(&mut buf[..n], at)?;
            h.update(&buf[..n]);
            z.write_all(&buf[..n])?;
            at += n as u64;
        }
        if Some(hex(&h.finalize())) != item.sha256 {
            return Err(corrupt(format!(
                "{} changed while it was archived (sha256 differs from the first read)",
                item.path
            )));
        }
        pad(&mut z, item.size)?;
    }
    z.write_all(&[0u8; 2 * BLOCK])?;
    let counted = z.finish()?.finish()?;
    let mut file = counted.inner.into_inner().map_err(|e| e.into_error())?;
    file.flush()?;
    file.sync_all()?;
    Ok(Built {
        bytes: counted.n,
        sha256: counted.hash.finalize().into(),
        contents,
        digest,
    })
}

// ---------------------------------------------------------------- reading

/// Turns every read error into `InvalidData`: whatever fails while the archive is decrypted,
/// decompressed or parsed is a corrupt archive. Also refuses to read past `left` bytes, so a
/// decompression bomb stops at the cap, with an error that [`is_cap_reached`] recognizes.
struct Capped<R: Read> {
    inner: R,
    left: u64,
    over: &'static str,
}

/// The decompressed stream passed the cap a [`Capped`] reader was given.
#[derive(Debug)]
struct CapReached(&'static str);

impl std::fmt::Display for CapReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for CapReached {}

/// Whether a read failed because the stream decompressed past its cap.
pub(crate) fn is_cap_reached(e: &Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<CapReached>())
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = self.inner.read(buf).map_err(|e| {
            if e.kind() == ErrorKind::InvalidData {
                e
            } else {
                corrupt(format!("the archive does not decompress: {e}"))
            }
        })?;
        if n as u64 > self.left {
            return Err(Error::new(ErrorKind::InvalidData, CapReached(self.over)));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

struct Header {
    path: String,
    kind: ItemKind,
    size: u64,
}

struct TarReader<R: Read> {
    r: R,
}

fn read_exact_or(r: &mut impl Read, buf: &mut [u8], what: &str) -> Result<()> {
    r.read_exact(buf).map_err(|e| {
        if e.kind() == ErrorKind::UnexpectedEof {
            corrupt(format!("the archive ends inside {what}"))
        } else {
            e
        }
    })
}

fn parse_octal(field: &[u8]) -> Option<u64> {
    let end = field
        .iter()
        .position(|&b| b == 0 || b == b' ')
        .unwrap_or(field.len());
    let (digits, rest) = field.split_at(end);
    if digits.is_empty()
        || digits.len() > 11
        || !digits.iter().all(|b| (b'0'..=b'7').contains(b))
        || !rest.iter().all(|&b| b == 0 || b == b' ')
    {
        return None;
    }
    u64::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok()
}

fn field_str(field: &[u8]) -> Option<&str> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    if field[end..].iter().any(|&b| b != 0) {
        return None;
    }
    std::str::from_utf8(&field[..end]).ok()
}

impl<R: Read> TarReader<R> {
    fn skip_pad(&mut self, n: u64) -> Result<()> {
        let r = (n % BLOCK as u64) as usize;
        if r != 0 {
            let mut p = [0u8; BLOCK];
            read_exact_or(&mut self.r, &mut p[..BLOCK - r], "padding")?;
            if p[..BLOCK - r].iter().any(|&b| b != 0) {
                return Err(corrupt("tar padding is not zero"));
            }
        }
        Ok(())
    }

    /// The next entry, or `None` at the end marker (after which only zeros may follow).
    fn next(&mut self) -> Result<Option<Header>> {
        let mut pax_path: Option<String> = None;
        loop {
            let mut h = [0u8; BLOCK];
            read_exact_or(&mut self.r, &mut h, "a tar header")?;
            if h.iter().all(|&b| b == 0) {
                if pax_path.is_some() {
                    return Err(corrupt("a pax header is followed by the end marker"));
                }
                read_exact_or(&mut self.r, &mut h, "the end marker")?;
                let mut rest = vec![0u8; CHUNK];
                let mut n = BLOCK;
                while n > 0 {
                    if rest[..n].iter().any(|&b| b != 0) || h.iter().any(|&b| b != 0) {
                        return Err(corrupt("data follows the tar end marker"));
                    }
                    n = self.r.read(&mut rest)?;
                }
                return Ok(None);
            }
            let want = parse_octal(&h[148..156]).ok_or_else(|| corrupt("a bad tar checksum"))?;
            let mut sum: u64 = h.iter().map(|&b| b as u64).sum();
            sum = sum - h[148..156].iter().map(|&b| b as u64).sum::<u64>() + 8 * b' ' as u64;
            if sum != want || &h[257..263] != b"ustar\0" || &h[263..265] != b"00" {
                return Err(corrupt(
                    "a tar header that is not ustar, or its checksum is wrong",
                ));
            }
            let size =
                parse_octal(&h[124..136]).ok_or_else(|| corrupt("a tar entry with a bad size"))?;
            if h[345..500].iter().any(|&b| b != 0) {
                return Err(corrupt("a tar entry uses the ustar prefix field"));
            }
            let kind = match h[156] {
                b'x' => {
                    if pax_path.is_some() {
                        return Err(corrupt("two pax headers in a row"));
                    }
                    if size > MAX_PAX {
                        return Err(corrupt("a pax header over 16 KiB"));
                    }
                    let mut rec = vec![0u8; size as usize];
                    read_exact_or(&mut self.r, &mut rec, "a pax header")?;
                    self.skip_pad(size)?;
                    pax_path = Some(parse_pax(&rec)?);
                    continue;
                }
                b'0' | 0 => ItemKind::File,
                b'5' => ItemKind::Dir,
                b'1' => return Err(corrupt("the archive holds a hard link")),
                b'2' => return Err(corrupt("the archive holds a symlink")),
                b'3' | b'4' | b'6' => {
                    return Err(corrupt("the archive holds a device or a fifo"));
                }
                other => {
                    return Err(corrupt(format!(
                        "the archive holds a tar entry of type {:?}",
                        other as char
                    )));
                }
            };
            let name =
                field_str(&h[..100]).ok_or_else(|| corrupt("a tar name that is not UTF-8"))?;
            let mut path = pax_path.take().unwrap_or_else(|| name.to_string());
            if kind == ItemKind::Dir {
                if size != 0 {
                    return Err(corrupt("a directory entry with data"));
                }
                path = path
                    .strip_suffix('/')
                    .map(str::to_string)
                    .ok_or_else(|| corrupt("a directory name without a trailing /"))?;
            }
            return Ok(Some(Header { path, kind, size }));
        }
    }
}

fn parse_pax(rec: &[u8]) -> Result<String> {
    let bad = || corrupt("a malformed pax header");
    let mut rest = rec;
    let mut path = None;
    while !rest.is_empty() {
        let sp = rest.iter().position(|&b| b == b' ').ok_or_else(bad)?;
        let len: usize = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(bad)?;
        if len <= sp + 1 || len > rest.len() || rest[len - 1] != b'\n' {
            return Err(bad());
        }
        let kv = std::str::from_utf8(&rest[sp + 1..len - 1]).map_err(|_| bad())?;
        let (k, v) = kv.split_once('=').ok_or_else(bad)?;
        if k != "path" || path.is_some() {
            return Err(corrupt(format!("an unexpected pax key '{k}'")));
        }
        path = Some(v.to_string());
        rest = &rest[len..];
    }
    path.ok_or_else(bad)
}

/// What [`unpack`] found.
#[derive(Debug)]
pub struct Unpacked {
    pub contents: Contents,
    /// SHA-256 of the inner manifest's bytes.
    pub digest: [u8; 32],
}

fn check_contents(c: &Contents, names: &Names, limits: &Limits) -> Result<()> {
    if c.format != CONTENTS_FORMAT {
        return Err(corrupt(format!("unknown contents format '{}'", c.format)));
    }
    if c.app != names.app || c.remote_id != names.remote_id {
        return Err(corrupt(format!(
            "the archive holds backup {} of app '{}', not {} of '{}'",
            c.remote_id, c.app, names.remote_id, names.app
        )));
    }
    if c.entries.len() as u64 > limits.max_entries {
        return Err(corrupt(format!(
            "the archive lists more than {} entries",
            limits.max_entries
        )));
    }
    let mut total = 0u64;
    let mut files = 0u64;
    for i in &c.entries {
        if !valid_path(&i.path, limits.max_depth) {
            return Err(corrupt(format!(
                "the archive lists an unsafe or too deep name {:?}",
                i.path.chars().take(200).collect::<String>()
            )));
        }
        match i.kind {
            ItemKind::Dir if i.size != 0 || i.sha256.is_some() => {
                return Err(corrupt("a directory entry with a size or a hash"));
            }
            ItemKind::File
                if !i
                    .sha256
                    .as_deref()
                    .is_some_and(|h| h.len() == 64 && super::seal::unhex(h).is_some()) =>
            {
                return Err(corrupt(format!("{} has no valid sha256", i.path)));
            }
            ItemKind::File => {
                files += 1;
                total = total.saturating_add(i.size);
            }
            ItemKind::Dir => {}
        }
    }
    if total > limits.max_bytes {
        return Err(corrupt(format!(
            "the archive holds {total} bytes of files, over the data cap of {} MB (--max-app-data-mb)",
            limits.max_bytes >> 20
        )));
    }
    if total != c.bytes || files != c.files {
        return Err(corrupt(
            "the inner manifest's totals do not match its entries",
        ));
    }
    Ok(())
}

/// A filesystem error from creating an entry: a name that already exists, or whose parent is
/// missing or not a directory, means the archive is malformed; anything else is the disk's.
fn create_err(path: &str, e: Error) -> Error {
    match e.raw_os_error() {
        Some(libc::EEXIST | libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => corrupt(format!(
            "the archive lists {path:?} twice, or before its directory"
        )),
        _ => e,
    }
}

/// Write `size` bytes from `r` to the new file `f`, leaving holes where a whole chunk is zeros,
/// and return their SHA-256.
fn write_sparse(r: &mut impl Read, f: &File, size: u64, buf: &mut [u8]) -> Result<[u8; 32]> {
    let mut h = Sha256::new();
    let mut at = 0u64;
    while at < size {
        let n = ((size - at) as usize).min(buf.len());
        read_exact_or(r, &mut buf[..n], "a file")?;
        h.update(&buf[..n]);
        if buf[..n].iter().any(|&b| b != 0) {
            f.write_all_at(&buf[..n], at)?;
        }
        at += n as u64;
    }
    f.set_len(size)?;
    f.sync_all()?;
    Ok(h.finalize().into())
}

/// Read a tar stream into `dest`, an empty directory, strictly (see the module comment).
fn untar(r: impl Read, dest: &Path, names: &Names, limits: &Limits) -> Result<Unpacked> {
    let mut t = TarReader {
        r: Capped {
            inner: r,
            left: limits.max_stream(),
            over: "the archive decompresses to more than the data cap allows (--max-app-data-mb)",
        },
    };
    let first = t.next()?.ok_or_else(|| corrupt("the archive is empty"))?;
    if first.path != INNER_MANIFEST || first.kind != ItemKind::File {
        return Err(corrupt(format!(
            "the archive starts with {:?}, not {INNER_MANIFEST}",
            first.path.chars().take(200).collect::<String>()
        )));
    }
    if first.size > MAX_INNER_MANIFEST {
        return Err(corrupt("the inner manifest is over 32 MiB"));
    }
    let mut manifest = vec![0u8; first.size as usize];
    read_exact_or(&mut t.r, &mut manifest, "the inner manifest")?;
    t.skip_pad(first.size)?;
    let digest: [u8; 32] = Sha256::digest(&manifest).into();
    let contents: Contents = serde_json::from_slice(&manifest)
        .map_err(|e| corrupt(format!("the inner manifest is not valid: {e}")))?;
    check_contents(&contents, names, limits)?;
    let root = Dir::open(dest)?;
    let mut buf = vec![0u8; CHUNK];
    let mut dirs = vec![];
    for (i, item) in contents.entries.iter().enumerate() {
        let h = t.next()?.ok_or_else(|| {
            corrupt(format!(
                "the archive ends after {i} of {} entries",
                contents.entries.len()
            ))
        })?;
        if h.path != item.path || h.kind != item.kind || h.size != item.size {
            return Err(corrupt(format!(
                "entry {} is {:?}, but the inner manifest lists {:?} there",
                i + 1,
                h.path.chars().take(200).collect::<String>(),
                item.path.chars().take(200).collect::<String>()
            )));
        }
        let (parent, leaf) =
            open_parent(&root, &item.path).map_err(|e| create_err(&item.path, e))?;
        let parent = parent.as_ref().unwrap_or(&root);
        match item.kind {
            ItemKind::Dir => {
                parent
                    .create_dir(OsStr::new(&leaf))
                    .map_err(|e| create_err(&item.path, e))?;
                dirs.push(item.path.clone());
            }
            ItemKind::File => {
                let f = parent
                    .create_file(OsStr::new(&leaf))
                    .map_err(|e| create_err(&item.path, e))?;
                let got = write_sparse(&mut t.r, &f, item.size, &mut buf)?;
                t.skip_pad(item.size)?;
                if Some(hex(&got)) != item.sha256 {
                    return Err(corrupt(format!(
                        "{} does not match its sha256 in the inner manifest",
                        item.path
                    )));
                }
            }
        }
    }
    if t.next()?.is_some() {
        return Err(corrupt(
            "the archive holds more entries than its inner manifest lists",
        ));
    }
    for d in dirs.iter().rev() {
        let (parent, leaf) = open_parent(&root, d)?;
        parent
            .as_ref()
            .unwrap_or(&root)
            .open_dir(OsStr::new(&leaf))?
            .sync()?;
    }
    root.sync()?;
    Ok(Unpacked { contents, digest })
}

/// Unpack the archive file `archive` into the empty directory `dest`. With `key`, the archive is
/// opened (segment by segment, each authenticated before its bytes are used) with that key for
/// `names`. Any problem with the archive itself is an `InvalidData` error; `dest` is then left
/// for the caller to remove.
pub fn unpack(
    archive: File,
    key: Option<&BackupKey>,
    dest: &Path,
    names: &Names,
    limits: &Limits,
) -> Result<Unpacked> {
    let r = BufReader::with_capacity(1 << 20, archive);
    match key {
        Some(k) => {
            let (opened, _) = Opener::new(k, super::seal::aad(names.app, names.remote_id), r)?;
            untar(decoder(opened)?, dest, names, limits)
        }
        None => untar(decoder(r)?, dest, names, limits),
    }
}

fn decoder<R: Read>(r: R) -> Result<impl Read> {
    let mut d = zstd::stream::read::Decoder::new(r)?;
    d.window_log_max(ZSTD_WINDOW_LOG_MAX)?;
    Ok(d)
}

// ---------------------------------------------------------------- streams of plain files

/// The most bytes the tar framing adds to one file: its header, a pax header for a long name with
/// its record (names are at most 1 KiB where this is used) and the padding of each.
pub(crate) const PER_FILE_OVERHEAD: u64 = 4 * BLOCK as u64 + 2048;

/// A zstd-compressed tar stream of regular files only, in the order they are added, with no
/// inner manifest: the upload format of an app's static files (`crate::statics`). The reader
/// hashes what it takes in, so nothing here needs to be trusted.
pub(crate) struct FileTarWriter<W: Write> {
    z: zstd::stream::write::Encoder<'static, W>,
}

impl<W: Write> FileTarWriter<W> {
    pub(crate) fn new(w: W) -> Result<Self> {
        let mut z = zstd::stream::write::Encoder::new(w, ZSTD_LEVEL)?;
        z.include_checksum(true)?;
        Ok(FileTarWriter { z })
    }

    pub(crate) fn add(&mut self, path: &str, data: &[u8]) -> Result<()> {
        write_entry(&mut self.z, path, ItemKind::File, data.len() as u64)?;
        self.z.write_all(data)?;
        pad(&mut self.z, data.len() as u64)
    }

    pub(crate) fn finish(mut self) -> Result<W> {
        self.z.write_all(&[0u8; 2 * BLOCK])?;
        self.z.finish()
    }
}

/// Reads a [`FileTarWriter`] stream with the same strict tar reader the backups use: ustar only,
/// pax only for a long name, and only regular files (a directory, a link, a device or a fifo is
/// refused). Every error is `InvalidData`; a stream that decompresses past `max_stream` bytes
/// fails with an error [`is_cap_reached`] recognizes, so a decompression bomb stops there.
pub(crate) struct FileTarReader<R: Read> {
    t: TarReader<Capped<R>>,
}

impl<'a> FileTarReader<Box<dyn Read + 'a>> {
    pub(crate) fn new(compressed: &'a [u8], max_stream: u64, over: &'static str) -> Result<Self> {
        let d = decoder(compressed).map_err(|e| corrupt(format!("not a zstd stream: {e}")))?;
        Ok(FileTarReader {
            t: TarReader {
                r: Capped {
                    inner: Box::new(d),
                    left: max_stream,
                    over,
                },
            },
        })
    }
}

impl<R: Read> FileTarReader<R> {
    /// The next file's name and size, or `None` at the end of the stream.
    pub(crate) fn next(&mut self) -> Result<Option<(String, u64)>> {
        match self.t.next()? {
            None => Ok(None),
            Some(h) if h.kind == ItemKind::File => Ok(Some((h.path, h.size))),
            Some(h) => Err(corrupt(format!(
                "the archive holds a directory entry ({:?}); it may hold regular files only",
                h.path.chars().take(200).collect::<String>()
            ))),
        }
    }

    /// The bytes of the file [`next`](Self::next) just returned, which is `size` long.
    pub(crate) fn read_file(&mut self, size: u64) -> Result<Vec<u8>> {
        let mut data = vec![0u8; size as usize];
        read_exact_or(&mut self.t.r, &mut data, "a file")?;
        self.t.skip_pad(size)?;
        Ok(data)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::remote::seal::SALT_LEN;

    pub(crate) struct Scratch(pub std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = crate::backup::remove_tree(&self.0);
        }
    }

    pub(crate) fn scratch(tag: &str) -> Scratch {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "spared-archive-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = crate::backup::remove_tree(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }

    const NAMES: Names = Names {
        app: "notes",
        remote_id: "20261112T040506Z.7f3a9c0a1b2c.1",
        local_id: "20261112T040506Z",
    };

    fn key() -> BackupKey {
        BackupKey::parse(&"42".repeat(32)).unwrap()
    }

    fn sample(root: &Path) {
        let src = root.join("src");
        std::fs::create_dir_all(src.join("sub/deeper")).unwrap();
        let mut db: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        db[100_000..250_000].fill(0);
        std::fs::write(src.join("app.db"), &db).unwrap();
        std::fs::write(src.join("sub/deeper/x.txt"), b"x").unwrap();
        std::fs::write(src.join("empty"), b"").unwrap();
        std::fs::write(src.join("sub").join("n".repeat(150)), b"long name").unwrap();
        std::fs::write(src.join("caf\u{e9}"), b"utf-8 name").unwrap();
    }

    fn tree(p: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = vec![];
        let mut stack = vec![(p.to_path_buf(), String::new())];
        while let Some((d, pre)) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let n = format!("{pre}{}", e.file_name().to_string_lossy());
                if e.file_type().unwrap().is_dir() {
                    out.push((n.clone() + "/", vec![]));
                    stack.push((e.path(), n + "/"));
                } else {
                    out.push((n, std::fs::read(e.path()).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    fn build_to(root: &Path, seal: bool) -> (std::path::PathBuf, Built) {
        let out = root.join(if seal { "a.sealed" } else { "a.zst" });
        let k = key();
        let salt = [9u8; SALT_LEN];
        let b = build(
            &root.join("src"),
            File::create(&out).unwrap(),
            &NAMES,
            seal.then_some((&k, &salt)),
            &Limits::new(1 << 30),
            u64::MAX,
        )
        .unwrap();
        (out, b)
    }

    fn unpack_to(root: &Path, archive: &Path, seal: bool, names: &Names) -> Result<Unpacked> {
        let dest = root.join("dest");
        let _ = crate::backup::remove_tree(&dest);
        std::fs::create_dir(&dest).unwrap();
        let k = key();
        unpack(
            File::open(archive).unwrap(),
            seal.then_some(&k),
            &dest,
            names,
            &Limits::new(1 << 30),
        )
    }

    #[test]
    fn round_trips_byte_for_byte_sealed_and_plain() {
        for seal in [true, false] {
            let s = scratch("rt");
            sample(&s.0);
            let (a, built) = build_to(&s.0, seal);
            assert_eq!(std::fs::metadata(&a).unwrap().len(), built.bytes);
            assert_eq!(
                Sha256::digest(std::fs::read(&a).unwrap()).as_slice(),
                built.sha256
            );
            assert_eq!(built.contents.files, 5);
            let u = unpack_to(&s.0, &a, seal, &NAMES).unwrap();
            assert_eq!(u.digest, built.digest);
            assert_eq!(u.contents, built.contents);
            assert_eq!(
                tree(&s.0.join("src")),
                tree(&s.0.join("dest")),
                "seal={seal}"
            );
            let (_, again) = build_to(&s.0, seal);
            assert_eq!(
                again.digest, built.digest,
                "the inner manifest is deterministic"
            );
        }
    }

    #[test]
    fn a_plain_archive_is_ordinary_zstd_and_tar() {
        let s = scratch("plain");
        sample(&s.0);
        let (a, _) = build_to(&s.0, false);
        let raw = zstd::stream::decode_all(File::open(&a).unwrap()).unwrap();
        assert_eq!(&raw[257..263], b"ustar\0");
        assert!(raw.starts_with(INNER_MANIFEST.as_bytes()));
    }

    #[test]
    fn a_file_over_the_tar_size_field_is_refused_before_hashing() {
        let s = scratch("huge");
        let src = s.0.join("src");
        std::fs::create_dir_all(&src).unwrap();
        File::create(src.join("app.db"))
            .unwrap()
            .set_len(8 << 30)
            .unwrap();
        let t = std::time::Instant::now();
        let e = build(
            &src,
            File::create(s.0.join("a")).unwrap(),
            &NAMES,
            None,
            &Limits::new(u64::MAX),
            u64::MAX,
        )
        .err()
        .expect("an 8 GiB file must be refused");
        println!("refused in {:?}: {e}", t.elapsed());
        assert!(is_too_big(&e), "{e}");
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn the_size_field_holds_its_largest_value_and_refuses_more() {
        let max = (8u64 << 30) - 1;
        let h = tar_header(b"x", b'0', max, 0o600).unwrap();
        assert_eq!(parse_octal(&h[124..136]), Some(max));
        assert!(tar_header(b"x", b'0', 8 << 30, 0o600).is_err());
        assert!(tar_header(b"x", b'0', u64::MAX, 0o600).is_err());
    }

    #[test]
    fn the_object_cap_stops_the_build() {
        let s = scratch("cap");
        sample(&s.0);
        let e = build(
            &s.0.join("src"),
            File::create(s.0.join("a")).unwrap(),
            &NAMES,
            None,
            &Limits::new(1 << 30),
            1000,
        )
        .err()
        .unwrap();
        assert!(is_too_big(&e), "{e}");
    }

    #[test]
    fn another_app_or_id_is_refused_even_unsealed() {
        let s = scratch("names");
        sample(&s.0);
        for seal in [true, false] {
            let (a, _) = build_to(&s.0, seal);
            let other = Names {
                app: "other",
                ..NAMES
            };
            let e = unpack_to(&s.0, &a, seal, &other).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidData, "{e}");
        }
    }

    #[test]
    fn paths_are_checked() {
        for ok in ["a", "a/b", "caf\u{e9}", ".hidden", "a..b"] {
            assert!(valid_path(ok, 64), "{ok}");
        }
        for bad in ["", "/a", "a//b", "a/./b", "../a", "a/..", "a/", "a\0b", "."] {
            assert!(!valid_path(bad, 64), "{bad:?}");
        }
        assert!(valid_path(&vec!["d"; 65].join("/"), 64));
        assert!(!valid_path(&vec!["d"; 66].join("/"), 64));
    }

    // ---- hostile archives, built by hand

    /// A tar stream: the inner manifest listing `listed`, then `body` (raw tar entries), then the
    /// end marker.
    fn hostile(listed: &[Item], body: &[u8]) -> Vec<u8> {
        let files = listed.iter().filter(|i| i.kind == ItemKind::File).count() as u64;
        let c = Contents {
            format: CONTENTS_FORMAT.into(),
            app: NAMES.app.into(),
            remote_id: NAMES.remote_id.into(),
            local_id: NAMES.local_id.into(),
            spared_version: "test".into(),
            journal_clean: true,
            files,
            bytes: listed.iter().map(|i| i.size).sum(),
            entries: listed.to_vec(),
        };
        let m = serde_json::to_vec(&c).unwrap();
        let mut t = vec![];
        write_entry(&mut t, INNER_MANIFEST, ItemKind::File, m.len() as u64).unwrap();
        t.extend_from_slice(&m);
        pad(&mut t, m.len() as u64).unwrap();
        t.extend_from_slice(body);
        t.extend_from_slice(&[0u8; 2 * BLOCK]);
        t
    }

    fn file_item(path: &str, data: &[u8]) -> Item {
        Item {
            path: path.into(),
            kind: ItemKind::File,
            size: data.len() as u64,
            sha256: Some(hex(&Sha256::digest(data))),
        }
    }

    fn raw_entry(name: &[u8], flag: u8, data: &[u8]) -> Vec<u8> {
        let mut v = tar_header(name, flag, data.len() as u64, 0o600)
            .unwrap()
            .to_vec();
        v.extend_from_slice(data);
        pad(&mut v, data.len() as u64).unwrap();
        v
    }

    /// Seal and unpack a hand-built tar stream; the error, and whether anything escaped `dest`.
    fn try_hostile(tar: &[u8], limits: Limits) -> Error {
        let s = scratch("hostile");
        let z = zstd::stream::encode_all(tar, 3).unwrap();
        let a = s.0.join("a");
        let k = key();
        let mut w = Sealer::new(
            &k,
            &[1; SALT_LEN],
            crate::remote::seal::aad(NAMES.app, NAMES.remote_id),
            File::create(&a).unwrap(),
        )
        .unwrap();
        w.write_all(&z).unwrap();
        w.finish().unwrap();
        let dest = s.0.join("box/dest");
        std::fs::create_dir_all(&dest).unwrap();
        let e = unpack(File::open(&a).unwrap(), Some(&k), &dest, &NAMES, &limits).unwrap_err();
        let outside: Vec<_> = std::fs::read_dir(s.0.join("box"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(outside, ["dest"], "something was written outside dest");
        assert!(!s.0.join("evil").exists());
        e
    }

    fn refused(tar: &[u8], want: &str) {
        let e = try_hostile(tar, Limits::new(1 << 30));
        assert_eq!(e.kind(), ErrorKind::InvalidData, "{e}");
        assert!(e.to_string().contains(want), "wanted {want:?}, got: {e}");
    }

    #[test]
    fn dot_dot_names_are_refused() {
        let item = file_item("../evil", b"x");
        refused(
            &hostile(&[item], &raw_entry(b"../evil", b'0', b"x")),
            "unsafe",
        );
        // Listed safely, but the tar entry itself names ../evil.
        let ok = file_item("fine", b"x");
        refused(
            &hostile(&[ok], &raw_entry(b"../evil", b'0', b"x")),
            "entry 1",
        );
        let abs = file_item("/tmp/evil", b"x");
        refused(
            &hostile(&[abs], &raw_entry(b"/tmp/evil", b'0', b"x")),
            "unsafe",
        );
    }

    #[test]
    fn links_devices_and_unknown_types_are_refused() {
        let ok = file_item("fine", b"x");
        for (flag, want) in [
            (b'2', "symlink"),
            (b'1', "hard link"),
            (b'3', "device"),
            (b'6', "fifo"),
            (b'g', "type 'g'"),
            (b'L', "type 'L'"),
        ] {
            let mut e = tar_header(b"fine", flag, 0, 0o777).unwrap();
            if flag == b'2' || flag == b'1' {
                e[157..167].copy_from_slice(b"/etc/hosts");
                e[148..156].fill(b' ');
                let sum: u64 = e.iter().map(|&b| b as u64).sum();
                e[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            }
            refused(&hostile(std::slice::from_ref(&ok), &e), want);
        }
    }

    #[test]
    fn a_decompression_bomb_stops_at_the_cap() {
        // 64 MiB of zeros listed as a 64 MiB file, read with a 1 MiB data cap.
        let big = vec![0u8; 64 << 20];
        let item = file_item("bomb", &big);
        let tar = hostile(std::slice::from_ref(&item), &raw_entry(b"bomb", b'0', &big));
        let e = try_hostile(
            &tar,
            Limits {
                max_bytes: 1 << 20,
                ..Limits::new(0)
            },
        );
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert!(e.to_string().contains("data cap"), "{e}");
        // Zeros after the end marker, which no entry lists: the cap on the whole stream stops it.
        let small = file_item("a", b"x");
        let mut tar = hostile(&[small], &raw_entry(b"a", b'0', b"x"));
        tar.extend_from_slice(&big);
        let limits = Limits {
            max_bytes: 1 << 20,
            max_entries: 10,
            max_depth: 64,
        };
        let e = try_hostile(&tar, limits);
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert!(e.to_string().contains("data cap"), "{e}");
    }

    #[test]
    fn order_hashes_duplicates_and_extras_are_checked() {
        let a = file_item("a", b"aaa");
        let b = file_item("b", b"bbb");
        let body = [raw_entry(b"b", b'0', b"bbb"), raw_entry(b"a", b'0', b"aaa")].concat();
        refused(&hostile(&[a.clone(), b.clone()], &body), "entry 1");
        refused(
            &hostile(std::slice::from_ref(&a), &raw_entry(b"a", b'0', b"aaX")),
            "does not match its sha256",
        );
        let body = [raw_entry(b"a", b'0', b"aaa"), raw_entry(b"a", b'0', b"aaa")].concat();
        refused(&hostile(&[a.clone(), a.clone()], &body), "twice");
        let body = [raw_entry(b"a", b'0', b"aaa"), raw_entry(b"b", b'0', b"bbb")].concat();
        refused(&hostile(std::slice::from_ref(&a), &body), "more entries");
        refused(
            &hostile(&[a.clone(), b], &raw_entry(b"a", b'0', b"aaa")),
            "ends after 1",
        );
        let in_missing_dir = file_item("nodir/a", b"aaa");
        refused(
            &hostile(&[in_missing_dir], &raw_entry(b"nodir/a", b'0', b"aaa")),
            "before its directory",
        );
    }

    #[test]
    fn a_tar_without_the_inner_manifest_first_is_refused() {
        let mut t = raw_entry(b"a", b'0', b"aaa");
        t.extend_from_slice(&[0u8; 2 * BLOCK]);
        refused(&t, "not .spared-manifest.json");
    }
}
