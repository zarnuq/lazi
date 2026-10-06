//! Watching the visible directories with inotify, so outside changes show up without a keypress.

use std::collections::HashMap;
use std::ffi::CString;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

/// Changes that alter a listing: names appearing or disappearing, or the directory itself going.
const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;

const HEADER: usize = mem::size_of::<libc::inotify_event>();

pub struct Watcher {
    fd: OwnedFd,
    /// Watch descriptor to the paths it covers; several when symlinks lead to one directory.
    dirs: HashMap<i32, Vec<PathBuf>>,
}

impl Watcher {
    /// None if inotify isn't available, in which case listings only refresh on keypresses.
    pub fn new() -> Option<Self> {
        // SAFETY: no pointer arguments; a non-negative return is a new fd we own.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        // SAFETY: fd is fresh and owned by nothing else.
        (fd >= 0).then(|| Self { fd: unsafe { OwnedFd::from_raw_fd(fd) }, dirs: HashMap::new() })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Watches exactly `dirs`, dropping watches on anything no longer among them.
    pub fn set(&mut self, dirs: &[PathBuf]) {
        let mut next: HashMap<i32, Vec<PathBuf>> = HashMap::new();
        for dir in dirs {
            let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else { continue };
            // Re-adding a watched directory just returns its descriptor again, and a directory
            // replaced at the same path gets a fresh one.
            // SAFETY: path is a live NUL-terminated string.
            let wd = unsafe { libc::inotify_add_watch(self.fd(), path.as_ptr(), MASK) };
            if wd >= 0 {
                next.entry(wd).or_default().push(dir.clone());
            }
        }
        for &wd in self.dirs.keys() {
            if !next.contains_key(&wd) {
                // SAFETY: no pointer arguments.
                unsafe { libc::inotify_rm_watch(self.fd(), wd) };
            }
        }
        self.dirs = next;
    }

    /// The watched directories that changed since the last call.
    pub fn changed(&mut self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: reads into a live local buffer of the length passed.
            let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut off = 0;
            while off + HEADER <= n as usize {
                // SAFETY: the kernel wrote a whole event header here; the buffer has no alignment
                // guarantee, hence the unaligned read.
                let event: libc::inotify_event = unsafe { ptr::read_unaligned(buf.as_ptr().add(off).cast()) };
                off += HEADER + event.len as usize;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    // Events were lost, so anything may have changed.
                    out.extend(self.dirs.values().flatten().cloned());
                } else if let Some(paths) = self.dirs.get(&event.wd) {
                    out.extend(paths.iter().cloned());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}
