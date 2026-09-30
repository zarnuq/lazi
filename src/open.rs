//! Choosing and running external programs.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};

use crate::config::CLIPBOARD;

#[derive(Clone, Copy)]
pub enum Kind {
    Dir,
    Image,
    Pdf,
    Media,
    Archive,
    Text,
    Other,
}

/// Classifies by extension, falling back to sniffing the first bytes for a NUL.
pub fn kind(path: &Path, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Dir;
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "svg" | "avif" | "heic" | "ico" | "jxl") => Kind::Image,
        Some("pdf" | "epub" | "djvu") => Kind::Pdf,
        Some(
            "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "flv" | "wmv" | "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a"
            | "aac" | "wma",
        ) => Kind::Media,
        Some("zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "txz" | "zst" | "7z" | "rar" | "iso" | "cpio" | "lz4") => {
            Kind::Archive
        }
        _ if looks_like_text(path) => Kind::Text,
        _ => Kind::Other,
    }
}

fn looks_like_text(path: &Path) -> bool {
    // Only regular files: opening a FIFO would block.
    if !fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = [0; 1024];
    let Ok(n) = File::open(path).and_then(|mut f| f.read(&mut head)) else { return false };
    !head[..n].contains(&0)
}

/// A `sh -c` snippet to run.
pub struct Cmd<'a> {
    /// Names the command in error messages.
    pub desc: &'a str,
    pub script: &'a str,
    /// `$@` for the script.
    pub args: &'a [PathBuf],
    /// Hand the terminal over and wait, instead of detaching.
    pub block: bool,
}

/// Runs `cmd` from `cwd`. A blocking command's failure is returned for the status line; a
/// detached one's is passed to `on_fail` if and when it happens.
pub fn run(
    term: &mut DefaultTerminal,
    cmd: &Cmd,
    cwd: &Path,
    on_fail: Box<dyn FnOnce(String) + Send>,
) -> io::Result<Option<String>> {
    let mut command = Command::new("sh");
    // Same calling convention as yazi: the files are "$@" to the snippet.
    command.arg("-c").arg(cmd.script).arg("sh").args(cmd.args).current_dir(cwd);

    if cmd.block {
        suspend(term)?;
        let status = command.status();
        resume(term)?;
        return Ok(match status {
            Ok(s) if s.success() => None,
            Ok(s) => Some(format!("{}: {s}", cmd.desc)),
            Err(e) => Some(format!("{}: {e}", cmd.desc)),
        });
    }

    // Detached: own process group so it outlives lazi. Stderr goes to an anonymous in-memory
    // file rather than a pipe, so nothing blocks on a full pipe or dies of SIGPIPE once lazi
    // exits; it's read back only if the command fails.
    let log = stderr_log();
    let stderr = match &log {
        Some(log) => Stdio::from(log.try_clone()?),
        None => Stdio::null(),
    };
    let child = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).process_group(0).spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return Ok(Some(format!("{}: {e}", cmd.desc))),
    };
    let desc = cmd.desc.to_owned();
    thread::spawn(move || {
        let Ok(status) = child.wait() else { return };
        if status.success() {
            return;
        }
        let mut text = String::new();
        if let Some(mut log) = log {
            let _ = log.rewind().and_then(|()| log.read_to_string(&mut text));
        }
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim);
        on_fail(format!("{desc}: {}", last.map_or_else(|| status.to_string(), str::to_owned)));
    });
    Ok(None)
}

fn stderr_log() -> Option<File> {
    // SAFETY: memfd_create takes a NUL-terminated name; a non-negative return is a fresh fd we own.
    let fd = unsafe { libc::memfd_create(c"lazi-stderr".as_ptr(), libc::MFD_CLOEXEC) };
    (fd >= 0).then(|| unsafe { File::from_raw_fd(fd) })
}

/// Runs a picker like fzf with the terminal handed over, returning what it printed.
/// None if it was cancelled or printed nothing.
pub fn capture(term: &mut DefaultTerminal, script: &str, cwd: &Path) -> io::Result<Option<String>> {
    suspend(term)?;
    // output() would give it an empty stdin, which fzf reads as its (empty) list of choices.
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output();
    resume(term)?;
    let out = out?;
    let text = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_owned();
    Ok((out.status.success() && !text.is_empty()).then_some(text))
}

pub fn clipboard(text: &str) -> io::Result<()> {
    let (prog, args) = CLIPBOARD.split_first().expect("CLIPBOARD names a command");
    let mut child = Command::new(prog).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    child.stdin.take().expect("stdin is piped").write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("{prog}: {status}"))) }
}

/// Hands the terminal back to a foreground program.
pub fn suspend(term: &mut DefaultTerminal) -> io::Result<()> {
    execute!(term.backend_mut(), LeaveAlternateScreen, Show)?;
    disable_raw_mode()
}

pub fn resume(term: &mut DefaultTerminal) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen, Hide)?;
    // Whatever the program left on screen is unknown, so the next frame redraws everything.
    term.clear()
}
