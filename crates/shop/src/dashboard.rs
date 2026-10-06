//! The start page, like Doom Emacs's dashboard: a menu of shortcuts (search for a project, go
//! to a folder, edit a file), then the files opened most recently. Nothing runs in the
//! background; shop hands it the recent files before each frame.

use std::path::PathBuf;

use lazi::{Key, Lookup, Outcome};
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

pub struct Dashboard {
    spec: DashSpec,
    keys: Vec<(Vec<Key>, Bind)>,
    /// Newest first, already capped at `recent`; set by shop before each frame.
    pub recent: Vec<PathBuf>,
    /// Over the menu items, then the recent files.
    cursor: usize,
    pending: Vec<Key>,
}

impl Dashboard {
    pub fn new(spec: DashSpec) -> Result<Self, String> {
        let mut keys = Vec::new();
        for (i, item) in spec.menu.iter().enumerate() {
            keys.push((lazi::sequence(&item.key).map_err(|e| format!("dashboard menu: {e}"))?, Bind::Item(i)));
        }
        keys.extend(spec.keys.iter().map(|(k, a)| (k.clone(), Bind::Action(a.clone()))));
        // As `lazi::sequences` checks within a map: a binding that starts another would hide it.
        for (i, (a, _)) in keys.iter().enumerate() {
            if keys.iter().enumerate().any(|(j, (b, _))| i != j && b.starts_with(a)) {
                return Err(format!("dashboard: \"{}\" is bound twice, or hides a longer binding", lazi::key_label(a)));
            }
        }
        Ok(Self { spec, keys, recent: Vec::new(), cursor: 0, pending: Vec::new() })
    }

    /// How many recent files to list.
    pub fn limit(&self) -> usize {
        self.spec.recent
    }

    pub fn key(&mut self, key: Key) -> Outcome {
        self.pending.push(key);
        let bind = match lazi::lookup(&self.keys, &self.pending) {
            Lookup::Pending => return Outcome::Continue,
            Lookup::Unbound => {
                self.pending.clear();
                return Outcome::Continue;
            }
            Lookup::Action(bind) => bind.clone(),
        };
        self.pending.clear();
        let items = self.spec.menu.len();
        let last = (items + self.recent.len()).saturating_sub(1);
        match bind {
            Bind::Item(i) => return self.run(i),
            Bind::Action(DashAction::Down) => self.cursor = (self.cursor + 1).min(last),
            Bind::Action(DashAction::Up) => self.cursor = self.cursor.saturating_sub(1),
            Bind::Action(DashAction::Top) => self.cursor = 0,
            Bind::Action(DashAction::Bottom) => self.cursor = last,
            Bind::Action(DashAction::Help) => return Outcome::Help,
            Bind::Action(DashAction::Open) if self.cursor < items => return self.run(self.cursor),
            Bind::Action(action @ (DashAction::Open | DashAction::Reveal)) => {
                if let Some(file) = self.cursor.checked_sub(items).and_then(|i| self.recent.get(i)) {
                    let file = file.clone();
                    return if matches!(action, DashAction::Open) { Outcome::Edit(file) } else { Outcome::Reveal(file) };
                }
            }
        }
        Outcome::Continue
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
        menu.chain(self.spec.keys.iter().map(|(keys, action)| (lazi::key_label(keys), format!("{action:?}")))).collect()
    }

    /// One centred column: the menu with its keys right-aligned, a gap, then the recent files.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let style = &self.spec.style;
        let mut rows: Vec<Option<Line>> = Vec::new();
        for item in &self.spec.menu {
            rows.push(Some(Line::from(vec![Span::raw(item.label.clone()), Span::styled(item.key.clone(), style.key)])));
        }
        rows.push(None);
        rows.push(Some(Line::from(Span::styled("Recently opened files", style.title))));
        let files = rows.len();
        rows.extend(self.recent.iter().map(|f| Some(Line::from(tilde(f)))));
        self.cursor = self.cursor.min((self.spec.menu.len() + self.recent.len()).saturating_sub(1));

        let width = rows.iter().flatten().map(|l| l.width() + 4).max().unwrap_or(0).clamp(40, 80).min(area.width as usize) as u16;
        let x = area.x + (area.width - width) / 2;
        // A third of the spare height above, as Doom sits a little high.
        let y = area.y + (area.height.saturating_sub(rows.len() as u16)) / 3;
        let cursor_row = if self.cursor < self.spec.menu.len() { self.cursor } else { files + self.cursor - self.spec.menu.len() };
        let buf = frame.buffer_mut();
        for (i, row) in rows.iter().enumerate() {
            let row_y = y + i as u16;
            if row_y >= area.bottom() {
                break;
            }
            let Some(line) = row else { continue };
            let line_area = Rect { x, y: row_y, width, height: 1 };
            if i < self.spec.menu.len() {
                // Label on the left, key on the right, as in Doom.
                buf.set_line(x, row_y, &Line::from(line.spans[0].clone()), width);
                let key = &line.spans[1];
                buf.set_span(x + width.saturating_sub(key.width() as u16), row_y, key, width);
            } else {
                buf.set_line(x, row_y, line, width);
            }
            if i == cursor_row {
                buf.set_style(line_area, style.cursor);
            }
        }
    }
}
