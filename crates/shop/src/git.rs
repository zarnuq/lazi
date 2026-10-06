//! The git panel: every repository directly under the configured roots, with the status
//! symbols the zhimmer prompt shows, kept current by a worker thread, a ticker thread and
//! inotify on each repo's `.git`.

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use std::{env, mem, slice, thread};

use lazi::wake::{self, Waker};
use lazi::watch::Watcher;
use lazi::{Cmd, Key, Lookup, Outcome};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{GitAction, GitSpec, GitStyles, GitSymbols};
use crate::status::{self, Status};

/// Work for the worker thread.
enum Job {
    Status(Vec<PathBuf>),
    Fetch(Vec<PathBuf>),
}

/// What comes back from the worker, the fetch threads, the ticker and detached commands.
enum Msg {
    Status(PathBuf, Result<Status, String>),
    Fetched(PathBuf, Result<(), String>),
    /// A detached `Run` failed.
    Failed(PathBuf, String),
    /// Time for a fetch round.
    Tick,
}

/// Sends to the panel and wakes the main loop, which may be asleep in `wake::wait`.
#[derive(Clone)]
struct Notifier {
    tx: Sender<Msg>,
    waker: Waker,
}

impl Notifier {
    /// Returns false once the panel is gone, so threads know to stop.
    fn send(&self, msg: Msg) -> bool {
        let sent = self.tx.send(msg).is_ok();
        if sent {
            self.waker.wake();
        }
        sent
    }
}

struct Root {
    /// As written in the config, for the header.
    label: String,
    /// None when the root couldn't be read.
    repos: Option<Vec<PathBuf>>,
}

#[derive(Default)]
struct Repo {
    /// None until the first read comes back.
    status: Option<Result<Status, String>>,
    fetching: bool,
    fetch_error: Option<String>,
    /// A command's failure, shown until the next key.
    run_error: Option<String>,
}

pub struct Git {
    spec: GitSpec,
    roots: Vec<Root>,
    repos: HashMap<PathBuf, Repo>,
    /// Index into `all()`.
    cursor: usize,
    /// First list row on screen.
    offset: usize,
    /// Keys of a sequence typed so far, like the first `g` of `gg`.
    pending: Vec<Key>,
    jobs: Sender<Job>,
    rx: Receiver<Msg>,
    notify: Notifier,
    watcher: Option<Watcher>,
    /// Local time the last fetch round finished, as HH:MM.
    fetched: Option<String>,
}

impl Git {
    pub fn new(spec: GitSpec) -> Result<Self, String> {
        let waker = Waker::new().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        let notify = Notifier { tx, waker };
        let (jobs, job_rx) = mpsc::channel();
        let to_main = notify.clone();
        thread::spawn(move || worker(job_rx, to_main));
        let ticker = notify.clone();
        let every = Duration::from_secs(spec.fetch_every);
        thread::spawn(move || {
            loop {
                thread::sleep(every);
                if !ticker.send(Msg::Tick) {
                    break;
                }
            }
        });
        let mut git = Self {
            spec,
            roots: Vec::new(),
            repos: HashMap::new(),
            cursor: 0,
            offset: 0,
            pending: Vec::new(),
            jobs,
            rx,
            notify,
            watcher: Watcher::new(),
            fetched: None,
        };
        git.refresh();
        Ok(git)
    }

    pub fn wake_fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.notify.waker.fd()];
        fds.extend(self.watcher.as_ref().map(Watcher::fd));
        fds
    }

    /// Re-reads the status of repos whose `.git` changed. Results arrive through `receive`,
    /// so this never needs a redraw itself.
    pub fn on_wake(&mut self) -> bool {
        wake::drain(self.notify.waker.fd());
        let Some(watcher) = &mut self.watcher else { return false };
        let changed = watcher.changed();
        if !changed.is_empty() {
            // Each repo's watched directories (.git, .git/refs/heads) lie under it.
            let repos = self.all().into_iter().filter(|repo| changed.iter().any(|dir| dir.starts_with(repo))).collect();
            self.status(repos);
        }
        false
    }

    /// Takes in what the threads sent. Returns whether anything arrived.
    pub fn receive(&mut self) -> bool {
        let mut any = false;
        while let Ok(msg) = self.rx.try_recv() {
            any = true;
            match msg {
                Msg::Status(path, result) => {
                    if let Some(repo) = self.repos.get_mut(&path) {
                        repo.status = Some(result);
                    }
                }
                Msg::Fetched(path, result) => {
                    if let Some(repo) = self.repos.get_mut(&path) {
                        repo.fetching = false;
                        repo.fetch_error = result.err();
                    }
                    // The fetch moved the remote-tracking branch, so the arrows changed.
                    self.status(vec![path]);
                    if !self.repos.values().any(|repo| repo.fetching) {
                        self.fetched = Some(clock());
                    }
                }
                Msg::Failed(path, err) => {
                    if let Some(repo) = self.repos.get_mut(&path) {
                        repo.run_error = Some(err);
                    }
                }
                Msg::Tick => self.fetch(self.all()),
            }
        }
        any
    }

    /// Edits in a working tree touch nothing in `.git`, so coming back to the panel is the
    /// moment to look again.
    pub fn show(&mut self) {
        self.status(self.all());
    }

    pub fn key(&mut self, term: &mut DefaultTerminal, key: Key) -> io::Result<Outcome> {
        for repo in self.repos.values_mut() {
            repo.run_error = None;
        }
        self.pending.push(key);
        let action = match lazi::lookup(&self.spec.keys, &self.pending) {
            Lookup::Pending => return Ok(Outcome::Continue),
            Lookup::Unbound => {
                self.pending.clear();
                return Ok(Outcome::Continue);
            }
            Lookup::Action(action) => action.clone(),
        };
        self.pending.clear();
        let all = self.all();
        let last = all.len().saturating_sub(1);
        match action {
            GitAction::Down => self.cursor = (self.cursor + 1).min(last),
            GitAction::Up => self.cursor = self.cursor.saturating_sub(1),
            GitAction::Top => self.cursor = 0,
            GitAction::Bottom => self.cursor = last,
            GitAction::Refresh => self.refresh(),
            GitAction::Open => {
                if let Some(path) = all.get(self.cursor) {
                    return Ok(Outcome::Open(path.clone()));
                }
            }
            GitAction::Run { run, block } => {
                if let Some(path) = all.get(self.cursor) {
                    let notify = self.notify.clone();
                    let failed = path.clone();
                    let on_fail = Box::new(move |msg| {
                        notify.send(Msg::Failed(failed, msg));
                    });
                    let cmd = Cmd { desc: &run, script: &run, args: slice::from_ref(path), block };
                    let err = lazi::run(term, &cmd, path, on_fail)?;
                    if let Some(repo) = self.repos.get_mut(path) {
                        repo.run_error = err;
                    }
                    if block {
                        // Whatever ran (a pull, lazygit) has likely changed the repo.
                        self.status(vec![path.clone()]);
                    }
                }
            }
        }
        Ok(Outcome::Continue)
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 2 {
            return;
        }
        let list = Rect { height: area.height - 1, ..area };
        let all = self.all();
        let name_w = all.iter().map(|p| name(p).chars().count()).max().unwrap_or(0).min(30);
        let branch_w = self.repos.values().filter_map(|r| r.status.as_ref()?.as_ref().ok()).map(|s| s.branch.chars().count()).max().unwrap_or(0).min(24);

        let mut rows: Vec<Line> = Vec::new();
        let mut cursor_row = None;
        let mut index = 0;
        for root in &self.roots {
            let mut header = vec![Span::styled(root.label.clone(), self.spec.style.root)];
            if root.repos.is_none() {
                header.push(Span::styled(" (missing)", self.spec.style.error));
            }
            rows.push(Line::from(header));
            for path in root.repos.iter().flatten() {
                if index == self.cursor {
                    cursor_row = Some(rows.len());
                }
                rows.push(self.row(path, name_w, branch_w));
                index += 1;
            }
        }

        let height = list.height as usize;
        if let Some(row) = cursor_row {
            if row < self.offset {
                // Show the root header above the first repo rather than cutting it off.
                self.offset = row.saturating_sub(1);
            } else if row >= self.offset + height {
                self.offset = row + 1 - height;
            }
        }
        self.offset = self.offset.min(rows.len().saturating_sub(1));

        let style = &self.spec.style;
        let buf = frame.buffer_mut();
        for (i, line) in rows.iter().skip(self.offset).take(height).enumerate() {
            let y = list.y + i as u16;
            buf.set_line(list.x, y, line, list.width);
            if cursor_row == Some(self.offset + i) {
                buf.set_style(Rect { y, height: 1, ..list }, style.cursor);
            }
        }
        let stamp = match &self.fetched {
            Some(time) => format!("fetched {time}"),
            None => "fetching…".to_owned(),
        };
        let width = stamp.chars().count() as u16;
        buf.set_stringn(list.right().saturating_sub(width), list.y, stamp, list.width as usize, Style::new());

        let (text, line_style) = self.status_line();
        buf.set_stringn(area.x, area.bottom() - 1, text, area.width as usize, line_style);
    }

    /// One repo's row: name, branch, symbols, and fetch state.
    fn row(&self, path: &Path, name_w: usize, branch_w: usize) -> Line<'static> {
        let style = &self.spec.style;
        let mut spans = vec![Span::raw(format!("  {:<name_w$}  ", name(path)))];
        let repo = self.repos.get(path);
        match repo.and_then(|r| r.status.as_ref()) {
            None => {}
            Some(Err(_)) => spans.push(Span::styled("✗", style.error)),
            Some(Ok(s)) => {
                // Dim: there's no upstream to compare with, so no arrows can show.
                let branch = if s.detached || s.ahead_behind.is_none() { style.branch.add_modifier(Modifier::DIM) } else { style.branch };
                // Cut to the column so a long branch name doesn't push the symbols out of line.
                let cut: String = s.branch.chars().take(branch_w).collect();
                spans.push(Span::styled(format!("{cut:<branch_w$}  "), branch));
                spans.extend(symbols(s, &self.spec.symbols, style));
            }
        }
        if repo.is_some_and(|r| r.fetching) {
            spans.push(Span::raw(" …"));
        }
        if repo.is_some_and(|r| r.fetch_error.is_some()) {
            spans.push(Span::styled(" ✗fetch", style.error));
        }
        Line::from(spans)
    }

    /// The cursor repo's error if it has one, else what needs doing across all repos.
    fn status_line(&self) -> (String, Style) {
        if let Some(repo) = self.all().get(self.cursor).and_then(|path| self.repos.get(path)) {
            let status_err = match &repo.status {
                Some(Err(e)) => Some(e),
                _ => None,
            };
            if let Some(err) = repo.run_error.as_ref().or(repo.fetch_error.as_ref()).or(status_err) {
                return (err.clone(), self.spec.style.error);
            }
        }
        let (mut push, mut pull, mut dirty) = (0, 0, 0);
        for s in self.repos.values().filter_map(|r| r.status.as_ref()?.as_ref().ok()) {
            let (ahead, behind) = s.ahead_behind.unwrap_or((0, 0));
            push += usize::from(ahead > 0);
            pull += usize::from(behind > 0);
            dirty += usize::from(s.dirty());
        }
        if push + pull + dirty == 0 {
            return ("all clean".to_owned(), Style::new());
        }
        (format!("{push} to push · {pull} to pull · {dirty} dirty"), Style::new())
    }

    /// Every repo, in screen order.
    fn all(&self) -> Vec<PathBuf> {
        self.roots.iter().flat_map(|root| root.repos.iter().flatten().cloned()).collect()
    }

    /// Finds the repos again, watches their `.git`, and asks for every status and a fetch.
    fn refresh(&mut self) {
        self.roots = self.spec.roots.iter().map(|label| Root { label: label.clone(), repos: status::discover(&expand(label)).ok() }).collect();
        let all = self.all();
        self.repos.retain(|path, _| all.contains(path));
        for path in &all {
            self.repos.entry(path.clone()).or_default();
        }
        if let Some(watcher) = &mut self.watcher {
            // git writes the index, HEAD and branch refs through a lock file and a rename, which
            // these catch; commits, checkouts and pulls made elsewhere show up at once.
            let dirs: Vec<PathBuf> = all.iter().flat_map(|repo| [repo.join(".git"), repo.join(".git/refs/heads")]).collect();
            watcher.set(&dirs);
        }
        self.cursor = self.cursor.min(all.len().saturating_sub(1));
        self.status(all.clone());
        self.fetch(all);
    }

    fn status(&self, repos: Vec<PathBuf>) {
        if !repos.is_empty() {
            let _ = self.jobs.send(Job::Status(repos));
        }
    }

    /// Fetches the repos not already fetching.
    fn fetch(&mut self, repos: Vec<PathBuf>) {
        let mut todo = Vec::new();
        for path in repos {
            if let Some(repo) = self.repos.get_mut(&path)
                && !repo.fetching
            {
                repo.fetching = true;
                todo.push(path);
            }
        }
        if !todo.is_empty() {
            let _ = self.jobs.send(Job::Fetch(todo));
        } else if !self.repos.values().any(|repo| repo.fetching) {
            // Nothing to fetch (no repos), so the round is already over.
            self.fetched = Some(clock());
        }
    }
}

/// zhimmer's order: unmerged, deleted, renamed, modified, staged, untracked, then the arrows
/// with their counts; `clean` when there's none of it.
fn symbols(s: &Status, sym: &GitSymbols, style: &GitStyles) -> Vec<Span<'static>> {
    let local: String = [
        (s.unmerged, &sym.unmerged),
        (s.deleted, &sym.deleted),
        (s.renamed, &sym.renamed),
        (s.modified, &sym.modified),
        (s.staged, &sym.staged),
        (s.untracked, &sym.untracked),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, symbol)| symbol.as_str())
    .collect();
    let (ahead, behind) = s.ahead_behind.unwrap_or((0, 0));
    let mut arrows = String::new();
    if ahead > 0 {
        arrows.push_str(&format!("{}{ahead}", sym.ahead));
    }
    if behind > 0 {
        arrows.push_str(&format!("{}{behind}", sym.behind));
    }
    if local.is_empty() && arrows.is_empty() {
        return vec![Span::styled(sym.clean.clone(), style.clean)];
    }
    vec![Span::styled(local, style.status), Span::styled(arrows, style.arrows)]
}

/// Runs status jobs in order, and each fetch on a thread of its own: fetches wait on the
/// network, so they're done in parallel.
fn worker(jobs: Receiver<Job>, notify: Notifier) {
    for job in jobs {
        match job {
            Job::Status(repos) => {
                for path in repos {
                    let result = git(&path, &["status", "--porcelain=v2", "--branch"]).map(|out| status::parse(&out));
                    if !notify.send(Msg::Status(path, result)) {
                        return;
                    }
                }
            }
            Job::Fetch(repos) => {
                for path in repos {
                    let notify = notify.clone();
                    thread::spawn(move || {
                        let result = git(&path, &["fetch", "--quiet", "--prune"]).map(drop);
                        notify.send(Msg::Fetched(path, result));
                    });
                }
            }
        }
    }
}

/// Runs git in `repo`: its stdout, or its last stderr line.
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(repo).stdin(Stdio::null());
    // Nothing may prompt on shop's terminal: a fetch that needs a password fails instead.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    // status would otherwise refresh the index, a write to .git that the watcher would see
    // and answer with another status, forever.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).map_or_else(|| out.status.to_string(), str::to_owned);
    Err(format!("git {}: {line}", args[0]))
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// A leading `~` is the home directory, as in a shell.
fn expand(root: &str) -> PathBuf {
    if let Some(rest) = root.strip_prefix('~')
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest.trim_start_matches('/'));
    }
    PathBuf::from(root)
}

/// The local time as HH:MM.
fn clock() -> String {
    // SAFETY: time accepts a null pointer and only returns the time.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: tm is plain data; all zeroes is a valid value for localtime_r to overwrite.
    let mut tm: libc::tm = unsafe { mem::zeroed() };
    // SAFETY: both pointers are to live locals of the right types.
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}
