//! The panels shop can show. An enum rather than a trait: shop knows every kind, and a new
//! kind is a variant plus an arm in each method.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::Duration;

use lazi::{Key, Lazi, Outcome};
use ratatui::layout::Rect;
use ratatui::{DefaultTerminal, Frame};

use crate::config::PanelSpec;
use crate::dashboard::Dashboard;
use crate::git::Git;

/// Boxed: they differ in size by hundreds of bytes.
pub enum Panel {
    Lazi(Box<Lazi>),
    Git(Box<Git>),
    Dashboard(Box<Dashboard>),
}

impl Panel {
    pub fn new(spec: &PanelSpec) -> Result<Self, String> {
        match spec {
            PanelSpec::Lazi { config, dir } => Ok(Panel::Lazi(Box::new(Lazi::new(config.clone(), dir.clone())?))),
            PanelSpec::Git(spec) => Ok(Panel::Git(Box::new(Git::new((**spec).clone())?))),
            PanelSpec::Dashboard(spec) => Ok(Panel::Dashboard(Box::new(Dashboard::new((**spec).clone())?))),
        }
    }

    /// Called when the panel gains focus.
    pub fn show(&mut self) {
        if let Panel::Git(p) = self {
            p.show();
        }
    }

    pub fn lazi(&self) -> Option<&Lazi> {
        match self {
            Panel::Lazi(p) => Some(p),
            _ => None,
        }
    }

    /// The repos a git panel found, for the search box.
    pub fn repos(&self) -> Vec<PathBuf> {
        match self {
            Panel::Git(p) => p.repos(),
            _ => Vec::new(),
        }
    }

    /// Whether the panel is taking text, so shop leaves every key to it.
    pub fn wants_text(&self) -> bool {
        match self {
            Panel::Lazi(p) => p.wants_text(),
            _ => false,
        }
    }

    /// The panel's own key bindings, as (keys, action).
    pub fn help(&self) -> Vec<(String, String)> {
        match self {
            Panel::Lazi(p) => p.help(),
            Panel::Git(p) => p.help(),
            Panel::Dashboard(p) => p.help(),
        }
    }

    /// The tab bar's label.
    pub fn name(&self) -> &'static str {
        match self {
            Panel::Lazi(_) => "lazi",
            Panel::Git(_) => "git",
            Panel::Dashboard(_) => "home",
        }
    }

    /// The terminal title while this panel has focus.
    pub fn title(&self) -> String {
        match self {
            Panel::Lazi(p) => p.title(),
            _ => self.name().into(),
        }
    }

    pub fn wake_fds(&self) -> Vec<RawFd> {
        match self {
            Panel::Lazi(p) => p.wake_fds(),
            Panel::Git(p) => p.wake_fds(),
            Panel::Dashboard(_) => Vec::new(),
        }
    }

    pub fn on_wake(&mut self) -> bool {
        match self {
            Panel::Lazi(p) => p.on_wake(),
            Panel::Git(p) => p.on_wake(),
            Panel::Dashboard(_) => false,
        }
    }

    pub fn receive(&mut self, grace: Option<Duration>) -> bool {
        match self {
            Panel::Lazi(p) => p.receive(grace),
            Panel::Git(p) => p.receive(),
            Panel::Dashboard(_) => false,
        }
    }

    pub fn key(&mut self, term: &mut DefaultTerminal, key: Key) -> io::Result<Outcome> {
        match self {
            Panel::Lazi(p) => p.key(term, key),
            Panel::Git(p) => p.key(term, key),
            Panel::Dashboard(p) => Ok(p.key(key)),
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        match self {
            Panel::Lazi(p) => p.draw(frame, area),
            Panel::Git(p) => p.draw(frame, area),
            Panel::Dashboard(p) => p.draw(frame, area),
        }
    }

    pub fn sync_image(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.sync_image(out),
            _ => Ok(()),
        }
    }

    /// Takes anything drawn outside ratatui's buffer (kitty images) off the screen.
    pub fn hide(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.hide(out),
            _ => Ok(()),
        }
    }

    pub fn clear_images(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.clear_images(out),
            _ => Ok(()),
        }
    }
}
