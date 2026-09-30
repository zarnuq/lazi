//! File previews, built on a worker thread that only ever works on the latest request.

use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Instant, SystemTime};

use image::DynamicImage;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::{IMAGE_MAX, PREVIEW_TIMEOUT, Previewer, TAB_SIZE, previewer};
use crate::open;

/// Text previews stop after this much input, or this many lines.
const MAX_BYTES: u64 = 256 * 1024;
/// A previewer's image output (a PNG, say) is cut off here.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINES: usize = 2000;

/// What to preview, and what it has to fit. Equal requests give equal previews.
#[derive(Clone, PartialEq)]
pub struct Request {
    pub path: PathBuf,
    /// So an edited file gets previewed again.
    pub mtime: Option<SystemTime>,
    /// The preview area in cells, and in pixels for images.
    pub cols: u16,
    pub rows: u16,
    pub px: (u32, u32),
}

pub enum Preview {
    Text(Vec<Line<'static>>),
    Image(Image),
    /// Why there's nothing to show.
    Note(String),
}

pub struct Image {
    /// Unique per decoded image, so the terminal side can tell when to transmit a new one.
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

type Mailbox = Arc<(Mutex<Option<Request>>, Condvar)>;

/// A worker thread holding at most one pending request: a newer one replaces it, so scrolling
/// past a hundred photos decodes only the one you stop on.
pub struct Worker {
    mailbox: Mailbox,
}

impl Worker {
    pub fn new(done: impl Fn(Request, Preview) + Send + 'static) -> Self {
        let mailbox: Mailbox = Arc::default();
        let inbox = mailbox.clone();
        thread::spawn(move || {
            loop {
                let req = {
                    let (lock, ready) = &*inbox;
                    let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(req) = slot.take() {
                            break req;
                        }
                        slot = ready.wait(slot).unwrap_or_else(|e| e.into_inner());
                    }
                };
                // A decoder choking on a corrupt file shouldn't take lazi down with it.
                let preview = panic::catch_unwind(AssertUnwindSafe(|| build(&req)))
                    .unwrap_or_else(|_| Preview::Note("previewer crashed".into()));
                done(req, preview);
            }
        });
        Self { mailbox }
    }

    pub fn request(&self, req: Request) {
        let (lock, ready) = &*self.mailbox;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(req);
        ready.notify_one();
    }
}

fn build(req: &Request) -> Preview {
    let kind = open::kind(&req.path, false);
    let res = match previewer(kind) {
        Previewer::Text => text(&req.path),
        Previewer::Image => image(req),
        Previewer::ImageCmd(script) => image_cmd(req, script),
        Previewer::Cmd(script) => run(script, req, MAX_BYTES).map(|out| Preview::Text(lines(&out))),
    };
    res.unwrap_or_else(Preview::Note)
}

fn text(path: &Path) -> Result<Preview, String> {
    let mut buf = Vec::new();
    File::open(path).and_then(|f| f.take(MAX_BYTES).read_to_end(&mut buf)).map_err(|e| e.to_string())?;
    Ok(Preview::Text(lines(&buf)))
}

fn image(req: &Request) -> Result<Preview, String> {
    let decoded = image::ImageReader::open(&req.path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())
        .and_then(|r| r.decode().map_err(|e| e.to_string()));
    match decoded {
        Ok(img) => Ok(fit(img, req)),
        // Formats the image crate doesn't do (avif, heic, jxl, ...) may still get through ffmpeg.
        Err(e) => image_cmd(req, r#"ffmpeg -v error -i "$1" -frames:v 1 -f image2pipe -c:v png -"#).map_err(|_| e),
    }
}

fn image_cmd(req: &Request, script: &str) -> Result<Preview, String> {
    let out = run(script, req, MAX_IMAGE_BYTES)?;
    let img = image::load_from_memory(&out).map_err(|e| e.to_string())?;
    Ok(fit(img, req))
}

/// Shrinks to fit the preview area (never enlarges), capped at IMAGE_MAX.
fn fit(img: DynamicImage, req: &Request) -> Preview {
    let max_w = req.px.0.min(IMAGE_MAX.0);
    let max_h = req.px.1.min(IMAGE_MAX.1);
    let img = if img.width() > max_w || img.height() > max_h { img.thumbnail(max_w, max_h) } else { img };
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    let (width, height) = (img.width(), img.height());
    Preview::Image(Image { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), width, height, rgba: img.into_rgba8().into_raw() })
}

/// Runs a previewer script with the file as $1 and the area in $COLUMNS/$LINES, returning at
/// most `limit` bytes of its stdout. It's killed if it takes longer than PREVIEW_TIMEOUT.
fn run(script: &str, req: &Request, limit: u64) -> Result<Vec<u8>, String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(&req.path)
        .env("COLUMNS", req.cols.to_string())
        .env("LINES", req.rows.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let watchdog = Watchdog::start(&child);
    let mut out = Vec::new();
    let read = child.stdout.take().expect("stdout is piped").take(limit).read_to_end(&mut out);
    // Enough output, or it's gone quiet: either way we're done with it.
    watchdog.kill();
    let status = child.wait().map_err(|e| e.to_string())?;
    let mut err = String::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(4096).read_to_string(&mut err);
    }
    read.map_err(|e| e.to_string())?;
    if out.is_empty() {
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim);
        return Err(last.map_or_else(|| format!("no output ({status})"), str::to_owned));
    }
    Ok(out)
}

/// Kills a previewer's process group if it runs past PREVIEW_TIMEOUT, or when told to.
struct Watchdog {
    pgid: i32,
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl Watchdog {
    fn start(child: &Child) -> Self {
        let pgid = child.id() as i32;
        let done: Arc<(Mutex<bool>, Condvar)> = Arc::default();
        let flag = done.clone();
        thread::spawn(move || {
            let (lock, cvar) = &*flag;
            let deadline = Instant::now() + PREVIEW_TIMEOUT;
            let mut finished = lock.lock().unwrap_or_else(|e| e.into_inner());
            while !*finished {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    kill_group(pgid);
                    return;
                }
                finished = cvar.wait_timeout(finished, left).unwrap_or_else(|e| e.into_inner()).0;
            }
        });
        Self { pgid, done }
    }

    fn kill(self) {
        kill_group(self.pgid);
        let (lock, cvar) = &*self.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_one();
    }
}

fn kill_group(pgid: i32) {
    // SAFETY: no pointer arguments. The group is the previewer's own (process_group(0)), and it
    // hasn't been reaped yet, so the id can't have been reused.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

/// Splits into lines, expanding tabs and applying ANSI colours (SGR); other escapes are dropped.
fn lines(bytes: &[u8]) -> Vec<Line<'static>> {
    let text = String::from_utf8_lossy(bytes);
    let mut style = Style::new();
    let mut out = Vec::new();
    for raw in text.lines().take(MAX_LINES) {
        let mut spans = Vec::new();
        let mut buf = String::new();
        let mut col = 0;
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => {
                    if chars.next_if_eq(&'[').is_none() {
                        chars.next();
                        continue;
                    }
                    let mut params = String::new();
                    let mut end = None;
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            end = Some(c);
                            break;
                        }
                        params.push(c);
                    }
                    if end == Some('m') {
                        if !buf.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut buf), style));
                        }
                        style = sgr(style, &params);
                    }
                }
                '\t' => {
                    let n = TAB_SIZE - col % TAB_SIZE;
                    buf.extend(std::iter::repeat_n(' ', n));
                    col += n;
                }
                c if c.is_control() => {}
                c => {
                    buf.push(c);
                    col += 1;
                }
            }
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Applies one SGR parameter list (the part between `ESC[` and `m`).
fn sgr(mut style: Style, params: &str) -> Style {
    let nums: Vec<u16> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => style = Style::new(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            n @ 30..=37 => style = style.fg(Color::Indexed((n - 30) as u8)),
            n @ 90..=97 => style = style.fg(Color::Indexed((n - 90 + 8) as u8)),
            n @ 40..=47 => style = style.bg(Color::Indexed((n - 40) as u8)),
            n @ 100..=107 => style = style.bg(Color::Indexed((n - 100 + 8) as u8)),
            39 => style = style.fg(Color::Reset),
            49 => style = style.bg(Color::Reset),
            n @ (38 | 48) => {
                let color = match nums.get(i + 1) {
                    Some(5) => {
                        i += 2;
                        nums.get(i).map(|&c| Color::Indexed(c as u8))
                    }
                    Some(2) => {
                        i += 4;
                        match nums.get(i - 2..=i) {
                            Some(&[r, g, b]) => Some(Color::Rgb(r as u8, g as u8, b as u8)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(color) = color {
                    style = if n == 38 { style.fg(color) } else { style.bg(color) };
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}
