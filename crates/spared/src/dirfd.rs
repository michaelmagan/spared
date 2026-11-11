use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

pub const MAX_WALK_DEPTH: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Other,
}

#[derive(Debug)]
pub struct Entry {
    pub name: OsString,
    pub kind: Kind,
    pub len: u64,
}

pub struct Dir {
    dirp: *mut libc::DIR,
}

unsafe impl Send for Dir {}

impl Drop for Dir {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.dirp) };
    }
}

fn clear_errno() {
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(not(target_os = "linux"))]
    unsafe {
        *libc::__error() = 0;
    }
}

fn cstr(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| Error::from(ErrorKind::InvalidInput))
}

fn cvt(r: libc::c_int) -> Result<libc::c_int> {
    if r < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(r)
    }
}

pub fn is_not_followed(e: &Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EMLINK)
    )
}

impl Dir {
    pub fn open(path: &Path) -> Result<Dir> {
        let c = cstr(path.as_os_str())?;
        let fd = cvt(unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        Self::from_fd(fd)
    }

    fn from_fd(fd: RawFd) -> Result<Dir> {
        let dirp = unsafe { libc::fdopendir(fd) };
        if dirp.is_null() {
            let e = Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(Dir { dirp })
    }

    fn fd(&self) -> RawFd {
        unsafe { libc::dirfd(self.dirp) }
    }

    pub fn open_dir(&self, name: &OsStr) -> Result<Dir> {
        about_to_open(name);
        let c = cstr(name)?;
        let fd = cvt(unsafe {
            libc::openat(
                self.fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        Self::from_fd(fd)
    }

    pub fn open_file(&self, name: &OsStr) -> Result<File> {
        about_to_open(name);
        let c = cstr(name)?;
        let fd = cvt(unsafe {
            libc::openat(
                self.fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        })?;
        let f = unsafe { File::from_raw_fd(fd) };
        if !f.metadata()?.is_file() {
            return Err(Error::from_raw_os_error(libc::ELOOP));
        }
        Ok(f)
    }

    pub fn read_entry(&mut self) -> Result<Option<Entry>> {
        loop {
            clear_errno();
            let ent = unsafe { libc::readdir(self.dirp) };
            if ent.is_null() {
                let e = Error::last_os_error();
                return if e.raw_os_error().unwrap_or(0) == 0 {
                    Ok(None)
                } else {
                    Err(e)
                };
            }
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let name = OsString::from_vec(name.to_vec());
            let c = cstr(&name)?;
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let r =
                unsafe { libc::fstatat(self.fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
            if r < 0 {
                continue;
            }
            let kind = match st.st_mode & libc::S_IFMT {
                libc::S_IFDIR => Kind::Dir,
                libc::S_IFREG => Kind::File,
                _ => Kind::Other,
            };
            return Ok(Some(Entry {
                name,
                kind,
                len: st.st_size as u64,
            }));
        }
    }

    pub fn unlink(&self, name: &OsStr) -> Result<()> {
        cvt(unsafe { libc::unlinkat(self.fd(), cstr(name)?.as_ptr(), 0) }).map(|_| ())
    }

    pub fn remove_dir(&self, name: &OsStr) -> Result<()> {
        cvt(unsafe { libc::unlinkat(self.fd(), cstr(name)?.as_ptr(), libc::AT_REMOVEDIR) })
            .map(|_| ())
    }

    pub fn rename_into(&self, name: &OsStr, to: &Dir, new_name: &OsStr) -> Result<()> {
        cvt(unsafe {
            libc::renameat(
                self.fd(),
                cstr(name)?.as_ptr(),
                to.fd(),
                cstr(new_name)?.as_ptr(),
            )
        })
        .map(|_| ())
    }
}

pub struct Walk {
    pub stopped: bool,
    pub too_deep: bool,
}

pub fn walk(root: &Path, mut visit: impl FnMut(&Entry) -> bool) -> Walk {
    let mut out = Walk {
        stopped: false,
        too_deep: false,
    };
    let Ok(top) = Dir::open(root) else {
        return out;
    };
    let mut stack = vec![top];
    while let Some(dir) = stack.last_mut() {
        let Ok(Some(e)) = dir.read_entry() else {
            stack.pop();
            continue;
        };
        if !visit(&e) {
            out.stopped = true;
            return out;
        }
        if e.kind == Kind::Dir {
            if stack.len() >= MAX_WALK_DEPTH {
                out.too_deep = true;
                continue;
            }
            if let Ok(child) = stack.last().expect("non-empty").open_dir(&e.name) {
                stack.push(child);
            }
        }
    }
    out
}

#[cfg(test)]
type RaceHook = Box<dyn FnMut(&OsStr)>;

#[cfg(test)]
thread_local! {
    static RACE: std::cell::RefCell<Option<RaceHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn on_open(f: impl FnMut(&OsStr) + 'static) {
    RACE.with(|r| *r.borrow_mut() = Some(Box::new(f)));
}

#[cfg(test)]
pub fn about_to_open(name: &OsStr) {
    RACE.with(|r| {
        if let Some(f) = r.borrow_mut().as_mut() {
            f(name);
        }
    });
}

#[cfg(not(test))]
pub fn about_to_open(_name: &OsStr) {}
