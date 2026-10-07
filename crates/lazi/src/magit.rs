//! A repo's status view in the git tab, like magit's: the head, then untracked, unstaged and
//! staged files, each with its diff a key away, to stage, unstage or discard one at a time.
//! It holds what the git worker last read; running git is the git panel's job, asked for
//! through `Step`.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::config::{GitStyles, StatusAction};
use crate::status::{Change, Section};

/// What the view needs the git panel to do after a key.
pub enum Step {
    Nothing,
    /// Read the files again.
    Reload,
    /// Read this file's diff.
    Diff(Change),
    /// Run git with these arguments in the repo, then read the files again.
    Exec(Vec<String>),
    Edit(PathBuf),
    Run { run: String, block: bool, file: Option<PathBuf> },
    Help,
    Close,
}

/// A line on screen.
enum Row {
    Head,
    Blank,
    Section(Section, usize),
    /// Index into `changes`.
    File(usize),
    /// A change's diff, line by line, or None while it's being read.
    Diff(usize, Option<usize>),
    Clean,
}

pub struct Magit {
    pub repo: PathBuf,
    changes: Vec<Change>,
    /// The last commit, short id and subject; empty before the first one.
    pub last: String,
    /// Changes whose diff is shown, with its lines once read.
    open: HashMap<(Section, String), Option<Vec<String>>>,
    /// Index among the rows the cursor can be on (files and their diff lines).
    cursor: usize,
    offset: usize,
    /// The change a first Discard picked; the same key again discards it.
    confirm: Option<(Section, String)>,
    /// Shown on the status line until the next key.
    pub note: Option<String>,
    loaded: bool,
}

impl Magit {
    pub fn new(repo: PathBuf) -> Self {
        Self { repo, changes: Vec::new(), last: String::new(), open: HashMap::new(), cursor: 0, offset: 0, confirm: None, note: None, loaded: false }
    }

    /// Takes in a fresh read of the files. Returns the shown diffs to read again, since they may
    /// have changed too.
    pub fn set_changes(&mut self, changes: Vec<Change>, last: String) -> Vec<Change> {
        self.changes = changes;
        self.last = last;
        self.loaded = true;
        let keys: Vec<(Section, String)> = self.changes.iter().map(|c| (c.section, c.path.clone())).collect();
        self.open.retain(|k, _| keys.contains(k));
        let again: Vec<Change> = self.changes.iter().filter(|c| self.open.contains_key(&(c.section, c.path.clone()))).cloned().collect();
        for change in &again {
            self.open.insert((change.section, change.path.clone()), None);
        }
        again
    }

    pub fn set_diff(&mut self, section: Section, path: String, lines: Vec<String>) {
        if let Some(slot) = self.open.get_mut(&(section, path)) {
            *slot = Some(lines);
        }
    }

    pub fn key(&mut self, action: &StatusAction) -> Step {
        let confirm = self.confirm.take();
        self.note = None;
        let rows = self.rows();
        let last = rows.iter().filter(|r| selectable(r)).count().saturating_sub(1);
        let change = self.at(&rows).map(|i| self.changes[i].clone());
        match action {
            StatusAction::Down => self.cursor = (self.cursor + 1).min(last),
            StatusAction::Up => self.cursor = self.cursor.saturating_sub(1),
            StatusAction::Top => self.cursor = 0,
            StatusAction::Bottom => self.cursor = last,
            StatusAction::Refresh => return Step::Reload,
            StatusAction::Close => return Step::Close,
            StatusAction::Help => return Step::Help,
            StatusAction::Toggle => {
                let Some(change) = change else { return Step::Nothing };
                let key = (change.section, change.path.clone());
                if self.open.remove(&key).is_none() {
                    self.open.insert(key, None);
                    return Step::Diff(change);
                }
                // The cursor may have been on a diff line that just went away.
                if let Some(file) = self.rows().iter().filter(|r| selectable(r)).position(|r| matches!(r, Row::File(i) if self.changes[*i] == change)) {
                    self.cursor = file;
                }
            }
            StatusAction::Stage => match change {
                Some(c) if c.section != Section::Staged => return exec(&["add", "--", &c.path]),
                Some(_) => self.note = Some("already staged".into()),
                None => {}
            },
            StatusAction::Unstage => match change {
                Some(c) if c.section == Section::Staged => return exec(&["restore", "--staged", "--", &c.path]),
                Some(_) => self.note = Some("not staged".into()),
                None => {}
            },
            StatusAction::Discard => {
                let Some(c) = change else { return Step::Nothing };
                if c.section == Section::Staged {
                    self.note = Some("unstage it first".into());
                } else if confirm == Some((c.section, c.path.clone())) {
                    return match c.section {
                        Section::Untracked => exec(&["clean", "-f", "--", &c.path]),
                        _ => exec(&["restore", "--", &c.path]),
                    };
                } else {
                    let what = if c.section == Section::Untracked { "delete" } else { "discard the changes to" };
                    self.note = Some(format!("{what} {}? Press the same key again", c.path));
                    self.confirm = Some((c.section, c.path));
                }
            }
            StatusAction::Open => {
                if let Some(c) = change {
                    return Step::Edit(self.repo.join(c.path));
                }
            }
            StatusAction::Run { run, block } => {
                return Step::Run { run: run.clone(), block: *block, file: change.map(|c| self.repo.join(c.path)) };
            }
        }
        Step::Nothing
    }

    /// The change under the cursor.
    fn at(&self, rows: &[Row]) -> Option<usize> {
        match rows.iter().filter(|r| selectable(r)).nth(self.cursor)? {
            Row::File(i) | Row::Diff(i, _) => Some(*i),
            _ => None,
        }
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row::Head, Row::Blank];
        if self.loaded && self.changes.is_empty() {
            rows.push(Row::Clean);
        }
        for section in [Section::Untracked, Section::Unstaged, Section::Staged] {
            let count = self.changes.iter().filter(|c| c.section == section).count();
            if count == 0 {
                continue;
            }
            rows.push(Row::Section(section, count));
            for (i, change) in self.changes.iter().enumerate().filter(|(_, c)| c.section == section) {
                rows.push(Row::File(i));
                match self.open.get(&(section, change.path.clone())) {
                    Some(Some(lines)) => rows.extend((0..lines.len()).map(|l| Row::Diff(i, Some(l)))),
                    Some(None) => rows.push(Row::Diff(i, None)),
                    None => {}
                }
            }
            rows.push(Row::Blank);
        }
        rows
    }

    /// `head` is the branch line, built by the git panel from what it knows of the repo.
    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, style: &GitStyles, head: Line<'static>) {
        let rows = self.rows();
        let count = rows.iter().filter(|r| selectable(r)).count();
        self.cursor = self.cursor.min(count.saturating_sub(1));
        let cursor_row = rows.iter().enumerate().filter(|(_, r)| selectable(r)).nth(self.cursor).map(|(i, _)| i);
        let height = area.height as usize;
        if let Some(row) = cursor_row {
            if row < self.offset {
                self.offset = row;
            } else if row >= self.offset + height {
                self.offset = row + 1 - height;
            }
        }
        for (i, row) in rows.iter().enumerate().skip(self.offset).take(height) {
            let y = area.y + (i - self.offset) as u16;
            let line = match row {
                Row::Head => head.clone(),
                Row::Blank => Line::default(),
                Row::Clean => Line::from(Span::styled("Nothing to commit, working tree clean", style.clean)),
                Row::Section(section, n) => {
                    let name = match section {
                        Section::Untracked => "Untracked files",
                        Section::Unstaged => "Unstaged changes",
                        Section::Staged => "Staged changes",
                    };
                    Line::from(Span::styled(format!("{name} ({n})"), style.root))
                }
                Row::File(c) => {
                    let change = &self.changes[*c];
                    let word = if change.section == Section::Untracked { String::new() } else { format!("{:<10} ", kind(change.code)) };
                    Line::from(vec![Span::styled(format!("  {word}"), style.status), Span::raw(change.path.clone())])
                }
                Row::Diff(c, line) => {
                    let change = &self.changes[*c];
                    match (line, self.open.get(&(change.section, change.path.clone()))) {
                        (Some(l), Some(Some(lines))) => diff_line(&lines[*l], style),
                        _ => Line::from("    …"),
                    }
                }
            };
            buf.set_line(area.x, y, &line, area.width);
            if cursor_row == Some(i) {
                buf.set_style(Rect { y, height: 1, ..area }, style.cursor);
            }
        }
    }

    /// The counts for the status line.
    pub fn summary(&self) -> String {
        let n = |s| self.changes.iter().filter(|c| c.section == s).count();
        format!("{} untracked · {} unstaged · {} staged", n(Section::Untracked), n(Section::Unstaged), n(Section::Staged))
    }
}

fn selectable(row: &Row) -> bool {
    matches!(row, Row::File(_) | Row::Diff(..))
}

fn exec(args: &[&str]) -> Step {
    Step::Exec(args.iter().map(|a| a.to_string()).collect())
}

/// magit's word for git's status letter.
fn kind(code: char) -> &'static str {
    match code {
        'M' => "modified",
        'A' => "new file",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'T' => "typechange",
        'U' => "unmerged",
        _ => "changed",
    }
}

fn diff_line(text: &str, style: &GitStyles) -> Line<'static> {
    let look = if text.starts_with("@@") {
        style.hunk
    } else if text.starts_with('+') {
        style.added
    } else if text.starts_with('-') {
        style.removed
    } else {
        Style::new()
    };
    Line::from(Span::styled(format!("    {text}"), look))
}

/// The lines of `git diff` worth showing: from the first hunk on, without the file header
/// that the file row above already says.
pub fn hunks(diff: &str) -> Vec<String> {
    diff.lines().skip_while(|l| !l.starts_with("@@")).map(|l| l.replace('\t', "    ")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_drop_the_file_header() {
        let diff = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(hunks(diff), ["@@ -1 +1 @@", "-old", "+new"]);
    }
}
