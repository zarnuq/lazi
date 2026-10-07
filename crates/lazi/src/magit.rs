//! A repo's status view in the git tab, like magit's: the head on top, then untracked,
//! unstaged and staged files on the left, to stage, unstage or discard one at a time, and the
//! diff of the one under the cursor on the right. It holds what the git worker last read;
//! running git is the git panel's job, asked for through `Step` and `wanted`.

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
    /// Run git with these arguments in the repo, then read the files again.
    Exec(Vec<String>),
    Edit(PathBuf),
    Run { run: String, block: bool, file: Option<PathBuf> },
    Help,
    Close,
}

/// A line of the file list.
enum Row {
    Blank,
    Section(Section, usize),
    /// Index into `changes`.
    File(usize),
    Clean,
}

pub struct Magit {
    pub repo: PathBuf,
    changes: Vec<Change>,
    /// The last commit, short id and subject; empty before the first one.
    pub last: String,
    /// Diffs read since the files were, by change; None while one is being read.
    diffs: HashMap<(Section, String), Option<Vec<String>>>,
    /// Index into `changes`, which is in screen order.
    cursor: usize,
    offset: usize,
    /// The diff's first row on screen.
    scroll: usize,
    /// The change a first Discard picked; the same key again discards it.
    confirm: Option<(Section, String)>,
    /// Shown on the status line until the next key.
    pub note: Option<String>,
    loaded: bool,
}

impl Magit {
    pub fn new(repo: PathBuf) -> Self {
        Self {
            repo,
            changes: Vec::new(),
            last: String::new(),
            diffs: HashMap::new(),
            cursor: 0,
            offset: 0,
            scroll: 0,
            confirm: None,
            note: None,
            loaded: false,
        }
    }

    /// Takes in a fresh read of the files, which makes every diff read so far stale.
    pub fn set_changes(&mut self, changes: Vec<Change>, last: String) {
        self.changes = changes;
        self.last = last;
        self.loaded = true;
        self.diffs.clear();
        self.cursor = self.cursor.min(self.changes.len().saturating_sub(1));
    }

    pub fn set_diff(&mut self, section: Section, path: String, lines: Vec<String>) {
        self.diffs.insert((section, path), Some(lines));
    }

    /// The change under the cursor, if its diff still needs reading; marks it as asked for.
    pub fn wanted(&mut self) -> Option<Change> {
        let change = self.changes.get(self.cursor)?.clone();
        let key = (change.section, change.path.clone());
        if self.diffs.contains_key(&key) {
            return None;
        }
        self.diffs.insert(key, None);
        Some(change)
    }

    pub fn key(&mut self, action: &StatusAction) -> Step {
        let confirm = self.confirm.take();
        self.note = None;
        let last = self.changes.len().saturating_sub(1);
        let before = self.cursor;
        let change = self.changes.get(self.cursor).cloned();
        match action {
            StatusAction::Down => self.cursor = (self.cursor + 1).min(last),
            StatusAction::Up => self.cursor = self.cursor.saturating_sub(1),
            StatusAction::Top => self.cursor = 0,
            StatusAction::Bottom => self.cursor = last,
            StatusAction::Scroll(lines) => self.scroll = self.scroll.saturating_add_signed(*lines),
            StatusAction::Refresh => return Step::Reload,
            StatusAction::Close => return Step::Close,
            StatusAction::Help => return Step::Help,
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
        if self.cursor != before {
            self.scroll = 0;
        }
        Step::Nothing
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.loaded && self.changes.is_empty() {
            rows.push(Row::Clean);
        }
        for section in [Section::Untracked, Section::Unstaged, Section::Staged] {
            let count = self.changes.iter().filter(|c| c.section == section).count();
            if count == 0 {
                continue;
            }
            if !rows.is_empty() {
                rows.push(Row::Blank);
            }
            rows.push(Row::Section(section, count));
            rows.extend(self.changes.iter().enumerate().filter(|(_, c)| c.section == section).map(|(i, _)| Row::File(i)));
        }
        rows
    }

    /// `head` is the branch line, built by the git panel from what it knows of the repo. The
    /// rest splits in two: the files on the left, the diff on the right.
    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, style: &GitStyles, head: Line<'static>) {
        if area.height < 3 {
            return;
        }
        buf.set_line(area.x, area.y, &head, area.width);
        let body = Rect { y: area.y + 2, height: area.height - 2, ..area };
        let left = Rect { width: (body.width * 2 / 5).max(20).min(body.width), ..body };
        let right = Rect { x: left.right() + 1, width: body.right().saturating_sub(left.right() + 1), ..body };
        if right.width > 0 {
            for y in body.top()..body.bottom() {
                buf.set_string(left.right(), y, "│", style.line_number);
            }
        }

        let rows = self.rows();
        let cursor_row = rows.iter().position(|r| matches!(r, Row::File(i) if *i == self.cursor));
        let height = left.height as usize;
        if let Some(row) = cursor_row {
            if row < self.offset {
                // Keep the section header above the first file in view.
                self.offset = row.saturating_sub(1);
            } else if row >= self.offset + height {
                self.offset = row + 1 - height;
            }
        }
        for (i, row) in rows.iter().enumerate().skip(self.offset).take(height) {
            let y = left.y + (i - self.offset) as u16;
            let line = match row {
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
            };
            buf.set_line(left.x, y, &line, left.width);
            if cursor_row == Some(i) {
                buf.set_style(Rect { y, height: 1, ..left }, style.cursor);
            }
        }

        let Some(change) = self.changes.get(self.cursor) else { return };
        let Some(Some(lines)) = self.diffs.get(&(change.section, change.path.clone())) else {
            buf.set_string(right.x, right.y, "…", Style::new());
            return;
        };
        let rows = render(&annotate(lines), right.width as usize, style);
        // Past the end shows the last screenful rather than nothing.
        self.scroll = self.scroll.min(rows.len().saturating_sub(right.height as usize));
        for (i, (line, look)) in rows.iter().skip(self.scroll).take(right.height as usize).enumerate() {
            let y = right.y + i as u16;
            buf.set_style(Rect { y, height: 1, ..right }, *look);
            buf.set_line(right.x, y, line, right.width);
        }
    }

    /// The counts for the status line.
    pub fn summary(&self) -> String {
        let n = |s| self.changes.iter().filter(|c| c.section == s).count();
        format!("{} untracked · {} unstaged · {} staged", n(Section::Untracked), n(Section::Unstaged), n(Section::Staged))
    }
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

/// The lines of `git diff` worth showing: from the first hunk on, without the file header
/// that the file list already says.
pub fn hunks(diff: &str) -> Vec<String> {
    diff.lines().skip_while(|l| !l.starts_with("@@")).map(|l| l.replace('\t', "    ")).collect()
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Kind {
    Added,
    Removed,
    Context,
    Hunk,
    /// git's "\ No newline at end of file", or a note in place of a diff ("binary").
    Note,
}

/// Each diff line with the number it has in its file: the old file's for a removed line, the
/// new one's otherwise. An untracked file's lines come without a hunk header, from line 1.
fn annotate(lines: &[String]) -> Vec<(Kind, Option<u64>, &str)> {
    let (mut old, mut new) = (1, 1);
    let mut out = Vec::new();
    for line in lines {
        let (kind, number) = if line.starts_with("@@") {
            // "@@ -12,5 +12,7 @@ fn name": where each side's hunk starts.
            let start = |sign: char| line.split(' ').find_map(|f| f.strip_prefix(sign)?.split(',').next()?.parse::<u64>().ok());
            old = start('-').unwrap_or(old);
            new = start('+').unwrap_or(new);
            (Kind::Hunk, None)
        } else if line.starts_with('+') {
            new += 1;
            (Kind::Added, Some(new - 1))
        } else if line.starts_with('-') {
            old += 1;
            (Kind::Removed, Some(old - 1))
        } else if line.starts_with(' ') {
            old += 1;
            new += 1;
            (Kind::Context, Some(new - 1))
        } else {
            (Kind::Note, None)
        };
        out.push((kind, number, line.as_str()));
    }
    out
}

/// Screen rows for the diff, `width` wide: a line-number column, then the line with its +/-
/// marker, wrapped. Added and removed rows are coloured across the whole width, as in
/// delta. Each comes with its row style.
fn render(lines: &[(Kind, Option<u64>, &str)], width: usize, style: &GitStyles) -> Vec<(Line<'static>, Style)> {
    // Room for the number column: four digits and a space.
    let text_w = width.saturating_sub(6).max(1);
    let mut rows = Vec::new();
    for &(kind, number, text) in lines {
        let look = match kind {
            Kind::Added => style.added,
            Kind::Removed => style.removed,
            Kind::Hunk => style.hunk,
            Kind::Context | Kind::Note => Style::new(),
        };
        if kind == Kind::Hunk {
            rows.push((Line::from(Span::raw(text.to_owned())), look));
            continue;
        }
        let (marker, body) = match kind {
            Kind::Note => ("", text),
            _ => text.split_at(1),
        };
        let chars: Vec<char> = body.chars().collect();
        // ponytail: wraps by char count, so a line of wide (CJK) characters overruns.
        let chunks: Vec<String> = if chars.is_empty() { vec![String::new()] } else { chars.chunks(text_w).map(|c| c.iter().collect()).collect() };
        for (i, chunk) in chunks.into_iter().enumerate() {
            let num = match (i, number) {
                (0, Some(n)) => format!("{n:>4} "),
                _ => "     ".to_owned(),
            };
            rows.push((Line::from(vec![Span::styled(num, style.line_number), Span::raw(format!("{marker}{chunk}"))]), look));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_drop_the_file_header() {
        let diff = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(hunks(diff), ["@@ -1 +1 @@", "-old", "+new"]);
    }

    #[test]
    fn diff_lines_are_numbered_by_their_side() {
        let lines: Vec<String> = ["@@ -196,3 +196,3 @@ fn x", " keep", "-was", "+now", " after", "\\ No newline at end of file"].map(String::from).into();
        let got: Vec<(Kind, Option<u64>)> = annotate(&lines).into_iter().map(|(k, n, _)| (k, n)).collect();
        assert_eq!(
            got,
            [(Kind::Hunk, None), (Kind::Context, Some(196)), (Kind::Removed, Some(197)), (Kind::Added, Some(197)), (Kind::Context, Some(198)), (Kind::Note, None)]
        );
    }
}
