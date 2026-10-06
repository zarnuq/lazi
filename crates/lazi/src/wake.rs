//! Sleeping until something needs attention: a key, a worker thread, a filesystem change, a resize.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

/// An eventfd that worker threads poke to wake the main loop out of `wait`.
#[derive(Clone)]
pub struct Waker(Arc<OwnedFd>);

impl Waker {
    pub fn new() -> io::Result<Self> {
        // SAFETY: no pointer arguments; a non-negative return is a new fd we own.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is fresh and owned by nothing else.
        Ok(Self(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writes the 8 bytes of a live u64, as eventfd requires. It can only fail if the
        // counter would overflow, and then it's already readable.
        unsafe { libc::write(self.fd(), (&raw const one).cast(), 8) };
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Blocks until one of `fds` is readable, or a signal (such as SIGWINCH) interrupts.
pub fn wait(fds: &[RawFd]) -> io::Result<()> {
    let mut polls: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: polls is a live array of the length passed.
    let n = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, -1) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(())
}

/// Reads and discards whatever is waiting on a non-blocking fd. Returns whether there was any.
pub fn drain(fd: RawFd) -> bool {
    let mut buf = [0u8; 64];
    let mut any = false;
    // SAFETY: reads into a live local buffer of the length passed.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {
        any = true;
    }
    any
}

/// The fd crossterm reads keys from, so `wait` sleeps on the same one: stdin if it's a
/// terminal, else /dev/tty. The File, if any, must stay alive as long as the fd is used.
pub fn tty() -> io::Result<(RawFd, Option<File>)> {
    // SAFETY: isatty has no preconditions.
    if unsafe { libc::isatty(0) } == 1 {
        return Ok((0, None));
    }
    let file = File::open("/dev/tty")?;
    Ok((file.as_raw_fd(), Some(file)))
}

/// A socket that turns readable on SIGWINCH, so a resize wakes `wait`. `drain` it after.
pub fn winch() -> io::Result<UnixStream> {
    let (winch, tx) = UnixStream::pair()?;
    winch.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, tx)?;
    Ok(winch)
}
