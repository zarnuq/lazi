//! The panels shop can show. An enum rather than a trait: shop knows every kind, and a new
//! kind is a variant plus an arm in each method.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::time::Duration;

use lazi::{Key, Lazi, Outcome};
use ratatui::layout::Rect;
use ratatui::{DefaultTerminal, Frame};

use crate::config::PanelSpec;
use crate::git::Git;

/// Boxed: the two differ in size by hundreds of bytes.
pub enum Panel {
    Lazi(Box<Lazi>),
    Git(Box<Git>),
}

impl Panel {
    pub fn new(spec: &PanelSpec) -> Result<Self, String> {
        match spec {
            PanelSpec::Lazi { config, dir } => Ok(Panel::Lazi(Box::new(Lazi::new(config.clone(), dir.clone())?))),
            PanelSpec::Git(spec) => Ok(Panel::Git(Box::new(Git::new((**spec).clone())?))),
        }
    }

    /// Called when the panel gains focus.
    pub fn show(&mut self) {
        match self {
            Panel::Lazi(_) => {}
            Panel::Git(p) => p.show(),
        }
    }

    pub fn lazi(&self) -> Option<&Lazi> {
        match self {
            Panel::Lazi(p) => Some(p),
            Panel::Git(_) => None,
        }
    }

    /// Whether the panel is taking text, so shop leaves every key to it.
    pub fn wants_text(&self) -> bool {
        match self {
            Panel::Lazi(p) => p.wants_text(),
            Panel::Git(_) => false,
        }
    }

    /// The panel's own key bindings, as (keys, action).
    pub fn help(&self) -> Vec<(String, String)> {
        match self {
            Panel::Lazi(p) => p.help(),
            Panel::Git(p) => p.help(),
        }
    }

    /// The tab bar's label.
    pub fn name(&self) -> &'static str {
        match self {
            Panel::Lazi(_) => "lazi",
            Panel::Git(_) => "git",
        }
    }

    /// The terminal title while this panel has focus.
    pub fn title(&self) -> String {
        match self {
            Panel::Lazi(p) => p.title(),
            Panel::Git(_) => "git".into(),
        }
    }

    pub fn wake_fds(&self) -> Vec<RawFd> {
        match self {
            Panel::Lazi(p) => p.wake_fds(),
            Panel::Git(p) => p.wake_fds(),
        }
    }

    pub fn on_wake(&mut self) -> bool {
        match self {
            Panel::Lazi(p) => p.on_wake(),
            Panel::Git(p) => p.on_wake(),
        }
    }

    pub fn receive(&mut self, grace: Option<Duration>) -> bool {
        match self {
            Panel::Lazi(p) => p.receive(grace),
            Panel::Git(p) => p.receive(),
        }
    }

    pub fn key(&mut self, term: &mut DefaultTerminal, key: Key) -> io::Result<Outcome> {
        match self {
            Panel::Lazi(p) => p.key(term, key),
            Panel::Git(p) => p.key(term, key),
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        match self {
            Panel::Lazi(p) => p.draw(frame, area),
            Panel::Git(p) => p.draw(frame, area),
        }
    }

    pub fn sync_image(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.sync_image(out),
            Panel::Git(_) => Ok(()),
        }
    }

    /// Takes anything drawn outside ratatui's buffer (kitty images) off the screen.
    pub fn hide(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.hide(out),
            Panel::Git(_) => Ok(()),
        }
    }

    pub fn clear_images(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.clear_images(out),
            Panel::Git(_) => Ok(()),
        }
    }
}
