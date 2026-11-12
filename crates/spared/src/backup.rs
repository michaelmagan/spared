//! File-level helpers for app backups: ids, a verified tree copy, listing, retention, and
//! deletion of app-controlled trees. The server (`server.rs`) decides when these run, with the
//! app's instance lock held. Nothing here opens an app's database: it is copied as bytes.

use crate::dirfd::{Dir, Kind};
use sha2::{Digest, Sha256};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::Path;

pub const PRE_RESTORE: &str = "pre-restore-";

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    pub files: u64,
    /// Entries that are neither regular files nor directories (symlinks, special files).
    pub skipped: u64,
    /// [`tree_stats`] stopped counting at its entry limit.
    pub truncated: bool,
}

pub struct Entry {
    pub id: String,
    pub pre_restore: bool,
    pub stats: Stats,
}

fn all_digits(s: &[u8]) -> bool {
    s.iter().all(u8::is_ascii_digit)
}

/// `20261110T040506Z`, optionally behind `pre-restore-`. Nothing else is accepted, so an id can
/// never name a path outside `<data-dir>/backups/<name>/`. It compares bytes, never slices the
/// text: an id comes from a request, and a byte index inside a multi-byte character panics.
pub fn valid_id(id: &str) -> bool {
    let ts = id.strip_prefix(PRE_RESTORE).unwrap_or(id);
    let b = ts.as_bytes();
    b.len() == 16 && all_digits(&b[..8]) && b[8] == b'T' && all_digits(&b[9..15]) && b[15] == b'Z'
}

pub fn is_pre_restore(id: &str) -> bool {
    id.starts_with(PRE_RESTORE)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `20261110T040506Z` for a Unix time in seconds (UTC).
pub fn timestamp_id(secs: u64) -> String {
    let (y, mo, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}{mo:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `2026-11-10T04:05:06Z` for a valid id (with or without the `pre-restore-` prefix).
pub fn created_at(id: &str) -> String {
    let t = id.strip_prefix(PRE_RESTORE).unwrap_or(id);
    let at = |a: usize, b: usize| t.get(a..b).unwrap_or("");
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        at(0, 4),
        at(4, 6),
        at(6, 8),
        at(9, 11),
        at(11, 13),
        at(13, 15)
    )
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix seconds for a valid id (with or without the `pre-restore-` prefix).
fn id_secs(id: &str) -> u64 {
    let t = id.strip_prefix(PRE_RESTORE).unwrap_or(id);
    let n = |a: usize, b: usize| t.get(a..b).and_then(|d| d.parse::<i64>().ok()).unwrap_or(0);
    let days = days_from_civil(n(0, 4), n(4, 6), n(6, 8));
    (days * 86_400 + n(9, 11) * 3600 + n(11, 13) * 60 + n(13, 15)).max(0) as u64
}

/// A fresh id under `dir`: `<prefix><timestamp>`. The timestamp is `secs`, or one second after
/// the newest id already there when `secs` is not later (two backups in the same second, or a
/// clock that stepped back), so ids are unique and increase with each one taken, even after
/// retention removed older ones.
pub fn unique_id(dir: &Path, prefix: &str, secs: u64) -> String {
    let newest = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| valid_id(n))
        .map(|n| id_secs(&n))
        .max();
    let secs = newest.map_or(secs, |m| secs.max(m + 1));
    format!("{prefix}{}", timestamp_id(secs))
}

/// The `integrity` value of every backup and restore: the host compares hashes and never parses
/// the database, because `app.db` is whatever bytes the app wrote (the backup code treats it as untrusted).
pub const INTEGRITY: &str = "copied, sha256 verified; database not opened by the host";

/// Limits on what a backup or restore copies. An app controls its data tree, so both are capped;
/// a tree over either limit is refused and nothing is copied into place.
pub const MAX_DEPTH: usize = 64;
pub const MAX_ENTRIES: u64 = 100_000;

/// Entries counted by [`tree_stats`] before it stops and reports `truncated`.
const STATS_MAX_ENTRIES: u64 = 1_000_000;

/// Copy `src` into the new directory `dst` (mode 0700, files 0600), fsyncing as it goes. The
/// files may add up to `max_bytes` at most, each counted at its apparent size (so a sparse file
/// counts in full); holes are kept holes in the copy, so the copy takes the disk space the
/// original does. Never
/// follows symlinks: an app controls what is in its data directory, so a link could point anywhere
/// on the host. Symlinks and special files are counted in `skipped`, not copied. Each file is
/// hashed while it is read and the written copy is read back and compared; a mismatch is an
/// `InvalidData` error. Walks with an explicit stack, never recursion.
pub fn copy_tree(src: &Path, dst: &Path, max_bytes: u64) -> std::io::Result<Stats> {
    copy_tree_limited(src, dst, MAX_DEPTH, MAX_ENTRIES, max_bytes)
}

fn too_big(what: String) -> std::io::Error {
    std::io::Error::other(format!("{what}; nothing was copied into place"))
}

fn copy_tree_limited(
    src: &Path,
    dst: &Path,
    max_depth: usize,
    max_entries: u64,
    max_bytes: u64,
) -> std::io::Result<Stats> {
    let mut stats = Stats::default();
    let mut entries = 0u64;
    let mut made = vec![];
    std::fs::DirBuilder::new().mode(0o700).create(dst)?;
    made.push(dst.to_path_buf());
    let mut stack = vec![(Dir::open(src)?, dst.to_path_buf())];
    while let Some((dir, to_dir)) = stack.last_mut() {
        let Some(e) = dir.read_entry()? else {
            stack.pop();
            continue;
        };
        entries += 1;
        if entries > max_entries {
            return Err(too_big(format!(
                "more than {max_entries} files and directories"
            )));
        }
        let to = to_dir.join(&e.name);
        match e.kind {
            Kind::Dir => {
                if stack.len() > max_depth {
                    return Err(too_big(format!(
                        "directories are nested more than {max_depth} levels deep"
                    )));
                }
                match stack.last().expect("non-empty").0.open_dir(&e.name) {
                    Ok(child) => {
                        std::fs::DirBuilder::new().mode(0o700).create(&to)?;
                        made.push(to.clone());
                        stack.push((child, to));
                    }
                    Err(err) if crate::dirfd::is_not_followed(&err) => stats.skipped += 1,
                    Err(err) => return Err(err),
                }
            }
            Kind::File => {
                if stats.bytes.saturating_add(e.len) > max_bytes {
                    return Err(too_big(format!(
                        "the files add up to more than {} MB, counting each at its full size (a sparse file counts in full)",
                        max_bytes >> 20
                    )));
                }
                let from = stack.last().expect("non-empty").0.open_file(&e.name);
                match from {
                    Ok(f) => {
                        stats.bytes += copy_file_verified(&f, &e.name, &to)?;
                        stats.files += 1;
                    }
                    Err(err) if crate::dirfd::is_not_followed(&err) => stats.skipped += 1,
                    Err(err) => return Err(err),
                }
            }
            Kind::Other => stats.skipped += 1,
        }
    }
    for d in made.iter().rev() {
        std::fs::File::open(d)?.sync_all()?;
    }
    Ok(stats)
}

/// Hash `n` zero bytes into `h`.
fn hash_zeros(h: &mut Sha256, mut n: u64) {
    static ZEROS: [u8; 1 << 16] = [0; 1 << 16];
    while n > 0 {
        let k = n.min(ZEROS.len() as u64) as usize;
        h.update(&ZEROS[..k]);
        n -= k as u64;
    }
}

/// Where the next stretch of data or hole starts at or after `pos` (`whence` is `SEEK_DATA` or
/// `SEEK_HOLE`). `None` when there is none, or when the file system cannot say, in which case the
/// whole file is treated as data.
fn seek_extent(f: &std::fs::File, pos: u64, whence: libc::c_int) -> Option<u64> {
    let r = unsafe { libc::lseek(f.as_raw_fd(), pos as libc::off_t, whence) };
    (r >= 0).then_some(r as u64)
}

/// The data stretches of `f` as (start, end) pairs, in order; one stretch for the whole file
/// where the file system has no notion of holes.
fn extents(f: &std::fs::File, len: u64) -> Vec<(u64, u64)> {
    let mut v = vec![];
    let mut pos = 0;
    while pos < len {
        let Some(start) = seek_extent(f, pos, libc::SEEK_DATA) else {
            if v.is_empty() && std::io::Error::last_os_error().raw_os_error() != Some(libc::ENXIO) {
                return vec![(0, len)];
            }
            break;
        };
        let end = seek_extent(f, start, libc::SEEK_HOLE)
            .unwrap_or(len)
            .min(len);
        if end <= start {
            break;
        }
        v.push((start, end));
        pos = end;
    }
    v
}

/// The SHA-256 of a file's contents, reading only its data stretches and counting each hole as
/// the zeros it reads as.
fn sha256_of(f: &std::fs::File) -> std::io::Result<[u8; 32]> {
    let len = f.metadata()?.len();
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut pos = 0;
    for (start, end) in extents(f, len) {
        hash_zeros(&mut h, start - pos);
        let mut at = start;
        while at < end {
            let n = ((end - at) as usize).min(buf.len());
            f.read_exact_at(&mut buf[..n], at)?;
            h.update(&buf[..n]);
            at += n as u64;
        }
        pos = end;
    }
    hash_zeros(&mut h, len - pos);
    Ok(h.finalize().into())
}

/// Copy one regular file, hashing what is read; then read the copy back and compare. Holes in the
/// original stay holes in the copy.
fn copy_file_verified(
    r: &std::fs::File,
    name: &std::ffi::OsStr,
    to: &Path,
) -> std::io::Result<u64> {
    let w = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    let len = r.metadata()?.len();
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut pos = 0;
    for (start, end) in extents(r, len) {
        hash_zeros(&mut h, start - pos);
        let mut at = start;
        while at < end {
            let n = ((end - at) as usize).min(buf.len());
            let got = r.read_at(&mut buf[..n], at)?;
            if got == 0 {
                break;
            }
            h.update(&buf[..got]);
            w.write_all_at(&buf[..got], at)?;
            at += got as u64;
        }
        pos = at;
    }
    hash_zeros(&mut h, len.saturating_sub(pos));
    w.set_len(len)?;
    w.sync_all()?;
    let read: [u8; 32] = h.finalize().into();
    let copied = sha256_of(&std::fs::File::open(to)?)?;
    if read != copied {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "the copy of {} does not match what was read (sha256 differs)",
                name.to_string_lossy()
            ),
        ));
    }
    Ok(len)
}

/// Size and file count of a tree already on disk (symlinks ignored), walked with directory
/// handles so a directory that is swapped for a link mid-walk is never followed. Stops after a
/// million entries, or at a nesting of [`crate::dirfd::MAX_WALK_DEPTH`] levels, and sets
/// `truncated`.
pub fn tree_stats(p: &Path) -> Stats {
    let mut s = Stats::default();
    let mut seen = 0u64;
    let w = crate::dirfd::walk(p, |e| {
        seen += 1;
        if seen > STATS_MAX_ENTRIES {
            return false;
        }
        match e.kind {
            Kind::Dir => {}
            Kind::File => {
                s.bytes += e.len;
                s.files += 1;
            }
            Kind::Other => s.skipped += 1,
        }
        true
    });
    s.truncated = w.stopped || w.too_deep;
    s
}

/// Delete a tree of any depth without recursion and with at most two directories open at a time:
/// each directory directly under `root` has its subdirectories moved up to `root` (so names stay
/// short whatever the depth) and its other entries unlinked, then it is removed. Every step is
/// relative to an open directory and refuses to follow a link, so a directory swapped for a
/// symlink while this runs is unlinked as a link, never entered. Symlinks are removed, never
/// followed. `std::fs::remove_dir_all` recurses once per level and keeps a descriptor open per
/// level, so an app that nests directories thousands deep can overflow the stack or run out of
/// descriptors with it.
pub fn remove_tree(root: &Path) -> std::io::Result<()> {
    let m = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !m.is_dir() {
        return std::fs::remove_file(root);
    }
    let mut top_dir = Dir::open(root)?;
    let mut n = 0u64;
    loop {
        let mut top = vec![];
        while let Some(e) = top_dir.read_entry()? {
            top.push(e);
        }
        if top.is_empty() {
            break;
        }
        for t in top {
            if t.kind != Kind::Dir {
                top_dir.unlink(&t.name)?;
                continue;
            }
            match top_dir.open_dir(&t.name) {
                Ok(mut dir) => {
                    while let Some(e) = dir.read_entry()? {
                        if e.kind == Kind::Dir {
                            loop {
                                n += 1;
                                let to = format!(".rm-{n}");
                                match dir.rename_into(&e.name, &top_dir, std::ffi::OsStr::new(&to))
                                {
                                    Err(e)
                                        if matches!(
                                            e.raw_os_error(),
                                            Some(libc::ENOTEMPTY | libc::EEXIST | libc::ENOTDIR)
                                        ) => {}
                                    r => break r?,
                                }
                            }
                        } else {
                            dir.unlink(&e.name)?;
                        }
                    }
                    top_dir.remove_dir(&t.name)?;
                }
                Err(e) if crate::dirfd::is_not_followed(&e) => top_dir.unlink(&t.name)?,
                Err(e) => return Err(e),
            }
        }
        top_dir = Dir::open(root)?;
    }
    drop(top_dir);
    std::fs::remove_dir(root)
}

/// Whether the copied database has no pending rollback journal or WAL beside it.
pub fn journal_clean(dir: &Path) -> bool {
    let non_empty = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() > 0);
    !non_empty(&dir.join("app.db-journal")) && !non_empty(&dir.join("app.db-wal"))
}

/// Valid backup ids under `dir`, unsorted, without walking their contents.
pub fn ids(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|id| valid_id(id))
        .collect()
}

/// Backups under `dir`, newest first (by timestamp, whatever the prefix). Entries whose names are
/// not valid ids (temporary copies, strays) are left out.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut v: Vec<Entry> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().to_string();
            (valid_id(&id) && e.path().is_dir()).then(|| Entry {
                pre_restore: is_pre_restore(&id),
                stats: tree_stats(&e.path()),
                id,
            })
        })
        .collect();
    v.sort_by(|a, b| {
        let key = |e: &Entry| e.id.strip_prefix(PRE_RESTORE).unwrap_or(&e.id).to_string();
        key(b).cmp(&key(a)).then(b.id.cmp(&a.id))
    });
    v
}

/// Keep the newest `keep` plain backups and, separately, the newest `keep` pre-restore copies.
/// Ids in `pinned` (backups a remote upload is reading) are never removed. Returns the ids
/// removed.
pub fn prune(dir: &Path, keep: usize, pinned: &std::collections::HashSet<String>) -> Vec<String> {
    let mut removed = vec![];
    for pre in [false, true] {
        let all: Vec<Entry> = list(dir)
            .into_iter()
            .filter(|e| e.pre_restore == pre)
            .collect();
        for e in all.into_iter().skip(keep) {
            if pinned.contains(&e.id) {
                continue;
            }
            // Renamed out of the id namespace first, so a removal that fails half way leaves no
            // half-deleted backup in the list; `.tmp-` leftovers are cleared by the next backup.
            let doomed = dir.join(format!(".tmp-rm-{}", e.id));
            if std::fs::rename(dir.join(&e.id), &doomed).is_ok() {
                let _ = remove_tree(&doomed);
                let _ = std::fs::remove_file(dir.join(".remote").join(format!("{}.json", e.id)));
                removed.push(e.id);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Scratch(std::path::PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = std::path::PathBuf;
        fn deref(&self) -> &std::path::PathBuf {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = remove_tree(&self.0);
        }
    }

    fn scratch(tag: &str) -> Scratch {
        let d = std::env::temp_dir().join(format!("spared-backup-{tag}-{}", std::process::id()));
        let _ = remove_tree(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }

    #[test]
    fn non_ascii_ids_are_refused_not_a_panic() {
        // 16 bytes with a two-byte character across byte 8, which `&ts[..8]` cannot slice
        for bad in [
            "1234567\u{e9}1234567",
            "pre-restore-1234567\u{e9}1234567",
            "20261110T04050\u{e9}",
            "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
            "2026111\u{661}T040506Z",
        ] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn ids() {
        assert!(valid_id("20261110T040506Z"));
        assert!(valid_id("pre-restore-20261110T040506Z"));
        for bad in [
            "",
            "..",
            "../20261110T040506Z",
            "20261110T040506Z/..",
            "20261110T040506",
            "2026111T0405066Z",
            "20261110t040506Z",
            "pre-restore-",
            "pre-restore-../x",
            "pre-restore-pre-restore-20261110T040506Z",
            "20261110T040506Z\n",
            "２０２61110T040506Z",
            "tmp-20261110T040506Z",
        ] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp_id(0), "19700101T000000Z");
        assert_eq!(timestamp_id(951_782_400 + 86_399), "20000229T235959Z");
        assert_eq!(timestamp_id(1_794_283_506), "20261110T040506Z");
        assert_eq!(created_at("20261110T040506Z"), "2026-11-10T04:05:06Z");
        assert_eq!(
            created_at("pre-restore-20261110T040506Z"),
            "2026-11-10T04:05:06Z"
        );
    }

    #[test]
    fn ids_increase_even_after_the_older_ones_are_gone() {
        let d = scratch("unique");
        assert_eq!(id_secs("20261110T040506Z"), 1_794_283_506);
        assert_eq!(
            id_secs("pre-restore-20000229T235959Z"),
            951_782_400 + 86_399
        );
        let a = unique_id(&d, "", 1_794_283_506);
        std::fs::create_dir(d.join(&a)).unwrap();
        let b = unique_id(&d, "", 1_794_283_506);
        std::fs::create_dir(d.join(&b)).unwrap();
        let c = unique_id(&d, PRE_RESTORE, 1_794_283_400);
        assert_eq!(a, "20261110T040506Z");
        assert_eq!(b, "20261110T040507Z");
        assert_eq!(c, "pre-restore-20261110T040508Z");
        remove_tree(&d.join(&a)).unwrap();
        remove_tree(&d.join(&b)).unwrap();
        assert_eq!(unique_id(&d, "", 1_794_283_506), "20261110T040506Z");
        std::fs::create_dir(d.join(&a)).unwrap();
        assert_eq!(unique_id(&d, "", 1_794_283_999), "20261110T041319Z");
        remove_tree(&d).unwrap();
    }

    #[test]
    fn copy_skips_symlinks_and_sets_modes() {
        let d = scratch("copy");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"xy").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", src.join("link")).unwrap();
        let dst = d.join("dst");
        let s = copy_tree(&src, &dst, u64::MAX).unwrap();
        assert_eq!(
            s,
            Stats {
                bytes: 7,
                files: 2,
                skipped: 1,
                truncated: false
            }
        );
        assert!(!dst.join("link").exists());
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"xy");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dst), 0o700);
        assert_eq!(mode(&dst.join("sub")), 0o700);
        assert_eq!(mode(&dst.join("a.txt")), 0o600);
        assert_eq!(tree_stats(&dst).files, 2);
        remove_tree(&d).unwrap();
    }

    /// `<root>/top/d/d/.../d/f`, `depth` directories below `top`, built with short paths only
    /// (wrap the chain in a new directory, repeatedly), the way a guest can nest past PATH_MAX.
    fn deep_tree(root: &Path, depth: usize) -> std::path::PathBuf {
        let chain = root.join("chain");
        std::fs::create_dir(&chain).unwrap();
        std::fs::write(chain.join("f"), b"bottom").unwrap();
        for _ in 0..depth {
            let wrap = root.join("wrap");
            std::fs::create_dir(&wrap).unwrap();
            std::fs::rename(&chain, wrap.join("d")).unwrap();
            std::fs::rename(&wrap, &chain).unwrap();
        }
        let top = root.join("top");
        std::fs::rename(&chain, &top).unwrap();
        top
    }

    #[test]
    fn remove_tree_handles_depth_without_recursion() {
        let d = scratch("deep");
        let top = deep_tree(&d, 20_000);
        assert!(
            tree_stats(&top).truncated,
            "a walk keeps one handle per level and stops at a fixed depth"
        );
        // A small stack: recursion per level (remove_dir_all) would overflow it.
        let t = top.clone();
        std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || remove_tree(&t).unwrap())
            .unwrap()
            .join()
            .unwrap();
        assert!(!top.exists());
        std::os::unix::fs::symlink("/etc", d.join("link")).unwrap();
        remove_tree(&d.join("link")).unwrap();
        assert!(std::path::Path::new("/etc/passwd").exists());
        remove_tree(&d).unwrap();
        remove_tree(&d).unwrap();
    }

    #[test]
    fn copy_is_verified_and_capped() {
        let d = scratch("caps");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("app.db"), &body).unwrap();
        let s = copy_tree(&src, &d.join("ok"), u64::MAX).unwrap();
        assert_eq!((s.bytes, s.files), (300_000, 1));
        assert_eq!(std::fs::read(d.join("ok/app.db")).unwrap(), body);
        assert!(journal_clean(&d.join("ok")));
        std::fs::write(d.join("ok/app.db-journal"), b"pending").unwrap();
        assert!(!journal_clean(&d.join("ok")));

        let deep = deep_tree(&src, MAX_DEPTH + 1);
        let e = copy_tree(&src, &d.join("too-deep"), u64::MAX)
            .unwrap_err()
            .to_string();
        assert!(e.contains("nested more than 64"), "{e}");
        remove_tree(&deep).unwrap();
        for i in 0..20 {
            std::fs::write(src.join(format!("f{i}")), b"x").unwrap();
        }
        let e = copy_tree_limited(&src, &d.join("too-many"), MAX_DEPTH, 10, u64::MAX)
            .unwrap_err()
            .to_string();
        assert!(e.contains("more than 10 files"), "{e}");
        remove_tree(&d).unwrap();
    }

    fn allocated(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).unwrap().blocks() * 512
    }

    #[test]
    fn a_sparse_file_stays_sparse_and_a_big_tree_is_refused() {
        let d = scratch("sparse");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();
        let f = std::fs::File::create(src.join("sparse.bin")).unwrap();
        f.set_len(64 << 20).unwrap();
        f.write_all_at(b"head", 0).unwrap();
        f.write_all_at(b"middle", 20 << 20).unwrap();
        f.write_all_at(b"tail", (64 << 20) - 4).unwrap();
        drop(f);
        std::fs::write(src.join("small"), b"abc").unwrap();
        let s = copy_tree(&src, &d.join("ok"), 1 << 30).unwrap();
        assert_eq!((s.bytes, s.files), ((64 << 20) + 3, 2));
        let copy = d.join("ok/sparse.bin");
        assert_eq!(std::fs::metadata(&copy).unwrap().len(), 64 << 20);
        if allocated(&src.join("sparse.bin")) < 16 << 20 {
            assert!(
                allocated(&copy) < 16 << 20,
                "the copy is as sparse as the original ({} bytes allocated)",
                allocated(&copy)
            );
        }
        assert_eq!(
            sha256_of(&std::fs::File::open(&copy).unwrap()).unwrap(),
            sha256_of(&std::fs::File::open(src.join("sparse.bin")).unwrap()).unwrap()
        );
        let mut head = [0u8; 4];
        std::fs::File::open(&copy)
            .unwrap()
            .read_exact_at(&mut head, 0)
            .unwrap();
        assert_eq!(&head, b"head");

        let e = copy_tree(&src, &d.join("over"), 16 << 20)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("more than 16 MB") && e.contains("nothing was copied"),
            "{e}"
        );
        assert!(!d.join("over/sparse.bin").exists());
        remove_tree(&d).unwrap();
    }

    #[test]
    fn list_newest_first_and_prune_keeps_newest() {
        let d = scratch("list");
        for id in [
            "20261110T000001Z",
            "20261110T000003Z",
            "pre-restore-20261110T000002Z",
            "20261110T000002Z",
        ] {
            std::fs::create_dir(d.join(id)).unwrap();
            std::fs::write(d.join(id).join("f"), id).unwrap();
        }
        std::fs::create_dir(d.join(".tmp-20261110T000009Z")).unwrap();
        std::fs::create_dir(d.join("stray")).unwrap();
        let ids = |v: Vec<Entry>| v.into_iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(
            ids(list(&d)),
            [
                "20261110T000003Z",
                "pre-restore-20261110T000002Z",
                "20261110T000002Z",
                "20261110T000001Z"
            ]
        );
        let pinned = std::collections::HashSet::from(["20261110T000001Z".to_string()]);
        assert!(prune(&d, 2, &pinned).is_empty(), "a pinned backup stays");
        let removed = prune(&d, 2, &Default::default());
        assert_eq!(removed, ["20261110T000001Z"]);
        assert_eq!(
            ids(list(&d)),
            [
                "20261110T000003Z",
                "pre-restore-20261110T000002Z",
                "20261110T000002Z"
            ]
        );
        remove_tree(&d).unwrap();
    }

    fn swap_for_link(
        root: &Path,
        name: &'static str,
        target: &Path,
    ) -> impl FnMut(&std::ffi::OsStr) + 'static {
        let (dir, target) = (root.join(name), target.to_path_buf());
        let mut done = false;
        move |n| {
            if n == name && !done {
                done = true;
                std::fs::remove_dir_all(&dir).unwrap();
                std::os::unix::fs::symlink(&target, &dir).unwrap();
            }
        }
    }

    fn outside(tag: &str) -> Scratch {
        let o = scratch(tag);
        std::fs::write(o.join("secret.txt"), vec![b's'; 5000]).unwrap();
        o
    }

    fn holds_secret(dir: &Path) -> bool {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let ft = e.file_type().unwrap();
                if ft.is_dir() {
                    stack.push(e.path());
                } else if e.file_name() == "secret.txt" {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn a_directory_swapped_for_a_link_while_copying_is_not_followed() {
        let base = scratch("race-copy");
        let (src, dst, out) = (base.join("src"), base.join("dst"), outside("race-copy-out"));
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::create_dir_all(src.join("b")).unwrap();
        std::fs::write(src.join("a/f.txt"), b"a").unwrap();
        std::fs::write(src.join("b/g.txt"), b"b").unwrap();
        crate::dirfd::on_open(swap_for_link(&src, "a", &out));
        let stats = copy_tree(&src, &dst, u64::MAX).unwrap();
        assert!(
            !holds_secret(&dst),
            "the copy holds a file from outside the data directory"
        );
        assert!(stats.skipped >= 1, "{stats:?}");
        assert!(dst.join("b/g.txt").is_file());
    }

    #[test]
    fn a_directory_swapped_for_a_link_while_measuring_is_not_followed() {
        let base = scratch("race-stats");
        let (src, out) = (base.join("src"), outside("race-stats-out"));
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::create_dir_all(src.join("b")).unwrap();
        std::fs::write(src.join("b/g.txt"), b"b").unwrap();
        crate::dirfd::on_open(swap_for_link(&src, "a", &out));
        let s = tree_stats(&src);
        assert_eq!(s.bytes, 1, "bytes outside the tree were counted: {s:?}");
    }

    #[test]
    fn a_directory_swapped_for_a_link_while_deleting_leaves_the_target_alone() {
        let base = scratch("race-rm");
        let (src, out) = (base.join("src"), outside("race-rm-out"));
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::write(src.join("a/f.txt"), b"a").unwrap();
        crate::dirfd::on_open(swap_for_link(&src, "a", &out));
        let _ = remove_tree(&src);
        assert!(
            out.join("secret.txt").is_file(),
            "a file outside the tree was deleted"
        );
    }

    #[test]
    fn a_file_swapped_for_a_link_or_a_pipe_while_copying_is_skipped() {
        for pipe in [false, true] {
            let base = scratch(if pipe { "race-pipe" } else { "race-file" });
            let (src, dst, out) = (base.join("src"), base.join("dst"), outside("race-file-out"));
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join("x"), b"x").unwrap();
            std::fs::write(src.join("y"), b"y").unwrap();
            let (x, secret) = (src.join("x"), out.join("secret.txt"));
            let mut done = false;
            crate::dirfd::on_open(move |n| {
                if n == "x" && !done {
                    done = true;
                    std::fs::remove_file(&x).unwrap();
                    if pipe {
                        let c = std::ffi::CString::new(x.to_str().unwrap()).unwrap();
                        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
                    } else {
                        std::os::unix::fs::symlink(&secret, &x).unwrap();
                    }
                }
            });
            let stats = copy_tree(&src, &dst, u64::MAX).unwrap();
            assert!(!holds_secret(&dst));
            assert!(!dst.join("x").exists(), "pipe={pipe}");
            assert!(stats.skipped >= 1, "{stats:?}");
        }
    }
}
