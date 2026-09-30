use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::io;
use std::os::fd::RawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{
    CACHE_MAX, CONFIRM_TRASH, Opener, PROGRESS_EVERY, Part, SCROLLOFF, SHOW_HIDDEN,
};
use crate::fs::{Entry, Listing, Matcher};
use crate::input::Input;
use crate::open;
use crate::ops::{Op, Progress};
use crate::wake::Waker;
use crate::watch::Watcher;

/// What worker threads send back.
pub enum Msg {
    Listed(PathBuf, Listing),
    Progress(u64, Progress),
    /// A task finished, with one message per failed item.
    Done(u64, Vec<String>),
    /// A detached program failed.
    Failed(String),
}

/// Sends to the main loop and wakes it, since it may be asleep in `wake::wait`.
#[derive(Clone)]
struct Notifier {
    tx: Sender<Msg>,
    waker: Waker,
}

impl Notifier {
    fn send(&self, msg: Msg) {
        if self.tx.send(msg).is_ok() {
            self.waker.wake();
        }
    }
}

/// The `O` popup: every opener for the targeted files.
pub struct Menu {
    pub openers: &'static [Opener],
    pub files: Vec<PathBuf>,
    pub cursor: usize,
}

/// A question in the status line, taking over the keyboard until answered.
pub enum Prompt {
    Create(Input),
    Rename { from: PathBuf, input: Input },
    Confirm { question: String, op: Op },
    /// Incremental find; `origin` is where the cursor goes back to if cancelled.
    Find { input: Input, backward: bool, origin: usize },
    Filter(Input),
    Cd(Input),
}

impl Prompt {
    pub fn input_mut(&mut self) -> Option<&mut Input> {
        match self {
            Prompt::Create(input)
            | Prompt::Rename { input, .. }
            | Prompt::Find { input, .. }
            | Prompt::Filter(input)
            | Prompt::Cd(input) => Some(input),
            Prompt::Confirm { .. } => None,
        }
    }
}

/// The cwd narrowed to names matching `query`.
pub struct Filter {
    pub query: String,
    entries: Vec<Entry>,
}

pub struct Yank {
    pub paths: BTreeSet<PathBuf>,
    pub cut: bool,
}

pub enum Mark {
    Selected,
    Copied,
    Cut,
}

/// Visual mode: everything between the anchor and the cursor is (un)selected.
struct Visual {
    anchor: usize,
    unset: bool,
}

pub struct App {
    pub cwd: PathBuf,
    pub cursor: usize,
    pub offset: usize,
    /// Rows in the listing area at the last draw, for page-sized moves.
    pub height: usize,
    /// Shown in the status line until the next key.
    pub error: Option<String>,
    pub info: Option<String>,
    pub filter: Option<Filter>,
    /// The find query, kept after the prompt closes for `n`/`N`.
    pub find: Option<String>,
    pub menu: Option<Menu>,
    pub prompt: Option<Prompt>,
    pub selected: BTreeSet<PathBuf>,
    pub yank: Option<Yank>,
    pub tasks: BTreeMap<u64, Progress>,
    visual: Option<Visual>,
    next_task: u64,
    show_hidden: bool,
    cache: HashMap<PathBuf, Listing>,
    loading: HashSet<PathBuf>,
    /// The name the cursor was last on in each visited directory.
    hovered: HashMap<PathBuf, OsString>,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    notify: Notifier,
    rx: Receiver<Msg>,
    watcher: Option<Watcher>,
}

impl App {
    pub fn new(cwd: PathBuf) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let notify = Notifier { tx, waker: Waker::new()? };
        let mut app = Self {
            cwd,
            cursor: 0,
            offset: 0,
            height: 0,
            error: None,
            info: None,
            filter: None,
            find: None,
            menu: None,
            prompt: None,
            selected: BTreeSet::new(),
            yank: None,
            tasks: BTreeMap::new(),
            visual: None,
            next_task: 0,
            show_hidden: SHOW_HIDDEN,
            cache: HashMap::new(),
            loading: HashSet::new(),
            hovered: HashMap::new(),
            back: Vec::new(),
            forward: Vec::new(),
            notify,
            rx,
            watcher: Watcher::new(),
        };
        // The cwd is read inline: there is nothing to draw without it anyway.
        let listing = Listing::read(&app.cwd, app.show_hidden);
        app.cache.insert(app.cwd.clone(), listing);
        app.remember();
        app.refresh();
        Ok(app)
    }

    pub fn listing(&self, dir: &Path) -> Option<&Listing> {
        self.cache.get(dir)
    }

    /// The cwd's entries as shown, i.e. after any filter.
    pub fn entries(&self) -> &[Entry] {
        match &self.filter {
            Some(filter) => &filter.entries,
            None => self.listing(&self.cwd).map_or(&[], |l| &l.entries),
        }
    }

    pub fn hovered_in(&self, dir: &Path) -> Option<&OsStr> {
        self.hovered.get(dir).map(OsString::as_os_str)
    }

    /// The hovered entry's path, and whether it is a directory.
    pub fn hovered(&self) -> Option<(PathBuf, bool)> {
        let entry = self.entries().get(self.cursor)?;
        Some((self.cwd.join(&entry.name), entry.is_dir))
    }

    /// The hovered directory, shown in the preview column.
    pub fn preview_dir(&self) -> Option<PathBuf> {
        let entry = self.entries().get(self.cursor)?;
        entry.is_dir.then(|| self.cwd.join(&entry.name))
    }

    pub fn is_loading(&self) -> bool {
        !self.loading.is_empty()
    }

    pub fn in_visual(&self) -> bool {
        self.visual.is_some()
    }

    /// What `wake::wait` should watch on the app's behalf.
    pub fn wake_fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.notify.waker.fd()];
        fds.extend(self.watcher.as_ref().map(Watcher::fd));
        fds
    }

    /// Handles a wakeup: re-reads directories inotify says changed. Messages from workers are
    /// picked up by `receive`. Returns whether anything changed.
    pub fn on_wake(&mut self) -> bool {
        crate::wake::drain(self.notify.waker.fd());
        let changed = self.watcher.as_mut().map(Watcher::changed).unwrap_or_default();
        for dir in &changed {
            if let Some(listing) = self.cache.get_mut(dir) {
                listing.invalidate();
            }
        }
        if changed.is_empty() {
            return false;
        }
        self.refresh();
        true
    }

    /// How `entry` (at `index` in `dir`) should be marked: selection wins over the yank register.
    pub fn mark(&self, dir: &Path, index: usize, entry: &Entry) -> Option<Mark> {
        if let Some(visual) = &self.visual
            && dir == self.cwd
            && (visual.anchor.min(self.cursor)..=visual.anchor.max(self.cursor)).contains(&index)
        {
            return (!visual.unset).then_some(Mark::Selected);
        }
        if self.selected.is_empty() && self.yank.is_none() {
            return None;
        }
        let path = dir.join(&entry.name);
        if self.selected.contains(&path) {
            return Some(Mark::Selected);
        }
        match &self.yank {
            Some(yank) if yank.paths.contains(&path) => Some(if yank.cut { Mark::Cut } else { Mark::Copied }),
            _ => None,
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        self.move_to(self.cursor.saturating_add_signed(delta));
    }

    pub fn page(&mut self, percent: isize) {
        self.move_by(self.height as isize * percent / 100);
    }

    pub fn move_to(&mut self, index: usize) {
        let len = self.entries().len();
        if len == 0 {
            return;
        }
        self.cursor = index.min(len - 1);
        self.remember();
        self.refresh();
    }

    pub fn leave(&mut self) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else { return };
        if let Some(name) = self.cwd.file_name() {
            self.hovered.insert(parent.clone(), name.to_owned());
        }
        self.cd(parent);
    }

    pub fn enter(&mut self) {
        if let Some(dir) = self.preview_dir() {
            self.cd(dir);
        }
    }

    pub fn goto(&mut self, dir: &str) {
        let dir = match (dir.strip_prefix('~'), env::var_os("HOME")) {
            (Some(rest), Some(home)) => PathBuf::from(home).join(rest.trim_start_matches('/')),
            _ => self.cwd.join(dir),
        };
        if dir.is_dir() {
            self.cd(dir);
        } else {
            self.error = Some(format!("not a directory: {}", dir.display()));
        }
    }

    pub fn back(&mut self) {
        if let Some(dir) = self.back.pop() {
            self.commit_visual();
            let old = mem::replace(&mut self.cwd, dir);
            self.forward.push(old);
            self.after_cd();
        }
    }

    pub fn forward(&mut self) {
        if let Some(dir) = self.forward.pop() {
            self.commit_visual();
            let old = mem::replace(&mut self.cwd, dir);
            self.back.push(old);
            self.after_cd();
        }
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh();
    }

    /// Keeps the cursor at least SCROLLOFF rows from the edges of the listing.
    pub fn scroll(&mut self) {
        let height = self.height;
        let so = SCROLLOFF.min(height.saturating_sub(1) / 2);
        if self.cursor < self.offset + so {
            self.offset = self.cursor.saturating_sub(so);
        } else if self.cursor + so >= self.offset + height {
            self.offset = self.cursor + so + 1 - height;
        }
        self.offset = self.offset.min(self.entries().len().saturating_sub(height));
    }

    pub fn toggle_select(&mut self) {
        if let Some((path, _)) = self.hovered()
            && !self.selected.remove(&path)
        {
            self.selected.insert(path);
        }
        self.move_by(1);
    }

    /// Selects everything in the cwd, or with `invert`, flips each entry.
    pub fn select_all(&mut self, invert: bool) {
        let paths: Vec<PathBuf> = self.entries().iter().map(|e| self.cwd.join(&e.name)).collect();
        for path in paths {
            if !(invert && self.selected.remove(&path)) {
                self.selected.insert(path);
            }
        }
    }

    pub fn visual(&mut self, unset: bool) {
        self.commit_visual();
        if !self.entries().is_empty() {
            self.visual = Some(Visual { anchor: self.cursor, unset });
        }
    }

    /// Undoes one thing, in order: visual mode (keeping its selection), the selection, the
    /// filter, the find highlight.
    pub fn escape(&mut self) {
        if self.visual.is_some() {
            self.commit_visual();
        } else if !self.selected.is_empty() {
            self.selected.clear();
        } else if self.filter.is_some() {
            self.set_filter("");
        } else {
            self.find = None;
        }
    }

    /// The selection, including any visual range, or else the hovered entry.
    pub fn targets(&mut self) -> Vec<PathBuf> {
        self.commit_visual();
        if self.selected.is_empty() {
            self.hovered().map(|(path, _)| path).into_iter().collect()
        } else {
            self.selected.iter().cloned().collect()
        }
    }

    pub fn yank(&mut self, cut: bool) {
        let paths = self.targets();
        if !paths.is_empty() {
            self.selected.clear();
            self.yank = Some(Yank { paths: paths.into_iter().collect(), cut });
        }
    }

    pub fn unyank(&mut self) {
        self.yank = None;
    }

    pub fn paste(&mut self, force: bool) {
        let Some(yank) = &self.yank else { return };
        let (srcs, dir) = (yank.paths.iter().cloned().collect(), self.cwd.clone());
        let op = if yank.cut {
            // The sources won't be there any more.
            self.yank = None;
            Op::Move { srcs, dir, force }
        } else {
            Op::Copy { srcs, dir, force }
        };
        self.start(op);
    }

    pub fn remove(&mut self, permanently: bool) {
        let paths = self.targets();
        let what = match paths.as_slice() {
            [] => return,
            [path] => format!("'{}'", path.file_name().unwrap_or_default().to_string_lossy()),
            paths => format!("{} items", paths.len()),
        };
        if permanently {
            let question = format!("Permanently delete {what}? (y/N)");
            self.prompt = Some(Prompt::Confirm { question, op: Op::Delete(paths) });
        } else if CONFIRM_TRASH {
            let question = format!("Trash {what}? (y/N)");
            self.prompt = Some(Prompt::Confirm { question, op: Op::Trash(paths) });
        } else {
            self.selected.clear();
            self.start(Op::Trash(paths));
        }
    }

    pub fn start_create(&mut self) {
        self.prompt = Some(Prompt::Create(Input::new(String::new(), 0)));
    }

    /// Starts renaming the hovered entry, with the cursor before the extension.
    pub fn start_rename(&mut self) {
        let Some((from, is_dir)) = self.hovered() else { return };
        let Some(name) = from.file_name().and_then(OsStr::to_str).map(str::to_owned) else {
            self.error = Some("can't rename a name that isn't valid UTF-8".into());
            return;
        };
        let cursor = match Path::new(&name).extension() {
            Some(ext) if !is_dir => name.len() - ext.len() - 1,
            _ => name.len(),
        };
        self.prompt = Some(Prompt::Rename { from, input: Input::new(name, cursor) });
    }

    /// Acts on the open prompt as if answered yes / submitted.
    pub fn submit(&mut self) {
        match self.prompt.take() {
            Some(Prompt::Create(input)) => self.create(&input.text),
            Some(Prompt::Rename { from, input }) => self.rename(&from, &input.text),
            Some(Prompt::Confirm { op, .. }) => {
                self.selected.clear();
                self.start(op);
            }
            Some(Prompt::Cd(input)) => self.goto(&input.text),
            // Find and filter already happened as the text was typed; they just stay.
            Some(Prompt::Find { .. } | Prompt::Filter(_)) | None => {}
        }
    }

    /// Closes the prompt, undoing a find or filter in progress.
    pub fn cancel_prompt(&mut self) {
        match self.prompt.take() {
            Some(Prompt::Find { origin, .. }) => {
                self.find = None;
                self.move_to(origin);
            }
            Some(Prompt::Filter(_)) => self.set_filter(""),
            _ => {}
        }
    }

    /// Updates a live find or filter after its text changed.
    pub fn prompt_changed(&mut self) {
        match &self.prompt {
            Some(Prompt::Find { input, backward, origin }) => {
                let (query, backward, origin) = (input.text.clone(), *backward, *origin);
                self.find = (!query.is_empty()).then_some(query);
                self.cursor = origin;
                if self.find.is_some() {
                    self.find_next(backward);
                } else {
                    self.move_to(origin);
                }
            }
            Some(Prompt::Filter(input)) => {
                let query = input.text.clone();
                self.set_filter(&query);
            }
            _ => {}
        }
    }

    pub fn start_find(&mut self, backward: bool) {
        let input = Input::new(String::new(), 0);
        self.prompt = Some(Prompt::Find { input, backward, origin: self.cursor });
    }

    /// Jumps to the next entry matching the find query, wrapping around.
    pub fn find_next(&mut self, backward: bool) {
        let Some(query) = &self.find else { return };
        let matcher = Matcher::new(query);
        let entries = self.entries();
        let (len, from) = (entries.len(), self.cursor);
        // Everything after the cursor in order, ending with the cursor itself.
        let found = (1..=len)
            .map(|k| if backward { (from + len - k % len) % len } else { (from + k) % len })
            .find(|&i| matcher.matches(&entries[i]));
        if let Some(i) = found {
            self.move_to(i);
        }
    }

    pub fn find_matcher(&self) -> Option<Matcher> {
        self.find.as_deref().map(Matcher::new)
    }

    pub fn start_filter(&mut self) {
        let query = self.filter.as_ref().map_or_else(String::new, |f| f.query.clone());
        let cursor = query.len();
        self.prompt = Some(Prompt::Filter(Input::new(query, cursor)));
    }

    fn set_filter(&mut self, query: &str) {
        self.filter = (!query.is_empty()).then(|| Filter { query: query.to_owned(), entries: Vec::new() });
        self.apply_filter();
        self.sync_cursor();
        self.refresh();
    }

    /// Recomputes the filtered entries from the cwd listing.
    fn apply_filter(&mut self) {
        let Some(filter) = &mut self.filter else { return };
        let matcher = Matcher::new(&filter.query);
        let all = self.cache.get(&self.cwd).map_or(&[][..], |l| &l.entries);
        filter.entries = all.iter().filter(|e| matcher.matches(e)).cloned().collect();
    }

    pub fn start_cd(&mut self) {
        self.prompt = Some(Prompt::Cd(Input::new(String::new(), 0)));
    }

    /// Goes to `path` (relative to the cwd) if it's a directory, or to its parent with it hovered.
    pub fn reveal(&mut self, path: &Path) {
        let path = self.cwd.join(path);
        if path.is_dir() {
            self.cd(path);
            return;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return };
        self.hovered.insert(parent.to_path_buf(), name.to_owned());
        if parent == self.cwd {
            self.sync_cursor();
            self.refresh();
        } else {
            self.cd(parent.to_path_buf());
        }
    }

    pub fn copy_path(&mut self, part: Part) {
        let targets = self.targets();
        let parts: Vec<_> = targets
            .iter()
            .map(|path| {
                let part = match part {
                    Part::Path => Some(path.as_os_str()),
                    Part::Dir => path.parent().map(Path::as_os_str),
                    Part::Name => path.file_name(),
                    Part::Stem => path.file_stem(),
                };
                part.unwrap_or_default().to_string_lossy()
            })
            .collect();
        let text = parts.join("\n");
        match open::clipboard(&text) {
            Ok(()) if parts.len() == 1 => self.info = Some(format!("copied {text}")),
            Ok(()) => self.info = Some(format!("copied {} paths", parts.len())),
            Err(e) => self.error = Some(format!("copy: {e}")),
        }
    }

    /// A callback for a detached program to report failure through.
    pub fn on_fail(&self) -> Box<dyn FnOnce(String) + Send> {
        let notify = self.notify.clone();
        Box::new(move |msg| {
            notify.send(Msg::Failed(msg));
        })
    }

    /// Takes messages from worker threads, waiting up to `grace` (forever if None) while
    /// directory reads are still running. Returns whether anything arrived.
    pub fn receive(&mut self, grace: Option<Duration>) -> bool {
        let deadline = grace.map(|g| Instant::now() + g);
        let mut got = false;
        loop {
            let msg = match deadline {
                _ if !self.is_loading() => self.rx.try_recv().ok(),
                Some(d) => self.rx.recv_timeout(d.saturating_duration_since(Instant::now())).ok(),
                None => self.rx.recv().ok(),
            };
            let Some(msg) = msg else { break };
            got = true;
            match msg {
                Msg::Listed(dir, listing) => {
                    self.loading.remove(&dir);
                    let is_cwd = dir == self.cwd;
                    self.cache.insert(dir, listing);
                    if is_cwd {
                        self.apply_filter();
                        self.sync_cursor();
                    }
                    // May start new reads, e.g. the preview once the cwd has arrived.
                    self.refresh();
                }
                Msg::Progress(id, progress) => {
                    self.tasks.insert(id, progress);
                }
                Msg::Done(id, errors) => {
                    self.tasks.remove(&id);
                    if let Some(first) = errors.first() {
                        self.error = Some(match errors.len() {
                            1 => first.clone(),
                            n => format!("{first} (and {} more)", n - 1),
                        });
                    }
                    self.refresh();
                }
                Msg::Failed(msg) => self.error = Some(msg),
            }
        }
        got
    }

    fn create(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        let path = self.cwd.join(name);
        let res = if name.ends_with('/') {
            std::fs::create_dir_all(&path)
        } else {
            path.parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::File::create_new(&path).map(drop))
        };
        match res {
            Ok(()) => self.hover_created(name),
            Err(e) => self.error = Some(format!("{name}: {e}")),
        }
        self.refresh();
    }

    fn rename(&mut self, from: &Path, name: &str) {
        if name.is_empty() || from.file_name() == Some(OsStr::new(name)) {
            return;
        }
        let to = from.with_file_name(name);
        let res = if std::fs::symlink_metadata(&to).is_ok() {
            Err(std::io::Error::other("already exists"))
        } else {
            std::fs::rename(from, &to)
        };
        match res {
            Ok(()) => self.hover_created(name),
            Err(e) => self.error = Some(format!("{name}: {e}")),
        }
        self.refresh();
    }

    /// Points the cursor at a new entry, which lands once the cwd is re-read.
    fn hover_created(&mut self, name: &str) {
        if let Some(Component::Normal(first)) = Path::new(name).components().next() {
            self.hovered.insert(self.cwd.clone(), first.to_owned());
        }
    }

    fn start(&mut self, op: Op) {
        let id = self.next_task;
        self.next_task += 1;
        self.tasks.insert(id, Progress { label: op.label(), done: 0, total: 0, bytes: false });
        let notify = self.notify.clone();
        thread::spawn(move || {
            let mut last: Option<Instant> = None;
            let errors = op.run(&mut |progress| {
                // A few updates a second is plenty, however many files go by.
                if last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                    last = Some(Instant::now());
                    notify.send(Msg::Progress(id, progress));
                }
            });
            notify.send(Msg::Done(id, errors));
        });
    }

    fn commit_visual(&mut self) {
        let Some(visual) = self.visual.take() else { return };
        let (lo, hi) = (visual.anchor.min(self.cursor), visual.anchor.max(self.cursor));
        let paths: Vec<PathBuf> =
            self.entries().iter().skip(lo).take(hi - lo + 1).map(|e| self.cwd.join(&e.name)).collect();
        for path in paths {
            if visual.unset {
                self.selected.remove(&path);
            } else {
                self.selected.insert(path);
            }
        }
    }

    fn cd(&mut self, dir: PathBuf) {
        if dir == self.cwd {
            return;
        }
        self.commit_visual();
        let old = mem::replace(&mut self.cwd, dir);
        self.back.push(old);
        self.forward.clear();
        self.after_cd();
    }

    fn after_cd(&mut self) {
        self.filter = None;
        self.find = None;
        self.cursor = 0;
        self.offset = 0;
        self.sync_cursor();
        self.refresh();
    }

    /// Puts the cursor back on the remembered name in the cwd, or clamps it if that name is gone.
    fn sync_cursor(&mut self) {
        if !self.cache.contains_key(&self.cwd) {
            return;
        }
        let entries = self.entries();
        let found = self.hovered.get(&self.cwd).and_then(|name| entries.iter().position(|e| &e.name == name));
        match found {
            Some(i) => self.cursor = i,
            None => {
                self.cursor = self.cursor.min(entries.len().saturating_sub(1));
                self.remember();
            }
        }
    }

    fn remember(&mut self) {
        if let Some(entry) = self.entries().get(self.cursor) {
            let name = entry.name.clone();
            self.hovered.insert(self.cwd.clone(), name);
        }
    }

    /// Makes sure the parent, cwd and preview are loaded and current, reading any that aren't.
    pub fn refresh(&mut self) {
        let visible: Vec<PathBuf> = [self.cwd.parent().map(Path::to_path_buf), Some(self.cwd.clone()), self.preview_dir()]
            .into_iter()
            .flatten()
            .collect();
        for dir in &visible {
            self.ensure(dir);
        }
        if let Some(watcher) = &mut self.watcher {
            watcher.set(&visible);
        }
        if self.cache.len() > CACHE_MAX {
            self.cache.retain(|dir, _| visible.contains(dir));
        }
    }

    fn ensure(&mut self, dir: &Path) {
        let fresh = self.cache.get(dir).is_some_and(|l| l.is_fresh(dir, self.show_hidden));
        if fresh || self.loading.contains(dir) {
            return;
        }
        self.loading.insert(dir.to_path_buf());
        let (dir, notify, hidden) = (dir.to_path_buf(), self.notify.clone(), self.show_hidden);
        thread::spawn(move || {
            let listing = Listing::read(&dir, hidden);
            notify.send(Msg::Listed(dir, listing));
        });
    }
}
