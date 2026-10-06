//! The catch-all search popup: one query, matched as an exact smart-case regex against repos,
//! recently visited folders, file names and file contents, shown grouped like Obsidian's search.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::{env, mem, thread};

use fancy_regex::Regex;
use lazi::wake::{self, Waker};
use lazi::{Key, Lookup};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

use crate::config::{SearchAction, SearchSpec};

/// Which sources a query searches; a prefix narrows it to one.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Scope {
    All,
    Repos,
    Dirs,
    Files,
    Content,
}

/// Takes a `repo:`, `dir:`, `file:` or `content:` prefix off the query, as Obsidian does.
pub fn split(query: &str) -> (Scope, &str) {
    for (prefix, scope) in [("repo:", Scope::Repos), ("dir:", Scope::Dirs), ("file:", Scope::Files), ("content:", Scope::Content)] {
        if let Some(rest) = query.strip_prefix(prefix) {
            return (scope, rest);
        }
    }
    (Scope::All, query)
}

/// The query as a regex: case-insensitive unless it has a capital, like lazi's find and rg's
/// --smart-case. None when empty, since an empty query would match everything.
pub fn compile(pattern: &str) -> Result<Option<Regex>, String> {
    if pattern.is_empty() {
        return Ok(None);
    }
    let pattern = if smart_case_ignores(pattern) { format!("(?i){pattern}") } else { pattern.to_owned() };
    Regex::new(&pattern).map(Some).map_err(|e| e.to_string())
}

/// Whether a pattern is all lowercase, so case shouldn't matter.
pub fn smart_case_ignores(pattern: &str) -> bool {
    !pattern.chars().any(char::is_uppercase)
}

/// One line of `rg --null --line-number --no-heading`: the path, a NUL (so a colon in the path
/// can't confuse it), then `line:text`.
pub fn parse_rg(line: &[u8]) -> Option<(PathBuf, u64, String)> {
    let nul = line.iter().position(|&b| b == 0)?;
    let (path, rest) = (&line[..nul], String::from_utf8_lossy(&line[nul + 1..]).into_owned());
    let (number, text) = rest.split_once(':')?;
    Some((PathBuf::from(OsStr::from_bytes(path)), number.parse().ok()?, text.to_owned()))
}

/// One result row's subject.
#[derive(Debug, PartialEq, Clone)]
pub enum Hit {
    Repo(PathBuf),
    Dir(PathBuf),
    /// A file whose name matched.
    File(PathBuf),
    /// A file with matching lines, heading them.
    Text(PathBuf),
    /// A matching line: file, line number, the line.
    Line(PathBuf, u64, String),
}

/// A row of the result list: a section header, or the hit at that index.
#[derive(Debug, PartialEq)]
pub enum Row {
    Section(&'static str),
    Hit(usize),
}

/// The list as drawn: a header wherever the kind of hit changes. `hits` comes grouped by kind.
pub fn rows<'a>(hits: impl IntoIterator<Item = &'a Hit>) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut last = None;
    for (i, hit) in hits.into_iter().enumerate() {
        let section = hit.section();
        if last != Some(section) {
            rows.push(Row::Section(section));
            last = Some(section);
        }
        rows.push(Row::Hit(i));
    }
    rows
}

impl Hit {
    fn section(&self) -> &'static str {
        match self {
            Hit::Repo(_) => "Repos",
            Hit::Dir(_) => "Folders",
            Hit::File(_) => "Files",
            Hit::Text(_) | Hit::Line(..) => "Content",
        }
    }
}

/// What the popup asks shop to do after a key.
pub enum Done {
    Stay,
    Close,
    /// Open the hit: a file in the editor, a folder or repo in lazi.
    Open(Hit),
    /// Show the hit in lazi instead.
    Reveal(Hit),
}

/// What the fd and rg threads send back, tagged with the query they ran for.
enum Msg {
    /// Every file under the root, relative to it.
    Files(Vec<PathBuf>),
    /// File names that matched query `u64`.
    Names(u64, Vec<Hit>),
    /// More file contents that matched query `u64`, a file's lines at a time, so the first
    /// show up while rg is still going through a big tree.
    Content(u64, Vec<Hit>),
}

/// At most this many file names and content lines per query; a broad query stays quick.
const NAMES: usize = 200;
const LINES: usize = 500;
/// Preview reads no further into a file than this.
const PREVIEW_BYTES: u64 = 64 * 1024;

pub struct Search {
    spec: SearchSpec,
    /// Where file names and contents are searched; result paths are relative to it.
    pub root: PathBuf,
    query: String,
    repos: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    /// From fd, once it's done.
    files: Option<Arc<Vec<PathBuf>>>,
    repo_hits: Vec<Hit>,
    dir_hits: Vec<Hit>,
    name_hits: Vec<Hit>,
    content_hits: Vec<Hit>,
    error: Option<String>,
    /// The current query's regex and scope, for filtering the file list when fd finishes.
    regex: Option<(Regex, Scope)>,
    /// Index into the hits, in screen order.
    cursor: usize,
    offset: usize,
    pending: Vec<Key>,
    /// Bumped per query; threads running an older one stop, and what they send is dropped.
    generation: Arc<AtomicU64>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    waker: Waker,
}

impl Search {
    /// Opens on `root`, listing its files in the background.
    pub fn open(spec: SearchSpec, root: PathBuf, repos: Vec<PathBuf>, dirs: Vec<PathBuf>) -> io::Result<Self> {
        let waker = Waker::new()?;
        let (tx, rx) = mpsc::channel();
        let search = Self {
            spec,
            root,
            query: String::new(),
            repos,
            dirs,
            files: None,
            repo_hits: Vec::new(),
            dir_hits: Vec::new(),
            name_hits: Vec::new(),
            content_hits: Vec::new(),
            error: None,
            regex: None,
            cursor: 0,
            offset: 0,
            pending: Vec::new(),
            generation: Arc::new(AtomicU64::new(0)),
            tx,
            rx,
            waker,
        };
        let (root, tx, waker) = (search.root.clone(), search.tx.clone(), search.waker.clone());
        thread::spawn(move || {
            // fd skips hidden and .gitignored files, which is what a search wants anyway.
            let out = Command::new("fd").args(["--type", "f", "--color", "never"]).current_dir(&root).stdin(Stdio::null()).stderr(Stdio::null()).output();
            let files = out.map(|o| o.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()).map(|l| PathBuf::from(OsStr::from_bytes(l))).collect()).unwrap_or_default();
            if tx.send(Msg::Files(files)).is_ok() {
                waker.wake();
            }
        });
        Ok(search)
    }

    pub fn wake_fd(&self) -> RawFd {
        self.waker.fd()
    }

    /// Takes in what the threads found. Returns whether anything changed.
    pub fn receive(&mut self) -> bool {
        wake::drain(self.waker.fd());
        let current = self.generation.load(Ordering::Relaxed);
        let mut any = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Files(files) => {
                    self.files = Some(Arc::new(files));
                    // A query typed before the list was ready never saw the files. Only the
                    // names: restarting the whole query would throw away rg's progress.
                    if let Some((regex, scope)) = &self.regex
                        && matches!(scope, Scope::All | Scope::Files)
                    {
                        self.names(current, regex.clone());
                    }
                }
                Msg::Names(n, hits) if n == current => self.name_hits = hits,
                Msg::Content(n, hits) if n == current => self.content_hits.extend(hits),
                _ => continue,
            }
            any = true;
        }
        any
    }

    pub fn key(&mut self, key: Key) -> Done {
        self.pending.push(key);
        let action = match lazi::lookup(&self.spec.keys, &self.pending) {
            Lookup::Pending => return Done::Stay,
            Lookup::Action(action) => action.clone(),
            Lookup::Unbound => {
                self.pending.clear();
                // Unbound printable keys type, as in lazi's prompts.
                if let (KeyCode::Char(c), mods) = key
                    && !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    self.query.push(c);
                    self.changed();
                }
                return Done::Stay;
            }
        };
        self.pending.clear();
        let count = self.hits().count();
        match action {
            SearchAction::Down => self.cursor = (self.cursor + 1).min(count.saturating_sub(1)),
            SearchAction::Up => self.cursor = self.cursor.saturating_sub(1),
            SearchAction::Close => return Done::Close,
            SearchAction::Open | SearchAction::Reveal => {
                let Some(hit) = self.hits().nth(self.cursor) else { return Done::Stay };
                let hit = hit.clone();
                return if matches!(action, SearchAction::Open) { Done::Open(hit) } else { Done::Reveal(hit) };
            }
            SearchAction::DeleteChar => {
                self.query.pop();
                self.changed();
            }
            SearchAction::DeleteWord => {
                let kept = self.query.trim_end().trim_end_matches(|c: char| !c.is_whitespace() && c != '/').len();
                self.query.truncate(kept);
                self.changed();
            }
            SearchAction::Clear => {
                self.query.clear();
                self.changed();
            }
        }
        Done::Stay
    }

    /// Every hit, in screen order.
    fn hits(&self) -> impl Iterator<Item = &Hit> {
        self.repo_hits.iter().chain(&self.dir_hits).chain(&self.name_hits).chain(&self.content_hits)
    }

    /// Runs the query again: repos and folders here (a few hundred paths), file names and
    /// contents on a thread.
    fn changed(&mut self) {
        let n = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.cursor = 0;
        self.offset = 0;
        let (scope, pattern) = split(&self.query);
        let regex = match compile(pattern) {
            Ok(regex) => {
                self.error = None;
                regex
            }
            Err(e) => {
                // Keep showing the last results while the regex is half typed.
                self.error = Some(e);
                return;
            }
        };
        self.regex = regex.clone().map(|r| (r, scope));
        let Some(regex) = regex else {
            self.repo_hits.clear();
            self.dir_hits.clear();
            self.name_hits.clear();
            self.content_hits.clear();
            return;
        };
        let wants = |s: Scope| scope == Scope::All || scope == s;
        let matching = |paths: &[PathBuf]| paths.iter().filter(|p| regex.is_match(&p.to_string_lossy()).unwrap_or(false)).cloned().collect::<Vec<_>>();
        self.repo_hits = if wants(Scope::Repos) { matching(&self.repos).into_iter().map(Hit::Repo).collect() } else { Vec::new() };
        self.dir_hits = if wants(Scope::Dirs) { matching(&self.dirs).into_iter().map(Hit::Dir).collect() } else { Vec::new() };
        self.name_hits.clear();
        self.content_hits.clear();
        if wants(Scope::Files) {
            self.names(n, regex);
        }
        if wants(Scope::Content) {
            let (root, tx, waker, generation) = (self.root.clone(), self.tx.clone(), self.waker.clone(), self.generation.clone());
            let pattern = pattern.to_owned();
            thread::spawn(move || {
                grep(&root, &pattern, &generation, n, |hits| {
                    if tx.send(Msg::Content(n, hits)).is_ok() {
                        waker.wake();
                    }
                });
            });
        }
    }

    /// Filters the file list for query `n` on a thread: it can be long.
    fn names(&self, n: u64, regex: Regex) {
        let Some(files) = self.files.clone() else { return };
        let (tx, waker) = (self.tx.clone(), self.waker.clone());
        thread::spawn(move || {
            let hits = files.iter().filter(|p| regex.is_match(&p.to_string_lossy()).unwrap_or(false)).take(NAMES).cloned().map(Hit::File).collect();
            if tx.send(Msg::Names(n, hits)).is_ok() {
                waker.wake();
            }
        });
    }

    pub fn draw(&mut self, buf: &mut Buffer, area: Rect) {
        // A margin round the box, so it reads as a popup over the tab.
        let area = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) };
        if area.width < 20 || area.height < 5 {
            return;
        }
        Clear.render(area, buf);
        let title = match &self.error {
            Some(e) => format!(" search: {} ", e.lines().last().unwrap_or(e)),
            None => format!(" search · {} results · repo: dir: file: content: ", self.hits().count()),
        };
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        block.render(area, buf);
        buf.set_stringn(inner.x, inner.y, format!("> {}", self.query), inner.width as usize, Style::new());
        let body = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
        let left = Rect { width: body.width * 45 / 100, ..body };
        let right = Rect { x: left.right() + 1, width: body.right().saturating_sub(left.right() + 1), ..body };
        for y in body.top()..body.bottom() {
            buf.set_string(left.right(), y, "│", Style::new());
        }

        let rows = rows(self.hits());
        let cursor_row = rows.iter().position(|r| *r == Row::Hit(self.cursor)).unwrap_or(0);
        let height = left.height as usize;
        if cursor_row < self.offset {
            // Keep the section header above the first hit in view.
            self.offset = cursor_row.saturating_sub(1);
        } else if cursor_row >= self.offset + height {
            self.offset = cursor_row + 1 - height;
        }
        let hits: Vec<&Hit> = self.hits().collect();
        let style = &self.spec.style;
        for (i, row) in rows.iter().enumerate().skip(self.offset).take(height) {
            let y = left.y + (i - self.offset) as u16;
            let line = match row {
                Row::Section(name) => Line::from(Span::styled(*name, style.section)),
                Row::Hit(h) => self.label(hits[*h]),
            };
            buf.set_line(left.x, y, &line, left.width);
            if *row == Row::Hit(self.cursor) {
                buf.set_style(Rect { y, height: 1, ..left }, style.cursor);
            }
        }
        if let Some(hit) = hits.get(self.cursor) {
            for (i, line) in self.preview(hit, right.height as usize).into_iter().enumerate() {
                buf.set_line(right.x, right.y + i as u16, &line, right.width);
            }
        }
    }

    fn label(&self, hit: &Hit) -> Line<'static> {
        match hit {
            Hit::Repo(p) | Hit::Dir(p) => Line::from(format!("  {}", tilde(p))),
            Hit::File(p) | Hit::Text(p) => Line::from(format!("  {}", p.display())),
            Hit::Line(_, n, text) => {
                Line::from(vec![Span::styled(format!("    {n}: "), self.spec.style.line_number), Span::raw(text.trim().to_owned())])
            }
        }
    }

    /// The right half: a folder's entries, or a file's text from the top or round the
    /// matching line, which is highlighted.
    fn preview(&self, hit: &Hit, height: usize) -> Vec<Line<'static>> {
        let (path, line) = match hit {
            Hit::Repo(p) | Hit::Dir(p) => return listing(p, height),
            Hit::File(p) | Hit::Text(p) => (self.root.join(p), None),
            Hit::Line(p, n, _) => (self.root.join(p), Some(*n)),
        };
        let mut bytes = Vec::new();
        if let Err(e) = File::open(&path).and_then(|f| f.take(PREVIEW_BYTES).read_to_end(&mut bytes)) {
            return vec![Line::from(e.to_string())];
        }
        if bytes.contains(&0) {
            return vec![Line::from("binary")];
        }
        let text = String::from_utf8_lossy(&bytes);
        let first = line.map_or(1, |n| n.saturating_sub(height as u64 / 2).max(1));
        text.lines()
            .enumerate()
            .skip(first as usize - 1)
            .take(height)
            .map(|(i, l)| {
                let n = i as u64 + 1;
                let number = Span::styled(format!("{n:>5} "), self.spec.style.line_number);
                let text = Span::raw(l.replace('\t', "    "));
                if Some(n) == line { Line::from(vec![number, text]).style(self.spec.style.cursor) } else { Line::from(vec![number, text]) }
            })
            .collect()
    }
}

/// The matching lines under `root` from rg, handed to `found` a file at a time (its heading,
/// then its lines); stops early, killing rg, at the cap or when a newer query has started.
fn grep(root: &Path, pattern: &str, generation: &AtomicU64, n: u64, mut found: impl FnMut(Vec<Hit>)) {
    let case = if smart_case_ignores(pattern) { "--ignore-case" } else { "--case-sensitive" };
    let child = Command::new("rg")
        .args(["--null", "--line-number", "--no-heading", "--color", "never", "--max-columns", "300", case, "-e", pattern])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    let mut hits: Vec<Hit> = Vec::new();
    let mut lines = 0;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).split(b'\n') {
            if lines >= LINES || generation.load(Ordering::Relaxed) != n {
                break;
            }
            let Some((path, number, text)) = line.ok().as_deref().and_then(parse_rg) else { continue };
            // rg prints a file's lines together, so a new file closes the last one's group.
            if !matches!(hits.last(), Some(Hit::Line(p, ..)) if *p == path) {
                if !hits.is_empty() {
                    found(mem::take(&mut hits));
                }
                hits.push(Hit::Text(path.clone()));
            }
            hits.push(Hit::Line(path, number, text));
            lines += 1;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    if !hits.is_empty() && generation.load(Ordering::Relaxed) == n {
        found(hits);
    }
}

/// A folder's entries for the preview, directories marked with a slash.
fn listing(dir: &Path, height: usize) -> Vec<Line<'static>> {
    let Ok(entries) = fs::read_dir(dir) else { return vec![Line::from("can't read this folder")] };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) { format!("{name}/") } else { name }
        })
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.into_iter().take(height).map(Line::from).collect()
}

/// A path with the home directory written as ~.
fn tilde(path: &Path) -> String {
    match env::var_os("HOME").and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_narrows_the_scope() {
        assert_eq!(split("repo:lazi"), (Scope::Repos, "lazi"));
        assert_eq!(split("dir:notes"), (Scope::Dirs, "notes"));
        assert_eq!(split("file:\\.ron$"), (Scope::Files, "\\.ron$"));
        assert_eq!(split("content:TODO"), (Scope::Content, "TODO"));
        assert_eq!(split("plain: words"), (Scope::All, "plain: words"));
    }

    #[test]
    fn lowercase_ignores_case_and_a_capital_makes_it_exact() {
        let lower = compile("cargo").unwrap().unwrap();
        assert!(lower.is_match("Cargo.toml").unwrap());
        let exact = compile("Cargo").unwrap().unwrap();
        assert!(exact.is_match("Cargo.toml").unwrap());
        assert!(!exact.is_match("cargo.toml").unwrap());
        assert!(compile("").unwrap().is_none());
        assert!(compile("(unclosed").is_err());
    }

    #[test]
    fn rg_lines_split_on_nul_so_colons_in_paths_survive() {
        assert_eq!(parse_rg(b"a:b/c.rs\x0012:let x = 1;"), Some((PathBuf::from("a:b/c.rs"), 12, "let x = 1;".into())));
        assert_eq!(parse_rg(b"no nul here"), None);
    }

    #[test]
    fn rows_put_one_header_before_each_kind() {
        let hits = [
            Hit::Repo("/r".into()),
            Hit::File("a".into()),
            Hit::File("b".into()),
            Hit::Text("c".into()),
            Hit::Line("c".into(), 3, "x".into()),
        ];
        assert_eq!(
            rows(&hits),
            [Row::Section("Repos"), Row::Hit(0), Row::Section("Files"), Row::Hit(1), Row::Hit(2), Row::Section("Content"), Row::Hit(3), Row::Hit(4)]
        );
    }
}
