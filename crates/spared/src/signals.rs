//! SIGTERM and SIGINT as something the server can wait for.
//!
//! The handler only writes the signal number to a pipe (the one thing a signal handler may do
//! safely); the server reads the pipe like any other file descriptor. This also covers the
//! container case: as PID 1 the kernel drops a signal that has no handler, so `docker stop` would
//! wait its full grace period and then kill the process. With a handler installed the signal is
//! delivered.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = sig as u8;
        unsafe {
            libc::write(fd, (&raw const byte).cast(), 1);
        }
    }
}

pub struct Signals {
    read: AsyncFd<OwnedFd>,
}

pub fn name(sig: i32) -> &'static str {
    match sig {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        _ => "a signal",
    }
}

/// Install handlers for SIGTERM and SIGINT. Call once, inside the runtime.
pub fn install() -> io::Result<Signals> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [read.as_raw_fd(), write.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0
            || unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    WRITE_FD.store(write.into_raw_fd(), Ordering::Relaxed);
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        if unsafe { libc::sigaction(sig, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: the descriptor is owned by `read`, which `AsyncFd` takes over and closes on drop.
    let read = unsafe { AsyncFd::register_with_interest(read, Interest::READABLE) }
        .map_err(|e| e.into_parts().1)?;
    Ok(Signals { read })
}

impl Signals {
    /// The number of the next signal received.
    pub async fn next(&self) -> i32 {
        loop {
            let Ok(mut ready) = self.read.readable().await else {
                return std::future::pending().await;
            };
            let mut byte = 0u8;
            let got = ready.try_io(|fd| {
                let n = unsafe { libc::read(fd.as_raw_fd(), (&raw mut byte).cast(), 1) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n)
                }
            });
            if let Ok(Ok(1)) = got {
                return i32::from(byte);
            }
        }
    }
}
