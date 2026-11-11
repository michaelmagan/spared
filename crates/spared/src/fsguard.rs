//! Per-app limits on what a guest can make the host do through `wasi:filesystem`.
//!
//! wasmtime-wasi answers `read-directory` by reading the whole directory into host memory, opens
//! a host descriptor for every `open-at`, and writes wherever the guest points. None of that is
//! counted against `memory_mb`. This module wraps the filesystem interface: every call goes to
//! wasmtime-wasi's own implementation, after a check against the app's [`AppCaps`] and with the
//! books ([`FsGuard`]) updated afterwards. A refused call returns an ordinary WASI error code, so
//! the guest sees a failed `open` or `read_dir`, never a trap.

use bytes::Bytes;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasmtime::component::{HasData, Resource};
use wasmtime_wasi::filesystem::{Descriptor, WasiFilesystemCtxView, WasiFilesystemView};
use wasmtime_wasi::p2::bindings::filesystem::types::{
    self, ErrorCode, HostDescriptor, HostDirectoryEntryStream,
};
use wasmtime_wasi::p2::{
    DynInputStream, DynOutputStream, FsError, FsResult, InputStream, OutputStream, Pollable,
    StreamError, StreamResult,
};

use crate::logcap::LogCaps;
use crate::sandbox::Guarded;

/// Operator ceilings on what one app's guest can ask of the host's filesystem.
#[derive(Debug, Clone)]
pub struct AppCaps {
    /// Files, directories and links in the app's data tree. This also bounds what one
    /// `read-directory` call can pull into host memory, since a listing holds every entry of
    /// one directory.
    pub max_entries: u64,
    /// Directory listings a guest may hold open at once.
    pub max_listings: usize,
    /// Host descriptors a guest may hold open at once: every file or directory it opened under
    /// /data, plus every stream on a file (a stream keeps its own handle, even after the guest
    /// drops the descriptor it came from).
    pub max_open: usize,
    /// Bytes an app's data tree may hold, counting a file at its apparent size (so a sparse
    /// file counts in full).
    pub max_data_bytes: u64,
    pub log: LogCaps,
    /// Requests one app may have running or waiting for its instance; the next gets a 503.
    pub max_queue: usize,
    /// Bytes of request bodies the server holds in memory at once, over every app (a body is
    /// read before its request takes a place in the app's queue). Never below the largest body
    /// one request may send.
    pub body_buffer_bytes: u64,
}

impl Default for AppCaps {
    fn default() -> Self {
        AppCaps {
            max_entries: 50_000,
            max_listings: 4,
            max_open: 64,
            max_data_bytes: 1 << 30,
            log: LogCaps::default(),
            max_queue: 16,
            body_buffer_bytes: 64 << 20,
        }
    }
}

/// What an app's data tree holds right now, and the handles its guest has open. One per live
/// instance, created at wake.
pub struct FsGuard {
    caps: AppCaps,
    root: PathBuf,
    /// Apparent bytes in the tree, as far as the books know. Writes that grow a file add to it.
    /// A removal, a truncation or a replaced file never subtracts, because the guest's word that
    /// something shrank is not a measurement: the figure comes back down when
    /// [`FsGuard::remeasure`] counts the disk, which runs before a charge is refused, at most four
    /// times a second. So between measurements the books never read less than the tree.
    used: AtomicU64,
    entries: AtomicU64,
    measured: Mutex<Instant>,
    #[cfg(test)]
    walks: AtomicU64,
    handles: Mutex<Handles>,
}

#[derive(Default)]
struct Handles {
    listings: HashSet<u32>,
    open: HashSet<u32>,
    /// Streams on files that are alive. Each holds a host descriptor, so each counts against
    /// `max_open` together with `open`.
    streams: usize,
}

/// A data tree's apparent size and entry count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub bytes: u64,
    pub entries: u64,
}

/// Measure the tree at `root` (symlinks counted as entries, never followed: every directory is
/// opened relative to its parent without following links, so one swapped for a link mid-walk is
/// not entered), stopping once the entries pass `stop_entries` or the bytes pass `stop_bytes`:
/// such a tree is over its limits and the exact figure does not matter. A tree nested deeper than
/// [`crate::dirfd::MAX_WALK_DEPTH`] levels counts as over the entry limit.
pub fn measure(root: &Path, stop_entries: u64, stop_bytes: u64) -> Usage {
    let mut u = Usage {
        bytes: 0,
        entries: 0,
    };
    let w = crate::dirfd::walk(root, |e| {
        u.entries += 1;
        if e.kind == crate::dirfd::Kind::File {
            u.bytes += e.len;
        }
        u.entries <= stop_entries && u.bytes <= stop_bytes
    });
    if w.too_deep {
        u.entries = u.entries.max(stop_entries.saturating_add(1));
    }
    u
}

fn no_space() -> StreamError {
    StreamError::LastOperationFailed(std::io::Error::from_raw_os_error(libc::ENOSPC).into())
}

impl FsGuard {
    /// The books for the tree at `root`, measured off the async runtime.
    pub async fn open(root: PathBuf, caps: AppCaps) -> Arc<FsGuard> {
        let r = root.clone();
        let (e, b) = (caps.max_entries.saturating_add(1), caps.max_data_bytes);
        let u = tokio::task::spawn_blocking(move || measure(&r, e, b))
            .await
            .unwrap_or(Usage {
                bytes: 0,
                entries: 0,
            });
        Arc::new(FsGuard {
            caps,
            root,
            used: AtomicU64::new(u.bytes),
            entries: AtomicU64::new(u.entries),
            measured: Mutex::new(Instant::now()),
            #[cfg(test)]
            walks: AtomicU64::new(0),
            handles: Mutex::new(Handles::default()),
        })
    }

    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn add_entry(&self) -> Result<(), ErrorCode> {
        if self.entries() >= self.caps.max_entries {
            self.refresh();
            if self.entries() >= self.caps.max_entries {
                return Err(ErrorCode::Quota);
            }
        }
        self.entries.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Take back an entry [`FsGuard::add_entry`] counted for a call that then failed.
    fn undo_entry(&self) {
        let _ = self
            .entries
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
    }

    /// Count the tree again and take the result as the truth.
    fn remeasure(&self) -> u64 {
        #[cfg(test)]
        self.walks.fetch_add(1, Ordering::Relaxed);
        let u = measure(
            &self.root,
            self.caps.max_entries.saturating_add(1),
            self.caps.max_data_bytes.saturating_mul(2),
        );
        self.used.store(u.bytes, Ordering::Relaxed);
        self.entries.store(u.entries, Ordering::Relaxed);
        *self.measured.lock().unwrap() = Instant::now();
        u.bytes
    }

    /// Put the books right before a "no", but at most four times a second, however the guest
    /// came to be refused: it can make every removal, truncation and rename it likes, and each
    /// count walks the whole tree on the reactor thread. A guest that frees space and asks for it
    /// within the window is told no until the window has passed.
    fn refresh(&self) {
        if self.measured.lock().unwrap().elapsed() >= MEASURE_INTERVAL {
            self.remeasure();
        }
    }

    /// Whether `growth` more bytes fit under the limit. The books are checked against the disk
    /// before a "no".
    fn fits(&self, growth: u64) -> bool {
        let max = self.caps.max_data_bytes;
        if self.used().saturating_add(growth) <= max {
            return true;
        }
        self.refresh();
        self.used().saturating_add(growth) <= max
    }

    /// Account for `growth` more bytes in the tree; false, with nothing counted, when that
    /// would pass the limit.
    fn charge(&self, growth: u64) -> bool {
        if growth == 0 {
            return true;
        }
        if !self.fits(growth) {
            return false;
        }
        self.used.fetch_add(growth, Ordering::Relaxed);
        true
    }

    /// Whether a stream that extends a file may be opened. A guest only sees the error code of a
    /// failed `write-via-stream`, not of a failed write on the stream (wasi-libc reports any such
    /// failure as `EIO`), so a stream that would grow the file is refused up front, with the
    /// code for a full disk, once less than `STREAM_SLACK` bytes are left.
    fn room_to_extend(&self) -> bool {
        self.fits(STREAM_SLACK)
    }

    /// A slot for one more stream on a file, against the descriptor limit.
    fn take_slot(self: &Arc<Self>) -> Result<StreamSlot, ErrorCode> {
        let mut h = self.handles.lock().unwrap();
        if h.open.len() + h.streams >= self.caps.max_open {
            return Err(ErrorCode::Quota);
        }
        h.streams += 1;
        Ok(StreamSlot(self.clone()))
    }
}

/// One of the app's descriptor slots, held by a stream on a file until the stream is dropped.
struct StreamSlot(Arc<FsGuard>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut h = self.0.handles.lock().unwrap();
        h.streams = h.streams.saturating_sub(1);
    }
}

/// Space kept free of extending streams: a write on a stream is at most this long.
const STREAM_SLACK: u64 = 64 << 10;

/// The shortest time between two counts of a data tree.
const MEASURE_INTERVAL: Duration = Duration::from_millis(250);

/// An output stream onto a file that charges the app's data limit for the bytes that grow it.
/// It reads the file's size from the file itself at each write, never from what the guest or an
/// earlier call said, so a file that was truncated or replaced after the stream opened is still
/// charged for what the write adds.
struct QuotaWriter {
    inner: DynOutputStream,
    guard: Arc<FsGuard>,
    file: Arc<std::fs::File>,
    /// Where the next byte lands; `None` appends.
    at: Option<u64>,
    _slot: StreamSlot,
}

impl QuotaWriter {
    fn growth(&self, len: u64) -> u64 {
        match self.at {
            Some(at) => match self.file.metadata() {
                Ok(m) => at.saturating_add(len).saturating_sub(m.len()),
                Err(_) => len,
            },
            None => len,
        }
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for QuotaWriter {
    async fn ready(&mut self) {
        self.inner.ready().await
    }
}

#[wasmtime_wasi::async_trait]
impl OutputStream for QuotaWriter {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        let len = bytes.len() as u64;
        let growth = self.growth(len);
        if !self.guard.charge(growth) {
            return Err(no_space());
        }
        if let Some(at) = self.at.as_mut() {
            *at += len;
        }
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> StreamResult<()> {
        self.inner.flush()
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        self.inner.check_write()
    }

    async fn cancel(&mut self) {
        self.inner.cancel().await
    }
}

/// An input stream on a file that holds one of the app's descriptor slots while it lives.
struct QuotaReader {
    inner: DynInputStream,
    _slot: StreamSlot,
}

#[wasmtime_wasi::async_trait]
impl Pollable for QuotaReader {
    async fn ready(&mut self) {
        self.inner.ready().await
    }
}

#[wasmtime_wasi::async_trait]
impl InputStream for QuotaReader {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        self.inner.read(size)
    }

    async fn blocking_read(&mut self, size: usize) -> StreamResult<Bytes> {
        self.inner.blocking_read(size).await
    }

    fn skip(&mut self, nelem: usize) -> StreamResult<usize> {
        self.inner.skip(nelem)
    }

    async fn blocking_skip(&mut self, nelem: usize) -> StreamResult<usize> {
        self.inner.blocking_skip(nelem).await
    }

    async fn cancel(&mut self) {
        self.inner.cancel().await
    }
}

/// The filesystem interface as one guest sees it: wasmtime-wasi's own view plus the app's books.
pub struct QuotaFs<'a> {
    fs: WasiFilesystemCtxView<'a>,
    guard: Option<Arc<FsGuard>>,
}

pub struct QuotaFsData;

impl HasData for QuotaFsData {
    type Data<'a> = QuotaFs<'a>;
}

fn quota_fs(g: &mut Guarded) -> QuotaFs<'_> {
    let guard = g.fsguard.clone();
    QuotaFs {
        fs: WasiFilesystemView::filesystem(g),
        guard,
    }
}

/// Replace the `wasi:filesystem/types` functions wasmtime-wasi linked with the checked ones.
/// Call after `add_to_linker_async`, with shadowing allowed on `linker`.
pub fn shadow_filesystem(
    linker: &mut wasmtime::component::Linker<Guarded>,
) -> wasmtime::Result<()> {
    types::add_to_linker::<Guarded, QuotaFsData>(linker, quota_fs)
}

fn refused(code: ErrorCode) -> FsError {
    code.into()
}

fn borrowed(fd: &Resource<types::Descriptor>) -> Resource<types::Descriptor> {
    Resource::new_borrow(fd.rep())
}

impl QuotaFs<'_> {
    /// The host file behind a descriptor, if it is a file.
    fn file_of(&mut self, fd: &Resource<types::Descriptor>) -> Option<Arc<std::fs::File>> {
        match self.fs.table.get(fd) {
            Ok(Descriptor::File(f)) => Some(f.file.clone()),
            _ => None,
        }
    }

    /// Put the stream wasmtime-wasi made for a file write behind a [`QuotaWriter`].
    fn guard_stream(
        &mut self,
        stream: Resource<DynOutputStream>,
        guard: Arc<FsGuard>,
        at: Option<u64>,
        file: Arc<std::fs::File>,
        slot: StreamSlot,
    ) -> FsResult<Resource<DynOutputStream>> {
        let inner: DynOutputStream = self.fs.table.delete(stream)?;
        let writer: DynOutputStream = Box::new(QuotaWriter {
            inner,
            guard,
            file,
            at,
            _slot: slot,
        });
        Ok(self.fs.table.push(writer)?)
    }
}

impl types::Host for QuotaFs<'_> {
    fn convert_error_code(&mut self, err: FsError) -> wasmtime::Result<ErrorCode> {
        types::Host::convert_error_code(&mut self.fs, err)
    }

    fn filesystem_error_code(
        &mut self,
        err: Resource<wasmtime::Error>,
    ) -> wasmtime::Result<Option<ErrorCode>> {
        types::Host::filesystem_error_code(&mut self.fs, err)
    }
}

impl HostDirectoryEntryStream for QuotaFs<'_> {
    async fn read_directory_entry(
        &mut self,
        stream: Resource<types::DirectoryEntryStream>,
    ) -> FsResult<Option<types::DirectoryEntry>> {
        HostDirectoryEntryStream::read_directory_entry(&mut self.fs, stream).await
    }

    fn drop(&mut self, stream: Resource<types::DirectoryEntryStream>) -> wasmtime::Result<()> {
        if let Some(g) = &self.guard {
            g.handles.lock().unwrap().listings.remove(&stream.rep());
        }
        HostDirectoryEntryStream::drop(&mut self.fs, stream)
    }
}

impl HostDescriptor for QuotaFs<'_> {
    async fn advise(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
        len: types::Filesize,
        advice: types::Advice,
    ) -> FsResult<()> {
        HostDescriptor::advise(&mut self.fs, fd, offset, len, advice).await
    }

    async fn sync_data(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        HostDescriptor::sync_data(&mut self.fs, fd).await
    }

    async fn get_flags(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorFlags> {
        HostDescriptor::get_flags(&mut self.fs, fd).await
    }

    async fn get_type(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorType> {
        HostDescriptor::get_type(&mut self.fs, fd).await
    }

    async fn set_size(
        &mut self,
        fd: Resource<types::Descriptor>,
        size: types::Filesize,
    ) -> FsResult<()> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::set_size(&mut self.fs, fd, size).await;
        };
        let now = HostDescriptor::stat(&mut self.fs, borrowed(&fd))
            .await?
            .size;
        let growth = size.saturating_sub(now);
        if !g.charge(growth) {
            return Err(refused(ErrorCode::InsufficientSpace));
        }
        HostDescriptor::set_size(&mut self.fs, fd, size).await
    }

    async fn set_times(
        &mut self,
        fd: Resource<types::Descriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> FsResult<()> {
        HostDescriptor::set_times(&mut self.fs, fd, atim, mtim).await
    }

    async fn read(
        &mut self,
        fd: Resource<types::Descriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> FsResult<(Vec<u8>, bool)> {
        HostDescriptor::read(&mut self.fs, fd, len, offset).await
    }

    async fn write(
        &mut self,
        fd: Resource<types::Descriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> FsResult<types::Filesize> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::write(&mut self.fs, fd, buf, offset).await;
        };
        let end = offset.saturating_add(buf.len() as u64);
        let now = HostDescriptor::stat(&mut self.fs, borrowed(&fd))
            .await?
            .size;
        let growth = end.saturating_sub(now);
        if !g.charge(growth) {
            return Err(refused(ErrorCode::InsufficientSpace));
        }
        HostDescriptor::write(&mut self.fs, fd, buf, offset).await
    }

    async fn read_directory(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<types::DirectoryEntryStream>> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::read_directory(&mut self.fs, fd).await;
        };
        if g.entries() > g.caps.max_entries
            || g.handles.lock().unwrap().listings.len() >= g.caps.max_listings
        {
            return Err(refused(ErrorCode::Quota));
        }
        let stream = HostDescriptor::read_directory(&mut self.fs, fd).await?;
        g.handles.lock().unwrap().listings.insert(stream.rep());
        Ok(stream)
    }

    async fn sync(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        HostDescriptor::sync(&mut self.fs, fd).await
    }

    async fn create_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::create_directory_at(&mut self.fs, fd, path).await;
        };
        g.add_entry()?;
        let r = HostDescriptor::create_directory_at(&mut self.fs, fd, path).await;
        if r.is_err() {
            g.undo_entry();
        }
        r
    }

    async fn stat(&mut self, fd: Resource<types::Descriptor>) -> FsResult<types::DescriptorStat> {
        HostDescriptor::stat(&mut self.fs, fd).await
    }

    async fn stat_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::DescriptorStat> {
        HostDescriptor::stat_at(&mut self.fs, fd, path_flags, path).await
    }

    async fn set_times_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> FsResult<()> {
        HostDescriptor::set_times_at(&mut self.fs, fd, path_flags, path, atim, mtim).await
    }

    async fn link_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        old_path_flags: types::PathFlags,
        old_path: String,
        new_descriptor: Resource<types::Descriptor>,
        new_path: String,
    ) -> FsResult<()> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::link_at(
                &mut self.fs,
                fd,
                old_path_flags,
                old_path,
                new_descriptor,
                new_path,
            )
            .await;
        };
        let size = HostDescriptor::stat_at(
            &mut self.fs,
            borrowed(&fd),
            old_path_flags,
            old_path.clone(),
        )
        .await
        .map_or(0, |st| st.size);
        g.add_entry()?;
        if !g.charge(size) {
            g.undo_entry();
            return Err(refused(ErrorCode::InsufficientSpace));
        }
        let r = HostDescriptor::link_at(
            &mut self.fs,
            fd,
            old_path_flags,
            old_path,
            new_descriptor,
            new_path,
        )
        .await;
        if r.is_err() {
            g.undo_entry();
        }
        r
    }

    async fn open_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::open_at(&mut self.fs, fd, path_flags, path, oflags, flags)
                .await;
        };
        {
            let h = g.handles.lock().unwrap();
            if h.open.len() + h.streams >= g.caps.max_open {
                return Err(refused(ErrorCode::Quota));
            }
        }
        let creates = oflags.contains(types::OpenFlags::CREATE)
            && HostDescriptor::stat_at(&mut self.fs, borrowed(&fd), path_flags, path.clone())
                .await
                .is_err();
        if creates {
            g.add_entry()?;
        }
        let r = HostDescriptor::open_at(&mut self.fs, fd, path_flags, path, oflags, flags).await;
        match &r {
            Ok(opened) => {
                g.handles.lock().unwrap().open.insert(opened.rep());
            }
            Err(_) if creates => g.undo_entry(),
            Err(_) => {}
        }
        r
    }

    fn drop(&mut self, fd: Resource<types::Descriptor>) -> wasmtime::Result<()> {
        if let Some(g) = &self.guard {
            g.handles.lock().unwrap().open.remove(&fd.rep());
        }
        HostDescriptor::drop(&mut self.fs, fd)
    }

    async fn readlink_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<String> {
        HostDescriptor::readlink_at(&mut self.fs, fd, path).await
    }

    async fn remove_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        HostDescriptor::remove_directory_at(&mut self.fs, fd, path).await
    }

    async fn rename_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        old_path: String,
        new_fd: Resource<types::Descriptor>,
        new_path: String,
    ) -> FsResult<()> {
        HostDescriptor::rename_at(&mut self.fs, fd, old_path, new_fd, new_path).await
    }

    async fn symlink_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        src_path: String,
        dest_path: String,
    ) -> FsResult<()> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::symlink_at(&mut self.fs, fd, src_path, dest_path).await;
        };
        g.add_entry()?;
        let r = HostDescriptor::symlink_at(&mut self.fs, fd, src_path, dest_path).await;
        if r.is_err() {
            g.undo_entry();
        }
        r
    }

    async fn unlink_file_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        HostDescriptor::unlink_file_at(&mut self.fs, fd, path).await
    }

    fn read_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynInputStream>> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::read_via_stream(&mut self.fs, fd, offset);
        };
        let slot = g.take_slot()?;
        let stream = HostDescriptor::read_via_stream(&mut self.fs, fd, offset)?;
        let inner: DynInputStream = self.fs.table.delete(stream)?;
        let reader: DynInputStream = Box::new(QuotaReader { inner, _slot: slot });
        Ok(self.fs.table.push(reader)?)
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynOutputStream>> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::write_via_stream(&mut self.fs, fd, offset);
        };
        let Some(file) = self.file_of(&fd) else {
            return HostDescriptor::write_via_stream(&mut self.fs, fd, offset);
        };
        let size = file.metadata().ok().map(|m| m.len());
        if size.is_none_or(|size| offset >= size) && !g.room_to_extend() {
            return Err(refused(ErrorCode::InsufficientSpace));
        }
        let slot = g.take_slot()?;
        let stream = HostDescriptor::write_via_stream(&mut self.fs, fd, offset)?;
        self.guard_stream(stream, g, Some(offset), file, slot)
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<DynOutputStream>> {
        let Some(g) = self.guard.clone() else {
            return HostDescriptor::append_via_stream(&mut self.fs, fd);
        };
        let Some(file) = self.file_of(&fd) else {
            return HostDescriptor::append_via_stream(&mut self.fs, fd);
        };
        if !g.room_to_extend() {
            return Err(refused(ErrorCode::InsufficientSpace));
        }
        let slot = g.take_slot()?;
        let stream = HostDescriptor::append_via_stream(&mut self.fs, fd)?;
        self.guard_stream(stream, g, None, file, slot)
    }

    async fn is_same_object(
        &mut self,
        a: Resource<types::Descriptor>,
        b: Resource<types::Descriptor>,
    ) -> wasmtime::Result<bool> {
        HostDescriptor::is_same_object(&mut self.fs, a, b).await
    }

    async fn metadata_hash(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::MetadataHashValue> {
        HostDescriptor::metadata_hash(&mut self.fs, fd).await
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::MetadataHashValue> {
        HostDescriptor::metadata_hash_at(&mut self.fs, fd, path_flags, path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(n: usize) -> tempdir::Dir {
        let d = tempdir::Dir::new();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        for i in 0..n {
            std::fs::write(d.path().join("sub").join(format!("f{i}")), b"x").unwrap();
        }
        d
    }

    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let p = std::env::temp_dir().join(format!(
                    "spared-fsguard-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn measures_entries_and_apparent_bytes_and_stops_early() {
        let d = tree(10);
        let sparse = std::fs::File::create(d.path().join("sparse")).unwrap();
        sparse.set_len(1 << 20).unwrap();
        let u = measure(d.path(), u64::MAX, u64::MAX);
        assert_eq!((u.entries, u.bytes), (12, 10 + (1 << 20)));
        assert!(measure(d.path(), 4, u64::MAX).entries > 4);
        assert!(measure(d.path(), u64::MAX, 100).bytes > 100);
    }

    fn caps(entries: u64, bytes: u64) -> AppCaps {
        AppCaps {
            max_entries: entries,
            max_data_bytes: bytes,
            ..AppCaps::default()
        }
    }

    #[tokio::test]
    async fn bytes_are_charged_until_the_limit_and_come_back_when_the_disk_says_so() {
        let d = tree(0);
        let g = FsGuard::open(d.path().to_path_buf(), caps(100, 1000)).await;
        assert!(g.charge(600));
        assert!(g.charge(400));
        assert!(!g.charge(1), "full");
        assert_eq!(g.used(), 1000, "a refused charge is not counted");
        assert_eq!(g.used(), 1000, "nothing the guest says is a count");
        std::thread::sleep(MEASURE_INTERVAL);
        assert!(
            g.charge(500),
            "but the disk is empty: the next refusal counts it again"
        );
        assert_eq!(g.used(), 500);
        assert!(g.charge(0), "growing by nothing is always allowed");
    }

    #[tokio::test]
    async fn bytes_that_are_still_on_disk_stay_charged_after_the_guest_says_they_left() {
        let d = tree(0);
        std::fs::write(d.path().join("big"), vec![1u8; 1000]).unwrap();
        let g = FsGuard::open(d.path().to_path_buf(), caps(100, 1000)).await;
        assert_eq!(g.used(), 1000);
        std::thread::sleep(MEASURE_INTERVAL);
        assert!(
            !g.charge(1),
            "a rename onto itself, an unlink of a link: the file is still there"
        );
        assert_eq!(g.used(), 1000);
        std::fs::remove_file(d.path().join("big")).unwrap();
        std::thread::sleep(MEASURE_INTERVAL);
        assert!(
            g.charge(1000),
            "once the file is gone the space is free again"
        );
    }

    #[tokio::test]
    async fn a_refusal_checks_the_disk_first() {
        let d = tree(0);
        let g = FsGuard::open(d.path().to_path_buf(), caps(100, 1000)).await;
        assert!(g.charge(1000));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            g.charge(1000),
            "the books said full but the disk is empty: charge counts again before refusing"
        );
        assert_eq!(g.used(), 1000);
    }

    #[tokio::test]
    async fn refusals_at_the_cap_count_the_disk_at_most_four_times_a_second() {
        let d = tree(0);
        std::fs::write(d.path().join("big"), vec![1u8; 1000]).unwrap();
        let g = FsGuard::open(d.path().to_path_buf(), caps(1, 1000)).await;
        let start = Instant::now();
        let mut refused = 0u64;
        while start.elapsed() < Duration::from_millis(1100) {
            assert!(!g.charge(1));
            assert_eq!(g.add_entry(), Err(ErrorCode::Quota));
            refused += 1;
        }
        let walks = g.walks.load(Ordering::Relaxed);
        assert!(refused > 1000, "the loop ran hot ({refused} refusals)");
        assert!(
            (3..=5).contains(&walks),
            "{refused} refusals in 1.1 s counted the disk {walks} times"
        );
    }

    #[tokio::test]
    async fn entries_stop_at_the_cap_and_come_back_when_the_disk_says_so() {
        let d = tree(3);
        let caps = caps(6, u64::MAX);
        let g = FsGuard::open(d.path().to_path_buf(), caps).await;
        let make = |n: &str| std::fs::write(d.path().join(n), b"").unwrap();
        assert_eq!(g.entries(), 4);
        assert!(g.add_entry().is_ok());
        make("a");
        assert!(g.add_entry().is_ok());
        make("b");
        assert_eq!(g.add_entry(), Err(ErrorCode::Quota));
        assert_eq!(g.entries(), 6, "a refused entry is not counted");
        std::thread::sleep(MEASURE_INTERVAL);
        assert_eq!(
            g.add_entry(),
            Err(ErrorCode::Quota),
            "an entry the guest says it removed is not taken back while the disk still holds it"
        );
        std::fs::remove_file(d.path().join("b")).unwrap();
        std::thread::sleep(MEASURE_INTERVAL);
        assert!(g.add_entry().is_ok(), "and is once the disk shows it gone");
        g.undo_entry();
        assert_eq!(
            g.entries(),
            5,
            "an entry whose call failed is taken back at once"
        );
    }

    #[tokio::test]
    async fn stream_slots_count_against_the_descriptor_limit_until_dropped() {
        let d = tree(0);
        let g = FsGuard::open(
            d.path().to_path_buf(),
            AppCaps {
                max_open: 3,
                ..AppCaps::default()
            },
        )
        .await;
        g.handles.lock().unwrap().open.insert(7);
        let a = g.take_slot().unwrap();
        let b = g.take_slot().unwrap();
        assert!(
            g.take_slot().is_err(),
            "one descriptor and two streams fill three slots"
        );
        drop(a);
        let c = g.take_slot().unwrap();
        assert!(g.take_slot().is_err());
        drop((b, c));
        assert_eq!(g.handles.lock().unwrap().streams, 0);
    }

    struct Sink(std::fs::File, u64);

    #[wasmtime_wasi::async_trait]
    impl Pollable for Sink {
        async fn ready(&mut self) {}
    }

    #[wasmtime_wasi::async_trait]
    impl OutputStream for Sink {
        fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
            std::os::unix::fs::FileExt::write_all_at(&self.0, &bytes, self.1).unwrap();
            self.1 += bytes.len() as u64;
            Ok(())
        }
        fn flush(&mut self) -> StreamResult<()> {
            Ok(())
        }
        fn check_write(&mut self) -> StreamResult<usize> {
            Ok(1 << 16)
        }
    }

    #[tokio::test]
    async fn a_stream_is_charged_for_the_file_as_it_is_now_not_as_it_was_when_opened() {
        let d = tree(0);
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(d.path().join("f"))
            .unwrap();
        file.set_len(700).unwrap();
        let g = FsGuard::open(d.path().to_path_buf(), caps(100, 1000)).await;
        let slot = g.take_slot().unwrap();
        let mut w = QuotaWriter {
            inner: Box::new(Sink(file.try_clone().unwrap(), 0)),
            guard: g.clone(),
            file: Arc::new(file.try_clone().unwrap()),
            at: Some(0),
            _slot: slot,
        };
        file.set_len(0).unwrap();
        for _ in 0..3 {
            w.write(Bytes::from(vec![0u8; 100])).unwrap();
        }
        assert_eq!(
            g.used(),
            700 + 300,
            "the file was truncated after the stream opened: its growth is counted from zero"
        );
    }

    #[test]
    fn a_directory_swapped_for_a_link_while_measuring_is_not_followed() {
        let outside = tempdir::Dir::new();
        std::fs::write(outside.path().join("secret.txt"), vec![b's'; 5000]).unwrap();
        let d = tempdir::Dir::new();
        std::fs::create_dir(d.path().join("a")).unwrap();
        std::fs::write(d.path().join("a/f"), b"x").unwrap();
        std::fs::create_dir(d.path().join("b")).unwrap();
        std::fs::write(d.path().join("b/g"), b"y").unwrap();
        let (a, target) = (d.path().join("a"), outside.path().to_path_buf());
        let mut done = false;
        crate::dirfd::on_open(move |n| {
            if n == "a" && !done {
                done = true;
                std::fs::remove_dir_all(&a).unwrap();
                std::os::unix::fs::symlink(&target, &a).unwrap();
            }
        });
        let u = measure(d.path(), u64::MAX, u64::MAX);
        assert!(u.bytes < 100, "bytes outside the tree were counted: {u:?}");
    }

    #[test]
    fn a_tree_nested_past_the_walk_depth_counts_as_over_the_entry_limit() {
        let d = tempdir::Dir::new();
        let mut p = d.path().to_path_buf();
        for _ in 0..(crate::dirfd::MAX_WALK_DEPTH + 20) {
            p.push("d");
        }
        std::fs::create_dir_all(&p).unwrap();
        assert!(measure(d.path(), 1000, u64::MAX).entries > 1000);
    }
}
