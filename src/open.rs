//! Choosing and running openers.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};

use crate::config::Opener;

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

/// Runs `opener` on `files` from `cwd`. Returns an error message for the status line, if any.
pub fn run(term: &mut DefaultTerminal, opener: &Opener, files: &[PathBuf], cwd: &Path) -> io::Result<Option<String>> {
    // Same calling convention as yazi: the files are "$@" to the snippet.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(opener.run).arg("sh").args(files).current_dir(cwd);

    if opener.block {
        suspend(term)?;
        let status = cmd.status();
        resume(term)?;
        return Ok(match status {
            Ok(s) if s.success() => None,
            Ok(s) => Some(format!("{}: {s}", opener.desc)),
            Err(e) => Some(format!("{}: {e}", opener.desc)),
        });
    }

    // Detached: own process group so it outlives lazi, and a thread to reap it when it exits.
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn();
    Ok(match child {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
            None
        }
        Err(e) => Some(format!("{}: {e}", opener.desc)),
    })
}

/// Hands the terminal back to a foreground program.
fn suspend(term: &mut DefaultTerminal) -> io::Result<()> {
    execute!(term.backend_mut(), LeaveAlternateScreen, Show)?;
    disable_raw_mode()
}

fn resume(term: &mut DefaultTerminal) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen, Hide)?;
    // Whatever the program left on screen is unknown, so the next frame redraws everything.
    term.clear()
}
