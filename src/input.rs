//! A single-line text input with readline-style keys.

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::text::Span;

use crate::config::Key;

pub struct Input {
    pub text: String,
    /// Byte offset, always on a char boundary.
    cursor: usize,
}

impl Input {
    pub fn new(text: String, cursor: usize) -> Self {
        Self { text, cursor }
    }

    /// Display width of the text before the cursor, for placing the terminal cursor.
    pub fn cursor_width(&self) -> usize {
        Span::raw(&self.text[..self.cursor]).width()
    }

    /// Returns false for keys it doesn't handle (Enter, Esc, ...).
    pub fn key(&mut self, (code, mods): Key) -> bool {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.text.truncate(self.cursor),
            KeyCode::Char('w') if ctrl => {
                let before = self.text[..self.cursor].trim_end_matches(' ');
                let start = before.rfind([' ', '/']).map_or(0, |i| i + 1);
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char(_) if ctrl => return false,
            KeyCode::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                if let Some(prev) = self.prev() {
                    self.text.drain(prev..self.cursor);
                    self.cursor = prev;
                }
            }
            KeyCode::Delete => {
                if let Some(next) = self.next() {
                    self.text.drain(self.cursor..next);
                }
            }
            KeyCode::Left => self.cursor = self.prev().unwrap_or(0),
            KeyCode::Right => self.cursor = self.next().unwrap_or(self.text.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
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
