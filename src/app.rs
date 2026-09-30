use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{CACHE_MAX, SCROLLOFF, SHOW_HIDDEN};
use crate::fs::{Entry, Listing};

pub struct App {
    pub cwd: PathBuf,
    pub cursor: usize,
    pub offset: usize,
    /// Rows in the listing area at the last draw, for page-sized moves.
    pub height: usize,
    show_hidden: bool,
    cache: HashMap<PathBuf, Listing>,
    loading: HashSet<PathBuf>,
    /// The name the cursor was last on in each visited directory.
    hovered: HashMap<PathBuf, OsString>,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    tx: Sender<(PathBuf, Listing)>,
    rx: Receiver<(PathBuf, Listing)>,
}

impl App {
    pub fn new(cwd: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            cwd,
            cursor: 0,
            offset: 0,
            height: 0,
            show_hidden: SHOW_HIDDEN,
            cache: HashMap::new(),
            loading: HashSet::new(),
            hovered: HashMap::new(),
            back: Vec::new(),
            forward: Vec::new(),
            tx,
            rx,
        };
        // The cwd is read inline: there is nothing to draw without it anyway.
        let listing = Listing::read(&app.cwd, app.show_hidden);
        app.cache.insert(app.cwd.clone(), listing);
        app.remember();
        app.refresh();
        app
    }

    pub fn listing(&self, dir: &Path) -> Option<&Listing> {
        self.cache.get(dir)
    }

    pub fn entries(&self) -> &[Entry] {
        self.listing(&self.cwd).map_or(&[], |l| &l.entries)
    }

    pub fn hovered_in(&self, dir: &Path) -> Option<&OsStr> {
        self.hovered.get(dir).map(OsString::as_os_str)
    }

    /// The hovered directory, shown in the preview column.
    pub fn preview_dir(&self) -> Option<PathBuf> {
        let entry = self.entries().get(self.cursor)?;
        entry.is_dir.then(|| self.cwd.join(&entry.name))
    }

    pub fn is_loading(&self) -> bool {
        !self.loading.is_empty()
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

    pub fn back(&mut self) {
        if let Some(dir) = self.back.pop() {
            let old = mem::replace(&mut self.cwd, dir);
            self.forward.push(old);
            self.after_cd();
        }
    }

    pub fn forward(&mut self) {
        if let Some(dir) = self.forward.pop() {
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

    /// Takes finished reads, waiting up to `grace` (forever if None) for ones still running.
    /// Returns whether anything arrived.
    pub fn receive(&mut self, grace: Option<Duration>) -> bool {
        let deadline = grace.map(|g| Instant::now() + g);
        let mut got = false;
        while self.is_loading() {
            let msg = match deadline {
                Some(d) => self.rx.recv_timeout(d.saturating_duration_since(Instant::now())).ok(),
                None => self.rx.recv().ok(),
            };
            let Some((dir, listing)) = msg else { break };
            self.loading.remove(&dir);
            let is_cwd = dir == self.cwd;
            self.cache.insert(dir, listing);
            if is_cwd {
                self.sync_cursor();
            }
            // May start new reads, e.g. the preview once the cwd has arrived.
            self.refresh();
            got = true;
        }
        got
    }

    fn cd(&mut self, dir: PathBuf) {
        let old = mem::replace(&mut self.cwd, dir);
        self.back.push(old);
        self.forward.clear();
        self.after_cd();
    }

    fn after_cd(&mut self) {
        self.cursor = 0;
        self.offset = 0;
        self.sync_cursor();
        self.refresh();
    }

    /// Puts the cursor back on the remembered name in the cwd, or clamps it if that name is gone.
    fn sync_cursor(&mut self) {
        let Some(listing) = self.cache.get(&self.cwd) else { return };
        match self.hovered.get(&self.cwd).and_then(|name| listing.position(name)) {
            Some(i) => self.cursor = i,
            None => {
                self.cursor = self.cursor.min(listing.entries.len().saturating_sub(1));
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
        let (dir, tx, hidden) = (dir.to_path_buf(), self.tx.clone(), self.show_hidden);
        thread::spawn(move || {
            let listing = Listing::read(&dir, hidden);
            let _ = tx.send((dir, listing));
        });
    }
}
