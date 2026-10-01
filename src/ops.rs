//! File operations. These run on worker threads and report progress through a callback.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, mem, ptr};

use crate::config;

pub enum Op {
    Copy { srcs: Vec<PathBuf>, dir: PathBuf, force: bool },
    Move { srcs: Vec<PathBuf>, dir: PathBuf, force: bool },
    Trash(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
}

#[derive(Clone, Copy)]
pub struct Progress {
    pub label: &'static str,
    pub done: u64,
    pub total: u64,
    /// `done` and `total` count bytes rather than items.
    pub bytes: bool,
}

impl Op {
    pub fn label(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "copying",
            Op::Move { .. } => "moving",
            Op::Trash(_) => "trashing",
            Op::Delete(_) => "deleting",
        }
    }

    /// Runs to completion, carrying on past failures. Returns one message per failed item.
    pub fn run(self, report: &mut dyn FnMut(Progress)) -> Vec<String> {
        let label = self.label();
        let mut errors = Vec::new();
        let mut fail = |path: &Path, e: io::Error| errors.push(format!("{}: {e}", path.display()));
        match self {
            Op::Copy { srcs, dir, force } => {
                let (bytes, files) = srcs.iter().map(|src| size(src)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                // Count bytes, unless it's all empty files and there are none to count.
                let by_bytes = bytes > 0;
                let total = if by_bytes { bytes } else { files };
                let mut progress = Progress { label, done: 0, total, bytes: by_bytes };
                for src in &srcs {
                    let mut on_bytes = |n| {
                        progress.done += if by_bytes { n } else { 1 };
                        report(progress);
                    };
                    if let Err(e) = paste(src, &dir, force, false, &mut on_bytes) {
                        fail(src, e);
                    }
                }
            }
            Op::Move { srcs, dir, force } => {
                let mut progress = Progress { label, done: 0, total: srcs.len() as u64, bytes: false };
                for src in &srcs {
                    if let Err(e) = paste(src, &dir, force, true, &mut |_| {}) {
                        fail(src, e);
                    }
                    progress.done += 1;
                    report(progress);
                }
            }
            Op::Trash(paths) => {
                let mut progress = Progress { label, done: 0, total: paths.len() as u64, bytes: false };
                for path in &paths {
                    if let Err(e) = trash(path) {
                        fail(path, e);
                    }
                    progress.done += 1;
                    report(progress);
                }
            }
            Op::Delete(paths) => {
                let mut progress = Progress { label, done: 0, total: paths.len() as u64, bytes: false };
                for path in &paths {
                    if let Err(e) = remove(path) {
                        fail(path, e);
                    }
                    progress.done += 1;
                    report(progress);
                }
            }
        }
        errors
    }
}

/// Copies or moves `src` into `dir`. Without `force`, a taken name gets a `_1`-style suffix;
/// with it, whatever is in the way is removed first.
fn paste(src: &Path, dir: &Path, force: bool, mv: bool, on_bytes: &mut dyn FnMut(u64)) -> io::Result<()> {
    let name = src.file_name().ok_or_else(|| io::Error::other("no file name"))?;
    if dir.starts_with(src) {
        return Err(io::Error::other("can't paste a directory into itself"));
    }
    let same_dir = src.parent() == Some(dir);
    if same_dir && (mv || force) {
        // Moving onto itself, or overwriting itself: nothing to do.
        return Ok(());
    }
    let dst = if force { dir.join(name) } else { unique(dir, name) };
    if force && exists(&dst) {
        remove(&dst)?;
    }
    if !mv {
        return copy_tree(src, &dst, on_bytes);
    }
    match fs::rename(src, &dst) {
        Err(e) if e.kind() == ErrorKind::CrossesDevices => {
            copy_tree(src, &dst, on_bytes)?;
            remove(src)
        }
        res => res,
    }
}

/// `dir/name`, or `dir/stem_1.ext`, `_2` and so on if that's taken.
fn unique(dir: &Path, name: &OsStr) -> PathBuf {
    let path = dir.join(name);
    if !exists(&path) {
        return path;
    }
    let stem = Path::new(name).file_stem().unwrap_or(name);
    let ext = Path::new(name).extension();
    (1..)
        .map(|n| {
            let mut candidate = stem.to_owned();
            candidate.push(format!("_{n}"));
            if let Some(ext) = ext {
                candidate.push(".");
                candidate.push(ext);
            }
            dir.join(candidate)
        })
        .find(|path| !exists(path))
        .expect("an unbounded range runs out of names")
}

/// Copies files, directories and symlinks (as symlinks) recursively.
fn copy_tree(src: &Path, dst: &Path, on_bytes: &mut dyn FnMut(u64)) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        symlink(fs::read_link(src)?, dst)
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
        for ent in fs::read_dir(src)? {
            let ent = ent?;
            copy_tree(&ent.path(), &dst.join(ent.file_name()), on_bytes)?;
        }
        // After the contents, in case the permissions make the directory read-only.
        fs::set_permissions(dst, meta.permissions())
    } else if ft.is_file() {
        // copy_file_range under the hood, which reflinks on btrfs/xfs.
        on_bytes(fs::copy(src, dst)?);
        Ok(())
    } else {
        // Reading a FIFO would block forever; devices and sockets can't be copied meaningfully.
        Err(io::Error::other("not a regular file, directory or symlink"))
    }
}

/// Total bytes and count of the regular files under `path`.
fn size(path: &Path) -> (u64, u64) {
    let Ok(meta) = fs::symlink_metadata(path) else { return (0, 0) };
    if meta.is_dir() {
        let entries = fs::read_dir(path).into_iter().flatten().flatten();
        entries.map(|ent| size(&ent.path())).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    } else if meta.is_file() {
        (meta.len(), 1)
    } else {
        (0, 0)
    }
}

fn exists(path: &Path) -> bool {
    // symlink_metadata, so a broken symlink still counts as taking the name.
    fs::symlink_metadata(path).is_ok()
}

/// Removes a file, symlink or whole directory tree, never following symlinks.
fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}

/// Moves `path` to the home trash per the freedesktop.org trash spec. Things on another
/// filesystem go through the configured fallback (e.g. `gio trash`), which knows about
/// per-mount trash directories.
fn trash(path: &Path) -> io::Result<()> {
    let name = path.file_name().ok_or_else(|| io::Error::other("no file name"))?;
    let trash = data_home().join("Trash");
    let (files, info) = (trash.join("files"), trash.join("info"));
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(&files)?;
    builder.create(&info)?;

    let (info_path, trashed) = reserve(&files, &info, name, path)?;
    match fs::rename(path, &trashed) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&info_path);
            if e.kind() == ErrorKind::CrossesDevices { trash_fallback(path).unwrap_or(Err(e)) } else { Err(e) }
        }
    }
}

/// Claims a free name in the trash by creating its .trashinfo exclusively.
fn reserve(files: &Path, info: &Path, name: &OsStr, original: &Path) -> io::Result<(PathBuf, PathBuf)> {
    for n in 0.. {
        let mut candidate: OsString = name.to_owned();
        if n > 0 {
            candidate.push(format!("_{n}"));
        }
        let mut info_name = candidate.clone();
        info_name.push(".trashinfo");
        let info_path = info.join(info_name);
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&info_path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let trashed = files.join(&candidate);
        if exists(&trashed) {
            // A leftover with no info file; don't clobber it.
            drop(file);
            fs::remove_file(&info_path)?;
            continue;
        }
        let contents = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", url_encode(original), local_now());
        if let Err(e) = file.write_all(contents.as_bytes()) {
            let _ = fs::remove_file(&info_path);
            return Err(e);
        }
        return Ok((info_path, trashed));
    }
    unreachable!()
}

/// None if no fallback is configured.
fn trash_fallback(path: &Path) -> Option<io::Result<()>> {
    let (prog, args) = config::get().trash_fallback.split_first()?;
    let out = match Command::new(prog).args(args).arg(path).stdin(Stdio::null()).output() {
        Ok(out) => out,
        Err(e) => return Some(Err(io::Error::new(e.kind(), format!("{prog}: {e}")))),
    };
    Some(if out.status.success() { Ok(()) } else { Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned())) })
}

fn data_home() -> PathBuf {
    match env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/share"),
    }
}

/// Percent-encodes everything but unreserved characters and `/`, as .trashinfo wants.
fn url_encode(path: &Path) -> String {
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Local time as YYYY-MM-DDThh:mm:ss.
fn local_now() -> String {
    // SAFETY: time() with a null pointer only returns; localtime_r writes into our zeroed tm.
    let tm = unsafe {
        let t = libc::time(ptr::null_mut());
        let mut tm: libc::tm = mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}
