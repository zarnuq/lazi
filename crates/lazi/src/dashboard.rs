//! The start page, like Doom Emacs's dashboard: a menu of shortcuts (search for a project, go
//! to a folder, edit a file), then the files opened and the folders visited most recently.
//! Nothing runs in the background; lazi hands it both lists before each frame.

use std::path::PathBuf;

use files::{Key, Lookup, Outcome};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::config::{DashAction, DashSpec, Shortcut};
use crate::git::expand;
use crate::search::tilde;

/// What a key is bound to: a menu item, by position, or one of the panel's own actions.
#[derive(Clone)]
enum Bind {
    Item(usize),
    Action(DashAction),
}

/// A row the cursor can be on.
enum Entry<'a> {
    Item(usize),
    File(&'a PathBuf),
    Folder(&'a PathBuf),
}

pub struct Dashboard {
    spec: DashSpec,
    keys: Vec<(Vec<Key>, Bind)>,
    /// Newest first, already capped at `recent`; set by lazi before each frame.
    pub files: Vec<PathBuf>,
    pub folders: Vec<PathBuf>,
    /// Over the menu items, then the files, then the folders.
    cursor: usize,
    pending: Vec<Key>,
}

impl Dashboard {
    pub fn new(spec: DashSpec) -> Result<Self, String> {
        let mut keys = Vec::new();
        for (i, item) in spec.menu.iter().enumerate() {
            keys.push((files::sequence(&item.key).map_err(|e| format!("dashboard menu: {e}"))?, Bind::Item(i)));
        }
        keys.extend(spec.keys.iter().map(|(k, a)| (k.clone(), Bind::Action(a.clone()))));
        // As `files::sequences` checks within a map: a binding that starts another would hide it.
        for (i, (a, _)) in keys.iter().enumerate() {
            if keys.iter().enumerate().any(|(j, (b, _))| i != j && b.starts_with(a)) {
                return Err(format!("dashboard: \"{}\" is bound twice, or hides a longer binding", files::key_label(a)));
            }
        }
        Ok(Self { spec, keys, files: Vec::new(), folders: Vec::new(), cursor: 0, pending: Vec::new() })
    }

    /// How many recent files, and folders, to list.
    pub fn limit(&self) -> usize {
        self.spec.recent
    }

    pub fn key(&mut self, key: Key) -> Outcome {
        self.pending.push(key);
        let bind = match files::lookup(&self.keys, &self.pending) {
            Lookup::Pending => return Outcome::Continue,
            Lookup::Unbound => {
                self.pending.clear();
                return Outcome::Continue;
            }
            Lookup::Action(bind) => bind.clone(),
        };
        self.pending.clear();
        let last = (self.spec.menu.len() + self.files.len() + self.folders.len()).saturating_sub(1);
        let open = match bind {
            Bind::Item(i) => return self.run(i),
            Bind::Action(DashAction::Down) => {
                self.cursor = (self.cursor + 1).min(last);
                return Outcome::Continue;
            }
            Bind::Action(DashAction::Up) => {
                self.cursor = self.cursor.saturating_sub(1);
                return Outcome::Continue;
            }
            Bind::Action(DashAction::Top) => {
                self.cursor = 0;
                return Outcome::Continue;
            }
            Bind::Action(DashAction::Bottom) => {
                self.cursor = last;
                return Outcome::Continue;
            }
            Bind::Action(DashAction::Help) => return Outcome::Help,
            Bind::Action(DashAction::Open) => true,
            Bind::Action(DashAction::Reveal) => false,
        };
        match self.entry(self.cursor) {
            Some(Entry::Item(i)) if open => self.run(i),
            Some(Entry::File(file)) if open => Outcome::Edit(file.clone()),
            Some(Entry::Folder(dir)) if open => Outcome::Open(dir.clone()),
            Some(Entry::File(path) | Entry::Folder(path)) => Outcome::Reveal(path.clone()),
            _ => Outcome::Continue,
        }
    }

    /// What row `i` of the menu, then the files, then the folders is.
    fn entry(&self, i: usize) -> Option<Entry<'_>> {
        let items = self.spec.menu.len();
        if i < items {
            return Some(Entry::Item(i));
        }
        let i = i - items;
        match self.files.get(i) {
            Some(file) => Some(Entry::File(file)),
            None => self.folders.get(i - self.files.len()).map(Entry::Folder),
        }
    }

    fn run(&self, item: usize) -> Outcome {
        match &self.spec.menu[item].run {
            Shortcut::Search(query) => Outcome::Search(query.clone()),
            Shortcut::Goto(dir) => Outcome::Open(expand(dir)),
            Shortcut::Edit(file) => Outcome::Edit(expand(file)),
        }
    }

    /// Every binding, menu items by their labels, for the help menu.
    pub fn help(&self) -> Vec<(String, String)> {
        let menu = self.spec.menu.iter().map(|item| (item.key.clone(), item.label.clone()));
        menu.chain(self.spec.keys.iter().map(|(keys, action)| (files::key_label(keys), format!("{action:?}")))).collect()
    }

    /// One centred column: the menu with its keys right-aligned, then the recent files and
    /// the recent folders, each under a heading.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let style = &self.spec.style;
        // Each row and, for one the cursor can be on, its place in the cursor's order.
        let mut rows: Vec<(Line, Option<usize>)> = Vec::new();
        for (i, item) in self.spec.menu.iter().enumerate() {
            rows.push((Line::from(vec![Span::raw(item.label.clone()), Span::styled(item.key.clone(), style.key)]), Some(i)));
        }
        let mut next = self.spec.menu.len();
        for (title, paths) in [("Recently opened files", &self.files), ("Recent folders", &self.folders)] {
            if paths.is_empty() {
                continue;
            }
            rows.push((Line::default(), None));
            rows.push((Line::from(Span::styled(title, style.title)), None));
            for path in paths {
                rows.push((Line::from(tilde(path)), Some(next)));
                next += 1;
            }
        }
        self.cursor = self.cursor.min(next.saturating_sub(1));

        let width = rows.iter().map(|(l, _)| l.width() + 4).max().unwrap_or(0).clamp(40, 80).min(area.width as usize) as u16;
        let x = area.x + (area.width - width) / 2;
        // A third of the spare height above, as Doom sits a little high.
        let y = area.y + (area.height.saturating_sub(rows.len() as u16)) / 3;
        let buf = frame.buffer_mut();
        for (i, (line, index)) in rows.iter().enumerate() {
            let row_y = y + i as u16;
            if row_y >= area.bottom() {
                break;
            }
            if index.is_some_and(|i| i < self.spec.menu.len()) {
                // Label on the left, key on the right, as in Doom.
                buf.set_line(x, row_y, &Line::from(line.spans[0].clone()), width);
                let key = &line.spans[1];
                buf.set_span(x + width.saturating_sub(key.width() as u16), row_y, key, width);
            } else {
                buf.set_line(x, row_y, line, width);
            }
            if *index == Some(self.cursor) {
                buf.set_style(Rect { x, y: row_y, width, height: 1 }, style.cursor);
            }
        }
    }
}
