//! The panels shop can show. An enum rather than a trait: shop knows every kind, and a new
//! kind is a variant plus an arm in each method.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::time::Duration;

use lazi::{Key, Lazi, Outcome};
use ratatui::layout::Rect;
use ratatui::{DefaultTerminal, Frame};

use crate::config::PanelSpec;

pub enum Panel {
    Lazi(Lazi),
}

impl Panel {
    pub fn new(spec: &PanelSpec) -> Result<Self, String> {
        match spec {
            PanelSpec::Lazi { config, dir } => Ok(Panel::Lazi(Lazi::new(config.clone(), dir.clone())?)),
        }
    }

    /// Called when the panel gains focus.
    pub fn show(&mut self) {
        match self {
            Panel::Lazi(_) => {}
        }
    }

    /// The tab bar's label.
    pub fn name(&self) -> &'static str {
        match self {
            Panel::Lazi(_) => "lazi",
        }
    }

    /// The terminal title while this panel has focus.
    pub fn title(&self) -> String {
        match self {
            Panel::Lazi(p) => p.title(),
        }
    }

    pub fn wake_fds(&self) -> Vec<RawFd> {
        match self {
            Panel::Lazi(p) => p.wake_fds(),
        }
    }

    pub fn on_wake(&mut self) -> bool {
        match self {
            Panel::Lazi(p) => p.on_wake(),
        }
    }

    pub fn receive(&mut self, grace: Option<Duration>) -> bool {
        match self {
            Panel::Lazi(p) => p.receive(grace),
        }
    }

    pub fn key(&mut self, term: &mut DefaultTerminal, key: Key) -> io::Result<Outcome> {
        match self {
            Panel::Lazi(p) => p.key(term, key),
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        match self {
            Panel::Lazi(p) => p.draw(frame, area),
        }
    }

    pub fn sync_image(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.sync_image(out),
        }
    }

    /// Takes anything drawn outside ratatui's buffer (kitty images) off the screen.
    pub fn hide(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.hide(out),
        }
    }

    pub fn clear_images(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Panel::Lazi(p) => p.clear_images(out),
        }
    }
}
