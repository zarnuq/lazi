//! A single-line text input with readline-style editing.

use ratatui::text::Span;

use crate::config::PromptAction;

pub struct Input {
    pub text: String,
    /// Byte offset, always on a char boundary.
    cursor: usize,
}

impl Input {
    pub fn new(text: String, cursor: usize) -> Self {
        Self { text, cursor }
    }

    /// Replaces the text, with the cursor at the end.
    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    /// Display width of the text before the cursor, for placing the terminal cursor.
    pub fn cursor_width(&self) -> usize {
        Span::raw(&self.text[..self.cursor]).width()
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Returns false for actions that aren't edits (submit, cancel, ...).
    pub fn edit(&mut self, action: &PromptAction) -> bool {
        match action {
            PromptAction::Home => self.cursor = 0,
            PromptAction::End => self.cursor = self.text.len(),
            PromptAction::KillToStart => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            PromptAction::KillToEnd => self.text.truncate(self.cursor),
            PromptAction::DeleteWord => {
                let before = self.text[..self.cursor].trim_end_matches(' ');
                let start = before.rfind([' ', '/']).map_or(0, |i| i + 1);
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            PromptAction::Backspace => {
                if let Some(prev) = self.prev() {
                    self.text.drain(prev..self.cursor);
                    self.cursor = prev;
                }
            }
            PromptAction::Delete => {
                if let Some(next) = self.next() {
                    self.text.drain(self.cursor..next);
                }
            }
            PromptAction::Left => self.cursor = self.prev().unwrap_or(0),
            PromptAction::Right => self.cursor = self.next().unwrap_or(self.text.len()),
            PromptAction::Submit | PromptAction::Cancel | PromptAction::Complete => return false,
        }
        true
    }

    fn prev(&self) -> Option<usize> {
        self.text[..self.cursor].char_indices().next_back().map(|(i, _)| i)
    }

    fn next(&self) -> Option<usize> {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8())
    }
}
